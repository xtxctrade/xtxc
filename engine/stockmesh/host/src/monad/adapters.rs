//! Typed venue boundary. Observation and execution are different states.
//! No generic target/calldata passthrough is admitted by this interface.
use super::feed::BlockRef;
use crate::{
    monad_contract::{Catalog, Operation},
    Result,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExecutionClass {
    MonadAtomic,
    IssuerAsync,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VenueState {
    pub venue_id: String,
    pub block: BlockRef,
    pub implementation_hash: String,
    pub typed_abi_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuoteRequest {
    pub asset_id: String,
    pub operation: Operation,
    pub input_atoms: u128,
    pub owner: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExactQuote {
    pub venue_id: String,
    pub execution_class: ExecutionClass,
    pub state: VenueState,
    pub asset_id: String,
    pub operation: Operation,
    pub input_atoms: u128,
    pub output_atoms: u128,
    pub fee_atoms: u128,
    pub expires_at_ms: u64,
    pub quote_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuiltCall {
    pub chain_id: u64,
    pub target: String,
    pub calldata: String,
    pub value_atoms: u128,
    pub spender: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SimulatedResult {
    pub success: bool,
    pub estimated_output_atoms: u128,
    pub gas_used: u64,
    pub state_block_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecodedReceipt {
    pub tx_hash: String,
    pub block_hash: String,
    pub wallet_input_delta_atoms: i128,
    pub wallet_output_delta_atoms: i128,
    pub pending_issuer_order: bool,
}

pub trait VenueAdapter {
    fn venue_id(&self) -> &str;
    fn execution_class(&self) -> ExecutionClass;
    fn discover(&self, catalog: &Catalog, asset_id: &str) -> Result<bool>;
    fn read(&self, block: &BlockRef, asset_id: &str) -> Result<VenueState>;
    fn quote(&self, request: &QuoteRequest, state: &VenueState) -> Result<ExactQuote>;
    fn build(&self, quote: &ExactQuote) -> Result<BuiltCall>;
    fn simulate(&self, call: &BuiltCall, state: &VenueState) -> Result<SimulatedResult>;
    fn decode_receipt(&self, raw: &[u8], quote: &ExactQuote) -> Result<DecodedReceipt>;
}

/// Official addresses and app-visible tickers may be discovered, but do not
/// constitute the reviewed ABI, fill mechanics or issuer authorization.
pub struct ObservedOnlyAdapter {
    pub venue_id: String,
    pub class: ExecutionClass,
}
impl VenueAdapter for ObservedOnlyAdapter {
    fn venue_id(&self) -> &str {
        &self.venue_id
    }
    fn execution_class(&self) -> ExecutionClass {
        self.class
    }
    fn discover(&self, catalog: &Catalog, asset_id: &str) -> Result<bool> {
        Ok(catalog.token_observations.iter().any(|t| {
            format!(
                "eip155:{}:erc20:{}:{}:{}",
                t.chain_id, t.token_address, t.issuer, t.issuer_product_id
            ) == asset_id
        }))
    }
    fn read(&self, _: &BlockRef, _: &str) -> Result<VenueState> {
        Err("observed venue has no admitted typed state reader".into())
    }
    fn quote(&self, _: &QuoteRequest, _: &VenueState) -> Result<ExactQuote> {
        Err("observed venue has no executable quote".into())
    }
    fn build(&self, _: &ExactQuote) -> Result<BuiltCall> {
        Err("observed venue has no approved call builder".into())
    }
    fn simulate(&self, _: &BuiltCall, _: &VenueState) -> Result<SimulatedResult> {
        Err("observed venue has no approved simulation".into())
    }
    fn decode_receipt(&self, _: &[u8], _: &ExactQuote) -> Result<DecodedReceipt> {
        Err("observed venue has no reviewed receipt ABI".into())
    }
}

/// Issuer continuation is not an atomic stock wallet delivery. A submitted
/// order must remain pending until its actual wallet/custody delta is observed.
pub fn receipt_completes_wallet_delivery(class: ExecutionClass, receipt: &DecodedReceipt) -> bool {
    !receipt.pending_issuer_order
        && receipt.wallet_output_delta_atoms > 0
        && receipt.wallet_input_delta_atoms < 0
        && matches!(
            class,
            ExecutionClass::MonadAtomic | ExecutionClass::IssuerAsync
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn observation_never_builds_transaction() {
        let catalog: Catalog =
            serde_json::from_str(include_str!("../../../monad/catalog/registry.v1.json")).unwrap();
        let token = &catalog.token_observations[0];
        let asset = format!(
            "eip155:143:erc20:{}:{}:{}",
            token.token_address, token.issuer, token.issuer_product_id
        );
        let adapter = ObservedOnlyAdapter {
            venue_id: "monday-stock-router".into(),
            class: ExecutionClass::IssuerAsync,
        };
        assert!(adapter.discover(&catalog, &asset).unwrap());
        let state = VenueState {
            venue_id: adapter.venue_id().into(),
            block: BlockRef {
                number: 1,
                hash: format!("0x{}", "1".repeat(64)),
                parent_hash: format!("0x{}", "0".repeat(64)),
                timestamp: 1,
            },
            implementation_hash: String::new(),
            typed_abi_hash: String::new(),
        };
        let request = QuoteRequest {
            asset_id: asset,
            operation: Operation::Buy,
            input_atoms: 100,
            owner: String::new(),
        };
        assert!(adapter.quote(&request, &state).is_err());
        assert!(adapter
            .build(&ExactQuote {
                venue_id: adapter.venue_id().into(),
                execution_class: adapter.execution_class(),
                state,
                asset_id: request.asset_id,
                operation: Operation::Buy,
                input_atoms: 100,
                output_atoms: 1,
                fee_atoms: 0,
                expires_at_ms: 1,
                quote_digest: String::new()
            })
            .is_err());
    }
    #[test]
    fn async_submission_is_not_delivery() {
        let mut receipt = DecodedReceipt {
            tx_hash: String::new(),
            block_hash: String::new(),
            wallet_input_delta_atoms: -10,
            wallet_output_delta_atoms: 0,
            pending_issuer_order: true,
        };
        assert!(!receipt_completes_wallet_delivery(
            ExecutionClass::IssuerAsync,
            &receipt
        ));
        receipt.wallet_output_delta_atoms = 2;
        receipt.pending_issuer_order = false;
        assert!(receipt_completes_wallet_delivery(
            ExecutionClass::IssuerAsync,
            &receipt
        ));
    }
}
