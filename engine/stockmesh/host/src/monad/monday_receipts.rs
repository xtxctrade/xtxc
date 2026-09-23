//! Read-only Monday RWA receipt lineage. A router submission is not a fill;
//! a fill is not proof that stock or USDC reached the owner's wallet.
use super::{
    feed::{chain_guard, read_block, Rpc},
    monday::{ROUTER, STOCK},
    monday_public::{encode_market_call, MarketRequest, MarketSide, UnsimulatedMondayCall},
};
use crate::{monad_contract::Catalog, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha3::{Digest, Keccak256};

const MAX_LOGS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalReceipt {
    pub transaction_hash: String,
    pub transaction_from: String,
    pub transaction_to: String,
    pub transaction_input: String,
    pub transaction_value_atoms: u128,
    pub block_hash: String,
    pub block_number: u64,
    pub logs: Vec<ChainLog>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainLog {
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmittedOrder {
    pub order_id: String,
    pub owner: String,
    pub stock_token: String,
    pub side: Side,
    pub input_atoms: u128,
    pub requested_amount: i128,
    pub submission_tx: String,
    pub submission_block_hash: String,
    pub submission_block_number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettledOrder {
    pub order_id: String,
    pub owner: String,
    pub stock_token: String,
    pub side: Side,
    pub stock_delta_atoms: i128,
    pub executed_price_atoms: u128,
    pub mint_fee_atoms: u128,
    pub protocol_fee_atoms: u128,
    pub settlement_tx: String,
    pub settlement_block_hash: String,
    pub settlement_block_number: u64,
}

fn hex(raw: &str, bytes: usize) -> Result<String> {
    if raw.len() != bytes * 2 + 2
        || !raw.starts_with("0x")
        || !raw[2..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("invalid Monday receipt hex field".into());
    }
    Ok(raw.to_ascii_lowercase())
}

fn bytes(raw: &str) -> Result<Vec<u8>> {
    let body = raw.strip_prefix("0x").ok_or("missing receipt hex prefix")?;
    if body.len() % 2 != 0 || !body.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid receipt bytes".into());
    }
    (0..body.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&body[i..i + 2], 16).map_err(|_| "invalid byte".into()))
        .collect()
}

fn word(data: &[u8], index: usize) -> Result<&[u8]> {
    data.get(index * 32..(index + 1) * 32)
        .ok_or_else(|| "truncated Monday event".into())
}

fn word_u128(value: &[u8]) -> Result<u128> {
    if value.len() != 32 || value[..16].iter().any(|b| *b != 0) {
        return Err("Monday uint exceeds u128".into());
    }
    Ok(u128::from_be_bytes(
        value[16..].try_into().map_err(|_| "uint length")?,
    ))
}

fn word_i96(value: &[u8]) -> Result<i128> {
    if value.len() != 32 {
        return Err("Monday int96 length".into());
    }
    let negative = value[20] & 0x80 != 0;
    let extension = if negative { 0xff } else { 0 };
    if value[..20].iter().any(|b| *b != extension) {
        return Err("Monday int96 sign extension".into());
    }
    let mut magnitude = [0u8; 16];
    magnitude[4..].copy_from_slice(&value[20..]);
    let n = u128::from_be_bytes(magnitude) as i128;
    Ok(if negative { n - (1i128 << 96) } else { n })
}

fn word_address(value: &[u8]) -> Result<String> {
    if value.len() != 32 || value[..12].iter().any(|b| *b != 0) {
        return Err("invalid Monday event address".into());
    }
    Ok(format!("0x{}", encode(&value[12..])))
}

fn encode(input: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(input.len() * 2);
    for byte in input {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
}

fn event_topic(signature: &str) -> String {
    format!("0x{}", encode(&Keccak256::digest(signature.as_bytes())))
}

fn indexed_address(topic: &str) -> Result<String> {
    word_address(&bytes(&hex(topic, 32)?)?)
}

fn receipt_field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("Monday receipt {key} missing"))
}

fn hex_u64(raw: &str) -> Result<u64> {
    u64::from_str_radix(raw.strip_prefix("0x").ok_or("missing hex number")?, 16)
        .map_err(|_| "invalid receipt block number".into())
}

/// Read a successful receipt only after its block is at or below Monad's
/// finalized head and its numbered block still resolves to the same hash.
pub fn read_finalized_receipt(rpc: &mut impl Rpc, tx_hash: &str) -> Result<CanonicalReceipt> {
    chain_guard(rpc)?;
    let tx_hash = hex(tx_hash, 32)?;
    let raw = rpc.call("eth_getTransactionReceipt", json!([tx_hash]))?;
    if raw.is_null() {
        return Err("Monday transaction not yet included".into());
    }
    if receipt_field(&raw, "status")? != "0x1" {
        return Err("Monday transaction reverted".into());
    }
    let receipt_tx = hex(receipt_field(&raw, "transactionHash")?, 32)?;
    if receipt_tx != tx_hash {
        return Err("Monday transaction hash mismatch".into());
    }
    let block_hash = hex(receipt_field(&raw, "blockHash")?, 32)?;
    let block_number = hex_u64(receipt_field(&raw, "blockNumber")?)?;
    let transaction = rpc.call("eth_getTransactionByHash", json!([tx_hash]))?;
    let transaction_from = hex(receipt_field(&transaction, "from")?, 20)?;
    let transaction_to = hex(receipt_field(&transaction, "to")?, 20)?;
    let transaction_input = receipt_field(&transaction, "input")?.to_ascii_lowercase();
    if hex(receipt_field(&transaction, "hash")?, 32)? != tx_hash
        || hex(receipt_field(&transaction, "blockHash")?, 32)? != block_hash
        || hex_u64(receipt_field(&transaction, "blockNumber")?)? != block_number
        || bytes(&transaction_input)?.len() > 8192
    { return Err("Monday transaction body does not match receipt".into()); }
    let transaction_value_atoms = u128::from_str_radix(
        receipt_field(&transaction, "value")?.strip_prefix("0x").ok_or("invalid Monad transaction value")?, 16)
        .map_err(|_| "Monad transaction value exceeds u128")?;
    let finalized = rpc.call("eth_getBlockByNumber", json!(["finalized", false]))?;
    let finalized_number = hex_u64(receipt_field(&finalized, "number")?)?;
    if block_number > finalized_number {
        return Err("Monday transaction is not finalized".into());
    }
    if read_block(rpc, block_number)?.hash != block_hash {
        return Err("Monday receipt block is not canonical".into());
    }
    let source_logs = raw["logs"]
        .as_array()
        .ok_or("Monday receipt logs missing")?;
    if source_logs.len() > MAX_LOGS {
        return Err("Monday receipt log bound exceeded".into());
    }
    let mut logs = Vec::with_capacity(source_logs.len());
    for log in source_logs {
        if log["removed"] == true
            || hex(receipt_field(log, "transactionHash")?, 32)? != tx_hash
            || hex(receipt_field(log, "blockHash")?, 32)? != block_hash
        {
            return Err("Monday receipt contains removed or foreign log".into());
        }
        let address = hex(receipt_field(log, "address")?, 20)?;
        let topics = log["topics"]
            .as_array()
            .ok_or("Monday log topics missing")?
            .iter()
            .map(|t| hex(t.as_str().ok_or("Monday topic missing")?, 32))
            .collect::<Result<Vec<_>>>()?;
        if topics.len() > 4 {
            return Err("Monday log topic bound exceeded".into());
        }
        let data = receipt_field(log, "data")?.to_ascii_lowercase();
        if bytes(&data)?.len() > 8192 {
            return Err("Monday log data bound exceeded".into());
        }
        logs.push(ChainLog {
            address,
            topics,
            data,
        });
    }
    Ok(CanonicalReceipt {
        transaction_hash: tx_hash,
        transaction_from,
        transaction_to,
        transaction_input,
        transaction_value_atoms,
        block_hash,
        block_number,
        logs,
    })
}

pub fn decode_submission(
    receipt: &CanonicalReceipt,
    expected_owner: &str,
    expected_stock: &str,
    expected_usdc: &str,
    side: Side,
) -> Result<SubmittedOrder> {
    let expected_owner = hex(expected_owner, 20)?;
    let expected_stock = hex(expected_stock, 20)?;
    let expected_usdc = hex(expected_usdc, 20)?;
    let signature = match side {
        Side::Buy => "DepositAndMarketBuy(bytes32,bytes32,address,address,uint96,address,int96)",
        Side::Sell => "DepositStockAndMarketSell(bytes32,address,address,uint256,int96)",
    };
    let topic = event_topic(signature);
    let mut found = None;
    for log in &receipt.logs {
        if log.address != ROUTER || log.topics.first() != Some(&topic) {
            continue;
        }
        if log.topics.len() != 4 {
            return Err("Monday submission topics malformed".into());
        }
        let data = bytes(&log.data)?;
        let (order_id, owner, stock, input, amount) = match side {
            Side::Buy => {
                if data.len() != 128 {
                    return Err("Monday buy data malformed".into());
                }
                let token = word_address(word(&data, 0)?)?;
                if token != expected_usdc {
                    return Err("Monday buy input is not expected USDC".into());
                }
                (
                    log.topics[2].clone(),
                    indexed_address(&log.topics[3])?,
                    word_address(word(&data, 2)?)?,
                    word_u128(word(&data, 1)?)?,
                    word_i96(word(&data, 3)?)?,
                )
            }
            Side::Sell => {
                if data.len() != 64 {
                    return Err("Monday sell data malformed".into());
                }
                (
                    log.topics[1].clone(),
                    indexed_address(&log.topics[2])?,
                    indexed_address(&log.topics[3])?,
                    word_u128(word(&data, 0)?)?,
                    word_i96(word(&data, 1)?)?,
                )
            }
        };
        if owner != expected_owner || stock != expected_stock || input == 0 || amount == 0 {
            return Err("Monday submission does not match intended owner/stock/amount".into());
        }
        if found.is_some() {
            return Err("ambiguous Monday submission events".into());
        }
        found = Some(SubmittedOrder {
            order_id,
            owner,
            stock_token: stock,
            side,
            input_atoms: input,
            requested_amount: amount,
            submission_tx: receipt.transaction_hash.clone(),
            submission_block_hash: receipt.block_hash.clone(),
            submission_block_number: receipt.block_number,
        });
    }
    found.ok_or_else(|| "Monday order submission event absent".into())
}

/// Bind a canonical submission to the exact wallet calldata previously shown
/// to the owner. A smaller/larger order with the same owner and stock is not
/// the same intent; neither submission nor its order ID implies a fill.
pub fn decode_submission_exact(
    catalog: &Catalog,
    receipt: &CanonicalReceipt,
    request: &MarketRequest,
    call: &UnsimulatedMondayCall,
) -> Result<SubmittedOrder> {
    let expected = encode_market_call(catalog, request, request.deadline_secs.saturating_sub(1))?;
    if *call != expected
        || receipt.transaction_from != call.from
        || receipt.transaction_to != call.to
        || receipt.transaction_input != call.data
        || receipt.transaction_value_atoms != 0
    { return Err("Monday wallet transaction differs from proposal".into()); }
    let side = match request.side { MarketSide::Buy => Side::Buy, MarketSide::Sell => Side::Sell };
    let submitted = decode_submission(receipt, &request.owner, &call.stock_token,
        &call.input_token, side)?;
    let expected_amount = match side {
        Side::Buy => i128::try_from(request.order_amount_atoms).map_err(|_| "Monday buy amount overflow")?,
        Side::Sell => -i128::try_from(request.order_amount_atoms).map_err(|_| "Monday sell amount overflow")?,
    };
    if submitted.input_atoms != request.wallet_debit_atoms
        || submitted.requested_amount != expected_amount
    { return Err("Monday onchain amount differs from wallet proposal".into()); }
    Ok(submitted)
}

pub fn decode_settlement(
    submitted: &SubmittedOrder,
    receipt: &CanonicalReceipt,
) -> Result<SettledOrder> {
    if receipt.block_number < submitted.submission_block_number {
        return Err("Monday settlement precedes submission".into());
    }
    let topic = event_topic(
        "MarketOrderSettled(bytes32,address,address,address,int96,uint96,uint256,uint256)",
    );
    let mut found = None;
    for log in &receipt.logs {
        if log.address != STOCK || log.topics.first() != Some(&topic) {
            continue;
        }
        if log.topics.len() != 3 || log.topics[1] != submitted.order_id {
            continue;
        }
        let data = bytes(&log.data)?;
        if data.len() != 192 {
            return Err("Monday settlement data malformed".into());
        }
        let stock = word_address(word(&data, 0)?)?;
        let owner = word_address(word(&data, 1)?)?;
        let stock_delta = word_i96(word(&data, 2)?)?;
        let price = word_u128(word(&data, 3)?)?;
        if stock != submitted.stock_token
            || owner != submitted.owner
            || price == 0
            || match submitted.side {
                Side::Buy => stock_delta <= 0,
                Side::Sell => stock_delta >= 0,
            }
        {
            return Err("Monday settlement contradicts submitted order".into());
        }
        if found.is_some() {
            return Err("ambiguous Monday settlement events".into());
        }
        found = Some(SettledOrder {
            order_id: submitted.order_id.clone(),
            owner,
            stock_token: stock,
            side: submitted.side,
            stock_delta_atoms: stock_delta,
            executed_price_atoms: price,
            mint_fee_atoms: word_u128(word(&data, 4)?)?,
            protocol_fee_atoms: word_u128(word(&data, 5)?)?,
            settlement_tx: receipt.transaction_hash.clone(),
            settlement_block_hash: receipt.block_hash.clone(),
            settlement_block_number: receipt.block_number,
        });
    }
    found.ok_or_else(|| "Monday order has no settlement receipt".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monad::monday_public::{MarketRequest, MarketSide};
    use std::collections::VecDeque;

    struct MockRpc {
        responses: VecDeque<Value>,
        used: u32,
    }
    impl Rpc for MockRpc {
        fn call(&mut self, _: &str, _: Value) -> Result<Value> {
            self.used += 1;
            self.responses
                .pop_front()
                .ok_or_else(|| "unexpected mock RPC call".into())
        }
        fn used(&self) -> u32 {
            self.used
        }
    }

    fn h(byte: u8) -> String {
        format!("0x{}", encode(&[byte; 32]))
    }
    fn address(byte: u8) -> String {
        format!("0x{}", encode(&[byte; 20]))
    }
    fn aword(byte: u8) -> String {
        format!("{}{}", "00".repeat(12), encode(&[byte; 20]))
    }
    fn uword(value: u128) -> String {
        format!("{:064x}", value)
    }
    fn iword(value: i128) -> String {
        if value < 0 {
            format!("{}{:024x}", "ff".repeat(20), (1i128 << 96) + value)
        } else {
            uword(value as u128)
        }
    }
    fn receipt(logs: Vec<ChainLog>, hash: u8, number: u64) -> CanonicalReceipt {
        CanonicalReceipt {
            transaction_hash: h(hash),
            transaction_from: address(3),
            transaction_to: ROUTER.into(),
            transaction_input: "0x".into(),
            transaction_value_atoms: 0,
            block_hash: h(hash + 1),
            block_number: number,
            logs,
        }
    }
    fn buy_log(owner: u8, stock: u8, usdc: u8) -> ChainLog {
        ChainLog {
            address: ROUTER.into(),
            topics: vec![
                event_topic(
                    "DepositAndMarketBuy(bytes32,bytes32,address,address,uint96,address,int96)",
                ),
                h(1),
                h(2),
                format!("0x{}", aword(owner)),
            ],
            data: format!(
                "0x{}{}{}{}",
                aword(usdc),
                uword(100_000_000),
                aword(stock),
                iword(100_000_000)
            ),
        }
    }
    fn sell_log(owner: u8, stock: u8) -> ChainLog {
        ChainLog {
            address: ROUTER.into(),
            topics: vec![
                event_topic("DepositStockAndMarketSell(bytes32,address,address,uint256,int96)"),
                h(2), format!("0x{}", aword(owner)), format!("0x{}", aword(stock)),
            ],
            data: format!("0x{}{}", uword(100_000_000), iword(-100_000_000)),
        }
    }
    fn settled_log(owner: u8, stock: u8, delta: i128) -> ChainLog {
        ChainLog {
            address: STOCK.into(),
            topics: vec![
                event_topic("MarketOrderSettled(bytes32,address,address,address,int96,uint96,uint256,uint256)"),
                h(2), format!("0x{}", aword(9)),
            ],
            data: format!("0x{}{}{}{}{}{}", aword(stock), aword(owner), iword(delta), uword(123), uword(2), uword(3)),
        }
    }
    #[test]
    fn buy_requires_router_submission_then_matching_stock_settlement() {
        let owner = address(3);
        let stock = address(4);
        let usdc = address(5);
        let submitted = decode_submission(
            &receipt(vec![buy_log(3, 4, 5)], 10, 100),
            &owner,
            &stock,
            &usdc,
            Side::Buy,
        )
        .unwrap();
        assert_eq!(submitted.order_id, h(2));
        let settled =
            decode_settlement(&submitted, &receipt(vec![settled_log(3, 4, 10)], 12, 102)).unwrap();
        assert_eq!(settled.stock_delta_atoms, 10);
        assert_eq!(settled.mint_fee_atoms, 2);
        assert_eq!(settled.protocol_fee_atoms, 3);
    }
    #[test]
    fn exact_amount_and_direction_bind_to_wallet_proposal() {
        let mut catalog: Catalog = serde_json::from_str(include_str!("../../../monad/catalog/registry.v1.json")).unwrap();
        catalog.token_observations[0].token_address = address(4);
        catalog.chain.usdc = address(5);
        // A valid, different catalog asset identity is deliberately not
        // fabricated here: use the canonical 112-entry fixture for encoding,
        // then prove an amount mismatch cannot be attached to its receipt.
        let token = &catalog.token_observations[0];
        let request = MarketRequest {
            owner: address(3),
            asset_id: format!("eip155:143:erc20:{}:{}:{}", token.token_address, token.issuer, token.issuer_product_id),
            side: MarketSide::Buy, wallet_debit_atoms: 100_000_000,
            order_amount_atoms: 99_000_000_000_000_000_000, deadline_secs: 1_800_000_100,
        };
        let proposal = encode_market_call(&catalog, &request, 1_800_000_000).unwrap();
        let mut buy_receipt = receipt(vec![buy_log(3, 4, 5)], 10, 100);
        buy_receipt.logs[0].data = format!("0x{}{}{}{}", aword(5),
            uword(request.wallet_debit_atoms), aword(4), iword(request.order_amount_atoms as i128));
        buy_receipt.transaction_input = proposal.data.clone();
        let submitted = decode_submission_exact(&catalog, &buy_receipt,
            &request, &proposal).unwrap();
        assert_eq!(submitted.requested_amount, 99_000_000_000_000_000_000);
        let mut wrong = request.clone();
        wrong.wallet_debit_atoms = 101_000_000;
        wrong.order_amount_atoms = 100_000_000_000_000_000_000;
        let wrong_proposal = encode_market_call(&catalog, &wrong, 1_800_000_000).unwrap();
        assert!(decode_submission_exact(&catalog, &buy_receipt,
            &wrong, &wrong_proposal).is_err());
        let mut sell = request;
        sell.side = MarketSide::Sell;
        sell.wallet_debit_atoms = 1_000_000_000_000_000_000;
        sell.order_amount_atoms = 1_000_000_000_000_000_000;
        let sell_proposal = encode_market_call(&catalog, &sell, 1_800_000_000).unwrap();
        let mut sell_receipt = receipt(vec![sell_log(3, 4)], 10, 100);
        sell_receipt.logs[0].data = format!("0x{}{}", uword(sell.wallet_debit_atoms),
            iword(-(sell.order_amount_atoms as i128)));
        sell_receipt.transaction_input = sell_proposal.data.clone();
        assert!(decode_submission_exact(&catalog, &sell_receipt,
            &sell, &sell_proposal).is_ok());
        assert!(decode_submission_exact(&catalog, &buy_receipt,
            &sell, &sell_proposal).is_err());
        sell_receipt.transaction_from = address(8);
        assert!(decode_submission_exact(&catalog, &sell_receipt, &sell, &sell_proposal).is_err());
    }
    #[test]
    fn submission_is_not_fill_and_wrong_owner_or_direction_fails() {
        let owner = address(3);
        let stock = address(4);
        let usdc = address(5);
        let submitted = decode_submission(
            &receipt(vec![buy_log(3, 4, 5)], 10, 100),
            &owner,
            &stock,
            &usdc,
            Side::Buy,
        )
        .unwrap();
        assert!(decode_settlement(&submitted, &receipt(vec![], 11, 101)).is_err());
        assert!(
            decode_settlement(&submitted, &receipt(vec![settled_log(3, 4, -10)], 12, 102)).is_err()
        );
        assert!(
            decode_settlement(&submitted, &receipt(vec![settled_log(7, 4, 10)], 12, 102)).is_err()
        );
        assert!(decode_submission(
            &receipt(vec![buy_log(3, 4, 5)], 10, 100),
            &address(8),
            &stock,
            &usdc,
            Side::Buy
        )
        .is_err());
    }
    #[test]
    fn reject_malformed_signed_amount_and_duplicate_settlement() {
        let mut bad = buy_log(3, 4, 5);
        bad.data.replace_range(2 + 96 * 2..2 + 96 * 2 + 2, "ff");
        assert!(decode_submission(
            &receipt(vec![bad], 10, 100),
            &address(3),
            &address(4),
            &address(5),
            Side::Buy
        )
        .is_err());
        let submitted = decode_submission(
            &receipt(vec![buy_log(3, 4, 5)], 10, 100),
            &address(3),
            &address(4),
            &address(5),
            Side::Buy,
        )
        .unwrap();
        let duplicate = settled_log(3, 4, 10);
        assert!(decode_settlement(
            &submitted,
            &receipt(vec![duplicate.clone(), duplicate], 12, 102)
        )
        .is_err());
    }
    fn receipt_rpc(finalized_number: u64, canonical_hash: String, removed: bool) -> MockRpc {
        let tx = h(10);
        let included_hash = h(11);
        MockRpc {
            responses: VecDeque::from(vec![
                json!("0x8f"),
                json!({
                    "status":"0x1", "transactionHash":tx, "blockHash":included_hash,
                    "blockNumber":"0x64", "logs":[{
                        "address":ROUTER,"topics":[h(1)],"data":"0x",
                        "transactionHash":h(10),"blockHash":h(11),"removed":removed
                    }]
                }),
                json!({"hash":h(10),"from":address(3),"to":ROUTER,"input":"0x",
                    "value":"0x0","blockHash":h(11),"blockNumber":"0x64"}),
                json!({"number":format!("0x{finalized_number:x}")}),
                json!({
                    "number":"0x64", "hash":canonical_hash, "parentHash":h(12),
                    "timestamp":"0x123"
                }),
            ]),
            used: 0,
        }
    }
    #[test]
    fn finalized_receipt_requires_chain_canonicality_and_unremoved_logs() {
        let mut accepted = receipt_rpc(101, h(11), false);
        let output = read_finalized_receipt(&mut accepted, &h(10)).unwrap();
        assert_eq!(output.block_number, 100);
        assert_eq!(output.logs.len(), 1);
        assert_eq!(accepted.used(), 5);
        assert!(read_finalized_receipt(&mut receipt_rpc(99, h(11), false), &h(10)).is_err());
        assert!(read_finalized_receipt(&mut receipt_rpc(101, h(13), false), &h(10)).is_err());
        assert!(read_finalized_receipt(&mut receipt_rpc(101, h(11), true), &h(10)).is_err());
    }
}
