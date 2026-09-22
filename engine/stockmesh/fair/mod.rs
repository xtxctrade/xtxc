//! Allocation-free execution-policy bands. This is interval consensus under a
//! configured fault budget, not a calibrated prediction of fundamental value.
//! Source authentication, identities and correlation groups belong to the host.
use crate::{Error, Result, SCALE};
pub const MAX_GROUPS: usize = 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Interval {
    pub group: u16,
    /// Quote atoms per base atom, Q32. Both mints must be bound by the host.
    pub low: u64,
    pub high: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Band {
    pub low: u64,
    pub high: u64,
    pub groups: u8,
    pub quorum: u8,
}

/// One vote per independent configured group. Reject split consensus regions,
/// rather than pretending their intervening unsupported prices are fair.
pub fn consensus(xs: &[Interval], faults: usize, max_width_bps: u16) -> Result<Band> {
    let n = xs.len();
    if !(3..=MAX_GROUPS).contains(&n)
        || faults == 0
        || faults >= n
        || n <= 2 * faults
        || max_width_bps == 0
        || max_width_bps > 2000
    {
        return Err(Error::Bounds);
    }
    let mut points = [0u64; MAX_GROUPS * 2];
    for (i, x) in xs.iter().enumerate() {
        if x.low == 0 || x.high < x.low || xs[..i].iter().any(|y| y.group == x.group) {
            return Err(Error::InvalidCurve);
        }
        points[2 * i] = x.low;
        points[2 * i + 1] = x.high;
    }
    points[..2 * n].sort_unstable();
    let quorum = n - faults;
    let covered = |p| xs.iter().filter(|x| x.low <= p && p <= x.high).count() >= quorum;
    let mut low = None;
    let mut high = 0;
    let mut ended = false;
    for i in 0..2 * n {
        let p = points[i];
        if covered(p) {
            if ended {
                return Err(Error::InvalidCurve);
            }
            low.get_or_insert(p);
            high = p;
        } else if low.is_some() {
            ended = true;
        }
        // Midpoints reveal unsupported open intervals between supported endpoints.
        if i + 1 < 2 * n && points[i + 1] > p && low.is_some() {
            let right = points[i + 1];
            let through = xs.iter().filter(|x| x.low <= p && x.high >= right).count();
            if through < quorum {
                ended = true;
            }
        }
    }
    let low = low.ok_or(Error::VenueUnavailable)?;
    if u128::from(high - low) * 10_000 > u128::from(low) * u128::from(max_width_bps) {
        return Err(Error::InvalidCurve);
    }
    Ok(Band {
        low,
        high,
        groups: n as u8,
        quorum: quorum as u8,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    BuyBase,
    SellBase,
}
impl Band {
    pub fn midpoint(self) -> Result<u64> {
        if self.low == 0 || self.high < self.low {
            return Err(Error::Bounds);
        }
        Ok(self.low + (self.high - self.low) / 2)
    }
    /// Tighten the user's signed floor; never replace it with a weaker model limit.
    pub fn min_output(self, input: u64, side: Side, user_floor: u64) -> Result<u64> {
        if input == 0 || user_floor == 0 || self.low == 0 || self.high < self.low {
            return Err(Error::Bounds);
        }
        let model = match side {
            Side::BuyBase => (u128::from(input) * SCALE).div_ceil(u128::from(self.high)),
            Side::SellBase => (u128::from(input) * u128::from(self.low)).div_ceil(SCALE),
        };
        Ok(user_floor.max(u64::try_from(model).map_err(|_| Error::Arithmetic)?))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crossing {
    pub base: u64,
    pub quote: u64,
    pub buyer_quote_residual: u64,
    pub seller_base_residual: u64,
    pub price_q32: u64,
}
/// Fixed exogenous midpoint, never an estimate derived from submitted limits or
/// our own flow. Any quantity-lot rounding must satisfy both owners' limits.
pub fn cross(
    band: Band,
    buy_quote: u64,
    buy_min_base: u64,
    sell_base: u64,
    sell_min_quote: u64,
) -> Result<Crossing> {
    if [buy_quote, buy_min_base, sell_base, sell_min_quote].contains(&0)
        || band.low == 0
        || band.high < band.low
    {
        return Err(Error::Bounds);
    }
    let p = band.midpoint()?;
    let affordable = u64::try_from(u128::from(buy_quote) * SCALE / u128::from(p))
        .map_err(|_| Error::Arithmetic)?;
    let base = sell_base.min(affordable);
    let quote = u64::try_from((u128::from(base) * u128::from(p)).div_ceil(SCALE))
        .map_err(|_| Error::Arithmetic)?;
    if base == 0
        || quote == 0
        || quote > buy_quote
        || u128::from(quote) * SCALE > u128::from(base) * u128::from(band.high)
        || u128::from(quote) * SCALE < u128::from(base) * u128::from(band.low)
        || u128::from(quote) * u128::from(buy_min_base) > u128::from(base) * u128::from(buy_quote)
        || u128::from(quote) * u128::from(sell_base) < u128::from(base) * u128::from(sell_min_quote)
    {
        return Err(Error::MinOut);
    }
    Ok(Crossing {
        base,
        quote,
        buyer_quote_residual: buy_quote - quote,
        seller_base_residual: sell_base - base,
        price_q32: p,
    })
}
