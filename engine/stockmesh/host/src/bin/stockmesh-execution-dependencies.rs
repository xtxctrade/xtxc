//! Expand quote-only market configs into complete same-bank CPI dependencies.
//!
//! This helper is run on trusted AWS while authoring a captured/live manifest.
//! It performs no RPC, signing, simulation or submission.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use skew_execution_host::{
    feed::{Account, Snapshot},
    native_wire,
    world::WorldConfig,
};
use std::{collections::BTreeSet, env, fs, path::PathBuf, time::Instant};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Probe {
    keys: Vec<String>,
    response: Value,
    worlds: Vec<WorldConfig>,
}

fn snapshot(probe: &Probe) -> Result<Snapshot, String> {
    if probe.keys.is_empty() || probe.keys.len() > 100 {
        return Err("dependency probe key bound".into());
    }
    let mut seen = BTreeSet::new();
    if probe.keys.iter().any(|key| !seen.insert(key)) {
        return Err("dependency probe duplicate key".into());
    }
    let slot = probe.response["context"]["slot"]
        .as_u64()
        .ok_or("dependency probe slot")?;
    let values = probe.response["value"]
        .as_array()
        .ok_or("dependency probe values")?;
    if values.len() != probe.keys.len() {
        return Err("dependency probe account count".into());
    }
    let accounts = probe
        .keys
        .iter()
        .zip(values)
        .map(|(key, value)| {
            if value.is_null() || value["data"][1] != "base64" {
                return Err("dependency probe missing/encoding".to_string());
            }
            Ok(Account {
                key: key.clone(),
                owner: value["owner"]
                    .as_str()
                    .ok_or("dependency probe owner")?
                    .to_owned(),
                executable: value["executable"]
                    .as_bool()
                    .ok_or("dependency probe executable")?,
                lamports: value["lamports"]
                    .as_u64()
                    .ok_or("dependency probe lamports")?,
                data: STANDARD
                    .decode(value["data"][0].as_str().ok_or("dependency probe data")?)
                    .map_err(|_| "dependency probe base64")?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Snapshot {
        slot,
        generation: 1,
        hash: [0; 32],
        accounts,
        observed: Instant::now(),
        slot_advanced: Instant::now(),
        revision: 1,
    })
}

fn main() -> Result<(), String> {
    let args = env::args().collect::<Vec<_>>();
    if args.len() != 2 && !(args.len() == 5 && args[2] == "--quote") {
        return Err("usage: stockmesh-execution-dependencies PROBE_JSON [--quote CASH_ATOMS PRODUCT_ATOMS]".into());
    }
    let path = PathBuf::from(&args[1]);
    if !path.is_file()
        || path
            .metadata()
            .map_err(|_| "dependency probe metadata")?
            .len()
            > 8 * 1024 * 1024
    {
        return Err("dependency probe file bound".into());
    }
    let mut probe: Probe =
        serde_json::from_slice(&fs::read(path).map_err(|_| "dependency probe read")?)
            .map_err(|_| "dependency probe JSON")?;
    if probe.worlds.is_empty() || probe.worlds.len() > 3 {
        return Err("dependency probe world bound".into());
    }
    let bank = snapshot(&probe)?;
    if args.len() == 5 {
        if probe.worlds.len() != 1 { return Err("native pair probe requires one world".into()); }
        let amounts = args[3..].iter().map(|v| v.parse::<u64>()
            .ok().filter(|v| (1..=500_000_000_000).contains(v))
            .ok_or_else(|| "native pair probe amount".to_string())).collect::<Result<Vec<_>,_>>()?;
        let mut quotes = Vec::new();
        for (index, world) in [probe.worlds[0].clone(), probe.worlds[0].reversed()].iter().enumerate() {
            let first = world.markets.first().ok_or("native pair probe market")?;
            let result = world.probe_declared_pair(&bank, amounts[index]);
            quotes.push(match result {
                Ok(value) => json!({"inputMint":first.input_mint,"inputAtoms":amounts[index],"outputMint":first.output_mint,"quote":value}),
                Err(error) => json!({"inputMint":first.input_mint,"inputAtoms":amounts[index],"outputMint":first.output_mint,"error":error}),
            });
        }
        println!("{}",json!({"schema":"stockmesh.native-pair-probe/v1","slot":bank.slot,"quotes":quotes,"executionAdmitted":false}));
        return Ok(());
    }
    let mut all = BTreeSet::new();
    for world in &mut probe.worlds {
        if !world.execution_keys.is_empty() {
            return Err("dependency probe already expanded".into());
        }
        let quote_keys = world.keys()?.into_iter().collect::<BTreeSet<_>>();
        let mut execution = BTreeSet::new();
        for market in &world.markets {
            execution.extend(
                native_wire::execution_dependencies(market, &bank)?
                    .into_iter()
                    .map(|key| key.to_string()),
            );
        }
        execution.retain(|key| !quote_keys.contains(key));
        if execution.len() > 64 {
            return Err("dependency probe execution key bound".into());
        }
        world.execution_keys = execution.into_iter().collect();
        all.extend(world.keys()?);
    }
    if all.is_empty() || all.len() > 100 {
        return Err("dependency probe expanded bank bound".into());
    }
    println!(
        "{}",
        json!({
            "schema":"skew.stockmesh.execution-dependencies/v1",
            "slot":bank.slot,
            "worlds":probe.worlds,
            "allKeys":all,
            "signedTransactions":0,
            "submittedTransactions":0
        })
    );
    Ok(())
}
