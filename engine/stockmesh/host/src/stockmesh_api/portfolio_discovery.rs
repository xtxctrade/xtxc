//! Wallet visibility is broader than execution admission. Catalog tokens remain
//! read-only until their exact issuer/mint is independently admitted to a bank.
use super::*;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DiscoveredHolding {
    pub instrument: String,
    pub name: String,
    pub product_id: String,
    pub issuer: String,
    pub mint: String,
    pub token_program: String,
    pub raw_decimals: u8,
    pub atoms: String,
    pub state_slot: u64,
    pub trade_allowed: bool,
}

pub(super) fn collect(
    response: &Value, owner: &str, token_program: &str,
    catalog: &BTreeMap<String, Product>, admitted: &BTreeMap<String, PortfolioProduct>,
    result: &mut BTreeMap<String, DiscoveredHolding>,
) -> Result<()> {
    let slot = response["context"]["slot"].as_u64().filter(|s| *s > 0).ok_or("wallet discovery slot")?;
    let accounts = response["value"].as_array().filter(|v| v.len() <= 8192).ok_or("wallet discovery account bound")?;
    let mut seen_accounts = BTreeSet::new();
    for row in accounts {
        let account = &row["account"];
        let info = &account["data"]["parsed"]["info"];
        let mint = info["mint"].as_str().ok_or("wallet discovery mint")?;
        let Some(product) = catalog.get(mint) else { continue; };
        if admitted.contains_key(mint) { continue; }
        let pubkey = row["pubkey"].as_str().ok_or("wallet discovery account identity")?;
        decode_key(pubkey)?;
        if !seen_accounts.insert(pubkey) || account["owner"].as_str() != Some(token_program)
            || info["owner"].as_str() != Some(owner)
            || !matches!(token_program, TOKEN_PROGRAM | TOKEN_2022_PROGRAM) {
            return Err("wallet discovery owner/program/duplicate".into());
        }
        let decimals = info["tokenAmount"]["decimals"].as_u64().filter(|n| *n <= 12).ok_or("wallet discovery precision")? as u8;
        let atoms = info["tokenAmount"]["amount"].as_str().ok_or("wallet discovery amount")?.parse::<u64>().map_err(|_| "wallet discovery amount range")?;
        if let Some(previous) = result.get_mut(mint) {
            if previous.raw_decimals != decimals || previous.token_program != token_program { return Err("wallet discovery conflicting identity".into()); }
            let sum = previous.atoms.parse::<u64>().map_err(|_| "wallet discovery total")?.checked_add(atoms).ok_or("wallet discovery overflow")?;
            previous.atoms = sum.to_string();
        } else if atoms > 0 {
            result.insert(mint.into(), DiscoveredHolding {
                instrument: product.instrument.clone(), name: product.name.clone(), product_id: product.product_id.clone(),
                issuer: product.issuer.clone(), mint: mint.into(), token_program: token_program.into(),
                raw_decimals: decimals, atoms: atoms.to_string(), state_slot: slot, trade_allowed: false,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn product() -> Product { Product {
        product_id: "catalog:test:wallet".into(), instrument: "NEW".into(), name: "Existing wallet stock".into(),
        kind: AssetKind::Equity, issuer: "Issuer".into(), chain: format!("solana:{MAINNET_GENESIS}"),
        address: Pubkey::new_from_array([3; 32]).to_string(), execution_class: ExecutionClass::SolanaNative,
        source_kind: SourceKind::IssuerCatalog, source_url: "https://issuer.example/catalog".into(),
        source_sha256: "a".repeat(64), observed_at: 1, issuer_tradable: Some(true), fractional: None, rights_hash: None,
    } }
    fn account(index: u8, owner: &str, mint: &str, amount: &str) -> Value {
        json!({"pubkey":Pubkey::new_from_array([index;32]).to_string(),"account":{"owner":TOKEN_2022_PROGRAM,
            "data":{"parsed":{"info":{"owner":owner,"mint":mint,"tokenAmount":{"amount":amount,"decimals":8}}}}}})
    }
    #[test]
    fn catalog_wallet_balance_is_exact_and_never_trade_authority() {
        let p = product(); let catalog = BTreeMap::from([(p.address.clone(), p.clone())]);
        let owner = Pubkey::new_from_array([2;32]).to_string(); let mut out = BTreeMap::new();
        let input = json!({"context":{"slot":12},"value":[account(4,&owner,&p.address,"9007199254740993"),account(5,&owner,&p.address,"2")]});
        collect(&input,&owner,TOKEN_2022_PROGRAM,&catalog,&BTreeMap::new(),&mut out).unwrap();
        assert_eq!(out[&p.address].atoms,"9007199254740995"); assert!(!out[&p.address].trade_allowed);
        assert_eq!(out[&p.address].state_slot,12);
    }
    #[test]
    fn duplicate_wrong_owner_program_and_decimal_conflict_are_rejected() {
        let p = product(); let catalog = BTreeMap::from([(p.address.clone(), p.clone())]);
        let owner = Pubkey::new_from_array([2;32]).to_string(); let row = account(4,&owner,&p.address,"10");
        let mut wrong_owner = row.clone(); wrong_owner["account"]["data"]["parsed"]["info"]["owner"] = json!("another-owner");
        let mut wrong_program = row.clone(); wrong_program["account"]["owner"] = json!(TOKEN_PROGRAM);
        let mut wrong_decimals = account(5,&owner,&p.address,"10"); wrong_decimals["account"]["data"]["parsed"]["info"]["tokenAmount"]["decimals"] = json!(9);
        for rows in [vec![row.clone(),row.clone()],vec![wrong_owner],vec![wrong_program],vec![row,wrong_decimals]] {
            assert!(collect(&json!({"context":{"slot":12},"value":rows}),&owner,TOKEN_2022_PROGRAM,&catalog,&BTreeMap::new(),&mut BTreeMap::new()).is_err());
        }
    }
}
