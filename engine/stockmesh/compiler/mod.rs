//! Marginal Liquidity IR: concave, non-increasing marginal output per input atom.
//! Venue identity stays outside the optimizer. Only independent, same-pair,
//! synchronously settleable edges belong in one separable optimization problem.
use crate::{
    as_u64, ceil_div, Error, Key, Result, WorkMeter, MAX_ATOMS, MAX_RATE, MAX_SEGMENTS, SCALE,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Segment {
    pub input_capacity: u64,
    pub marginal_q32: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Curve {
    pub segments: [Segment; MAX_SEGMENTS],
    pub len: u8,
    pub capacity: u64,
    /// For every x: exact(x) <= floor(IR(x)) + upper_error_atoms.
    pub upper_error_atoms: u64,
    /// For every x: max(0, floor(IR(x)) - lower_error_atoms) <= exact(x).
    pub lower_error_atoms: u64,
}

impl Default for Curve {
    fn default() -> Self {
        Self {
            segments: [Segment::default(); MAX_SEGMENTS],
            len: 0,
            capacity: 0,
            upper_error_atoms: 0,
            lower_error_atoms: 0,
        }
    }
}

impl Curve {
    pub fn validate(&self) -> Result<()> {
        if self.len == 0 || self.len as usize > MAX_SEGMENTS {
            return Err(Error::InvalidCurve);
        }
        let mut capacity = 0u64;
        let mut prev = MAX_RATE;
        for s in &self.segments[..self.len as usize] {
            if s.input_capacity == 0 || s.marginal_q32 > prev {
                return Err(Error::InvalidCurve);
            }
            prev = s.marginal_q32;
            capacity = capacity
                .checked_add(s.input_capacity)
                .ok_or(Error::Arithmetic)?;
        }
        if capacity != self.capacity || capacity > MAX_ATOMS {
            return Err(Error::InvalidCurve);
        }
        Ok(())
    }

    pub fn quote(&self, mut input: u64) -> Result<u64> {
        self.validate()?;
        if input > self.capacity {
            return Err(Error::Capacity);
        }
        let mut output = 0u128;
        for s in &self.segments[..self.len as usize] {
            let take = input.min(s.input_capacity);
            output += u128::from(take) * u128::from(s.marginal_q32);
            input -= take;
            if input == 0 {
                break;
            }
        }
        as_u64(output / SCALE)
    }

    pub fn lower_quote(&self, input: u64) -> Result<u64> {
        Ok(self.quote(input)?.saturating_sub(self.lower_error_atoms))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// Inline bounded storage is intentional: this core has no allocator and callers
// own its workspace. Boxing a book would add allocation and erase Copy semantics.
#[allow(clippy::large_enum_variant)]
pub enum Model {
    /// Normalized net executable input bands. A venue adapter must separately
    /// prove lot sizes, fee semantics, account decoding and CPI conformance.
    Book {
        levels: [Segment; MAX_SEGMENTS],
        len: u8,
    },
    /// Generic constant-product model with ceil(input * fee_ppm / 1e6).
    /// This name deliberately does not claim Raydium deployment compatibility.
    ConstantProduct {
        reserve_in: u64,
        reserve_out: u64,
        fee_ppm: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub market: Key,
    pub program: Key,
    pub input_mint: Key,
    pub output_mint: Key,
    /// Same underlying pool must have the same identity, even across wrappers.
    pub liquidity_id: Key,
    /// Bitset over the envelope's canonical *venue state* accounts. User token
    /// accounts are shared intentionally and are not included in this bitset.
    pub writable_resources: u64,
    pub slot: u64,
    pub generation: u64,
    pub expires_at_slot: u64,
    pub capacity: u64,
    pub atomic: bool,
    pub enabled: bool,
    pub model: Model,
}

impl Snapshot {
    pub fn exact_quote(&self, input: u64) -> Result<u64> {
        if input > self.capacity || input > MAX_ATOMS {
            return Err(Error::Capacity);
        }
        match self.model {
            Model::Book { levels, len } => {
                let capacity = levels.iter().take(len as usize).try_fold(0u64, |a, s| {
                    a.checked_add(s.input_capacity).ok_or(Error::Arithmetic)
                })?;
                Curve {
                    segments: levels,
                    len,
                    capacity,
                    ..Curve::default()
                }
                .quote(input)
            }
            Model::ConstantProduct {
                reserve_in,
                reserve_out,
                fee_ppm,
            } => {
                validate_cp(reserve_in, reserve_out, fee_ppm)?;
                let net = u128::from(input) * u128::from(1_000_000 - fee_ppm) / 1_000_000;
                as_u64(u128::from(reserve_out) * net / (u128::from(reserve_in) + net))
            }
        }
    }
}

fn validate_cp(r: u64, y: u64, fee: u32) -> Result<()> {
    if r == 0 || y == 0 || r > MAX_ATOMS || y > MAX_ATOMS || fee >= 1_000_000 {
        return Err(Error::InvalidCurve);
    }
    Ok(())
}

// Continuous CP envelope, floor in Q32, with division before scaling to avoid
// the otherwise overflowing product reserve_out * input * fee * SCALE.
fn cp_fp(r: u64, y: u64, f: u32, x: u64) -> Result<u128> {
    let n = u128::from(y) * u128::from(x) * u128::from(f);
    let d = u128::from(r) * 1_000_000 + u128::from(x) * u128::from(f);
    Ok((n / d) * SCALE + (n % d) * SCALE / d)
}

fn derivative_upper(r: u64, y: u64, f: u32, x: u64) -> Result<u128> {
    let d = u128::from(r) * 1_000_000 + u128::from(x) * u128::from(f);
    let first = ceil_div(u128::from(y) * u128::from(f) * SCALE, d)?;
    ceil_div(first * u128::from(r) * 1_000_000, d)
}

/// Compile at most 16 adaptive chords. Splitting the interval with the largest
/// tangent/chord bound is deterministic and independent of the order notional's
/// number of atomic units. There is no dollar-by-dollar optimization loop.
pub fn compile(s: &Snapshot, horizon: u64, meter: &mut WorkMeter) -> Result<Curve> {
    if !s.atomic {
        return Err(Error::Unsupported);
    }
    if !s.enabled {
        return Err(Error::VenueUnavailable);
    }
    if s.capacity == 0 || s.capacity > MAX_ATOMS || horizon == 0 || horizon > MAX_ATOMS {
        return Err(Error::InvalidAmount);
    }
    let capacity = s.capacity.min(horizon);
    match s.model {
        Model::Book { levels, len } => {
            let full_capacity = levels.iter().take(len as usize).try_fold(0u64, |a, l| {
                a.checked_add(l.input_capacity).ok_or(Error::Arithmetic)
            })?;
            let full = Curve {
                segments: levels,
                len,
                capacity: full_capacity,
                ..Curve::default()
            };
            full.validate()?;
            if s.capacity > full_capacity {
                return Err(Error::Capacity);
            }
            let mut curve = Curve {
                capacity,
                ..Curve::default()
            };
            let mut left = capacity;
            for level in &levels[..len as usize] {
                meter.charge(1)?;
                if left == 0 {
                    break;
                }
                let take = left.min(level.input_capacity);
                curve.segments[curve.len as usize] = Segment {
                    input_capacity: take,
                    ..*level
                };
                curve.len += 1;
                left -= take;
            }
            Ok(curve)
        }
        Model::ConstantProduct {
            reserve_in: r,
            reserve_out: y,
            fee_ppm,
        } => {
            validate_cp(r, y, fee_ppm)?;
            let f = 1_000_000 - fee_ppm;
            let derivative_zero = derivative_upper(r, y, f, 0)?;
            if derivative_zero > u128::from(MAX_RATE) {
                return Err(Error::InvalidCurve);
            }
            let mut boundaries = [0u64; MAX_SEGMENTS + 1];
            boundaries[1] = capacity;
            // A split changes two adjacent intervals, not the other fourteen.
            // Keep endpoint values/tangents and interval priorities in bounded
            // caller-local arrays. Re-running u128 division for unchanged
            // intervals dominated this compiler. Index order remains the old
            // left-to-right order, including equal-priority tie breaking.
            let mut values = [0u128; MAX_SEGMENTS + 1];
            let mut tangents = [0u128; MAX_SEGMENTS + 1];
            let mut gaps = [0u128; MAX_SEGMENTS];
            values[1] = cp_fp(r, y, f, capacity)?;
            tangents[0] = derivative_zero;
            gaps[0] = derivative_zero.saturating_sub(values[1] / u128::from(capacity))
                * u128::from(capacity);
            let mut n = 1usize;
            while n < MAX_SEGMENTS {
                let mut best = None;
                let mut worst = 0u128;
                for i in 0..n {
                    meter.charge(1)?;
                    let a = boundaries[i];
                    let b = boundaries[i + 1];
                    if b - a <= 1 {
                        continue;
                    }
                    let gap = gaps[i];
                    if best.is_none() || gap > worst {
                        best = Some(i);
                        worst = gap;
                    }
                }
                let Some(i) = best else {
                    break;
                };
                for j in ((i + 2)..=(n + 1)).rev() {
                    boundaries[j] = boundaries[j - 1];
                    values[j] = values[j - 1];
                    tangents[j] = tangents[j - 1];
                }
                for j in ((i + 2)..=n).rev() {
                    gaps[j] = gaps[j - 1];
                }
                boundaries[i + 1] = boundaries[i] + (boundaries[i + 2] - boundaries[i]) / 2;
                values[i + 1] = cp_fp(r, y, f, boundaries[i + 1])?;
                tangents[i + 1] = derivative_upper(r, y, f, boundaries[i + 1])?;
                for j in i..=i + 1 {
                    let width = u128::from(boundaries[j + 1] - boundaries[j]);
                    let slope = (values[j + 1] - values[j]) / width;
                    gaps[j] = tangents[j].saturating_sub(slope) * width;
                }
                n += 1;
            }
            let mut curve = Curve {
                len: n as u8,
                capacity,
                ..Curve::default()
            };
            let mut integral = 0u128;
            let mut error_fp = 0u128;
            let mut previous = MAX_RATE;
            for i in 0..n {
                meter.charge(1)?;
                let a = boundaries[i];
                let b = boundaries[i + 1];
                let width = b - a;
                let fa = values[i];
                let fb = values[i + 1];
                let slope = as_u64((fb - fa) / u128::from(width))?.min(previous);
                previous = slope;
                curve.segments[i] = Segment {
                    input_capacity: width,
                    marginal_q32: slope,
                };
                // The true continuous function is below its left tangent.
                // +1 accounts for the Q32 endpoint's downward rounding.
                let tangent_gap = fa + 1 - integral
                    + tangents[i].saturating_sub(u128::from(slope)) * u128::from(width);
                error_fp = error_fp.max(tangent_gap);
                integral += u128::from(slope) * u128::from(width);
            }
            curve.upper_error_atoms = as_u64(ceil_div(error_fp, SCALE)?)?;
            // Floor of fee-adjusted input loses <1 effective input atom, and
            // final output flooring loses <1 output atom.
            curve.lower_error_atoms = as_u64(ceil_div(u128::from(y), u128::from(r))? + 1)?;
            curve.validate()?;
            Ok(curve)
        }
    }
}
