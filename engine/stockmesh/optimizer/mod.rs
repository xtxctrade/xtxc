//! Global Marginal Solver. Continuous water filling over compiled bands, followed
//! by exact integer evaluation. Enumerate venue subsets to enforce the leg limit;
//! do not round allocations to 25%/50% route candidates.
use crate::compiler::{Curve, Snapshot};
use crate::{as_u64, Error, Result, WorkMeter, MAX_ATOMS, MAX_EDGES, MAX_LEGS, SCALE};

pub mod oracle;
pub mod tape;
use tape::ResidualTape;
pub const MAX_FINALISTS: usize = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Allocation {
    pub inputs: [u64; MAX_EDGES],
    pub output: u64,
    pub mask: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchStats {
    pub masks_considered: u16,
    pub feasible_allocations: u16,
    pub exact_simulations: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Certificate {
    /// Relaxed optimum + model envelope errors, for these admitted edges only.
    /// It allows more than four legs and thus remains an upper bound when the
    /// executable search is cardinality constrained or truncates its shortlist.
    pub upper_output: u64,
    pub achieved_output: u64,
    pub gap_atoms: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchReport {
    pub plan: Allocation,
    pub certificate: Certificate,
    pub stats: SearchStats,
}

/// Merge <=8 already sorted marginal streams; <=128 segments are consumed.
/// Allocation is continuous until it reaches the input token's atomic unit.
pub fn waterfill(
    curves: &[Curve],
    amount: u64,
    mask: u8,
    meter: &mut WorkMeter,
) -> Result<Allocation> {
    if curves.is_empty() || curves.len() > MAX_EDGES || amount == 0 || amount > MAX_ATOMS {
        return Err(Error::InvalidAmount);
    }
    if mask == 0 || u16::from(mask) >= (1u16 << curves.len()) {
        return Err(Error::Bounds);
    }
    let mut cursor = [0usize; MAX_EDGES];
    let mut plan = Allocation::default();
    let mut remaining = amount;
    let mut score = 0u128;
    for (i, curve) in curves.iter().enumerate() {
        if mask & (1 << i) != 0 {
            curve.validate()?;
        }
    }
    while remaining > 0 {
        let mut best: Option<usize> = None;
        for (i, curve) in curves.iter().enumerate() {
            meter.charge(1)?;
            if mask & (1 << i) == 0 || cursor[i] >= curve.len as usize {
                continue;
            }
            if best.is_none_or(|j| {
                curve.segments[cursor[i]].marginal_q32 > curves[j].segments[cursor[j]].marginal_q32
            }) {
                best = Some(i);
            }
        }
        let i = best.ok_or(Error::Capacity)?;
        let s = curves[i].segments[cursor[i]];
        let take = remaining.min(s.input_capacity);
        plan.inputs[i] += take;
        plan.mask |= 1 << i;
        score += u128::from(take) * u128::from(s.marginal_q32);
        remaining -= take;
        cursor[i] += 1;
    }
    plan.output = as_u64(score / SCALE)?;
    Ok(plan)
}

pub fn exact_output(snapshots: &[Snapshot], plan: &Allocation) -> Result<u64> {
    let mut out = 0u64;
    for (i, s) in snapshots.iter().enumerate() {
        if plan.inputs[i] == 0 {
            continue;
        }
        out = out
            .checked_add(s.exact_quote(plan.inputs[i])?)
            .ok_or(Error::Arithmetic)?;
    }
    Ok(out)
}

fn independent(snapshots: &[Snapshot], mask: u8) -> bool {
    let mut resources = 0u64;
    for (i, s) in snapshots.iter().enumerate() {
        if mask & (1 << i) == 0 {
            continue;
        }
        if resources & s.writable_resources != 0 {
            return false;
        }
        resources |= s.writable_resources;
    }
    true
}

/// Search all <=162 nonempty <=4-venue subsets of the 8-edge envelope.
/// The four best conservative estimates are evaluated using the original models.
pub fn search(
    snapshots: &[Snapshot],
    curves: &[Curve],
    amount: u64,
    eligible: u8,
    max_legs: usize,
    meter: &mut WorkMeter,
) -> Result<SearchReport> {
    let tape = ResidualTape::build(curves, eligible, meter)?;
    search_tape(snapshots, curves, &tape, amount, eligible, max_legs, meter)
}

pub fn search_tape(
    snapshots: &[Snapshot],
    curves: &[Curve],
    tape: &ResidualTape,
    amount: u64,
    eligible: u8,
    max_legs: usize,
    meter: &mut WorkMeter,
) -> Result<SearchReport> {
    if !tape.matches(curves, eligible) {
        return Err(Error::Identity);
    }
    search_with(
        snapshots,
        curves,
        amount,
        eligible,
        max_legs,
        meter,
        |mask, lower, meter| {
            if lower {
                tape.allocate_lower(amount, mask, curves, meter)
            } else {
                tape.allocate(amount, mask, meter)
            }
        },
    )
}

/// Independent streaming path retained for differential testing and ablation.
pub fn search_reference(
    snapshots: &[Snapshot],
    curves: &[Curve],
    amount: u64,
    eligible: u8,
    max_legs: usize,
    meter: &mut WorkMeter,
) -> Result<SearchReport> {
    search_with(
        snapshots,
        curves,
        amount,
        eligible,
        max_legs,
        meter,
        |mask, lower, meter| {
            let mut p = waterfill(curves, amount, mask, meter)?;
            if lower {
                p.output = 0;
                for (i, c) in curves.iter().enumerate() {
                    if p.inputs[i] > 0 {
                        p.output = p
                            .output
                            .checked_add(c.lower_quote(p.inputs[i])?)
                            .ok_or(Error::Arithmetic)?;
                    }
                }
            }
            Ok(p)
        },
    )
}

fn search_with<F>(
    snapshots: &[Snapshot],
    curves: &[Curve],
    amount: u64,
    eligible: u8,
    max_legs: usize,
    meter: &mut WorkMeter,
    mut query: F,
) -> Result<SearchReport>
where
    F: FnMut(u8, bool, &mut WorkMeter) -> Result<Allocation>,
{
    if snapshots.len() != curves.len()
        || snapshots.is_empty()
        || snapshots.len() > MAX_EDGES
        || max_legs == 0
        || max_legs > MAX_LEGS
    {
        return Err(Error::Bounds);
    }
    if amount == 0 || amount > MAX_ATOMS {
        return Err(Error::InvalidAmount);
    }
    let mut pair = None;
    for (i, s) in snapshots.iter().enumerate() {
        if eligible & (1 << i) == 0 {
            continue;
        }
        if s.input_mint == s.output_mint || !s.atomic || !s.enabled {
            return Err(Error::Unsupported);
        }
        let current_pair = (s.input_mint, s.output_mint);
        if pair.is_some_and(|p| p != current_pair) {
            return Err(Error::Identity);
        }
        pair = Some(current_pair);
        if curves[i].capacity > s.capacity {
            return Err(Error::Capacity);
        }
        for (j, prior) in snapshots[..i].iter().enumerate() {
            if eligible & (1 << j) != 0
                && (prior.market == s.market || prior.liquidity_id == s.liquidity_id)
            {
                return Err(Error::AliasedLiquidity);
            }
        }
    }
    let relaxed = query(eligible, false, meter)?;
    let mut upper = relaxed.output;
    for (i, c) in curves.iter().enumerate() {
        if eligible & (1 << i) != 0 {
            upper = upper
                .checked_add(c.upper_error_atoms)
                .ok_or(Error::Arithmetic)?;
        }
    }
    let mut finalists = [Allocation::default(); MAX_FINALISTS];
    let mut count = 0usize;
    let mut stats = SearchStats::default();
    for m in 1..(1u16 << curves.len()) {
        meter.charge(1)?;
        let mask = m as u8;
        if mask & !eligible != 0 || mask.count_ones() as usize > max_legs {
            continue;
        }
        stats.masks_considered += 1;
        if !independent(snapshots, mask) {
            continue;
        }
        let p = match query(mask, true, meter) {
            Ok(p) => p,
            Err(Error::Capacity) => continue,
            Err(e) => return Err(e),
        };
        stats.feasible_allocations += 1;
        if finalists[..count].iter().any(|f| f.inputs == p.inputs) {
            continue;
        }
        let pos = finalists[..count]
            .iter()
            .position(|f| p.output > f.output)
            .unwrap_or(count);
        if pos >= MAX_FINALISTS {
            continue;
        }
        for j in ((pos + 1)..count.min(MAX_FINALISTS - 1) + 1).rev() {
            finalists[j] = finalists[j - 1];
        }
        finalists[pos] = p;
        count = (count + 1).min(MAX_FINALISTS);
    }
    if count == 0 {
        return Err(Error::Capacity);
    }
    let mut best = Allocation::default();
    for candidate in &finalists[..count] {
        meter.charge(snapshots.len() as u32)?;
        stats.exact_simulations += 1;
        let exact = exact_output(snapshots, candidate)?;
        if best.mask == 0 || exact > best.output {
            best = Allocation {
                output: exact,
                ..*candidate
            };
        }
    }
    if best.output > upper {
        return Err(Error::InvalidCurve);
    }
    Ok(SearchReport {
        plan: best,
        stats,
        certificate: Certificate {
            upper_output: upper,
            achieved_output: best.output,
            gap_atoms: upper - best.output,
        },
    })
}
