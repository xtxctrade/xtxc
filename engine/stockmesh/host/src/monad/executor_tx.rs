//! Exact ABI lowering for the admitted atomic executor. Producing these bytes
//! does not authorize a wallet transaction: the full outer call still needs a
//! pinned-state simulation, a reviewed release and explicit wallet consent.
use super::{compile::{executor_product_id, AtomicCandidate}, orders::{address, digest, Order, Phase, Side}};
use crate::{monad_contract::{Catalog, Operation, MAINNET_CHAIN_ID}, Result};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

const EXECUTE_SIGNATURE: &str = "execute((bytes32,bytes32,address,address,address,address,uint256,uint256,uint256,uint256,uint256,uint256,bool))";
const VENUE_SIGNATURE: &str = "swapExactIn(address,address,uint256,uint256,address)";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UnsimulatedExecutorCall {
    pub chain_id: u64,
    pub from: String,
    pub to: String,
    pub value_atoms: u8,
    pub data: String,
    pub pinned_block_hash: String,
    pub execution_nonce: String,
    pub deadline_secs: u64,
}

fn bytes<const N: usize>(hex: &str) -> Result<[u8; N]> {
    if hex.len() != N * 2 + 2 || !hex.starts_with("0x") { return Err("invalid executor ABI hex".into()); }
    let mut output = [0u8; N];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 + index * 2..4 + index * 2], 16)
            .map_err(|_| "invalid executor ABI hex")?;
    }
    Ok(output)
}

fn word(value: u128) -> [u8; 32] {
    let mut output = [0u8; 32];
    output[16..].copy_from_slice(&value.to_be_bytes());
    output
}

fn address_word(value: &str) -> Result<[u8; 32]> {
    let mut output = [0u8; 32];
    output[12..].copy_from_slice(&bytes::<20>(value)?);
    Ok(output)
}

fn encoded(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(2 + bytes.len() * 2);
    hex.push_str("0x");
    for byte in bytes { hex.push_str(&format!("{byte:02x}")); }
    hex
}

fn exact_venue_data(stock: &str, usdc: &str, candidate: &AtomicCandidate, executor: &str) -> Result<String> {
    let selector = Keccak256::digest(VENUE_SIGNATURE.as_bytes());
    let token_in = if candidate.operation == Operation::Buy { usdc } else { stock };
    let token_out = if candidate.operation == Operation::Buy { stock } else { usdc };
    let mut data = Vec::with_capacity(4 + 5 * 32);
    data.extend_from_slice(&selector[..4]);
    data.extend_from_slice(&address_word(token_in)?);
    data.extend_from_slice(&address_word(token_out)?);
    data.extend_from_slice(&word(candidate.venue_input_atoms));
    data.extend_from_slice(&word(candidate.min_output_atoms));
    data.extend_from_slice(&address_word(executor)?);
    Ok(encoded(&data))
}

