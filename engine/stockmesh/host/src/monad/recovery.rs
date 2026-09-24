//! Bounded, read-only EVM attempt resolution. A wallet nonce advancing is not
//! proof of this order: only a canonical, finalized transaction whose full
//! body matches a registered attempt can close the economic intent.
use super::feed::{chain_guard, read_block, Rpc};
use crate::Result;
use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizedAttempt {
    pub hash: String,
    pub block_hash: String,
    pub block_number: u64,
    pub from: String,
    pub to: String,
    pub input: String,
    pub value: u128,
    pub nonce: u64,
    pub success: bool,
}

fn field<'a>(object: &'a Value, name: &str) -> Result<&'a str> {
    object.get(name).and_then(Value::as_str)
        .ok_or_else(|| format!("Monad transaction {name} missing"))
}

fn hex_bytes(value: &str, count: usize) -> Result<String> {
    if value.len() != 2 + 2 * count || !value.starts_with("0x")
        || !value[2..].bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid Monad transaction hex".into());
    }
    Ok(value.to_ascii_lowercase())
}

fn quantity(value: &str) -> Result<u128> {
    let digits = value.strip_prefix("0x").ok_or("invalid Monad quantity")?;
    if digits.is_empty() || digits.len() > 32 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid Monad quantity".into());
    }
    u128::from_str_radix(digits, 16).map_err(|_| "Monad quantity overflow".into())
}

/// None means not included, not failed or replaced. The caller must keep the
/// order unresolved and must not broadcast another economic transaction.
pub fn finalized_attempt(rpc: &mut impl Rpc, hash: &str) -> Result<Option<FinalizedAttempt>> {
    chain_guard(rpc)?;
    let hash = hex_bytes(hash, 32)?;
    let receipt = rpc.call("eth_getTransactionReceipt", json!([hash]))?;
    if receipt.is_null() { return Ok(None); }
    if hex_bytes(field(&receipt, "transactionHash")?, 32)? != hash {
        return Err("Monad receipt hash mismatch".into());
    }
    let block_hash = hex_bytes(field(&receipt, "blockHash")?, 32)?;
    let block_number = u64::try_from(quantity(field(&receipt, "blockNumber")?)?)
        .map_err(|_| "Monad block number overflow")?;
    let success = match field(&receipt, "status")? {
        "0x1" => true, "0x0" => false, _ => return Err("invalid Monad receipt status".into()),
    };
    let logs = receipt.get("logs").and_then(Value::as_array)
        .ok_or("Monad receipt logs missing")?;
    if logs.len() > 256 { return Err("Monad receipt log bound exceeded".into()); }
    for log in logs {
        if log.get("removed") == Some(&Value::Bool(true))
            || hex_bytes(field(log, "transactionHash")?, 32)? != hash
            || hex_bytes(field(log, "blockHash")?, 32)? != block_hash {
            return Err("Monad removed or foreign log".into());
        }
    }
    let transaction = rpc.call("eth_getTransactionByHash", json!([hash]))?;
    if hex_bytes(field(&transaction, "hash")?, 32)? != hash
        || hex_bytes(field(&transaction, "blockHash")?, 32)? != block_hash
        || quantity(field(&transaction, "blockNumber")?)? != block_number as u128 {
        return Err("Monad transaction body differs from receipt".into());
    }
    let from = hex_bytes(field(&transaction, "from")?, 20)?;
    let to = hex_bytes(field(&transaction, "to")?, 20)?;
    let input = field(&transaction, "input")?.to_ascii_lowercase();
    if !input.starts_with("0x") || input.len() > 16386 || input.len() % 2 != 0
        || !input[2..].bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid Monad transaction input".into());
    }
    let value = quantity(field(&transaction, "value")?)?;
    let nonce = u64::try_from(quantity(field(&transaction, "nonce")?)?)
        .map_err(|_| "Monad nonce overflow")?;
    if let Some(chain_id) = transaction.get("chainId").and_then(Value::as_str) {
        if quantity(chain_id)? != 143 { return Err("Monad transaction chain changed".into()); }
    }
    let finalized = rpc.call("eth_getBlockByNumber", json!(["finalized", false]))?;
    let finalized_number = quantity(field(&finalized, "number")?)?;
    if block_number as u128 > finalized_number { return Ok(None); }
    if read_block(rpc, block_number)?.hash != block_hash {
        return Err("Monad receipt no longer canonical".into());
    }
    Ok(Some(FinalizedAttempt { hash, block_hash, block_number,
        from, to, input, value, nonce, success }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    struct Fake { replies: VecDeque<Value> }
    impl Rpc for Fake {
        fn call(&mut self, _: &str, _: Value) -> Result<Value> {
            self.replies.pop_front().ok_or_else(|| "mock exhausted".into())
        }
        fn used(&self) -> u32 { 0 }
    }
    fn h(byte: char) -> String { format!("0x{}", byte.to_string().repeat(64)) }
    fn a(byte: char) -> String { format!("0x{}", byte.to_string().repeat(40)) }
    fn fixture(canonical: bool, removed: bool, finalized: bool) -> Fake {
        let hash = h('a'); let block = h('b');
        Fake { replies: VecDeque::from(vec![
            json!("0x8f"),
            json!({"transactionHash":hash,"blockHash":block,"blockNumber":"0x64",
                "status":"0x1","logs":[{"transactionHash":h('a'),"blockHash":h('b'),
                    "removed":removed}]}),
            json!({"hash":h('a'),"blockHash":h('b'),"blockNumber":"0x64",
                "from":a('1'),"to":a('2'),"input":"0x1234","value":"0x0",
                "nonce":"0x7","chainId":"0x8f"}),
            json!({"number":if finalized {"0x65"} else {"0x63"}}),
            json!({"number":"0x64","hash":if canonical {h('b')} else {h('c')},
                "parentHash":h('d'),"timestamp":"0x1"}),
        ]) }
    }
    #[test]
    fn only_finalized_canonical_and_unremoved_receipt_resolves() {
        let got = finalized_attempt(&mut fixture(true, false, true), &h('a')).unwrap().unwrap();
        assert_eq!(got.nonce, 7);
        assert!(got.success);
        assert!(finalized_attempt(&mut fixture(true, true, true), &h('a')).is_err());
        assert!(finalized_attempt(&mut fixture(false, false, true), &h('a')).is_err());
        assert!(finalized_attempt(&mut fixture(true, false, false), &h('a')).unwrap().is_none());
    }
}
