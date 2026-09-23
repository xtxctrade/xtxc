//! One-shot read-only state inventory. No subscriptions, signer or sender.
use skew_execution_host::{
    monad::feed::{chain_guard, read_latest, scan_catalog, BoundedRpc},
    monad_contract::Catalog,
};
use std::{env, fs, path::Path};

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        return Err("usage: monad-market-probe CATALOG OUTPUT".into());
    }
    let endpoint = match env::var("MONAD_RPC_URL_FILE") {
        Ok(path) => fs::read_to_string(path)
            .map_err(|_| "Monad RPC file unreadable")?
            .trim()
            .to_owned(),
        Err(_) => "https://rpc.monad.xyz".into(),
    };
    let catalog: Catalog =
        serde_json::from_slice(&fs::read(&args[1]).map_err(|_| "catalog unreadable")?)
            .map_err(|_| "catalog malformed")?;
    let mut rpc = BoundedRpc::new(endpoint, 128)?;
    chain_guard(&mut rpc)?;
    let block = read_latest(&mut rpc)?;
    let snapshot = scan_catalog(&mut rpc, &catalog, block)?;
    let output = Path::new(&args[2]);
    let temp = output.with_extension("tmp");
    let data = serde_json::to_vec_pretty(&snapshot).map_err(|_| "snapshot serialization")?;
    fs::write(&temp, data).map_err(|_| "snapshot temporary write")?;
    fs::rename(&temp, output).map_err(|_| "snapshot atomic rename")?;
    println!(
        "read-only Monad state: token identities={}, deployed={}, calls={}, BUY=0, SELL=0, ETF=0",
        snapshot.tokens.len(),
        snapshot.tokens.iter().filter(|t| t.deployed).count(),
        snapshot.provider_calls + 2
    );
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("monad-market-probe: {e}");
        std::process::exit(1);
    }
}
