//! Canonical submission observer for wallet-owned Monday market orders.
//! Finalized submission proves only that the issuer accepted an order, not
//! that stock or cash was delivered. No browser-provided receipt is trusted.
use super::{feed::{chain_guard, Rpc}, journal::MonadJournal,
    monday_receipts::{decode_submission_exact, read_finalized_receipt},
    orders::{AttemptKind, Order, Phase}, recovery::finalized_attempt};
use crate::{monad_contract::Catalog, Result};
use serde_json::json;

fn finalized_nonce(rpc: &mut impl Rpc, owner: &str) -> Result<u64> {
    chain_guard(rpc)?;
    let raw = rpc.call("eth_getTransactionCount", json!([owner, "finalized"]))?;
    let value = raw.as_str().ok_or("finalized wallet nonce missing")?
        .strip_prefix("0x").ok_or("invalid finalized wallet nonce")?;
    u64::from_str_radix(value, 16).map_err(|_| "finalized wallet nonce overflow".into())
}

pub fn refresh_submission(rpc: &mut impl Rpc, catalog: &Catalog,
    journal: &mut MonadJournal, id: &str, owner: &str) -> Result<Order> {
    let order = journal.get(id, owner).ok_or("Monad order not found")?.clone();
    if order.market_issuer_order_id.is_some() {
        let block = order.finalized_block_hash.as_ref().ok_or("issuer submission missing finality")?;
        let hash = order.included_tx_hash.as_deref().or(order.tx_hash.as_deref())
            .ok_or("issuer submission missing transaction")?;
        let check = finalized_attempt(rpc, hash)?
            .ok_or("previously finalized issuer submission disappeared")?;
        if &check.block_hash != block || !check.success {
            return Err("previously finalized issuer submission changed".into());
        }
        return Ok(order);
    }
    let binding = order.market_binding.as_ref().ok_or("not a direct issuer order")?;
    let attempts = order.attempts();
    if attempts.is_empty() { return Err("order has no submitted transaction".into()); }
    let mut found = None;
    for attempt in &attempts {
        if let Some(outcome) = finalized_attempt(rpc, &attempt.hash)? {
            if found.replace((attempt, outcome)).is_some() {
                return Err("multiple same-nonce transactions finalized".into());
            }
        }
    }
    let Some((attempt, outcome)) = found else {
        if finalized_nonce(rpc, &order.intent.owner)? > order.tx_nonce.unwrap_or(u64::MAX) {
            journal.mark_unknown(id, owner)?;
            return Err("wallet nonce consumed by an unregistered transaction".into());
        }
        if order.phase == Phase::Submitted { journal.mark_unknown(id, owner)?; }
        return Err("Monday transaction not yet included".into());
    };
    if outcome.from != order.intent.owner.to_ascii_lowercase()
        || Some(outcome.nonce) != order.tx_nonce || outcome.value != 0 {
        return Err("finalized transaction does not bind wallet nonce".into());
    }
    match attempt.kind {
        AttemptKind::Cancellation => {
            if outcome.to != outcome.from || outcome.input != "0x" {
                return Err("wallet replacement is not a cancellation".into());
            }
            journal.resolve_attempt(id, &outcome.hash, &outcome.block_hash,
                outcome.success, AttemptKind::Cancellation).cloned()
        }
        AttemptKind::Execution => {
            if outcome.to != binding.call.to || outcome.input != binding.call.data {
                return Err("replacement calldata differs from prepared order".into());
            }
            if !outcome.success {
                return journal.resolve_attempt(id, &outcome.hash, &outcome.block_hash,
                    false, AttemptKind::Execution).cloned();
            }
            let receipt = read_finalized_receipt(rpc, &outcome.hash)?;
            if receipt.block_hash != outcome.block_hash || receipt.block_number != outcome.block_number {
                return Err("issuer receipt changed while reconciling".into());
            }
            let submitted = decode_submission_exact(catalog, &receipt,
                &binding.request, &binding.call)?;
            journal.observe_market_submission(id, &submitted).cloned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monad::{journal::MonadJournal,
        monday_public::{encode_market_call, MarketRequest, MarketSide},
        orders::{fixture_intent, MarketBinding}};
    use serde_json::{json, Value};
    use sha3::{Digest, Keccak256};
    use std::path::PathBuf;

    fn h(byte: char) -> String { format!("0x{}", byte.to_string().repeat(64)) }
    fn a(byte: char) -> String { format!("0x{}", byte.to_string().repeat(40)) }
    fn addr_word(address: &str) -> String { format!("{}{}", "0".repeat(24), &address[2..]) }
    fn uword(n: u128) -> String { format!("{n:064x}") }
    struct FakeRpc { original: String, replacement: String, transaction: Value,
        receipt: Value, nonce: &'static str }
    impl Rpc for FakeRpc {
        fn call(&mut self, method: &str, params: Value) -> Result<Value> {
            match method {
                "eth_chainId" => Ok(json!("0x8f")),
                "eth_getTransactionReceipt" => {
                    if params[0] == self.original { Ok(Value::Null) }
                    else if params[0] == self.replacement { Ok(self.receipt.clone()) }
                    else { Err("unexpected transaction".into()) }
                }
                "eth_getTransactionByHash" => Ok(self.transaction.clone()),
                "eth_getTransactionCount" => Ok(json!(self.nonce)),
                "eth_getBlockByNumber" if params[0] == "finalized" =>
                    Ok(json!({"number":"0x65"})),
                "eth_getBlockByNumber" => Ok(json!({"number":"0x64","hash":h('d'),
                    "parentHash":h('e'),"timestamp":"0x1"})),
                _ => Err("unexpected RPC method".into()),
            }
        }
        fn used(&self) -> u32 { 0 }
    }
    fn path() -> PathBuf {
        std::env::temp_dir().join(format!("xtxc-market-recovery-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .unwrap().as_nanos()))
    }
    fn setup(kind: AttemptKind) -> (Catalog, MonadJournal, String, FakeRpc, PathBuf) {
        let catalog: Catalog = serde_json::from_str(
            include_str!("../../../monad/catalog/registry.v1.json")).unwrap();
        let token = &catalog.token_observations[0];
        let owner = a('1');
        let request = MarketRequest { owner: owner.clone(),
            asset_id: format!("eip155:143:erc20:{}:{}:{}", token.token_address,
                token.issuer, token.issuer_product_id), side: MarketSide::Buy,
            wallet_debit_atoms: 10_000_000, order_amount_atoms: 9_000_000_000_000_000_000,
            deadline_secs: 1_800_000_100 };
        let call = encode_market_call(&catalog, &request, 1_800_000_000).unwrap();
        let binding = MarketBinding { request: request.clone(), call: call.clone(),
            simulated_block_hash: h('f'), router_implementation_sha256: "a".repeat(64),
            stock_implementation_sha256: "b".repeat(64) };
        let mut intent = fixture_intent("mon_observed", "idempotency_observed_0001");
        intent.owner = owner.clone();
        intent.asset_id = request.asset_id.clone();
        intent.quantity_atoms = request.order_amount_atoms.to_string();
        intent.max_input_atoms = request.wallet_debit_atoms.to_string();
        let path = path();
        let mut journal = MonadJournal::open(&path).unwrap();
        journal.prepare(Order::new_market(intent, binding).unwrap()).unwrap();
        let original = h('a'); let replacement = h('b'); let block = h('d');
        journal.report_submission("mon_observed", &owner, &original, 7).unwrap();
        journal.report_replacement("mon_observed", &owner, &replacement, 7, kind).unwrap();
        let to = if kind == AttemptKind::Cancellation { owner.clone() } else { call.to.clone() };
        let input = if kind == AttemptKind::Cancellation { "0x".into() } else { call.data.clone() };
        let transaction = json!({"hash":replacement,"blockHash":block,"blockNumber":"0x64",
            "from":owner,"to":to,"input":input,"value":"0x0","nonce":"0x7",
            "chainId":"0x8f"});
        let topic = format!("0x{:x}", Keccak256::digest(
            b"DepositAndMarketBuy(bytes32,bytes32,address,address,uint96,address,int96)"));
        let log = json!({"address":crate::monad::monday::ROUTER,
            "topics":[topic,h('c'),h('9'),format!("0x{}",addr_word(&owner))],
            "data":format!("0x{}{}{}{}",addr_word(&call.input_token),
                uword(request.wallet_debit_atoms),addr_word(&call.stock_token),
                uword(request.order_amount_atoms)),"transactionHash":h('b'),
            "blockHash":h('d'),"removed":false});
        let receipt = json!({"transactionHash":h('b'),"blockHash":h('d'),
            "blockNumber":"0x64","status":"0x1",
            "logs":if kind == AttemptKind::Cancellation { vec![] } else { vec![log] }});
        let rpc = FakeRpc { original, replacement, transaction, receipt, nonce:"0x8" };
        (catalog, journal, owner, rpc, path)
    }
    #[test]
    fn same_nonce_reprice_recovers_exact_submission_after_restart() {
        let (catalog, journal, owner, mut rpc, path) = setup(AttemptKind::Execution);
        drop(journal);
        let mut journal = MonadJournal::open(&path).unwrap();
        let got = refresh_submission(&mut rpc, &catalog, &mut journal,
            "mon_observed", &owner).unwrap();
        assert_eq!(got.phase, Phase::Finalized);
        assert_eq!(got.included_tx_hash.as_deref(), Some(h('b').as_str()));
        assert_eq!(got.market_issuer_order_id.as_deref(), Some(h('9').as_str()));
        drop(journal);
        let reopened = MonadJournal::open(&path).unwrap();
        assert_eq!(reopened.get("mon_observed", &owner).unwrap().phase, Phase::Finalized);
        drop(reopened);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(format!("{}.lock", path.display())).unwrap();
    }
    #[test]
    fn finalized_wallet_cancellation_never_becomes_stock_delivery() {
        let (catalog, mut journal, owner, mut rpc, path) = setup(AttemptKind::Cancellation);
        let got = refresh_submission(&mut rpc, &catalog, &mut journal,
            "mon_observed", &owner).unwrap();
        assert_eq!(got.phase, Phase::Cancelled);
        assert!(got.market_issuer_order_id.is_none());
        drop(journal);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(format!("{}.lock", path.display())).unwrap();
    }
    #[test]
    fn consumed_nonce_without_registered_receipt_stays_unknown() {
        let (catalog, mut journal, owner, mut rpc, path) = setup(AttemptKind::Execution);
        rpc.receipt = Value::Null;
        assert!(refresh_submission(&mut rpc, &catalog, &mut journal,
            "mon_observed", &owner).is_err());
        assert_eq!(journal.get("mon_observed", &owner).unwrap().phase, Phase::Unknown);
        drop(journal);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(format!("{}.lock", path.display())).unwrap();
    }
}
