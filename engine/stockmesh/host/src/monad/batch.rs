//! One-pass, per-product preflight for a venue cohort. This never signs,
//! submits, or promotes a product to the executable catalog. A successful
//! row means the common adapter can quote/build/simulate both directions;
//! actual buy/settle/wallet-delivery/sell/return is a separate live gate.
use super::{
    adapters::{BuiltCall, ExactQuote, ExecutionClass, QuoteRequest, VenueAdapter, VenueState},
    feed::BlockRef,
};
use crate::{
    monad_contract::{Catalog, Operation, TokenObservation, MAINNET_CHAIN_ID},
    Result,
};
use serde::{Deserialize, Serialize};

const MAX_COHORT: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Stage {
    Discovery,
    State,
    BuyQuote,
    BuyBuild,
    BuySimulation,
    SellQuote,
    SellBuild,
    SellSimulation,
    PreflightPassed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Row {
    pub asset_id: String,
    pub instrument_id: String,
    pub token_address: String,
    pub venue_id: String,
    pub stage: Stage,
    pub error: Option<String>,
    pub buy_quote_digest: Option<String>,
    pub sell_quote_digest: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Report {
    pub block: BlockRef,
    pub venue_id: String,
    pub rows: Vec<Row>,
    pub preflight_passed: usize,
    pub live_round_trips: usize,
}

fn asset_id(token: &TokenObservation) -> String {
    format!(
        "eip155:{}:erc20:{}:{}:{}",
        token.chain_id, token.token_address, token.issuer, token.issuer_product_id
    )
}

fn address(value: &str) -> bool {
    value.len() == 42
        && value.starts_with("0x")
        && value[2..].bytes().all(|b| b.is_ascii_hexdigit())
        && value[2..].bytes().any(|b| b != b'0')
}

fn check_quote(
    quote: &ExactQuote,
    request: &QuoteRequest,
    state: &VenueState,
    class: ExecutionClass,
    now_ms: u64,
) -> Result<()> {
    if quote.venue_id != state.venue_id
        || quote.execution_class != class
        || quote.state != *state
        || quote.asset_id != request.asset_id
        || quote.operation != request.operation
        || quote.input_atoms != request.input_atoms
        || quote.output_atoms == 0
        || quote.expires_at_ms <= now_ms
        || quote.quote_digest.is_empty()
    {
        return Err("venue quote contradicts the pinned request or has expired".into());
    }
    Ok(())
}

fn check_call(call: &BuiltCall) -> Result<()> {
    if call.chain_id != MAINNET_CHAIN_ID
        || !address(&call.target)
        || call.calldata.len() < 10
        || !call.calldata.starts_with("0x")
        || call.calldata.len() % 2 != 0
        || !call.calldata[2..].bytes().all(|b| b.is_ascii_hexdigit())
        || call
            .spender
            .as_deref()
            .is_some_and(|spender| !address(spender))
    {
        return Err("venue built invalid Monad call".into());
    }
    Ok(())
}

fn inspect(
    catalog: &Catalog,
    adapter: &impl VenueAdapter,
    token: &TokenObservation,
    block: &BlockRef,
    owner: &str,
    buy_input_atoms: u128,
    now_ms: u64,
) -> Row {
    let asset_id = asset_id(token);
    let mut row = Row {
        asset_id: asset_id.clone(),
        instrument_id: token.instrument_id.clone(),
        token_address: token.token_address.clone(),
        venue_id: adapter.venue_id().to_owned(),
        stage: Stage::Discovery,
        error: None,
        buy_quote_digest: None,
        sell_quote_digest: None,
    };
    let outcome = (|| -> Result<()> {
        if !adapter.discover(catalog, &asset_id)? {
            return Err("venue did not discover observed product".into());
        }
        row.stage = Stage::State;
        let state = adapter.read(block, &asset_id)?;
        if state.venue_id != adapter.venue_id()
            || state.block != *block
            || state.implementation_hash.is_empty()
            || state.typed_abi_hash.is_empty()
        {
            return Err("venue state is not pinned to requested block and version".into());
        }
        for (operation, input_atoms, quote_stage, build_stage, simulation_stage) in [
            (
                Operation::Buy,
                buy_input_atoms,
                Stage::BuyQuote,
                Stage::BuyBuild,
                Stage::BuySimulation,
            ),
            (
                Operation::Sell,
                10u128.pow(token.token_decimals as u32),
                Stage::SellQuote,
                Stage::SellBuild,
                Stage::SellSimulation,
            ),
        ] {
            let request = QuoteRequest {
                asset_id: asset_id.clone(),
                operation,
                input_atoms,
                owner: owner.to_owned(),
            };
            row.stage = quote_stage;
            let quote = adapter.quote(&request, &state)?;
            check_quote(&quote, &request, &state, adapter.execution_class(), now_ms)?;
            match operation {
                Operation::Buy => row.buy_quote_digest = Some(quote.quote_digest.clone()),
                Operation::Sell => row.sell_quote_digest = Some(quote.quote_digest.clone()),
                _ => unreachable!(),
            }
            row.stage = build_stage;
            let call = adapter.build(&quote)?;
            check_call(&call)?;
            row.stage = simulation_stage;
            let result = adapter.simulate(&call, &state)?;
            if !result.success
                || result.state_block_hash != block.hash
                || (adapter.execution_class() == ExecutionClass::MonadAtomic
                    && result.estimated_output_atoms < quote.output_atoms)
            {
                return Err("venue simulation failed or disagreed with pinned quote".into());
            }
        }
        row.stage = Stage::PreflightPassed;
        Ok(())
    })();
    if let Err(error) = outcome {
        row.error = Some(error);
    }
    row
}

/// Evaluates the whole observed-token cohort in one invocation; a failure in
/// one row does not silently pass or stop the others. The caller may not use
/// `preflight_passed` as a count of delivered or tradeable products.
pub fn preflight_all(
    catalog: &Catalog,
    adapter: &impl VenueAdapter,
    block: &BlockRef,
    owner: &str,
    buy_input_atoms: u128,
    now_ms: u64,
) -> Result<Report> {
    catalog.validate()?;
    if catalog.token_observations.is_empty()
        || catalog.token_observations.len() > MAX_COHORT
        || !address(owner)
        || buy_input_atoms == 0
    {
        return Err("invalid Monad batch preflight parameters".into());
    }
    let rows = catalog
        .token_observations
        .iter()
        .map(|token| {
            inspect(
                catalog,
                adapter,
                token,
                block,
                owner,
                buy_input_atoms,
                now_ms,
            )
        })
        .collect::<Vec<_>>();
    let preflight_passed = rows
        .iter()
        .filter(|row| row.stage == Stage::PreflightPassed)
        .count();
    Ok(Report {
        block: block.clone(),
        venue_id: adapter.venue_id().to_owned(),
        rows,
        preflight_passed,
        live_round_trips: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monad::adapters::{DecodedReceipt, SimulatedResult};

    fn catalog() -> Catalog {
        serde_json::from_str(include_str!("../../../monad/catalog/registry.v1.json")).unwrap()
    }
    fn block() -> BlockRef {
        BlockRef {
            number: 100,
            hash: format!("0x{}", "1".repeat(64)),
            parent_hash: format!("0x{}", "0".repeat(64)),
            timestamp: 1,
        }
    }
    struct FixtureAdapter {
        fail_asset: Option<String>,
        wrong_block: bool,
    }
    impl VenueAdapter for FixtureAdapter {
        fn venue_id(&self) -> &str {
            "fixture"
        }
        fn execution_class(&self) -> ExecutionClass {
            ExecutionClass::MonadAtomic
        }
        fn discover(&self, catalog: &Catalog, asset_id: &str) -> Result<bool> {
            Ok(catalog
                .token_observations
                .iter()
                .any(|t| super::asset_id(t) == asset_id))
        }
        fn read(&self, block: &BlockRef, _: &str) -> Result<VenueState> {
            let mut block = block.clone();
            if self.wrong_block {
                block.number += 1;
            }
            Ok(VenueState {
                venue_id: self.venue_id().into(),
                block,
                implementation_hash: "implementation".into(),
                typed_abi_hash: "abi".into(),
            })
        }
        fn quote(&self, request: &QuoteRequest, state: &VenueState) -> Result<ExactQuote> {
            if self.fail_asset.as_deref() == Some(request.asset_id.as_str())
                && request.operation == Operation::Sell
            {
                return Err("fixture sell unavailable".into());
            }
            Ok(ExactQuote {
                venue_id: self.venue_id().into(),
                execution_class: self.execution_class(),
                state: state.clone(),
                asset_id: request.asset_id.clone(),
                operation: request.operation,
                input_atoms: request.input_atoms,
                output_atoms: 1,
                fee_atoms: 0,
                expires_at_ms: 200,
                quote_digest: "fixture-digest".into(),
            })
        }
        fn build(&self, _: &ExactQuote) -> Result<BuiltCall> {
            Ok(BuiltCall {
                chain_id: MAINNET_CHAIN_ID,
                target: format!("0x{}", "1".repeat(40)),
                calldata: "0x12345678".into(),
                value_atoms: 0,
                spender: None,
            })
        }
        fn simulate(&self, _: &BuiltCall, state: &VenueState) -> Result<SimulatedResult> {
            Ok(SimulatedResult {
                success: true,
                estimated_output_atoms: 1,
                gas_used: 1,
                state_block_hash: state.block.hash.clone(),
            })
        }
        fn decode_receipt(&self, _: &[u8], _: &ExactQuote) -> Result<DecodedReceipt> {
            Err("fixture is never a live receipt".into())
        }
    }

    #[test]
    fn one_run_covers_all_112_and_isolates_a_single_failure() {
        let catalog = catalog();
        assert_eq!(catalog.token_observations.len(), 112);
        let fail_asset = asset_id(&catalog.token_observations[17]);
        let report = preflight_all(
            &catalog,
            &FixtureAdapter {
                fail_asset: Some(fail_asset.clone()),
                wrong_block: false,
            },
            &block(),
            &format!("0x{}", "2".repeat(40)),
            1_000_000,
            100,
        )
        .unwrap();
        assert_eq!(report.rows.len(), 112);
        assert_eq!(report.preflight_passed, 111);
        assert_eq!(report.live_round_trips, 0);
        assert_eq!(
            report
                .rows
                .iter()
                .find(|r| r.asset_id == fail_asset)
                .unwrap()
                .stage,
            Stage::SellQuote
        );
    }

    #[test]
    fn observation_and_stale_state_never_pass_preflight() {
        let catalog = catalog();
        let observed = super::super::adapters::ObservedOnlyAdapter {
            venue_id: "monday-stock-router".into(),
            class: ExecutionClass::IssuerAsync,
        };
        let owner = format!("0x{}", "2".repeat(40));
        let blocked = preflight_all(&catalog, &observed, &block(), &owner, 1_000_000, 100).unwrap();
        assert_eq!(blocked.rows.len(), 112);
        assert_eq!(blocked.preflight_passed, 0);
        assert!(blocked.rows.iter().all(|r| r.stage == Stage::State));
        let stale = preflight_all(
            &catalog,
            &FixtureAdapter {
                fail_asset: None,
                wrong_block: true,
            },
            &block(),
            &owner,
            1_000_000,
            100,
        )
        .unwrap();
        assert_eq!(stale.preflight_passed, 0);
        assert!(stale.rows.iter().all(|r| r.stage == Stage::State));
    }
}
