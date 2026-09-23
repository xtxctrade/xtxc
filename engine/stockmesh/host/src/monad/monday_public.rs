//! Exact, wallet-direct Monday RWA market-order calldata. This is an
//! *unsimulated proposal*, not a fill quote, a signature, or submission
//! authority. Monday settles a market order later; no stock minimum is
//! enforceable by these entrypoints.
use super::{monday::ROUTER, orders::address};
use crate::{monad_contract::{Catalog, MAINNET_CHAIN_ID}, Result};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

const BUY: &str = "depositAndMarketBuy(address,uint96,address,int96,uint32)";
const SELL: &str = "depositStockAndMarketSell(address,uint256,int96,uint32)";
const MAX_U96: u128 = (1u128 << 96) - 1;
const MAX_I96: u128 = (1u128 << 95) - 1;
const MAX_DEADLINE_WINDOW: u64 = 15 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MarketSide { Buy, Sell }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MarketRequest {
    pub owner: String,
    pub asset_id: String,
    pub side: MarketSide,
    /// BUY: wallet USDC atoms. SELL: wallet stock atoms.
    pub wallet_debit_atoms: u128,
    /// BUY: positive mUSD accounting atoms, independently supplied by the
    /// observed venue quote. SELL: exactly the stock amount being deposited.
    pub order_amount_atoms: u128,
    pub deadline_secs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UnsimulatedMondayCall {
    pub chain_id: u64,
    pub from: String,
    pub to: String,
    pub value_atoms: u8,
    pub data: String,
    pub input_token: String,
    pub stock_token: String,
    pub allowance_spender: String,
    pub allowance_atoms: u128,
    pub deadline_secs: u64,
    pub execution_class: String,
    pub guarantees_stock_minimum: bool,
}

fn hex_bytes(raw: &str) -> Result<Vec<u8>> {
    if !address(raw) { return Err("invalid Monday token address".into()); }
    (2..raw.len()).step_by(2)
        .map(|i| u8::from_str_radix(&raw[i..i+2], 16).map_err(|_| "invalid address hex".into()))
        .collect()
}
fn word(value: u128) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[16..].copy_from_slice(&value.to_be_bytes());
    out
}
fn address_word(value: &str) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(&hex_bytes(value)?);
    Ok(out)
}
fn signed_negative_word(magnitude: u128) -> Result<[u8; 32]> {
    if magnitude == 0 || magnitude > MAX_I96 { return Err("Monday sell amount exceeds int96".into()); }
    let mut out = [0xffu8; 32];
    let encoded = ((1u128 << 96) - magnitude).to_be_bytes();
    out[20..].copy_from_slice(&encoded[4..]);
    Ok(out)
}
fn encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
}

