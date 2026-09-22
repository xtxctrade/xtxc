//! Residual Tape: deletion-closed rank/select over a merged marginal book.
//!
//! Prefix row P[k][i] stores the number of complete bands owned by venue i among
//! the first k globally sorted bands. The indexed curve maps that byte directly
//! to both capacity and integral. Select the first row k whose selected capacity
//! sum reaches the residual. Its predecessor is the complete-band
//! allocation; exactly one marginal band takes the remaining atoms.
//!
//! Removing a venue preserves relative marginal order, so the same tape serves
//! every subset and every residual. This is an exact acceleration of the IR
//! solver, not a coarser allocation grid or a new source of execution alpha.
use crate::compiler::Curve;
use crate::optimizer::Allocation;
use crate::{as_u64, Error, Result, WorkMeter, MAX_ATOMS, MAX_EDGES, MAX_SEGMENTS, SCALE};

pub const MAX_BANDS: usize = MAX_EDGES * MAX_SEGMENTS;

// Leaf ranges are fixed, so a tie always selects the lower venue index.
// Zero-rate bands are valid; exhaustion needs its own sentinel, not rate=0.
const NO_EDGE: u8 = MAX_EDGES as u8;

#[inline]
fn winner(left: u8, right: u8, rates: &[u64; MAX_EDGES]) -> u8 {
    if left == NO_EDGE {
        right
    } else if right == NO_EDGE || rates[left as usize] >= rates[right as usize] {
        left
    } else {
        right
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct IndexedCurve {
    // Index zero is the exact empty prefix. Index n is n completed bands.
    capacity: [u64; MAX_SEGMENTS + 1],
    integral: [u128; MAX_SEGMENTS + 1],
    rates: [u64; MAX_SEGMENTS],
    len: usize,
}

/// Fixed-size caller-owned storage. No heap allocation or sort in allocate().
/// Large enough to require an explicit heap/account layout in an SBF adapter;
/// do not put this value in a 4 KiB SBF stack frame.
pub struct ResidualTape {
    prefix: [[u8; MAX_EDGES]; MAX_BANDS + 1],
    owner: [u8; MAX_BANDS + 1],
    curves: [IndexedCurve; MAX_EDGES],
    bands: usize,
    edges: usize,
    compiled_mask: u8,
}

impl ResidualTape {
    pub fn matches(&self, curves: &[Curve], mask: u8) -> bool {
        if curves.len() != self.edges || mask & !self.compiled_mask != 0 {
            return false;
        }
        for (i, c) in curves.iter().enumerate() {
            if mask & (1 << i) == 0 {
                continue;
            }
            if c.len as usize != self.curves[i].len || c.len as usize > MAX_SEGMENTS {
                return false;
            }
            let mut capacity = 0u64;
            for (j, s) in c.segments[..c.len as usize].iter().enumerate() {
                let Some(sum) = capacity.checked_add(s.input_capacity) else {
                    return false;
                };
                capacity = sum;
                if capacity != self.curves[i].capacity[j + 1]
                    || s.marginal_q32 != self.curves[i].rates[j]
                {
                    return false;
                }
            }
            if capacity != c.capacity {
                return false;
            }
        }
        true
    }

    pub fn build(curves: &[Curve], mask: u8, meter: &mut WorkMeter) -> Result<Self> {
        if curves.is_empty()
            || curves.len() > MAX_EDGES
            || mask == 0
            || u16::from(mask) >= (1u16 << curves.len())
        {
            return Err(Error::Bounds);
        }
        let mut tape = Self {
            prefix: [[0; MAX_EDGES]; MAX_BANDS + 1],
            owner: [0; MAX_BANDS + 1],
            curves: [IndexedCurve::default(); MAX_EDGES],
            bands: 0,
            edges: curves.len(),
            compiled_mask: mask,
        };
        let mut cursor = [0usize; MAX_EDGES];
        let mut winners = [NO_EDGE; MAX_EDGES * 2];
        let mut rates = [0u64; MAX_EDGES];
        for (i, c) in curves.iter().enumerate() {
            if mask & (1 << i) == 0 {
                continue;
            }
            c.validate()?;
            winners[MAX_EDGES + i] = i as u8;
            rates[i] = c.segments[0].marginal_q32;
            let mut cap = 0;
            let mut integral = 0;
            let mut indexed = IndexedCurve {
                len: c.len as usize,
                ..IndexedCurve::default()
            };
            for (j, s) in c.segments[..c.len as usize].iter().enumerate() {
                meter.charge(1)?;
                cap += s.input_capacity;
                integral += u128::from(s.input_capacity) * u128::from(s.marginal_q32);
                indexed.capacity[j + 1] = cap;
                indexed.integral[j + 1] = integral;
                indexed.rates[j] = s.marginal_q32;
            }
            tape.curves[i] = indexed;
        }
        if mask.count_ones() <= 2 {
            // A tournament is slower for one/two streams. Keep just their
            // heads; use the tree only when its three-level update pays off.
            let mut left = mask.trailing_zeros() as u8;
            let rest = mask & (mask - 1);
            let mut right = if rest == 0 {
                NO_EDGE
            } else {
                rest.trailing_zeros() as u8
            };
            loop {
                meter.charge(1)?;
                let best = winner(left, right, &rates);
                if best == NO_EDGE {
                    break;
                }
                let i = best as usize;
                let k = tape.bands + 1;
                tape.prefix[k] = tape.prefix[k - 1];
                tape.prefix[k][i] += 1;
                tape.owner[k] = best;
                tape.bands = k;
                cursor[i] += 1;
                if cursor[i] < curves[i].len as usize {
                    rates[i] = curves[i].segments[cursor[i]].marginal_q32;
                } else if best == left {
                    left = NO_EDGE;
                } else {
                    right = NO_EDGE;
                }
            }
            return Ok(tape);
        }
        for node in (1..MAX_EDGES).rev() {
            meter.charge(1)?;
            winners[node] = winner(winners[node * 2], winners[node * 2 + 1], &rates);
        }
        while winners[1] != NO_EDGE {
            let i = winners[1] as usize;
            let k = tape.bands + 1;
            tape.prefix[k] = tape.prefix[k - 1];
            tape.prefix[k][i] += 1;
            tape.owner[k] = i as u8;
            tape.bands = k;
            cursor[i] += 1;
            let mut node = MAX_EDGES + i;
            if cursor[i] < curves[i].len as usize {
                rates[i] = curves[i].segments[cursor[i]].marginal_q32;
            } else {
                winners[node] = NO_EDGE;
            }
            // Only one frontier changed: repair three ancestors instead of
            // loading and comparing all eight Curve frontiers for every band.
            node /= 2;
            while node != 0 {
                meter.charge(1)?;
                winners[node] = winner(winners[node * 2], winners[node * 2 + 1], &rates);
                node /= 2;
            }
        }
        Ok(tape)
    }

    fn rank(&self, row: usize, mut mask: u8, meter: &mut WorkMeter) -> Result<u64> {
        let mut sum = 0;
        // Cardinality-constrained queries usually select <=4 of 8 edges. Walk
        // only those edges; do not branch across every absent venue per rank.
        while mask != 0 {
            meter.charge(1)?;
            let i = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            sum += self.curves[i].capacity[self.prefix[row][i] as usize];
        }
        Ok(sum)
    }

    pub fn allocate(&self, amount: u64, mask: u8, meter: &mut WorkMeter) -> Result<Allocation> {
        self.allocate_inner::<false>(amount, mask, &[], meter)
    }

    // Only search_tape calls this, after matches() binds capacities and rates.
    // Envelope errors remain caller-supplied: the tape does not cache policy.
    pub(super) fn allocate_lower(
        &self,
        amount: u64,
        mask: u8,
        curves: &[Curve],
        meter: &mut WorkMeter,
    ) -> Result<Allocation> {
        self.allocate_inner::<true>(amount, mask, curves, meter)
    }

    fn allocate_inner<const LOWER: bool>(
        &self,
        amount: u64,
        mask: u8,
        curves: &[Curve],
        meter: &mut WorkMeter,
    ) -> Result<Allocation> {
        if amount == 0 || amount > MAX_ATOMS {
            return Err(Error::InvalidAmount);
        }
        if mask == 0 || mask & !self.compiled_mask != 0 {
            return Err(Error::Bounds);
        }
        if mask.is_power_of_two() {
            // There is no allocation problem with one selected venue. Search
            // its <=16 bands directly, not the <=128-row global tape that may
            // contain seven unrelated streams. Keep the same integral/floors.
            meter.charge(1)?;
            let i = mask.trailing_zeros() as usize;
            let curve = &self.curves[i];
            if amount > curve.capacity[curve.len] {
                return Err(Error::Capacity);
            }
            let (mut lo, mut hi) = (1, curve.len);
            while lo < hi {
                meter.charge(1)?;
                let mid = lo + (hi - lo) / 2;
                if curve.capacity[mid] >= amount {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            let before = lo - 1;
            let integral = curve.integral[before]
                + u128::from(amount - curve.capacity[before]) * u128::from(curve.rates[before]);
            let output = as_u64(integral / SCALE)?;
            let mut plan = Allocation {
                mask,
                output,
                ..Allocation::default()
            };
            plan.inputs[i] = amount;
            if LOWER {
                plan.output = output.saturating_sub(curves[i].lower_error_atoms);
            }
            return Ok(plan);
        }
        if self.rank(self.bands, mask, meter)? < amount {
            return Err(Error::Capacity);
        }
        let mut lo = 1;
        let mut hi = self.bands;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.rank(mid, mask, meter)? >= amount {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        let mut plan = Allocation::default();
        let mut filled = 0;
        let mut selected = mask;
        while selected != 0 {
            meter.charge(1)?;
            let i = selected.trailing_zeros() as usize;
            selected &= selected - 1;
            plan.inputs[i] = self.curves[i].capacity[self.prefix[lo - 1][i] as usize];
            filled += plan.inputs[i];
        }
        let last = self.owner[lo] as usize;
        let partial = amount - filled;
        plan.inputs[last] += partial;
        let mut integral = 0;
        let mut lower = 0u64;
        selected = mask;
        while selected != 0 {
            let i = selected.trailing_zeros() as usize;
            selected &= selected - 1;
            if plan.inputs[i] > 0 {
                plan.mask |= 1 << i;
                let completed = self.prefix[lo - 1][i] as usize;
                let mut venue_integral = self.curves[i].integral[completed];
                if i == last {
                    venue_integral +=
                        u128::from(partial) * u128::from(self.curves[i].rates[completed]);
                }
                integral += venue_integral;
                if LOWER {
                    // Reuse the already indexed integral. The previous search
                    // discarded it and rescanned/revalidated the full Curve
                    // for every selected venue of every candidate subset.
                    lower = lower
                        .checked_add(
                            as_u64(venue_integral / SCALE)?
                                .saturating_sub(curves[i].lower_error_atoms),
                        )
                        .ok_or(Error::Arithmetic)?;
                }
            }
        }
        // Keep the relaxed-total overflow guard even for conservative output.
        plan.output = as_u64(integral / SCALE)?;
        if LOWER {
            plan.output = lower;
        }
        Ok(plan)
    }
}
