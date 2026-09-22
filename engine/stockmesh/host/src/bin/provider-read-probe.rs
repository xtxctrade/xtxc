//! Bounded public-state probe. No wallet key, signature, sender or transaction.
use serde_json::json;
use skew_execution_host::{feed::Feed, provider::ProviderConfig, Result};
use std::{
    path::Path,
    time::{Duration, Instant},
};

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 2 {
        return Err("provider-read-probe PROFILE".into());
    }
    let config = ProviderConfig::load(Path::new(&args[1]))?;
    // Probe itself must stay small even if handed a long-running profile.
    if config.max_requests > 20 {
        return Err("probe run budget must be <= 20 requests".into());
    }
    let rpc = config.connect()?;
    let feed = Feed::new(
        vec![
            "SysvarC1ock11111111111111111111111111111111".into(),
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
        ],
        Duration::from_secs(5),
        1024 * 1024,
    )?;
    let began = Instant::now();
    let first = feed.refresh(&rpc)?;
    std::thread::sleep(Duration::from_secs(2));
    let second = feed.refresh(&rpc)?;
    if second.slot <= first.slot {
        return Err("provider bank did not advance during probe".into());
    }
    let denied = rpc.call("sendTransaction", json!([])).is_err();
    if !denied {
        return Err("read-only boundary failed".into());
    }
    println!(
        "{}",
        json!({
            "evidence":"LIVE_PROVIDER_OBSERVED_READ_ONLY", "firstSlot":first.slot, "secondSlot":second.slot,
            "accountCount":second.accounts.len(), "accountBytes":second.accounts.iter().map(|a| a.data.len()).sum::<usize>(),
            "elapsedMs":began.elapsed().as_millis(), "genesis":rpc.genesis(), "metrics":rpc.provider_metrics(),
            "writeDenied":denied, "transactionsSigned":0, "transactionsSubmitted":0,
            "directExecutionProven":false, "productGatePassed":false
        })
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("provider probe failed: {error}");
        std::process::exit(1);
    }
}
