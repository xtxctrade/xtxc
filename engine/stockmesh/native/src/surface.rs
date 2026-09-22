//! Bounded state-bound sampled surface for opaque programs. Interpolation is a
//! candidate-ranking estimate, NEVER an exact quote or an output guarantee.
use crate::{Error, Result};
pub const MAX_KNOTS: usize = 129;
#[derive(Clone, Copy, Debug, Default)]
pub struct Knot {
    pub input: u64,
    pub output: u64,
    pub cu: u64,
}
#[derive(Clone)]
pub struct Surface {
    knots: [Knot; MAX_KNOTS],
    len: usize,
    pub generation: u64,
}
impl Surface {
    pub fn new(generation: u64) -> Self {
        Self {
            knots: [Knot::default(); MAX_KNOTS],
            len: 1,
            generation,
        }
    }
    pub fn push(&mut self, k: Knot) -> Result<()> {
        if self.len == MAX_KNOTS
            || k.input <= self.knots[self.len - 1].input
            || k.output < self.knots[self.len - 1].output
        {
            return Err(Error::Unsupported);
        }
        self.knots[self.len] = k;
        self.len += 1;
        Ok(())
    }
    /// Select where adjacent secants change most, weighted by interval width.
    /// This is a refinement priority, not a certified error bound for an opaque program.
    pub fn refinement_input(&self) -> Option<u64> {
        if self.len == MAX_KNOTS || self.len < 3 {
            return None;
        }
        let slope = |a: Knot, b: Knot| {
            ((u128::from(b.output - a.output)) << 32) / u128::from(b.input - a.input)
        };
        let mut best = (0u128, 0u64);
        for i in 1..self.len {
            let a = self.knots[i - 1];
            let b = self.knots[i];
            let width = b.input - a.input;
            if width < 2 {
                continue;
            }
            let here = slope(a, b);
            let left = if i > 1 {
                here.abs_diff(slope(self.knots[i - 2], a))
            } else {
                0
            };
            let right = if i + 1 < self.len {
                here.abs_diff(slope(b, self.knots[i + 1]))
            } else {
                0
            };
            let priority = left.max(right).saturating_mul(u128::from(width));
            if priority > best.0 {
                best = (priority, a.input + width / 2);
            }
        }
        (best.0 > 0).then_some(best.1)
    }
    pub fn insert_refinement(&mut self, k: Knot) -> Result<()> {
        let i = self.knots[..self.len].partition_point(|x| x.input < k.input);
        if self.len == MAX_KNOTS
            || i == 0
            || i == self.len
            || self.knots[i].input == k.input
            || k.output < self.knots[i - 1].output
            || k.output > self.knots[i].output
        {
            return Err(Error::Unsupported);
        }
        self.knots.copy_within(i..self.len, i + 1);
        self.knots[i] = k;
        self.len += 1;
        Ok(())
    }
    pub fn estimate(&self, input: u64, generation: u64) -> Result<(u64, u64)> {
        if generation != self.generation {
            return Err(Error::Stale);
        }
        let i = self.knots[..self.len].partition_point(|k| k.input < input);
        if i == self.len {
            return Err(Error::Capacity);
        }
        let b = self.knots[i];
        if b.input == input {
            return Ok((b.output, b.cu));
        }
        if i == 0 {
            return Err(Error::Capacity);
        }
        let a = self.knots[i - 1];
        let o = a.output
            + ((u128::from(b.output - a.output) * u128::from(input - a.input))
                / u128::from(b.input - a.input)) as u64;
        // Resource cost is a proposal as well. Using the maximum endpoint cost
        // creates artificial step walls that discard feasible large allocations.
        // Interpolation may underpredict a tick-crossing cost; final SBF must reject it.
        let delta = (u128::from(a.cu.abs_diff(b.cu)) * u128::from(input - a.input)
            / u128::from(b.input - a.input)) as u64;
        let cu = if b.cu >= a.cu {
            a.cu.checked_add(delta)
        } else {
            a.cu.checked_sub(delta)
        }
        .ok_or(Error::Arithmetic)?;
        Ok((o, cu))
    }
}
