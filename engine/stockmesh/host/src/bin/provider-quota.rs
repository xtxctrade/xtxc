//! Explicit quota initialization/inspection. No network or endpoint-file read.
use skew_execution_host::{provider::ProviderConfig, provider_quota::DurableQuota, Result};
use std::path::Path;

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 || !matches!(args[1].as_str(), "init" | "status") {
        return Err("provider-quota init|status PROFILE".into());
    }
    let profile = ProviderConfig::load(Path::new(&args[2]))?;
    let quota = DurableQuota::new(
        profile
            .durable_quota
            .ok_or("profile has no durable quota")?,
        &profile.provider_id,
        &profile.expected_genesis,
    )?;
    if args[1] == "init" {
        quota.initialize()?;
    }
    println!(
        "{}",
        serde_json::to_string(&quota.snapshot()?).map_err(|_| "quota metrics encoding")?
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("provider quota: {error}");
        std::process::exit(1);
    }
}
