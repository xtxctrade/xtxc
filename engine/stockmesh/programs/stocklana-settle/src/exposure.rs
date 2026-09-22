//! StockMesh economic-exposure settlement.
//!
//! Opcode 13 executes several issuer-product graphs for one signed stock intent
//! and accepts only their aggregate conservative underlying-share exposure.
//! Each graph and issuer policy is preflighted before the first CPI. Product
//! mints remain distinct throughout execution; only the postcondition is in the
//! common Q32 exposure unit.
use super::*;
use dex::graph::Graph;
use skew_native::ScaledUiAmount;

pub(super) const MAX_PRODUCTS: usize = 4;
const HEADER_LEN: usize = 28;
const DESCRIPTOR_LEN: usize = 32;
pub(super) const FIXED_RATIONAL: u8 = 0;
pub(super) const TOKEN_2022_SCALED_UI: u8 = 1;

/// Mint-bound conversion compiled once before an on-chain marginal search.
/// Re-decoding Token-2022 extensions for every oracle probe would waste CU and
/// make the optimizer's work limit a poor latency bound.
#[derive(Clone, Copy)]
pub(super) struct Converter {
    scaled: ScaledUiAmount,
    numerator: u64,
    denominator: u64,
    conservative_bps: u16,
}

impl Converter {
    #[allow(clippy::too_many_arguments)]
    pub fn decode(
        a: &[AccountInfo],
        output_mint: usize,
        exposure_model: u8,
        conservative_bps: u16,
        numerator: u64,
        denominator: u64,
        timestamp: i64,
    ) -> Result<Self, ProgramError> {
        let mint = &a[output_mint];
        let scaled = match exposure_model {
            FIXED_RATIONAL => {
                let data = mint.try_borrow_data()?;
                need(data.len() >= 82 && data[45] == 1, IDENTITY)?;
                ScaledUiAmount {
                    decimals: data[44],
                    multiplier_q32: 1u64 << 32,
                    next_multiplier_effective_timestamp: i64::MAX,
                    next_multiplier_q32: 1u64 << 32,
                }
            }
            TOKEN_2022_SCALED_UI => {
                need(*mint.owner == super::graph::TOKEN_2022, IDENTITY)?;
                let scaled = native(ScaledUiAmount::decode(
                    &mint.try_borrow_data()?,
                    timestamp,
                ))?;
                let distance = i128::from(timestamp)
                    .checked_sub(i128::from(scaled.next_multiplier_effective_timestamp))
                    .ok_or(err(BOUNDS))?
                    .abs();
                need(
                    scaled.next_multiplier_effective_timestamp <= 0 || distance > 900,
                    EXPIRED,
                )?;
                scaled
            }
            _ => return Err(err(BOUNDS)),
        };
        need(
            numerator > 0
                && denominator > 0
                && (1..=10_000).contains(&conservative_bps),
            BOUNDS,
        )?;
        Ok(Self {
            scaled,
            numerator,
            denominator,
            conservative_bps,
        })
    }

    #[inline]
    pub fn convert(self, raw_output: u64) -> Result<u64, ProgramError> {
        native(self.scaled.exposure_q32(
            raw_output,
            self.numerator,
            self.denominator,
            self.conservative_bps,
        ))
    }
}

#[derive(Clone, Copy)]
struct ProductGraph<'a> {
    policy_index: usize,
    exposure_model: u8,
    conservative_bps: u16,
    policy_version: u64,
    numerator: u64,
    denominator: u64,
    bytes: &'a [u8],
    output_mint: usize,
}