/// Observed identity alone is sufficient to *encode* a wallet proposal, but
/// not to admit a price, execute, or mark the product tradable. The caller
/// must separately obtain a current venue quote, check its implementation,
/// simulate, and ask the owner to sign. No transaction is broadcast here.
pub fn encode_market_call(
    catalog: &Catalog, request: &MarketRequest, now_secs: u64,
) -> Result<UnsimulatedMondayCall> {
    catalog.validate()?;
    if !address(&request.owner)
        || request.owner.eq_ignore_ascii_case("0x0000000000000000000000000000000000000000")
        || request.wallet_debit_atoms == 0 || request.order_amount_atoms == 0
        || request.deadline_secs <= now_secs
        || request.deadline_secs - now_secs > MAX_DEADLINE_WINDOW
        || request.deadline_secs > u32::MAX as u64
        || request.order_amount_atoms > MAX_I96
    { return Err("invalid Monday market-order bounds".into()); }
    let token = catalog.token_observations.iter().find(|token| {
        format!("eip155:{}:erc20:{}:{}:{}", token.chain_id, token.token_address,
            token.issuer, token.issuer_product_id) == request.asset_id
    }).ok_or("Monday token identity not observed")?;
    if token.chain_id != MAINNET_CHAIN_ID || token.token_decimals != 18 {
        return Err("Monday token chain or decimals changed".into());
    }
    let (signature, input_token, allowance_atoms) = match request.side {
        MarketSide::Buy => {
            if request.wallet_debit_atoms > MAX_U96 || request.order_amount_atoms > request.wallet_debit_atoms {
                return Err("Monday buy amount exceeds deposited USDC".into());
            }
            (BUY, catalog.chain.usdc.as_str(), request.wallet_debit_atoms)
        }
        MarketSide::Sell => {
            if request.wallet_debit_atoms != request.order_amount_atoms {
                return Err("Monday sell must deposit exact stock quantity".into());
            }
            (SELL, token.token_address.as_str(), request.wallet_debit_atoms)
        }
    };
    let hash = Keccak256::digest(signature.as_bytes());
    let mut data = Vec::with_capacity(4 + match request.side { MarketSide::Buy => 5, MarketSide::Sell => 4 } * 32);
    data.extend_from_slice(&hash[..4]);
    match request.side {
        MarketSide::Buy => {
            data.extend_from_slice(&address_word(&catalog.chain.usdc)?);
            data.extend_from_slice(&word(request.wallet_debit_atoms));
            data.extend_from_slice(&address_word(&token.token_address)?);
            data.extend_from_slice(&word(request.order_amount_atoms));
        }
        MarketSide::Sell => {
            data.extend_from_slice(&address_word(&token.token_address)?);
            data.extend_from_slice(&word(request.wallet_debit_atoms));
            data.extend_from_slice(&signed_negative_word(request.order_amount_atoms)?);
        }
    }
    data.extend_from_slice(&word(request.deadline_secs as u128));
    Ok(UnsimulatedMondayCall {
        chain_id: MAINNET_CHAIN_ID,
        from: request.owner.to_ascii_lowercase(), to: ROUTER.into(), value_atoms: 0,
        data: encode(&data), input_token: input_token.to_ascii_lowercase(),
        stock_token: token.token_address.to_ascii_lowercase(),
        allowance_spender: ROUTER.into(), allowance_atoms,
        deadline_secs: request.deadline_secs, execution_class: "ISSUER_ASYNC".into(),
        guarantees_stock_minimum: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn catalog() -> Catalog { serde_json::from_str(include_str!("../../../monad/catalog/registry.v1.json")).unwrap() }
    fn request(side: MarketSide) -> MarketRequest {
        let token = &catalog().token_observations[0];
        MarketRequest { owner: "0x1111111111111111111111111111111111111111".into(),
            asset_id: format!("eip155:143:erc20:{}:{}:{}", token.token_address, token.issuer, token.issuer_product_id),
            side, wallet_debit_atoms: 1_000_000, order_amount_atoms: 1_000_000,
            deadline_secs: 1_800_000_100 }
    }
    #[test]
    fn exact_public_router_buy_and_sell_abi() {
        let catalog = catalog();
        let buy = encode_market_call(&catalog, &request(MarketSide::Buy), 1_800_000_000).unwrap();
        assert_eq!(&buy.data[..10], "0x4e4bc420");
        assert_eq!(buy.data.len(), 2 + (4 + 5 * 32) * 2);
        assert_eq!(buy.input_token, catalog.chain.usdc);
        assert!(!buy.guarantees_stock_minimum);
        assert_eq!(buy.data, "0x4e4bc420000000000000000000000000754704bc059f8c67012fed69bc8a327a5aafb60300000000000000000000000000000000000000000000000000000000000f424000000000000000000000000017683e492d0c8910f7c0157d04af31cb7a23ad7100000000000000000000000000000000000000000000000000000000000f4240000000000000000000000000000000000000000000000000000000006b49d264");
        let sell = encode_market_call(&catalog, &request(MarketSide::Sell), 1_800_000_000).unwrap();
        assert_eq!(&sell.data[..10], "0x0e5d1a7a");
        assert_eq!(sell.data.len(), 2 + (4 + 4 * 32) * 2);
        assert!(sell.data[2 + (4 + 2 * 32) * 2..].starts_with(&"ff".repeat(20)));
        assert_eq!(sell.data, "0x0e5d1a7a00000000000000000000000017683e492d0c8910f7c0157d04af31cb7a23ad7100000000000000000000000000000000000000000000000000000000000f4240fffffffffffffffffffffffffffffffffffffffffffffffffffffffffff0bdc0000000000000000000000000000000000000000000000000000000006b49d264");
    }
    #[test]
    fn rejects_wrong_identity_amount_and_expiry() {
        let catalog = catalog();
        let mut buy = request(MarketSide::Buy);
        buy.asset_id.push('x');
        assert!(encode_market_call(&catalog, &buy, 1_800_000_000).is_err());
        buy = request(MarketSide::Buy);
        buy.order_amount_atoms = buy.wallet_debit_atoms + 1;
        assert!(encode_market_call(&catalog, &buy, 1_800_000_000).is_err());
        buy.order_amount_atoms = 1;
        buy.deadline_secs = 1_800_001_000;
        assert!(encode_market_call(&catalog, &buy, 1_800_000_000).is_err());
        let mut sell = request(MarketSide::Sell);
        sell.wallet_debit_atoms = 99;
        assert!(encode_market_call(&catalog, &sell, 1_800_000_000).is_err());
        sell.wallet_debit_atoms = MAX_I96 + 1;
        sell.order_amount_atoms = MAX_I96 + 1;
        assert!(encode_market_call(&catalog, &sell, 1_800_000_000).is_err());
    }
}
