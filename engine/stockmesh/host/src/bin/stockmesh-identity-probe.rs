//! Finite read-only comparison of the configured release, chain program and
//! issuer catalog. Endpoint stays behind ProviderConfig; no wallet or sender.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use skew_execution_host::provider::ProviderConfig;
use std::{fs, path::Path};

fn run() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 { return Err("stockmesh-identity-probe PROFILE PUBLIC_DEPLOYMENT OUTPUT".into()); }
    let config = ProviderConfig::load(Path::new(&args[1]))?;
    if config.max_requests > 20 { return Err("identity probe max 20 reads".into()); }
    let bytes = fs::read(&args[2])?;
    if bytes.len() > 1024 * 1024 { return Err("deployment size".into()); }
    let deployment: Value = serde_json::from_slice(&bytes)?;
    if deployment["network"] != "mainnet-beta" { return Err("deployment network".into()); }
    let program = deployment["settlementProgram"].as_str().ok_or("settlement program")?;
    let mut keys = vec![program.to_string()];
    for p in deployment["products"].as_array().ok_or("deployment products")? {
        let key = p["policy"].as_str().ok_or("product policy")?.to_string();
        if !keys.contains(&key) { keys.push(key); }
    }
    if keys.len() > 100 || keys.iter().any(|k| bs58::decode(k).into_vec().map_or(true, |b| b.len() != 32)) {
        return Err("deployment public addresses".into());
    }
    let rpc = config.connect()?;
    let response = rpc.call("getMultipleAccounts", json!([keys, {"encoding":"base64","commitment":"confirmed"}]))?;
    let values = response["value"].as_array().ok_or("account response")?;
    if values.len() != keys.len() { return Err("account count".into()); }
    let rows: Vec<_> = keys.iter().zip(values).map(|(key, account)| {
        json!({"address":key,"exists":!account.is_null(),"owner":account["owner"],"executable":account["executable"]})
    }).collect();
    let mut program_data = Value::Null;
    if let Some(encoded) = values[0]["data"][0].as_str() {
        let body = STANDARD.decode(encoded)?;
        if values[0]["owner"] == "BPFLoaderUpgradeab1e11111111111111111111111" && body.len() == 36 && body[..4] == 2u32.to_le_bytes() {
            let address = bs58::encode(&body[4..]).into_string();
            let result = rpc.call("getAccountInfo", json!([address,{"encoding":"base64","commitment":"confirmed","minContextSlot":response["context"]["slot"]}]))?;
            let account = &result["value"];
            let body = STANDARD.decode(account["data"][0].as_str().ok_or("program data")?)?;
            if body.len() < 45 || body[..4] != 3u32.to_le_bytes() { return Err("program data layout".into()); }
            program_data = json!({"address":address,"slot":result["context"]["slot"],"bytes":body.len(),
                "upgradeAuthorityPresent":body[12] != 0,"elfSha256":format!("{:x}",Sha256::digest(&body[45..]))});
        }
    }
    let report = json!({"schema":"stockmesh.configured-chain-identity/v1","observation":"PROVIDER_OBSERVED",
        "slot":response["context"]["slot"],"accounts":rows,"programData":program_data,
        "deploymentSha256":format!("{:x}",Sha256::digest(&bytes)),"metrics":rpc.provider_metrics(),
        "signedTransactions":0,"submittedTransactions":0});
    fs::write(&args[3], serde_json::to_vec_pretty(&report)?)?;
    println!("{}",report);
    Ok(())
}
fn main() {
    if let Err(error) = run() { eprintln!("identity probe failed: {error}"); std::process::exit(1); }
}