fn references(graph: &Graph, index: usize) -> bool {
    graph.assets[..graph.asset_count]
        .iter()
        .any(|asset| [asset.token, asset.mint, asset.program].contains(&(index as u8)))
        || graph.legs[..graph.leg_count]
            .iter()
            .flatten()
            .any(|leg| leg.program as usize == index || leg.accounts.contains(&(index as u8)))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn convert(
    a: &[AccountInfo],
    output_mint: usize,
    exposure_model: u8,
    conservative_bps: u16,
    numerator: u64,
    denominator: u64,
    raw_output: u64,
    timestamp: i64,
) -> Result<u64, ProgramError> {
    Converter::decode(
        a,
        output_mint,
        exposure_model,
        conservative_bps,
        numerator,
        denominator,
        timestamp,
    )?
    .convert(raw_output)
}

fn native<T>(value: skew_native::Result<T>) -> Result<T, ProgramError> {
    value.map_err(|_| err(ADAPTER))
}

/// Layout: `opcode, product_count, allow_closed, reserved, deadline,
/// minimum_exposure_q32, maximum_policy_age`; then for each product a 32-byte
/// descriptor followed by one typed graph. The descriptor is
/// `policy_index, exposure_model, conservative_bps, policy_version,
/// numerator, denominator, graph_len, reserved`.
#[inline(never)]
pub(super) fn execute(program: &Pubkey, a: &[AccountInfo], d: &[u8]) -> ProgramResult {
    need(
        d.len() >= HEADER_LEN && d[0] == 13 && d[2] <= 1 && d[3] == 0,
        BOUNDS,
    )?;
    let count = d[1] as usize;
    need((1..=MAX_PRODUCTS).contains(&count) && a.len() <= 64, BOUNDS)?;
    let deadline = ad(dex::u64_at(d, 4))?;
    let minimum_exposure = ad(dex::u64_at(d, 12))?;
    let maximum_policy_age = ad(dex::u64_at(d, 20))?;
    need(
        minimum_exposure > 0 && (1..=150).contains(&maximum_policy_age),
        BOUNDS,
    )?;
    let clock = Clock::get()?;
    need(clock.slot <= deadline && clock.unix_timestamp >= 0, EXPIRED)?;
    need(
        a.len() > 2 && a[0].is_signer && a[1].is_writable && a[1].owner == program,
        IDENTITY,
    )?;

    let mut products: [Option<ProductGraph<'_>>; MAX_PRODUCTS] = [None; MAX_PRODUCTS];
    let mut graphs: [Option<Graph<'_>>; MAX_PRODUCTS] = core::array::from_fn(|_| None);
    let mut cursor = HEADER_LEN;
    let mut sequence = None;
    let mut source = None;
    let mut total_input = 0u64;
    let mut maximum_legs = 0usize;
    for position in 0..count {
        let descriptor = d
            .get(cursor..cursor.checked_add(DESCRIPTOR_LEN).ok_or(err(BOUNDS))?)
            .ok_or(err(BOUNDS))?;
        cursor = cursor.checked_add(DESCRIPTOR_LEN).ok_or(err(BOUNDS))?;
        need(descriptor[30..32] == [0, 0], BOUNDS)?;
        let graph_len = usize::from(u16::from_le_bytes([descriptor[28], descriptor[29]]));
        let bytes = d
            .get(cursor..cursor.checked_add(graph_len).ok_or(err(BOUNDS))?)
            .ok_or(err(BOUNDS))?;
        cursor = cursor.checked_add(graph_len).ok_or(err(BOUNDS))?;
        let graph = ad(Graph::decode(bytes, a.len()))?;
        need(graph.deadline <= deadline && graph.min_out > 0, BOUNDS)?;
        let source_asset = graph.assets[0];
        let sink = graph.assets[graph.asset_count - 1];
        let current_source = (source_asset.token, source_asset.mint, source_asset.program);
        if let Some(expected) = source {
            need(current_source == expected, IDENTITY)?;
        } else {
            source = Some(current_source);
        }
        if let Some(expected) = sequence {
            need(graph.sequence == expected, REPLAY)?;
        } else {
            sequence = Some(graph.sequence);
        }
        total_input = total_input.checked_add(graph.input).ok_or(err(BOUNDS))?;
        maximum_legs = maximum_legs
            .checked_add(if graph.reflow_calls > 0 {
                4
            } else {
                graph.leg_count
            })
            .ok_or(err(BOUNDS))?;
        need(total_input <= dex::MAX_INPUT && maximum_legs <= 4, BOUNDS)?;
        let product = ProductGraph {
            policy_index: descriptor[0] as usize,
            exposure_model: descriptor[1],
            conservative_bps: u16::from_le_bytes([descriptor[2], descriptor[3]]),
            policy_version: ad(dex::u64_at(descriptor, 4))?,
            numerator: ad(dex::u64_at(descriptor, 12))?,
            denominator: ad(dex::u64_at(descriptor, 20))?,
            bytes,
            output_mint: sink.mint as usize,
        };
        need(
            product.policy_version > 0
                && product.numerator > 0
                && product.denominator > 0
                && (1..=10_000).contains(&product.conservative_bps)
                && matches!(
                    product.exposure_model,
                    FIXED_RATIONAL | TOKEN_2022_SCALED_UI
                ),
            BOUNDS,
        )?;
        super::stock::validate_policy_for_graph(
            program,
            a,
            product.policy_index,
            product.policy_version,
            maximum_policy_age,
            d[2] == 1,
            &graph,
            &clock,
        )?;
        for prior in graphs[..position].iter().flatten() {
            let prior_sink = prior.assets[prior.asset_count - 1];
            need(
                a[prior_sink.mint as usize].key != a[sink.mint as usize].key,
                IDENTITY,
            )?;
            for (index, account) in a.iter().enumerate() {
                if account.is_writable
                    && index != source_asset.token as usize
                    && references(prior, index)
                    && references(&graph, index)
                {
                    return Err(err(IDENTITY));
                }
            }
        }
        products[position] = Some(product);
        graphs[position] = Some(graph);
    }
    need(cursor == d.len(), BOUNDS)?;
    let mut aggregate_asset_tokens = [u8::MAX; MAX_PRODUCTS * dex::graph::MAX_ASSETS];
    let mut aggregate_asset_count = 0usize;
    for graph in graphs[..count].iter().flatten() {
        for asset in &graph.assets[..graph.asset_count] {
            if !aggregate_asset_tokens[..aggregate_asset_count].contains(&asset.token) {
                aggregate_asset_tokens[aggregate_asset_count] = asset.token;
                aggregate_asset_count = aggregate_asset_count.checked_add(1).ok_or(err(BOUNDS))?;
            }
        }
    }
    // Every graph sees one aggregate account list. Admit only wallet token
    // accounts proven to be typed assets of a sibling graph, then validate all
    // graphs before the first DEX CPI.
    for product in products[..count].iter().flatten() {
        super::graph::preflight_graph_for_allowing(
            program,
            a,
            product.bytes,
            0,
            1,
            &aggregate_asset_tokens[..aggregate_asset_count],
        )?;
    }
    let (source_token, _, _) = source.ok_or(err(BOUNDS))?;
    let first = graphs[0].as_ref().ok_or(err(BOUNDS))?;
    let before_input = graph::checked_asset(
        &a[0],
        &a[source_token as usize],
        &a[first.assets[0].mint as usize],
        &a[first.assets[0].program as usize],
        false,
    )?;
    need(before_input >= total_input, BOUNDS)?;

    // All products, policies, graphs and writable-account partitions have now
    // passed. Any later failure rolls every CPI back with the Solana instruction.
    let mut raw_outputs = [0u64; MAX_PRODUCTS];
    let mut exposures = [0u64; MAX_PRODUCTS];
    let mut total_exposure = 0u64;
    for position in 0..count {
        let product = products[position].ok_or(err(BOUNDS))?;
        let outcome = super::graph::execute_preflighted_graph_for(a, product.bytes, 0)?;
        need(outcome.sequence == sequence.ok_or(err(REPLAY))?, REPLAY)?;
        raw_outputs[position] = outcome.actual_out;
        exposures[position] = convert(
            a,
            product.output_mint,
            product.exposure_model,
            product.conservative_bps,
            product.numerator,
            product.denominator,
            outcome.actual_out,
            clock.unix_timestamp,
        )?;
        total_exposure = total_exposure
            .checked_add(exposures[position])
            .ok_or(err(BOUNDS))?;
    }
    let after_input = graph::checked_asset(
        &a[0],
        &a[source_token as usize],
        &a[first.assets[0].mint as usize],
        &a[first.assets[0].program as usize],
        false,
    )?;
    need(
        before_input.checked_sub(after_input) == Some(total_input),
        CONSERVATION,
    )?;
    need(total_exposure >= minimum_exposure, MIN_OUT)?;

    let sequence = sequence.ok_or(err(REPLAY))?;
    let next = sequence.checked_add(1).ok_or(err(BOUNDS))?;
    {
        let mut nonce = a[1].try_borrow_mut_data()?;
        nonce[40..48].copy_from_slice(&next.to_le_bytes());
        nonce[48..56].copy_from_slice(&total_input.to_le_bytes());
        nonce[56..64].copy_from_slice(&total_exposure.to_le_bytes());
    }
    let mut receipt = [0u8; 40 + MAX_PRODUCTS * 16];
    receipt[..8].copy_from_slice(b"SKEWEXP1");
    for (chunk, value) in receipt[8..40].chunks_exact_mut(8).zip([
        sequence,
        total_input,
        total_exposure,
        count as u64,
    ]) {
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    for position in 0..count {
        let offset = 40 + position * 16;
        receipt[offset..offset + 8].copy_from_slice(&raw_outputs[position].to_le_bytes());
        receipt[offset + 8..offset + 16].copy_from_slice(&exposures[position].to_le_bytes());
    }
    set_return_data(&receipt[..40 + count * 16]);
    Ok(())
}
