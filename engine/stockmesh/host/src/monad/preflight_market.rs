//! Same-block wallet preflight for Monday's direct Router market path.
//! A passing eth_call proves only that submission is currently possible;
//! the venue settles the stock order asynchronously at an unknown price.
use super::{feed::{chain_guard, read_block, read_latest, Rpc},
    monday::{read_contract_state, MondayState, CASHIER, STOCK},
    monday_public::{encode_market_call, MarketRequest, MarketSide, UnsimulatedMondayCall}};
use crate::{monad_contract::Catalog, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha3::{Digest, Keccak256};

const CASH_CENT_WAD: u128 = 10_000_000_000_000_000;
const MIN_MINT_FEE_WAD: u128 = 20_000_000_000_000_000;
const FEE_DENOMINATOR: u128 = 1_000_000;
const MAX_I96: u128 = (1u128 << 95) - 1;
const MAX_U96: u128 = (1u128 << 96) - 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MarketBudgetRequest {
    pub owner: String,
    pub asset_id: String,
    pub side: MarketSide,
    /// BUY: USDC 6-decimal atoms; SELL: stock 18-decimal atoms.
    pub wallet_debit_atoms: u128,
    pub deadline_secs: u64,
}

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

fn ceil_fee(amount: u128, rate: u128) -> Result<u128> {
    amount.checked_mul(rate).and_then(|n| n.checked_add(FEE_DENOMINATOR - 1))
        .map(|n| n / FEE_DENOMINATOR).ok_or("Monday fee arithmetic overflow".into())
}

fn affordable_order(credit: u128, mint_rate: u128, protocol_rate: u128,
                    min_order_value: u128) -> Result<u128> {
    if mint_rate > FEE_DENOMINATOR || protocol_rate > FEE_DENOMINATOR {
        return Err("Monday fee rate out of bounds".into());
    }
    let mut low = 0u128;
    let mut high = credit.min(MAX_I96) / CASH_CENT_WAD;
    while low < high {
        let mid = low + (high - low + 1) / 2;
        let amount = mid.checked_mul(CASH_CENT_WAD).ok_or("Monday order overflow")?;
        let protocol_fee = ceil_fee(amount, protocol_rate)?;
        let mint_fee = ceil_fee(amount, mint_rate)?.max(MIN_MINT_FEE_WAD);
        let cost = amount.checked_add(protocol_fee).and_then(|n| n.checked_add(mint_fee))
            .ok_or("Monday order cost overflow")?;
        if cost <= credit { low = mid; } else { high = mid - 1; }
    }
    let amount = low.checked_mul(CASH_CENT_WAD).ok_or("Monday order overflow")?;
    if amount == 0 || amount < min_order_value { return Err("Monday budget below minimum order".into()); }
    Ok(amount)
}

fn buy_order_from_budget(rpc: &mut impl Rpc, state: &MondayState,
                         catalog: &Catalog, budget_atoms: u128) -> Result<u128> {
    if budget_atoms == 0 || budget_atoms > MAX_U96 { return Err("Monday budget invalid".into()); }
    let fee_raw = rpc.call("eth_call", json!([{"to":STOCK,"data":selector("getFeeInfo()")}, state.block.hash]))?;
    let fee_hex = fee_raw.as_str().ok_or("Monday fee info absent")?;
    if fee_hex.len() != 258 || !fee_hex.starts_with("0x") { return Err("Monday fee info ABI changed".into()); }
    let values = (0..4).map(|index| strict_u128(&format!("0x{}", &fee_hex[2+index*64..2+(index+1)*64])))
        .collect::<Result<Vec<_>>>()?;
    let data = format!("{}{}{}", selector("tokenToBalanceInstant(address,uint96)"),
        address_word(&catalog.chain.usdc)?, format!("{budget_atoms:064x}"));
    let credit = token_call(rpc, CASHIER, data, &state.block.hash)?;
    affordable_order(credit, values[0], values[1], values[2])
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

fn preflight_at_state(rpc: &mut impl Rpc, catalog: &Catalog, request: &MarketRequest,
                      now_secs: u64, state: MondayState) -> Result<MarketPreflight> {
    let call = encode_market_call(catalog, request, now_secs)?;
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

pub fn preflight_market_call(rpc: &mut impl Rpc, catalog: &Catalog,
    request: &MarketRequest, now_secs: u64) -> Result<MarketPreflight> {
    chain_guard(rpc)?;
    let block = read_latest(rpc)?;
    let state = read_contract_state(rpc, block)?;
    preflight_at_state(rpc, catalog, request, now_secs, state)
}

/// Derive the largest cent-precise issuer order from the user's USDC cap at
/// one canonical block, then simulate the exact Router calldata there.
pub fn preflight_market_budget(rpc: &mut impl Rpc, catalog: &Catalog,
    budget: &MarketBudgetRequest, now_secs: u64) -> Result<(MarketRequest, MarketPreflight)> {
    chain_guard(rpc)?;
    let block = read_latest(rpc)?;
    let state = read_contract_state(rpc, block)?;
    let order_amount_atoms = match budget.side {
        MarketSide::Buy => buy_order_from_budget(rpc, &state, catalog, budget.wallet_debit_atoms)?,
        MarketSide::Sell => budget.wallet_debit_atoms,
    };
    let request = MarketRequest { owner: budget.owner.clone(), asset_id: budget.asset_id.clone(),
        side: budget.side, wallet_debit_atoms: budget.wallet_debit_atoms,
        order_amount_atoms, deadline_secs: budget.deadline_secs };
    let result = preflight_at_state(rpc, catalog, &request, now_secs, state)?;
    Ok((request, result))
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
    #[test]
    fn budget_fee_search_is_exact_and_fails_under_minimum() {
        assert_eq!(affordable_order(10_000_000_000_000_000_000, 1000, 1000, 0).unwrap(),
            9_970_000_000_000_000_000);
        assert!(affordable_order(10_000_000_000_000_000, 1000, 1000, 0).is_err());
        assert!(affordable_order(100_000_000_000_000_000, 1_000_001, 0, 0).is_err());
    }
}
