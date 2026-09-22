//! Isolated/trusted compiler driver, not a public API. Phase one emits a review
//! of an unsigned message. Phase two accepts the owner's exact signed wire and
//! returns a durable entry; it never signs or sends a transaction.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use skew_execution_host::{
    fair,
    feed::Feed,
    journal::Journal,
    model_store::ModelStore,
    pipeline::{Limits, Prepared},
    receipt::CostModel,
    rpc::Rpc,
    stock::{EconomicIntent, StockContext},
    world::WorldConfig,
};
use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    keys: Vec<String>,
    world: WorldConfig,
    context: StockContext,
    fair_policy: fair::Policy,
    observations: Vec<fair::SignedObservation>,
    intent: EconomicIntent,
    limits: Limits,
    message: String,
}
fn now() -> Result<u64, String> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis(),
    )
    .map_err(|_| "clock overflow".into())
}
fn read(input: &mut impl BufRead) -> Result<Value, String> {
    let mut line = String::new();
    std::io::Read::take(&mut *input, 1024 * 1024 + 1)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    if line.len() > 1024 * 1024 {
        return Err("request bound".into());
    }
    serde_json::from_str(&line).map_err(|e| e.to_string())
}
fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: plan-gate RPC EXPECTED_GENESIS JOURNAL".into());
    }
    let mut input = io::stdin().lock();
    let request: Request = serde_json::from_value(read(&mut input)?).map_err(|e| e.to_string())?;
    let rpc = Rpc::pinned(args[1].clone(), args[2].clone())?;
    let feed = Feed::new(request.keys, Duration::from_secs(3), 4 * 1024 * 1024)?;
    let snapshot = feed.refresh(&rpc)?;
    let world = request.world.compile(&snapshot, None)?;
    let mut engine = fair::Engine::new(request.fair_policy)?;
    let mut model_path = PathBuf::from(&args[3]).into_os_string();
    model_path.push(".model");
    let (mut model_store, recovered) =
        ModelStore::open(&PathBuf::from(model_path), 4 * 1024 * 1024)?;
    let mut cost_model = CostModel::default();
    if let Some(state) = recovered {
        engine.restore_watermarks(state.fair)?;
        cost_model.restore(state.cost)?;
    }
    let decision = engine.evaluate(
        &request.context,
        &request.observations,
        snapshot.slot,
        now()?,
    )?;
    // Persist anti-replay/equivocation state before returning a review. The
    // same sequence+digest can be retried; a lower sequence or same-sequence
    // conflicting observation remains rejected across process restarts.
    model_store.save(engine.watermarks(), cost_model.state())?;
    drop(model_store);
    let strict = decision.tighten(&request.context, &request.intent, snapshot.slot, now()?)?;
    let native = world.quote_intent(&request.context, &strict)?;
    let message = STANDARD
        .decode(request.message)
        .map_err(|e| e.to_string())?;
    let mut candidates = vec![message.clone()];
    let decoded = skew_execution_host::pipeline::decode(&message)?;
    let account_keys = skew_execution_host::pipeline::resolved(&decoded, &snapshot)?;
    for ix in decoded.instructions() {
        if ix.data.first() != Some(&7) || ix.data.get(20) != Some(&9) {
            continue;
        }
        let graph = stocklana_adapters::graph::Graph::decode(&ix.data[20..], ix.accounts.len())
            .map_err(|_| "seed template")?;
        // Resource candidates contract the unexecuted residual around the native
        // proposal. They are alternatives for exact validation, not a claim of
        // globally optimal CU-constrained allocation.
        let leg = graph.legs[0].ok_or("seed leg")?;
        let local = leg.accounts[leg.venue.bindings(leg.direction).0] as usize;
        let pool = account_keys
            .get(ix.accounts[local] as usize)
            .ok_or("seed pool")?;
        let native_seed = native["legs"]
            .as_array()
            .ok_or("native legs")?
            .iter()
            .find(|l| l["pool"].as_str() == Some(pool.as_str()))
            .and_then(|l| l["inputAtoms"].as_u64())
            .ok_or("seed pool absent from native proposal")?;
        for seed in [
            (graph.input + native_seed) / 2,
            (3 * graph.input + native_seed) / 4,
        ] {
            if seed == 0 || seed >= graph.input {
                continue;
            }
            let mut variant = decoded.clone();
            let instructions = match &mut variant {
                solana_message::VersionedMessage::Legacy(m) => &mut m.instructions,
                solana_message::VersionedMessage::V0(m) => &mut m.instructions,
            };
            let target = instructions
                .iter_mut()
                .find(|i| i.data.first() == Some(&7))
                .ok_or("stock instruction")?;
            target.data[62..70].copy_from_slice(&seed.to_le_bytes());
            let bytes = variant.serialize();
            if !candidates.contains(&bytes) {
                candidates.push(bytes);
            }
        }
    }
    let mut selected = None;
    let mut checks = Vec::new();
    for message in candidates {
        match Prepared::simulate(
            &feed,
            &snapshot,
            &rpc,
            &request.context,
            &decision,
            &request.intent,
            &request.limits,
            &message,
            now()?,
        ) {
            Ok(p) => {
                let output = p.summary()["simulationOutput"]
                    .as_u64()
                    .ok_or("simulated output")?;
                checks.push(
                    json!({"output":output,"cu":p.summary()["simulationCU"],"admitted":true}),
                );
                if selected
                    .as_ref()
                    .is_none_or(|(_, _, prior)| output > *prior)
                {
                    selected = Some((p, message, output));
                }
            }
            Err(e) => checks.push(json!({"admitted":false,"reason":e})),
        }
    }
    let (prepared, message, _) =
        selected.ok_or_else(|| format!("no executable candidate: {checks:?}"))?;
    println!(
        "{}",
        json!({"prepared":prepared.summary(),"native":native,"exactCandidates":checks,"message":STANDARD.encode(message),"expected":prepared.expected()})
    );
    io::stdout().flush().map_err(|e| e.to_string())?;
    let second = read(&mut input)?;
    let wire = STANDARD
        .decode(second["wire"].as_str().ok_or("owner signed wire")?)
        .map_err(|e| e.to_string())?;
    let entry = prepared.authorize(
        &feed,
        &snapshot,
        &request.context,
        &decision,
        &wire,
        second["id"].as_str().ok_or("id")?.into(),
        second["last_valid_height"]
            .as_u64()
            .ok_or("blockhash height")?,
        now()?,
    )?;
    let mut journal = Journal::open(&PathBuf::from(&args[3]), 256, 16 * 1024 * 1024)?;
    journal.insert(entry)?;
    println!("{}", json!({"authorized":true,"submission":false}));
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1)
    }
}
