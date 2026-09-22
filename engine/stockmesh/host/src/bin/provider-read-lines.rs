//! Bounded JSON-lines read conduit for the catalog compiler. No endpoint on
//! stdout/argv and no arbitrary RPC methods. One shared durable run budget.
use serde_json::{json, Value};
use skew_execution_host::provider::ProviderConfig;
use std::{io::{self, BufRead, Write}, path::Path};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("profile required")?;
    let config = ProviderConfig::load(Path::new(&path))?;
    if config.max_requests > 2000 { return Err("catalog capture limit 2000".into()); }
    let rpc = config.connect()?;
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    for _ in 0..config.max_requests {
        let mut line = Vec::new();
        // Do not allocate an unbounded line before validating it.
        loop {
            let bytes = input.fill_buf()?;
            if bytes.is_empty() { break; }
            let take = bytes.iter().position(|b| *b == b'\n').map_or(bytes.len(), |p| p + 1);
            if line.len() + take > 65_536 { return Err("request line bound".into()); }
            line.extend_from_slice(&bytes[..take]); input.consume(take);
            if line.last() == Some(&b'\n') { break; }
        }
        if line.is_empty() { break; }
        let v: Value = serde_json::from_slice(&line)?;
        let method = v["method"].as_str().ok_or("method")?;
        if !["getMultipleAccounts", "getAccountInfo"].contains(&method) { return Err("read method denied".into()); }
        if method == "getMultipleAccounts" && v["params"][0].as_array().is_none_or(|a| a.is_empty() || a.len() > 100) { return Err("account count".into()); }
        let response = match rpc.call(method, v["params"].clone()) {
            Ok(result) => json!({"result":result}),
            Err(error) => json!({"error": if error == "RPC response budget exceeded" { "RESPONSE_SIZE" }
                else if error.contains("rate limit") { "RATE_LIMIT" }
                else if error.contains("cooling down") || error.contains("429") { "COOLDOWN" }
                else if error.contains("budget") || error.contains("quota") { "BUDGET" }
                else { "PROVIDER_READ_FAILED" }}),
        };
        writeln!(output, "{response}")?; output.flush()?;
    }
    Ok(())
}
