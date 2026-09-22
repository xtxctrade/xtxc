//! OneBook: a bounded market over economic exposure whose settlement claim is
//! selected at fill time.
//!
//! This module does not custody orders and does not pretend a sampled AMM quote
//! is resting liquidity. It compiles state-bound executable slices into virtual
//! levels and continuously crosses compatible signed claim constraints. The
//! resulting exact claim allocations are inputs to the StockMesh settlement
//! compiler and on-chain aggregate-exposure postcondition.
use crate::{as_u64, ceil_div, Error, Key, Result, WorkMeter, MAX_ATOMS, SCALE};

pub const MAX_BOOK_ORDERS: usize = 16;
pub const MAX_CLAIMS: usize = 8;
pub const MAX_MATCHES: usize = 32;
pub const MAX_VIRTUAL_LEVELS: usize = 32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Side {
    #[default]
    Buy,
    Sell,
}

/// A signed economic order. `exposure_q32` is underlying-share exposure. A
/// sell order delivers one exact claim; a buy order accepts an explicit set of
/// claims. No mint may be substituted outside that set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SettlementOptionalOrder {
    pub owner: Key,
    pub nonce: u64,
    pub instrument: Key,
    pub side: Side,
    pub exposure_q32: u64,
    /// USDC atomic units per underlying share in Q32.
    pub limit_price_q32: u64,
    pub acceptable_claims: [Key; MAX_CLAIMS],
    pub claim_count: u8,
    /// Required for sells and zero for buys.
    pub delivered_claim: Key,
    pub max_conversion_bps: u16,
    pub expires_at_slot: u64,
    /// Time-priority sequence assigned before matching.
    pub arrival_sequence: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrossClaimFill {
    pub buyer: u8,
    pub seller: u8,
    pub claim: Key,
    pub exposure_q32: u64,
    pub price_q32: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MatchReport {
    pub fills: [CrossClaimFill; MAX_MATCHES],
    pub fill_count: u8,
    pub filled_exposure_q32: [u64; MAX_BOOK_ORDERS],
    pub residual_exposure_q32: [u64; MAX_BOOK_ORDERS],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExecutableSlice {
    pub claim: Key,
    pub source: Key,
    pub state_hash: Key,
    pub input_usdc_atoms: u64,
    pub exposure_q32: u64,
    pub conversion_bps: u16,
    pub expires_at_slot: u64,
}

/// A virtual level is a state-bound materialization recipe, never a promise of
/// resting inventory. It expires with the source state and must be exact-
/// simulated again before a wallet signs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VirtualLevel {
    pub claim: Key,
    pub source: Key,
    pub state_hash: Key,
    pub price_q32: u64,
    pub exposure_q32: u64,
    pub input_usdc_atoms: u64,
    pub conversion_bps: u16,
    pub expires_at_slot: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VirtualBook {
    pub levels: [VirtualLevel; MAX_VIRTUAL_LEVELS],
    pub level_count: u8,
    pub total_exposure_q32: u128,
    pub state_hash: Key,
}

fn contains_claim(order: &SettlementOptionalOrder, claim: &Key) -> bool {
    order.acceptable_claims[..usize::from(order.claim_count)]
        .iter()
        .any(|candidate| candidate == claim)
}

fn validate_order(order: &SettlementOptionalOrder, slot: u64) -> Result<()> {
    if order.owner == [0; 32]
        || order.instrument == [0; 32]
        || order.exposure_q32 == 0
        || order.exposure_q32 > MAX_ATOMS
        || order.limit_price_q32 == 0
        || order.claim_count == 0
        || usize::from(order.claim_count) > MAX_CLAIMS
        || order.max_conversion_bps > 10_000
        || order.expires_at_slot < slot
    {
        return Err(Error::InvalidAmount);
    }
    for index in 0..usize::from(order.claim_count) {
        let claim = order.acceptable_claims[index];
        if claim == [0; 32] || order.acceptable_claims[..index].contains(&claim) {
            return Err(Error::Duplicate);
        }
    }
    match order.side {
        Side::Buy if order.delivered_claim != [0; 32] => Err(Error::Identity),
        Side::Sell
            if order.delivered_claim == [0; 32]
                || !contains_claim(order, &order.delivered_claim) =>
        {
            Err(Error::Identity)
        }
        _ => Ok(()),
    }
}

fn crosses(buyer: &SettlementOptionalOrder, seller: &SettlementOptionalOrder) -> Result<bool> {
    let adjusted_ask = u128::from(seller.limit_price_q32)
        .checked_mul(u128::from(10_000u16 + buyer.max_conversion_bps))
        .ok_or(Error::Arithmetic)?;
    Ok(contains_claim(buyer, &seller.delivered_claim)
        && adjusted_ask
            <= u128::from(buyer.limit_price_q32)
                .checked_mul(10_000)
                .ok_or(Error::Arithmetic)?)
}

/// Continuous price/time matching across distinct issuer claims. The engine
/// picks the best compatible ask for the best bid on every iteration; it does
/// not wait for an epoch or auction window.
pub fn match_continuous(
    orders: &[SettlementOptionalOrder],
    slot: u64,
    meter: &mut WorkMeter,
) -> Result<MatchReport> {
    if orders.len() < 2 || orders.len() > MAX_BOOK_ORDERS {
        return Err(Error::Bounds);
    }
    for (index, order) in orders.iter().enumerate() {
        validate_order(order, slot)?;
        if orders[..index]
            .iter()
            .any(|prior| prior.owner == order.owner && prior.nonce == order.nonce)
        {
            return Err(Error::Duplicate);
        }
        if orders[0].instrument != order.instrument {
            return Err(Error::Identity);
        }
    }
    let mut report = MatchReport::default();
    for (index, order) in orders.iter().enumerate() {
        report.residual_exposure_q32[index] = order.exposure_q32;
    }
    loop {
        let mut best: Option<(usize, usize)> = None;
        for (buyer_index, buyer) in orders.iter().enumerate() {
            if buyer.side != Side::Buy || report.residual_exposure_q32[buyer_index] == 0 {
                continue;
            }
            for (seller_index, seller) in orders.iter().enumerate() {
                meter.charge(1)?;
                if seller.side != Side::Sell
                    || report.residual_exposure_q32[seller_index] == 0
                    || !crosses(buyer, seller)?
                {
                    continue;
                }
                let candidate = (buyer_index, seller_index);
                if best.is_none_or(|(best_buyer, best_seller)| {
                    (
                        core::cmp::Reverse(buyer.limit_price_q32),
                        buyer.arrival_sequence,
                        seller.limit_price_q32,
                        seller.arrival_sequence,
                        buyer_index,
                        seller_index,
                    ) < (
                        core::cmp::Reverse(orders[best_buyer].limit_price_q32),
                        orders[best_buyer].arrival_sequence,
                        orders[best_seller].limit_price_q32,
                        orders[best_seller].arrival_sequence,
                        best_buyer,
                        best_seller,
                    )
                }) {
                    best = Some(candidate);
                }
            }
        }
        let Some((buyer, seller)) = best else { break };
        let exposure =
            report.residual_exposure_q32[buyer].min(report.residual_exposure_q32[seller]);
        let position = usize::from(report.fill_count);
        if position >= MAX_MATCHES {
            return Err(Error::Bounds);
        }
        report.fills[position] = CrossClaimFill {
            buyer: buyer as u8,
            seller: seller as u8,
            claim: orders[seller].delivered_claim,
            exposure_q32: exposure,
            price_q32: orders[seller].limit_price_q32,
        };
        report.fill_count += 1;
        report.residual_exposure_q32[buyer] -= exposure;
        report.residual_exposure_q32[seller] -= exposure;
        report.filled_exposure_q32[buyer] = report.filled_exposure_q32[buyer]
            .checked_add(exposure)
            .ok_or(Error::Arithmetic)?;
        report.filled_exposure_q32[seller] = report.filled_exposure_q32[seller]
            .checked_add(exposure)
            .ok_or(Error::Arithmetic)?;
    }
    Ok(report)
}

/// Compile executable venue/issuer slices into ask-side OneBook depth. Every
/// level is tied to one coherent state hash. Mixing bank generations is fatal.
pub fn compile_virtual_book(
    slices: &[ExecutableSlice],
    slot: u64,
    meter: &mut WorkMeter,
) -> Result<VirtualBook> {
    if slices.is_empty() || slices.len() > MAX_VIRTUAL_LEVELS {
        return Err(Error::Bounds);
    }
    let state_hash = slices[0].state_hash;
    if state_hash == [0; 32] {
        return Err(Error::Identity);
    }
    let mut book = VirtualBook {
        state_hash,
        ..VirtualBook::default()
    };
    for slice in slices {
        meter.charge(1)?;
        if slice.claim == [0; 32]
            || slice.source == [0; 32]
            || slice.state_hash != state_hash
            || slice.input_usdc_atoms == 0
            || slice.input_usdc_atoms > MAX_ATOMS
            || slice.exposure_q32 == 0
            || slice.exposure_q32 > MAX_ATOMS
            || slice.conversion_bps > 10_000
            || slice.expires_at_slot < slot
        {
            return Err(Error::InvalidAmount);
        }
        let numerator = u128::from(slice.input_usdc_atoms)
            .checked_mul(SCALE)
            .and_then(|value| value.checked_mul(SCALE))
            .ok_or(Error::Arithmetic)?;
        let price_q32 = as_u64(ceil_div(numerator, u128::from(slice.exposure_q32))?)?;
        let level = VirtualLevel {
            claim: slice.claim,
            source: slice.source,
            state_hash,
            price_q32,
            exposure_q32: slice.exposure_q32,
            input_usdc_atoms: slice.input_usdc_atoms,
            conversion_bps: slice.conversion_bps,
            expires_at_slot: slice.expires_at_slot,
        };
        let mut position = usize::from(book.level_count);
        while position > 0 {
            let prior = book.levels[position - 1];
            if (prior.price_q32, prior.claim, prior.source)
                <= (level.price_q32, level.claim, level.source)
            {
                break;
            }
            book.levels[position] = prior;
            position -= 1;
        }
        book.levels[position] = level;
        book.level_count += 1;
        book.total_exposure_q32 = book
            .total_exposure_q32
            .checked_add(u128::from(slice.exposure_q32))
            .ok_or(Error::Arithmetic)?;
    }
    Ok(book)
}
