//! Bounded state machine: read -> compile -> solve -> invoke -> observe balances.
//! A successful partial/zero fill can cause reallocation. A CPI error cannot.
use crate::compiler::{compile, Curve};
use crate::optimizer::tape::ResidualTape;
use crate::optimizer::{search_tape, SearchReport};
use crate::runtime::{EdgePin, ExecutionHost};
use crate::{Error, Key, Result, WorkMeter, MAX_ATOMS, MAX_EDGES, MAX_LEGS, MAX_REFLOWS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Intent {
    pub input_mint: Key,
    pub output_mint: Key,
    pub amount_in: u64,
    pub min_out: u64,
    pub expires_at_slot: u64,
    pub max_snapshot_age: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_legs: u8,
    pub max_reflows: u8,
    pub max_work: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_legs: 4,
            max_reflows: 3,
            max_work: 200_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LegTrace {
    pub edge: u8,
    pub requested: u64,
    pub spent: u64,
    pub received: u64,
    pub residual: u64,
    pub eligible_mask: u8,
    pub chosen_mask: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Receipt {
    pub initial: SearchReport,
    pub input_spent: u64,
    pub output_received: u64,
    pub legs: u8,
    pub reflows: u8,
    pub work_units: u32,
    pub curves_compiled: u16,
    pub tape_builds: u8,
    pub trace: [LegTrace; MAX_LEGS],
}

pub fn execute<H: ExecutionHost>(
    host: &mut H,
    pins: &[EdgePin],
    intent: Intent,
    limits: Limits,
) -> Result<Receipt> {
    if pins.is_empty()
        || pins.len() > MAX_EDGES
        || limits.max_legs == 0
        || limits.max_legs as usize > MAX_LEGS
        || limits.max_reflows as usize > MAX_REFLOWS
    {
        return Err(Error::Bounds);
    }
    if intent.amount_in == 0
        || intent.amount_in > MAX_ATOMS
        || intent.input_mint == intent.output_mint
    {
        return Err(Error::InvalidAmount);
    }
    for (i, pin) in pins.iter().enumerate() {
        for other in &pins[..i] {
            if pin.market == other.market || pin.liquidity_id == other.liquidity_id {
                return Err(Error::AliasedLiquidity);
            }
        }
    }
    let slot = host.slot();
    if slot > intent.expires_at_slot {
        return Err(Error::Expired);
    }
    let start = host.balances()?;
    if start.input < intent.amount_in {
        return Err(Error::BalanceInvariant);
    }
    let mut before = start;
    let mut remaining = intent.amount_in;
    let mut used = 0u8;
    let mut used_resources = 0u64;
    let mut meter = WorkMeter::new(limits.max_work);
    let mut receipt = Receipt::default();
    let mut snapshots = [host.snapshot(0)?; MAX_EDGES];
    let mut curves = [Curve::default(); MAX_EDGES];
    let mut tape = None;
    loop {
        if receipt.legs >= limits.max_legs {
            return Err(Error::Bounds);
        }
        if receipt.legs > 0 {
            if receipt.reflows >= limits.max_reflows {
                return Err(Error::Bounds);
            }
            receipt.reflows += 1;
        }
        if host.slot() != slot {
            return Err(Error::Stale);
        }
        let mut dirty = tape.is_none();
        let mut eligible = 0u8;
        for (i, pin) in pins.iter().enumerate() {
            meter.charge(1)?;
            let s = host.snapshot(i)?;
            if s.market != pin.market
                || s.program != pin.program
                || s.liquidity_id != pin.liquidity_id
                || s.input_mint != intent.input_mint
                || s.output_mint != intent.output_mint
                || s.writable_resources != pin.writable_resources
            {
                return Err(Error::Identity);
            }
            if used & (1 << i) != 0 || used_resources & s.writable_resources != 0 {
                continue;
            }
            // Unavailable/expired/stale candidates are skipped BEFORE CPI.
            if !s.enabled
                || !s.atomic
                || s.capacity == 0
                || s.expires_at_slot < slot
                || s.slot > slot
                || slot - s.slot > intent.max_snapshot_age
                || s.generation < pin.min_generation
            {
                continue;
            }
            // The transaction holds account locks. For independent unexecuted
            // pools, our own CPI cannot change their curves. Reuse the tape for
            // smaller residuals; if host state does change, rebuild explicitly.
            if curves[i].len == 0 || snapshots[i] != s {
                curves[i] = compile(&s, intent.amount_in, &mut meter)?;
                receipt.curves_compiled += 1;
                dirty = true;
            }
            snapshots[i] = s;
            eligible |= 1 << i;
        }
        if eligible == 0 {
            return Err(Error::Capacity);
        }
        if dirty {
            tape = Some(ResidualTape::build(
                &curves[..pins.len()],
                eligible,
                &mut meter,
            )?);
            receipt.tape_builds += 1;
        }
        let leg_budget = usize::from(limits.max_legs - receipt.legs)
            .min(usize::from(limits.max_reflows - receipt.reflows) + 1);
        let report = search_tape(
            &snapshots[..pins.len()],
            &curves[..pins.len()],
            tape.as_ref().ok_or(Error::InvalidCurve)?,
            remaining,
            eligible,
            leg_budget,
            &mut meter,
        )?;
        if receipt.legs == 0 {
            receipt.initial = report;
        }
        // Execute the most imminent-expiry selected venue first; stable tie by
        // envelope index. This policy is explicit and benchmarked, not clairvoyant.
        let edge = (0..pins.len())
            .filter(|i| report.plan.inputs[*i] > 0)
            .min_by_key(|i| (snapshots[*i].expires_at_slot, *i))
            .ok_or(Error::Capacity)?;
        let request = report.plan.inputs[edge];
        meter.charge(1)?;
        host.invoke(edge, request)?; // fatal; NEVER catch-and-route-on-CPI-error
        let after = host.balances()?;
        let spent = before
            .input
            .checked_sub(after.input)
            .ok_or(Error::BalanceInvariant)?;
        let received = after
            .output
            .checked_sub(before.output)
            .ok_or(Error::BalanceInvariant)?;
        if spent > request
            || spent > remaining
            || (spent == 0 && received != 0)
            || (spent > 0 && received == 0)
        {
            return Err(Error::BalanceInvariant);
        }
        remaining -= spent;
        receipt.trace[receipt.legs as usize] = LegTrace {
            edge: edge as u8,
            requested: request,
            spent,
            received,
            residual: remaining,
            eligible_mask: eligible,
            chosen_mask: report.plan.mask,
        };
        receipt.legs += 1;
        used |= 1 << edge;
        used_resources |= snapshots[edge].writable_resources;
        before = after;
        if remaining == 0 {
            break;
        }
    }
    receipt.input_spent = start
        .input
        .checked_sub(before.input)
        .ok_or(Error::BalanceInvariant)?;
    receipt.output_received = before
        .output
        .checked_sub(start.output)
        .ok_or(Error::BalanceInvariant)?;
    if receipt.input_spent != intent.amount_in {
        return Err(Error::BalanceInvariant);
    }
    if receipt.output_received < intent.min_out {
        return Err(Error::MinOut);
    }
    receipt.work_units = meter.used;
    Ok(receipt)
}
