//! Monday RWA contract-state reader, not an executable quote adapter.
//! The public ABI creates asynchronous order IDs; settlement is separate.
use super::feed::{chain_guard, read_block, BlockRef, Rpc};
use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use sha3::Keccak256;

pub const ROUTER: &str = "0x2f903ac6ddaf57eadcbbc46adc3ad739c3506a2d";
pub const STOCK: &str = "0xff63e828254888c3ea46268d7c3db63deb80e25d";
pub const CASHIER: &str = "0xb983787a2882da59bbf9e09cb5a0e47ff0c30141";
pub const ROUTER_ABI_SHA256: &str =
    "d70a1e60cd5d0c9161e7f7facadf217a88687da3fba78fca93335ec2e8722a61";
pub const STOCK_ABI_SHA256: &str =
    "174a02b5cd8d35969f2bfc4a5310d59be756db68f5c93a90c989883b5ca053fa";
const EIP1967_IMPLEMENTATION_SLOT: &str =
    "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";

fn selector(signature: &str) -> String {
    let digest = Keccak256::digest(signature.as_bytes());
    format!(
        "0x{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}
fn address(raw: &str) -> Result<String> {
    if raw.len() != 42 || !raw.starts_with("0x") || !raw[2..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("invalid Monad address".into());
    }
    Ok(raw.to_ascii_lowercase())
}
fn decode_address(raw: &str) -> Result<String> {
    if raw.len() != 66
        || !raw.starts_with("0x")
        || !raw[2..].bytes().all(|b| b.is_ascii_hexdigit())
        || !raw[2..26].bytes().all(|b| b == b'0')
    {
        return Err("invalid ABI address result".into());
    }
    address(&format!("0x{}", &raw[26..]))
}
fn decode_atoms(raw: &str) -> Result<u128> {
    if raw.len() != 66
        || !raw.starts_with("0x")
        || !raw[2..].bytes().all(|b| b.is_ascii_hexdigit())
        || !raw[2..34].bytes().all(|b| b == b'0')
    {
        return Err("ABI uint256 exceeds u128".into());
    }
    u128::from_str_radix(&raw[34..], 16).map_err(|_| "ABI uint256 invalid".into())
}
fn word_address(raw: &str) -> Result<String> {
    Ok(format!("{:0>64}", &address(raw)?[2..]))
}
fn code_hash(rpc: &mut impl Rpc, contract: &str, block: &BlockRef) -> Result<String> {
    let result = rpc.call(
        "eth_getCode",
        json!([contract, format!("0x{:x}", block.number)]),
    )?;
    let hex = result
        .as_str()
        .ok_or("contract code absent")?
        .strip_prefix("0x")
        .ok_or("contract code malformed")?;
    if hex.is_empty() || hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("contract code missing or malformed".into());
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect::<Vec<_>>();
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
fn implementation(rpc: &mut impl Rpc, proxy: &str, block: &BlockRef) -> Result<(String, String)> {
    let raw = rpc.call(
        "eth_getStorageAt",
        json!([proxy, EIP1967_IMPLEMENTATION_SLOT, block.hash]),
    )?;
    let address = decode_address(raw.as_str().ok_or("implementation slot absent")?)?;
    if address == "0x0000000000000000000000000000000000000000" || address == proxy {
        return Err("Monday implementation pointer missing or circular".into());
    }
    let hash = code_hash(rpc, &address, block)?;
    Ok((address, hash))
}
fn call(rpc: &mut impl Rpc, to: &str, data: &str, block: &BlockRef) -> Result<String> {
    let value = rpc.call("eth_call", json!([{"to":to,"data":data},block.hash]))?;
    Ok(value.as_str().ok_or("ABI call result absent")?.to_owned())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MondayState {
    pub block: BlockRef,
    pub router_code_sha256: String,
    pub stock_code_sha256: String,
    pub cashier_code_sha256: String,
    pub router_implementation: String,
    pub router_implementation_sha256: String,
    pub stock_implementation: String,
    pub stock_implementation_sha256: String,
    pub cashier_implementation: String,
    pub cashier_implementation_sha256: String,
    pub router_abi_sha256: String,
    pub stock_abi_sha256: String,
    pub quote_admitted: bool,
}

pub fn read_contract_state(rpc: &mut impl Rpc, block: BlockRef) -> Result<MondayState> {
    chain_guard(rpc)?;
    let router_code_sha256 = code_hash(rpc, ROUTER, &block)?;
    let stock_code_sha256 = code_hash(rpc, STOCK, &block)?;
    let cashier_code_sha256 = code_hash(rpc, CASHIER, &block)?;
    let (router_implementation, router_implementation_sha256) =
        implementation(rpc, ROUTER, &block)?;
    let (stock_implementation, stock_implementation_sha256) = implementation(rpc, STOCK, &block)?;
    let (cashier_implementation, cashier_implementation_sha256) =
        implementation(rpc, CASHIER, &block)?;
    if decode_address(&call(rpc, ROUTER, &selector("stock()"), &block)?)? != STOCK
        || decode_address(&call(rpc, ROUTER, &selector("cashier()"), &block)?)? != CASHIER
    {
        return Err("Monday router dependency changed".into());
    }
    if read_block(rpc, block.number)?.hash != block.hash {
        return Err("Monday block changed during read".into());
    }
    Ok(MondayState {
        block,
        router_code_sha256,
        stock_code_sha256,
        cashier_code_sha256,
        router_implementation,
        router_implementation_sha256,
        stock_implementation,
        stock_implementation_sha256,
        cashier_implementation,
        cashier_implementation_sha256,
        router_abi_sha256: ROUTER_ABI_SHA256.into(),
        stock_abi_sha256: STOCK_ABI_SHA256.into(),
        quote_admitted: false,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionBalances {
    pub wallet_atoms: u128,
    pub monday_internal_atoms: u128,
}

pub fn read_position(
    rpc: &mut impl Rpc,
    state: &MondayState,
    owner: &str,
    token: &str,
) -> Result<PositionBalances> {
    chain_guard(rpc)?;
    let owner = address(owner)?;
    let token = address(token)?;
    let wallet_data = format!(
        "{}{}",
        selector("balanceOf(address)"),
        word_address(&owner)?
    );
    let internal_data = format!(
        "{}{}{}",
        selector("stockBalance(address,address)"),
        word_address(&owner)?,
        word_address(&token)?
    );
    let wallet_atoms = decode_atoms(&call(rpc, &token, &wallet_data, &state.block)?)?;
    let monday_internal_atoms = decode_atoms(&call(rpc, STOCK, &internal_data, &state.block)?)?;
    if read_block(rpc, state.block.number)?.hash != state.block.hash {
        return Err("Monday position block changed".into());
    }
    Ok(PositionBalances {
        wallet_atoms,
        monday_internal_atoms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn standard_erc20_selector_and_strict_outputs() {
        assert_eq!(selector("balanceOf(address)"), "0x70a08231");
        assert!(decode_address(&format!("0x{}{}", "1".repeat(24), "2".repeat(40))).is_err());
        assert_eq!(decode_atoms(&format!("0x{}a", "0".repeat(63))).unwrap(), 10);
        assert!(decode_atoms(&format!("0x1{}", "0".repeat(63))).is_err());
    }
}