/// Use this only after `compile_atomic_candidate`; it intentionally has no gas
/// limit or `submitAllowed` bit because the outer executor call was not yet
/// simulated. A live prepare path must validate the *whole* call at this block.
pub fn encode_executor_call(
    catalog: &Catalog,
    order: &Order,
    candidate: &AtomicCandidate,
    executor: &str,
    now_ms: u64,
) -> Result<UnsimulatedExecutorCall> {
    catalog.validate()?;
    if order.phase != Phase::Prepared || now_ms >= candidate.expires_at_ms
        || candidate.expires_at_ms > order.intent.quote_expires_at_ms
        || !address(executor) || executor.eq_ignore_ascii_case("0x0000000000000000000000000000000000000000")
        || !digest(&candidate.state_block_hash)
    { return Err("executor call is stale or unadmitted".into()); }
    let product = catalog.products.iter().find(|p| p.asset_id().ok().as_deref() == Some(candidate.asset_id.as_str()))
        .ok_or("executor product is not admitted")?;
    let venue = catalog.admitted_venues.iter().find(|v| v.venue_id == candidate.venue_id
        && v.contract.eq_ignore_ascii_case(&candidate.call.target))
        .ok_or("executor venue is not admitted")?;
    let operation = match order.intent.side { Side::Buy => Operation::Buy, Side::Sell => Operation::Sell };
    if !product.operations.iter().any(|o| o.operation == operation && o.venue_id == venue.venue_id)
        || candidate.operation != operation || candidate.asset_id != order.intent.asset_id
        || candidate.order_id != order.intent.order_id
        || !candidate.owner.eq_ignore_ascii_case(&order.intent.owner)
        || !candidate.receiver.eq_ignore_ascii_case(&order.intent.owner)
        || !candidate.quote_digest.eq_ignore_ascii_case(&order.intent.quote_digest)
        || candidate.product_id != executor_product_id(&candidate.asset_id)
        || candidate.call.chain_id != MAINNET_CHAIN_ID || candidate.call.value_atoms != 0
        || candidate.wallet_input_atoms == 0 || candidate.venue_input_atoms == 0
        || candidate.min_output_atoms == 0 || candidate.expected_output_atoms == 0
        || candidate.platform_fee_atoms > candidate.platform_fee_cap_atoms
    { return Err("executor call differs from admitted order".into()); }
    let max_debit = order.intent.max_input_atoms.parse::<u128>()
        .map_err(|_| "invalid executor max debit")?;
    if candidate.wallet_input_atoms > max_debit { return Err("executor debit exceeds order".into()); }
    let expected_fee = if operation == Operation::Buy {
        candidate.wallet_input_atoms / 20_000
    } else { candidate.expected_output_atoms / 20_000 };
    if candidate.platform_fee_atoms != expected_fee
        || (operation == Operation::Buy && candidate.venue_input_atoms != candidate.wallet_input_atoms - expected_fee)
        || (operation == Operation::Sell && candidate.venue_input_atoms != candidate.wallet_input_atoms)
        || (operation == Operation::Buy && candidate.min_output_atoms < order.intent.quantity_atoms.parse::<u128>()
            .map_err(|_| "invalid executor stock quantity")?)
        || !candidate.call.calldata.eq_ignore_ascii_case(&exact_venue_data(
            &product.token_address, &catalog.chain.usdc, candidate, executor)?)
    { return Err("executor and simulated venue call differ".into()); }
    let deadline_secs = candidate.expires_at_ms / 1000;
    if deadline_secs == 0 || deadline_secs * 1000 <= now_ms { return Err("executor deadline expired".into()); }

    // The order ID is unique in the durable owner journal. Hash it with the
    // owner and a domain label so executor replay protection cannot collide
    // with the EOA transaction nonce or another StockMesh order namespace.
    let nonce = Keccak256::digest(format!("xtxc.monad.order/v1:{}:{}", order.intent.owner.to_ascii_lowercase(), order.intent.order_id).as_bytes());
    let selector = Keccak256::digest(EXECUTE_SIGNATURE.as_bytes());
    let mut data = Vec::with_capacity(4 + 13 * 32);
    data.extend_from_slice(&selector[..4]);
    data.extend_from_slice(&bytes::<32>(&candidate.product_id)?);
    data.extend_from_slice(&bytes::<32>(&candidate.quote_digest)?);
    data.extend_from_slice(&address_word(&order.intent.owner)?);
    data.extend_from_slice(&address_word(&candidate.receiver)?);
    data.extend_from_slice(&address_word(&product.token_address)?);
    data.extend_from_slice(&address_word(&candidate.call.target)?);
    data.extend_from_slice(&nonce);
    data.extend_from_slice(&word(candidate.wallet_input_atoms));
    data.extend_from_slice(&word(max_debit));
    data.extend_from_slice(&word(candidate.min_output_atoms));
    data.extend_from_slice(&word(candidate.platform_fee_cap_atoms));
    data.extend_from_slice(&word(deadline_secs as u128));
    data.extend_from_slice(&word(u128::from(operation == Operation::Buy)));
    Ok(UnsimulatedExecutorCall {
        chain_id: MAINNET_CHAIN_ID, from: order.intent.owner.clone(), to: executor.to_ascii_lowercase(),
        value_atoms: 0, data: encoded(&data), pinned_block_hash: candidate.state_block_hash.clone(),
        execution_nonce: encoded(&nonce), deadline_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monad::{adapters::BuiltCall, orders::fixture_intent};
    use crate::monad_contract::{AdmittedExecutionClass, Evidence, OperationAdmission, Product, VenueAdmission};

    fn hash(c: char) -> String { format!("0x{}", c.to_string().repeat(64)) }
    fn fixture() -> (Catalog, Order, AtomicCandidate) {
        let mut catalog: Catalog = serde_json::from_str(include_str!("../../../monad/catalog/registry.v1.json")).unwrap();
        let token = &catalog.token_observations[0];
        let evidence = Evidence { source_url: "https://example.org/review".into(), observed_at: "2026-09-23T00:00:00Z".into(), runtime_code_hash: hash('3') };
        let venue = VenueAdmission { venue_id: "fixture".into(), contract: format!("0x{}", "4".repeat(40)), typed_abi_hash: hash('5'), execution_class: AdmittedExecutionClass::MonadAtomic, evidence: evidence.clone() };
        let product = Product { instrument_id: token.instrument_id.clone(), issuer: token.issuer.clone(), issuer_product_id: token.issuer_product_id.clone(), chain_id: token.chain_id, token_address: token.token_address.clone(), token_decimals: token.token_decimals, rights_hash: hash('6'), token_evidence: evidence.clone(), operations: vec![
            OperationAdmission { operation: Operation::Buy, venue_id: venue.venue_id.clone(), evidence: evidence.clone() },
            OperationAdmission { operation: Operation::Sell, venue_id: venue.venue_id.clone(), evidence },
        ] };
        let asset_id = product.asset_id().unwrap();
        catalog.admitted_venues.push(venue.clone());
        catalog.products.push(product);
        catalog.validate().unwrap();
        let mut intent = fixture_intent("mon_executor", "idempotency_executor_0001");
        intent.asset_id = asset_id.clone();
        let order = Order::new(intent).unwrap();
        let mut candidate = AtomicCandidate {
            order_id: order.intent.order_id.clone(), product_id: executor_product_id(&asset_id), asset_id,
            operation: Operation::Buy, owner: order.intent.owner.clone(), receiver: order.intent.owner.clone(),
            venue_id: venue.venue_id.clone(), quote_digest: order.intent.quote_digest.clone(),
            state_block_hash: hash('1'), wallet_input_atoms: 1_000_000, venue_input_atoms: 999_950,
            min_output_atoms: 100, expected_output_atoms: 100, venue_fee_atoms: 1,
            platform_fee_atoms: 50, platform_fee_cap_atoms: 50, expires_at_ms: 2_000_000_000_000,
            gas_used: 200_000, call: BuiltCall { chain_id: MAINNET_CHAIN_ID, target: venue.contract, calldata: "0x12345678".into(), value_atoms: 0, spender: None },
        };
        candidate.call.calldata = exact_venue_data(
            &catalog.products[0].token_address, &catalog.chain.usdc, &candidate,
            &format!("0x{}", "7".repeat(40)),
        ).unwrap();
        (catalog, order, candidate)
    }

    #[test]
    fn exact_static_abi_and_owner_nonce_are_stable() {
        let (catalog, order, candidate) = fixture();
        let executor = format!("0x{}", "7".repeat(40));
        let encoded = encode_executor_call(&catalog, &order, &candidate, &executor, 1).unwrap();
        assert_eq!(&encoded.data[..10], "0x893bcd1b"); // ethers Interface selector
        assert_eq!(encoded.data.len(), 2 + (4 + 13 * 32) * 2);
        assert_eq!(&encoded.data[10..74], &candidate.product_id[2..]);
        assert_eq!(&encoded.data[74..138], &candidate.quote_digest[2..]);
        assert_eq!(encoded.execution_nonce.len(), 66);
        assert_eq!(encoded.deadline_secs, 2_000_000_000);
        assert_eq!(encoded.to, executor);
    }

    #[test]
    fn tampered_candidate_cannot_become_wallet_calldata() {
        let (catalog, order, mut candidate) = fixture();
        let executor = format!("0x{}", "7".repeat(40));
        candidate.product_id = hash('9');
        assert!(encode_executor_call(&catalog, &order, &candidate, &executor, 1).is_err());
        candidate.product_id = executor_product_id(&candidate.asset_id);
        candidate.receiver = format!("0x{}", "8".repeat(40));
        assert!(encode_executor_call(&catalog, &order, &candidate, &executor, 1).is_err());
        candidate.receiver = order.intent.owner.clone();
        candidate.call.calldata = "0x12345678".into();
        assert!(encode_executor_call(&catalog, &order, &candidate, &executor, 1).is_err());
        candidate.call.calldata = exact_venue_data(&catalog.products[0].token_address, &catalog.chain.usdc, &candidate, &executor).unwrap();
        candidate.expires_at_ms = 1;
        assert!(encode_executor_call(&catalog, &order, &candidate, &executor, 1).is_err());
    }

    #[test]
    fn sell_uses_stock_input_and_net_usdc_output() {
        let (catalog, mut order, mut candidate) = fixture();
        let executor = format!("0x{}", "7".repeat(40));
        order.intent.side = Side::Sell;
        candidate.operation = Operation::Sell;
        candidate.wallet_input_atoms = 100;
        candidate.venue_input_atoms = 100;
        candidate.expected_output_atoms = 1_000_000;
        candidate.min_output_atoms = 999_950;
        candidate.platform_fee_atoms = 50;
        candidate.call.calldata = exact_venue_data(
            &catalog.products[0].token_address, &catalog.chain.usdc, &candidate, &executor,
        ).unwrap();
        let encoded = encode_executor_call(&catalog, &order, &candidate, &executor, 1).unwrap();
        assert!(encoded.data.ends_with(&"0".repeat(64)));
        assert!(candidate.call.calldata.contains(&catalog.products[0].token_address[2..]));
    }
}
