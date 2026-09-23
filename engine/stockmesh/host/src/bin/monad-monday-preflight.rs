//! Read-only same-block wallet preflight. Never signs, approves or broadcasts.
use skew_execution_host::{monad::{feed::BoundedRpc,
    monday_public::MarketRequest, preflight_market::{preflight_market_call,
        preflight_market_budget, MarketBudgetRequest}},
    monad_contract::Catalog};
use std::{env, fs, path::Path, time::{SystemTime, UNIX_EPOCH}};

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 4 {
        return Err("usage: monad-monday-preflight CATALOG REQUEST OUTPUT".into());
    }
    let catalog: Catalog = serde_json::from_slice(&fs::read(&args[1]).map_err(|_| "catalog unreadable")?)
        .map_err(|_| "catalog malformed")?;
    let raw: serde_json::Value = serde_json::from_slice(&fs::read(&args[2]).map_err(|_| "request unreadable")?)
        .map_err(|_| "request malformed")?;
    let endpoint = match env::var("MONAD_RPC_URL_FILE") {
        Ok(path) => fs::read_to_string(path).map_err(|_| "Monad RPC file unreadable")?.trim().to_owned(),
        Err(_) => "https://rpc.monad.xyz".into(),
    };
    let mut rpc = BoundedRpc::new(endpoint, 32)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| "clock")?.as_secs();
    let result = if raw.get("orderAmountAtoms").is_some() {
        let request: MarketRequest = serde_json::from_value(raw).map_err(|_| "request malformed")?;
        preflight_market_call(&mut rpc, &catalog, &request, now)?
    } else {
        let request: MarketBudgetRequest = serde_json::from_value(raw).map_err(|_| "budget request malformed")?;
        preflight_market_budget(&mut rpc, &catalog, &request, now)?.1
    };
    let output = Path::new(&args[3]);
    let temp = output.with_extension("tmp");
    fs::write(&temp, serde_json::to_vec_pretty(&result).map_err(|_| "serialize")?)
        .map_err(|_| "write")?;
    fs::rename(&temp, output).map_err(|_| "rename")?;
    println!("market preflight: block={} approval_required={} simulated={} settlement=pending_after_submission",
        result.block_number, result.approval_required, result.simulated);
    Ok(())
}
fn main() { if let Err(error) = run() { eprintln!("monad-monday-preflight: {error}"); std::process::exit(1); } }
