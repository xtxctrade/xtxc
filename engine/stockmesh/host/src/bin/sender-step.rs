//! Isolated proof driver. stdin is signed PUBLIC wire + trusted compiler approval;
//! no private key or signer is accepted. This CLI is not a public admission API.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use skew_execution_host::{
    journal::Journal,
    model_store::ModelStore,
    receipt::CostModel,
    rpc::Rpc,
    sender::{authorize, Authorization, Sender},
};
use std::{
    io::{self, Read},
    path::PathBuf,
};
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: sender-step JOURNAL RPC EXPECTED_GENESIS".into());
    }
    let mut data = Vec::new();
    io::stdin()
        .take(16 * 1024 + 1)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    if data.len() > 16 * 1024 {
        return Err("request budget".into());
    }
    let v: Value = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
    let id = v["id"].as_str().ok_or("id")?;
    let mut journal = Journal::open(&PathBuf::from(&args[1]), 256, 16 * 1024 * 1024)?;
    if journal.get(id).is_none() {
        let wire = STANDARD
            .decode(v["wire"].as_str().ok_or("signed wire")?)
            .map_err(|e| e.to_string())?;
        let hash: Vec<u8> = serde_json::from_value(v["approved_message_hash"].clone())
            .map_err(|e| e.to_string())?;
        let resources: Vec<[u8; 32]> =
            serde_json::from_value(v["resources"].clone()).map_err(|e| e.to_string())?;
        let a = Authorization {
            intent_id: id.into(),
            message_hash: hash.try_into().map_err(|_| "hash length")?,
            last_valid_height: v["last_valid_height"].as_u64().ok_or("last valid height")?,
            resources,
        };
        journal.insert(authorize(&wire, a)?)?;
    }
    let mut sender = Sender {
        journal,
        rpc: Rpc::pinned(args[2].clone(), args[3].clone())?,
        max_attempts: 3,
    };
    let mut receipt = Value::Null;
    if v["reconcile"].as_bool() == Some(true) {
        if v.get("expected_receipt").is_some() && v.get("expected_exposure").is_some() {
            return Err("one receipt expectation type is required".into());
        }
        if let Some(x) = v.get("expected_exposure") {
            let expected: skew_execution_host::exposure_receipt::ExpectedExposure =
                serde_json::from_value(x.clone()).map_err(|e| e.to_string())?;
            receipt = expected
                .fetch(&sender.rpc, sender.journal.get(id).ok_or("entry missing")?)?
                .summary();
        }
        if let Some(x) = v.get("expected_receipt") {
            let expected = serde_json::from_value(x.clone()).map_err(|e| e.to_string())?;
            let verified = skew_execution_host::receipt::fetch(
                &sender.rpc,
                sender.journal.get(id).ok_or("entry missing")?,
                &expected,
            )?;
            let mut model = CostModel::default();
            let mut model_path = PathBuf::from(&args[1]).into_os_string();
            model_path.push(".model");
            let model_path = PathBuf::from(model_path);
            let mut durable = None;
            if model_path.exists() {
                let (store, recovered) = ModelStore::open(&model_path, 4 * 1024 * 1024)?;
                let state = recovered.ok_or("model snapshot missing")?;
                model.restore(state.cost.clone())?;
                durable = Some((store, state.fair));
            }
            assert!(model.observe(&verified)?);
            assert!(!model.observe(&verified)?);
            let model_generation = if let Some((mut store, fair)) = durable {
                Some(store.save(fair, model.state())?.generation)
            } else {
                None
            };
            receipt = verified.summary();
            receipt["routePenaltyBps"] = json!(model.penalty_bps(verified.route()));
            receipt["duplicateFeedbackIgnored"] = json!(true);
            receipt["durableFeedback"] = json!(model_path.exists());
            receipt["modelGeneration"] = json!(model_generation);
        }

        if receipt.is_null() {
            return Err("reconciliation requires a verified receipt".into());
        }
        sender
            .journal
            .update(id, skew_execution_host::journal::Phase::Reconciled, false)?;
    }
    let phase = sender.step(id)?;
    let e = sender.journal.get(id).unwrap();
    println!(
        "{}",
        json!({"id":id,"phase":format!("{phase:?}"),"signature":e.signature,"attempts":e.attempts,"same_signed_wire":true,"verifiedReceipt":receipt})
    );
    Ok(())
}
