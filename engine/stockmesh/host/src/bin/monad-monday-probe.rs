//! Bounded, read-only contract check. No quote, signing or submission.
use skew_execution_host::monad::{
    feed::{chain_guard, read_latest, BoundedRpc},
    monday::read_contract_state,
};
use std::{env, fs, path::Path};
fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        return Err("usage: monad-monday-probe OUTPUT".into());
    }
    let endpoint = match env::var("MONAD_RPC_URL_FILE") {
        Ok(path) => fs::read_to_string(path)
            .map_err(|_| "Monad RPC file unreadable")?
            .trim()
            .to_owned(),
        Err(_) => "https://rpc.monad.xyz".into(),
    };
    let mut rpc = BoundedRpc::new(endpoint, 12)?;
    chain_guard(&mut rpc)?;
    let block = read_latest(&mut rpc)?;
    let state = read_contract_state(&mut rpc, block)?;
    let output = Path::new(&args[1]);
    let temp = output.with_extension("tmp");
    fs::write(
        &temp,
        serde_json::to_vec_pretty(&state).map_err(|_| "serialize")?,
    )
    .map_err(|_| "write")?;
    fs::rename(&temp, output).map_err(|_| "rename")?;
    println!(
        "Monday RWA contracts: block={} router/stock/cashier verified; executable quote={}",
        state.block.number, state.quote_admitted
    );
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("monad-monday-probe: {e}");
        std::process::exit(1);
    }
}
