//! Same-block wallet preflight for Monday's direct Router market path.
//! A passing eth_call proves only that submission is currently possible;
//! the venue settles the stock order asynchronously at an unknown price.
use super::{feed::{chain_guard, read_block, read_latest, Rpc},
    monday::{read_contract_state, MondayState},
    monday_public::{encode_market_call, MarketRequest, UnsimulatedMondayCall}};
use crate::{monad_contract::Catalog, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha3::{Digest, Keccak256};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketPreflight {
    pub call: UnsimulatedMondayCall,
    pub block_number: u64,
    pub block_hash: String,
    pub router_implementation_sha256: String,
    pub stock_implementation_sha256: String,
    pub input_balance_atoms: u128,
    pub input_allowance_atoms: u128,
    pub approval_required: bool,
    pub simulated: bool,
    pub settlement_pending_after_submission: bool,
}

fn selector(signature: &str) -> String {
    let hash = Keccak256::digest(signature.as_bytes());
    format!("0x{:02x}{:02x}{:02x}{:02x}", hash[0], hash[1], hash[2], hash[3])
}
fn address_word(value: &str) -> Result<String> {
    if value.len() != 42 || !value.starts_with("0x")
        || !value[2..].bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid preflight address".into());
    }
    Ok(format!("{:0>64}", &value[2..].to_ascii_lowercase()))
}
fn strict_u128(value: &str) -> Result<u128> {
    if value.len() != 66 || !value.starts_with("0x")
        || !value[2..34].bytes().all(|byte| byte == b'0')
        || !value[34..].bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid preflight uint256 result".into());
    }
    u128::from_str_radix(&value[34..], 16).map_err(|_| "preflight amount overflow".into())
}
fn token_call(rpc: &mut impl Rpc, token: &str, data: String, block_hash: &str) -> Result<u128> {
    let result = rpc.call("eth_call", json!([{"to":token,"data":data},block_hash]))?;
    strict_u128(result.as_str().ok_or("preflight ERC20 call absent")?)
}
fn read_wallet_input(rpc: &mut impl Rpc, call: &UnsimulatedMondayCall,
                     state: &MondayState) -> Result<(u128, u128)> {
    let owner = address_word(&call.from)?;
    let spender = address_word(&call.allowance_spender)?;
    let balance = token_call(rpc, &call.input_token,
        format!("{}{}", selector("balanceOf(address)"), owner), &state.block.hash)?;
    let allowance = token_call(rpc, &call.input_token,
        format!("{}{}{}", selector("allowance(address,address)"), owner, spender), &state.block.hash)?;
    Ok((balance, allowance))
}

pub fn preflight_market_call(rpc: &mut impl Rpc, catalog: &Catalog,
    request: &MarketRequest, now_secs: u64) -> Result<MarketPreflight> {
    let call = encode_market_call(catalog, request, now_secs)?;
    chain_guard(rpc)?;
    let block = read_latest(rpc)?;
    let state = read_contract_state(rpc, block)?;
    if state.block.timestamp > now_secs + 30 || now_secs.saturating_sub(state.block.timestamp) > 30 {
        return Err("Monday state is not fresh".into());
    }
    let (balance, allowance) = read_wallet_input(rpc, &call, &state)?;
    if balance < call.allowance_atoms {
        return Err("wallet input balance insufficient".into());
    }
    let approval_required = allowance < call.allowance_atoms;
    if !approval_required {
        let result = rpc.call("eth_call", json!([{
            "from":call.from, "to":call.to, "data":call.data, "value":"0x0"
        }, state.block.hash]))?;
        let raw = result.as_str().ok_or("Monday simulation result absent")?;
        let required = if matches!(request.side, super::monday_public::MarketSide::Buy) { 130 } else { 66 };
        if raw.len() != required || !raw.starts_with("0x")
            || !raw[2..].bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("Monday simulation return shape changed".into());
        }
    }
    if read_block(rpc, state.block.number)?.hash != state.block.hash {
        return Err("Monad block changed during market preflight".into());
    }
    Ok(MarketPreflight {
        call, block_number: state.block.number, block_hash: state.block.hash,
        router_implementation_sha256: state.router_implementation_sha256,
        stock_implementation_sha256: state.stock_implementation_sha256,
        input_balance_atoms: balance, input_allowance_atoms: allowance,
        approval_required, simulated: !approval_required,
        settlement_pending_after_submission: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_erc20_words_and_spender_encoding() {
        assert_eq!(selector("balanceOf(address)"), "0x70a08231");
        assert_eq!(selector("allowance(address,address)"), "0xdd62ed3e");
        assert_eq!(strict_u128(&format!("0x{}f", "0".repeat(63))).unwrap(), 15);
        assert!(strict_u128(&format!("0x1{}", "0".repeat(63))).is_err());
        assert_eq!(address_word("0x1111111111111111111111111111111111111111").unwrap().len(), 64);
    }
}
