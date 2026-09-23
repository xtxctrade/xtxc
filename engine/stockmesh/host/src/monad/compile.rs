//! PR04: a bounded, single-venue EVM execution candidate. This module does
//! not sign, broadcast, grant approvals, or turn an observed token into a route.
//! A venue-specific adapter must own ABI encoding and full-call simulation.
use super::{
    adapters::{BuiltCall, ExactQuote, ExecutionClass, QuoteRequest, SimulatedResult, VenueAdapter},
    feed::BlockRef,
    orders::{address, digest, Order, Phase, Side},
};
use crate::{
    monad_contract::{AdmittedExecutionClass, Catalog, Operation, MAINNET_CHAIN_ID},
    Result,
};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

const MAX_CALLDATA_HEX: usize = 16_386;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionLimits {
    /// V1 never delegates delivery to a third party.
    pub receiver: String,
    /// BUY: total wallet USDC debit (including platform fee); SELL: stock debit.
    pub wallet_input_atoms: u128,
    /// BUY: stock atoms; SELL: USDC atoms. Wallet authorization must bind this.
    pub min_output_atoms: u128,
    /// USDC fee ceiling explicitly shown to and authorized by the wallet.
    pub platform_fee_cap_atoms: u128,
    pub max_gas: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AtomicCandidate {
    pub order_id: String,
    pub asset_id: String,
    /// keccak256 of the exact catalog asset ID bytes; this is the executor's
    /// bytes32 productId, never a ticker or a frontend-generated alias.
    pub product_id: String,
    pub operation: Operation,
    pub owner: String,
    pub receiver: String,
    pub venue_id: String,
    pub quote_digest: String,
    pub state_block_hash: String,
    pub wallet_input_atoms: u128,
    pub venue_input_atoms: u128,
    pub min_output_atoms: u128,
    pub expected_output_atoms: u128,
    pub venue_fee_atoms: u128,
    pub platform_fee_atoms: u128,
    pub platform_fee_cap_atoms: u128,
    pub expires_at_ms: u64,
    pub gas_used: u64,
    pub call: BuiltCall,
}

fn amount(value: &str) -> Result<u128> {
    value.parse::<u128>().map_err(|_| "Monad order amount exceeds compiler range".into())
}

fn same_hex(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

pub(crate) fn executor_product_id(asset_id: &str) -> String {
    let hash = Keccak256::digest(asset_id.as_bytes());
    let mut encoded = String::with_capacity(66);
    encoded.push_str("0x");
    for byte in hash { encoded.push_str(&format!("{byte:02x}")); }
    encoded
}

/// Build and simulate the *whole* adapter call at one pinned block. The
/// returned candidate is not a wallet transaction or execution authorization.
pub fn compile_atomic_candidate(
    catalog: &Catalog,
    order: &Order,
    adapter: &impl VenueAdapter,
    block: &BlockRef,
    limits: &ExecutionLimits,
    now_ms: u64,
) -> Result<AtomicCandidate> {
    catalog.validate()?;
    order.intent.validate()?;
    if order.phase != Phase::Prepared || order.tx_hash.is_some() || order.tx_nonce.is_some() {
        return Err("Monad order is no longer unsigned and prepared".into());
    }
    if now_ms >= order.intent.quote_expires_at_ms
        || !address(&limits.receiver)
        || !same_hex(&limits.receiver, &order.intent.owner)
        || limits.wallet_input_atoms == 0 || limits.min_output_atoms == 0
        || limits.max_gas == 0
        || !digest(&block.hash)
        || adapter.execution_class() != ExecutionClass::MonadAtomic
    {
        return Err("invalid or expired atomic execution limits".into());
    }
    let operation = match order.intent.side { Side::Buy => Operation::Buy, Side::Sell => Operation::Sell };
    let product = catalog.products.iter().find(|p| p.asset_id().ok().as_deref() == Some(&order.intent.asset_id))
        .ok_or("Monad product is not admitted")?;
    let venue = catalog.admitted_venues.iter().find(|v| v.venue_id == adapter.venue_id()
        && v.execution_class == AdmittedExecutionClass::MonadAtomic)
        .ok_or("atomic venue is not admitted")?;
    if !product.operations.iter().any(|op| op.operation == operation && op.venue_id == venue.venue_id) {
        return Err("product operation is not admitted at venue".into());
    }
    let stock_amount = amount(&order.intent.quantity_atoms)?;
    let max_debit = amount(&order.intent.max_input_atoms)?;
    if limits.wallet_input_atoms > max_debit
        || (operation == Operation::Sell && limits.wallet_input_atoms != stock_amount)
    { return Err("wallet debit exceeds order bounds".into()); }
    // The release fee is 0.5 bps, floored in USDC atoms. For BUY it is
    // included in wallet debit; for SELL it is deducted from USDC output.
    let buy_platform_fee = if operation == Operation::Buy { limits.wallet_input_atoms / 20_000 } else { 0 };
    if buy_platform_fee > limits.platform_fee_cap_atoms {
        return Err("platform fee exceeds wallet cap".into());
    }
    let input_atoms = if operation == Operation::Buy {
        limits.wallet_input_atoms.checked_sub(buy_platform_fee).ok_or("fee exceeds debit")?
    } else { stock_amount };
    if operation == Operation::Buy && limits.min_output_atoms < stock_amount {
        return Err("buy output falls below order quantity".into());
    }
    if input_atoms == 0 { return Err("zero input".into()); }
    let request = QuoteRequest {
        asset_id: order.intent.asset_id.clone(), operation, input_atoms,
        owner: order.intent.owner.clone(),
    };
    if !adapter.discover(catalog, &request.asset_id)? {
        return Err("admitted venue did not discover product".into());
    }
    let state = adapter.read(block, &request.asset_id)?;
    if state.venue_id != venue.venue_id || state.block != *block
        || !same_hex(&state.implementation_hash, &venue.evidence.runtime_code_hash)
        || !same_hex(&state.typed_abi_hash, &venue.typed_abi_hash)
    {
        return Err("venue implementation, ABI or block changed".into());
    }
    let quote: ExactQuote = adapter.quote(&request, &state)?;
    if quote.venue_id != venue.venue_id || quote.execution_class != ExecutionClass::MonadAtomic
        || quote.state != state || quote.asset_id != request.asset_id
        || quote.operation != request.operation || quote.input_atoms != input_atoms
        || quote.output_atoms < limits.min_output_atoms || quote.output_atoms == 0
        || quote.expires_at_ms <= now_ms
        || quote.expires_at_ms > order.intent.quote_expires_at_ms
        || !same_hex(&quote.quote_digest, &order.intent.quote_digest)
    {
        return Err("quote does not bind the prepared order and limits".into());
    }
    // BUY fees are USDC-denominated and must fit the wallet debit. SELL fees
    // are taken from quote output, never silently from stock input.
    if (quote.fee_atoms >= quote.input_atoms && operation == Operation::Buy)
        || (quote.fee_atoms >= quote.output_atoms && operation == Operation::Sell)
    {
        return Err("venue fee exceeds economic amount".into());
    }
    let platform_fee = if operation == Operation::Sell { quote.output_atoms / 20_000 } else { buy_platform_fee };
    if platform_fee > limits.platform_fee_cap_atoms
        || (operation == Operation::Sell && quote.output_atoms.saturating_sub(platform_fee) < limits.min_output_atoms)
    { return Err("net output or platform fee exceeds wallet limits".into()); }
    let call = adapter.build(&quote)?;
    if call.chain_id != MAINNET_CHAIN_ID || !same_hex(&call.target, &venue.contract)
        || call.value_atoms != 0
        || call.spender.as_deref().is_some_and(|s| !same_hex(s, &venue.contract))
        || call.calldata.len() < 10 || call.calldata.len() > MAX_CALLDATA_HEX
        || call.calldata.len() % 2 != 0 || !call.calldata.starts_with("0x")
        || !call.calldata[2..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("call is outside the admitted atomic venue boundary".into());
    }
    let simulation: SimulatedResult = adapter.simulate(&call, &state)?;
    let simulated_platform_fee = if operation == Operation::Sell { simulation.estimated_output_atoms / 20_000 } else { buy_platform_fee };
    if !simulation.success || !same_hex(&simulation.state_block_hash, &block.hash)
        || simulation.estimated_output_atoms.saturating_sub(if operation == Operation::Sell { simulated_platform_fee } else { 0 }) < limits.min_output_atoms
        || simulated_platform_fee > limits.platform_fee_cap_atoms
        || simulation.gas_used == 0 || simulation.gas_used > limits.max_gas
    {
        return Err("full-call simulation failed at pinned state or limits".into());
    }
    Ok(AtomicCandidate {
        order_id: order.intent.order_id.clone(), product_id: executor_product_id(&request.asset_id),
        asset_id: request.asset_id,
        operation, owner: request.owner, receiver: limits.receiver.clone(),
        venue_id: venue.venue_id.clone(), quote_digest: quote.quote_digest,
        state_block_hash: block.hash.clone(), wallet_input_atoms: limits.wallet_input_atoms,
        venue_input_atoms: input_atoms,
        min_output_atoms: limits.min_output_atoms,
        expected_output_atoms: simulation.estimated_output_atoms,
        venue_fee_atoms: quote.fee_atoms, platform_fee_atoms: simulated_platform_fee,
        platform_fee_cap_atoms: limits.platform_fee_cap_atoms,
        expires_at_ms: quote.expires_at_ms,
        gas_used: simulation.gas_used, call,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monad::{adapters::{DecodedReceipt, VenueState}, orders::{fixture_intent, Intent}};
    use crate::monad_contract::{Evidence, OperationAdmission, Product, VenueAdmission};

    fn hash(c: char) -> String { format!("0x{}", c.to_string().repeat(64)) }
    fn block() -> BlockRef { BlockRef { number: 1, hash: hash('1'), parent_hash: hash('2'), timestamp: 1 } }
    fn catalog() -> Catalog {
        let mut c: Catalog = serde_json::from_str(include_str!("../../../monad/catalog/registry.v1.json")).unwrap();
        let t = &c.token_observations[0];
        let evidence = Evidence { source_url: "https://example.org/review".into(), observed_at: "2026-09-23T00:00:00Z".into(), runtime_code_hash: hash('3') };
        c.admitted_venues.push(VenueAdmission { venue_id: "fixture".into(), contract: format!("0x{}", "4".repeat(40)), typed_abi_hash: hash('5'), execution_class: AdmittedExecutionClass::MonadAtomic, evidence: evidence.clone() });
        c.products.push(Product { instrument_id: t.instrument_id.clone(), issuer: t.issuer.clone(), issuer_product_id: t.issuer_product_id.clone(), chain_id: t.chain_id, token_address: t.token_address.clone(), token_decimals: t.token_decimals, rights_hash: hash('6'), token_evidence: evidence.clone(), operations: vec![OperationAdmission { operation: Operation::Buy, venue_id: "fixture".into(), evidence: evidence.clone() }, OperationAdmission { operation: Operation::Sell, venue_id: "fixture".into(), evidence }] });
        c.validate().unwrap(); c
    }
    fn order(c: &Catalog, side: Side) -> Order {
        let mut intent: Intent = fixture_intent("mon_compile", "idempotency_compile_0001");
        intent.asset_id = c.products[0].asset_id().unwrap();
        intent.side = side;
        intent.quote_digest = hash('a');
        Order::new(intent).unwrap()
    }
    fn limits() -> ExecutionLimits { ExecutionLimits { receiver: format!("0x{}", "1".repeat(40)), wallet_input_atoms: 1_000_000, min_output_atoms: 100, platform_fee_cap_atoms: 50, max_gas: 300_000 } }
    struct Fixture { wrong_block: bool, wrong_target: bool, wrong_quote: bool, low_output: bool, gas: u64 }
    impl Default for Fixture { fn default() -> Self { Self { wrong_block: false, wrong_target: false, wrong_quote: false, low_output: false, gas: 20_000 } } }
    impl VenueAdapter for Fixture {
        fn venue_id(&self) -> &str { "fixture" }
        fn execution_class(&self) -> ExecutionClass { ExecutionClass::MonadAtomic }
        fn discover(&self, _: &Catalog, _: &str) -> Result<bool> { Ok(true) }
        fn read(&self, block: &BlockRef, _: &str) -> Result<VenueState> {
            let mut block = block.clone(); if self.wrong_block { block.number += 1; }
            Ok(VenueState { venue_id: "fixture".into(), block, implementation_hash: hash('3'), typed_abi_hash: hash('5') })
        }
        fn quote(&self, request: &QuoteRequest, state: &VenueState) -> Result<ExactQuote> {
            Ok(ExactQuote { venue_id: "fixture".into(), execution_class: ExecutionClass::MonadAtomic, state: state.clone(), asset_id: request.asset_id.clone(), operation: request.operation, input_atoms: request.input_atoms, output_atoms: if self.low_output { 99 } else { 100 }, fee_atoms: 1, expires_at_ms: 2_000_000_000_000, quote_digest: if self.wrong_quote { hash('b') } else { hash('a') } })
        }
        fn build(&self, _: &ExactQuote) -> Result<BuiltCall> {
            Ok(BuiltCall { chain_id: MAINNET_CHAIN_ID, target: if self.wrong_target { format!("0x{}", "7".repeat(40)) } else { format!("0x{}", "4".repeat(40)) }, calldata: "0x12345678".into(), value_atoms: 0, spender: None })
        }
        fn simulate(&self, _: &BuiltCall, state: &VenueState) -> Result<SimulatedResult> {
            Ok(SimulatedResult { success: true, estimated_output_atoms: if self.low_output { 99 } else { 100 }, gas_used: self.gas, state_block_hash: state.block.hash.clone() })
        }
        fn decode_receipt(&self, _: &[u8], _: &ExactQuote) -> Result<DecodedReceipt> { Err("fixture only".into()) }
    }
    #[test]
    fn admitted_buy_and_sell_compile_with_exact_pins() {
        let c = catalog();
        for side in [Side::Buy, Side::Sell] {
            let mut limits = limits();
            if side == Side::Sell { limits.wallet_input_atoms = 100; }
            let candidate = compile_atomic_candidate(&c, &order(&c, side), &Fixture::default(), &block(), &limits, 1).unwrap();
            assert_eq!(candidate.venue_input_atoms, if side == Side::Buy { 999_950 } else { 100 });
            assert_eq!(candidate.state_block_hash, block().hash);
            assert_eq!(candidate.product_id, executor_product_id(&candidate.asset_id));
        }
    }
    #[test]
    fn observed_cohort_does_not_gain_execution() {
        let c: Catalog = serde_json::from_str(include_str!("../../../monad/catalog/registry.v1.json")).unwrap();
        assert!(compile_atomic_candidate(&c, &order(&catalog(), Side::Buy), &Fixture::default(), &block(), &limits(), 1).is_err());
    }
    #[test]
    fn stale_mixed_or_unsafe_candidates_fail_closed() {
        let c = catalog(); let o = order(&c, Side::Buy);
        let cases = [Fixture { wrong_block: true, ..Fixture::default() }, Fixture { wrong_target: true, ..Fixture::default() }, Fixture { wrong_quote: true, ..Fixture::default() }, Fixture { low_output: true, ..Fixture::default() }, Fixture { gas: 300_001, ..Fixture::default() }];
        for fixture in cases { assert!(compile_atomic_candidate(&c, &o, &fixture, &block(), &limits(), 1).is_err()); }
        let mut receiver = limits(); receiver.receiver = format!("0x{}", "9".repeat(40));
        assert!(compile_atomic_candidate(&c, &o, &Fixture::default(), &block(), &receiver, 1).is_err());
        let mut submitted = o.clone(); submitted.report_submission(&hash('8'), 0).unwrap();
        assert!(compile_atomic_candidate(&c, &submitted, &Fixture::default(), &block(), &limits(), 1).is_err());
    }
}
