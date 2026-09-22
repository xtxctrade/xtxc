//! Bounded mint-presence scan, not market admission or liquidity evidence.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use skew_execution_host::{catalog::Catalog, provider::ProviderConfig, Result};
use std::{path::Path, time::Duration};

fn inspect(row: &Value) -> Value {
    if row.is_null() {
        return json!({"status":"ACCOUNT_ABSENT"});
    }
    let owner = row["owner"].as_str().unwrap_or("");
    if row["executable"].as_bool() != Some(false)
        || !matches!(
            owner,
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
                | "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
        )
    {
        return json!({"status":"NOT_TOKEN_MINT_OWNER"});
    }
    let data = match row["data"].as_array() {
        Some(v) if v.len() == 2 && v[1] == "base64" => {
            v[0].as_str().and_then(|s| STANDARD.decode(s).ok())
        }
        _ => None,
    };
    let Some(bytes) = data else {
        return json!({"status":"INVALID_ACCOUNT_ENCODING"});
    };
    // Legacy mint is exactly 82 bytes; extension mint has a padded base and
    // the Token-2022 account-type discriminator at 165. Never admit a token
    // account simply because its first 82 bytes happen to look like a mint.
    let layout = bytes.len() == 82
        || (owner == "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
            && bytes.len() > 165
            && bytes[165] == 1);
    if !layout || bytes[45] != 1 || bytes[44] > 18 {
        return json!({"status":"MINT_LAYOUT_NOT_ADMITTED"});
    }
    json!({"status":"INITIALIZED_MINT_OBSERVED","tokenProgram":owner,"decimals":bytes[44],
        "accountBytes":bytes.len(),"extensionSemanticsValidated":false,"executionAdmitted":false})
}

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("stock-catalog-scan PROFILE CATALOG".into());
    }
    let profile = ProviderConfig::load(Path::new(&args[1]))?;
    if profile.max_requests > 20 {
        return Err("catalog scan budget must be <=20".into());
    }
    let catalog = Catalog::read(Path::new(&args[2]))?;
    let products: Vec<_> = catalog
        .products
        .iter()
        .filter(|p| p.chain == format!("solana:{}", profile.expected_genesis))
        .collect();
    if products.is_empty()
        || products.len() > 1000
        || products.len().div_ceil(80) + 1 > profile.max_requests as usize
    {
        return Err("catalog scan asset/request bounds".into());
    }
    let rpc = profile.connect()?;
    let mut rows = Vec::new();
    let mut minimum_slot = 0u64;
    for chunk in products.chunks(80) {
        std::thread::sleep(Duration::from_millis(1100));
        let addresses: Vec<_> = chunk.iter().map(|p| p.address.as_str()).collect();
        let response=rpc.call("getMultipleAccounts",json!([addresses,{"encoding":"base64","commitment":"confirmed","minContextSlot":minimum_slot}]))?;
        let slot = response["context"]["slot"]
            .as_u64()
            .filter(|s| *s >= minimum_slot)
            .ok_or("scan slot missing or regressed")?;
        minimum_slot = slot;
        let values = response["value"]
            .as_array()
            .filter(|v| v.len() == chunk.len())
            .ok_or("scan account response length")?;
        for (product, account) in chunk.iter().zip(values) {
            rows.push(json!({"productId":product.product_id,"instrument":product.instrument,"mint":product.address,"slot":slot,"observation":inspect(account)}));
        }
    }
    let present = rows
        .iter()
        .filter(|r| r["observation"]["status"] == "INITIALIZED_MINT_OBSERVED")
        .count();
    println!(
        "{}",
        json!({"schema":"skew.catalog-mint-observation/v1","catalogRevision":catalog.revision,
        "evidence":"LIVE_PROVIDER_OBSERVED_READ_ONLY","products":rows.len(),"initializedMintsObserved":present,
        "observations":rows,"singleAtomicSnapshot":false,"executionAdmitted":0,"liquidityProven":false,
        "metrics":rpc.provider_metrics(),"transactionsSigned":0,"transactionsSubmitted":0})
    );
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("catalog scan failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_types_never_upgrade_to_execution_admission() {
        assert_eq!(inspect(&Value::Null)["status"], "ACCOUNT_ABSENT");
        let mut data = vec![0; 82];
        data[44] = 9;
        data[45] = 1;
        let mut row = json!({"owner":"TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb","executable":false,"data":[STANDARD.encode(&data),"base64"]});
        assert_eq!(inspect(&row)["status"], "INITIALIZED_MINT_OBSERVED");
        assert_eq!(inspect(&row)["executionAdmitted"], false);
        data.resize(166, 0);
        data[165] = 2;
        row["data"][0] = json!(STANDARD.encode(&data));
        assert_eq!(inspect(&row)["status"], "MINT_LAYOUT_NOT_ADMITTED");
        data[165] = 1;
        row["data"][0] = json!(STANDARD.encode(&data));
        assert_eq!(inspect(&row)["status"], "INITIALIZED_MINT_OBSERVED");
        row["owner"] = json!("11111111111111111111111111111111");
        assert_eq!(inspect(&row)["status"], "NOT_TOKEN_MINT_OWNER");
    }
}
