//! Independent one-range DAMMv2 quote from public pool ABI and integer reserve
//! equations. Liquidity is scaled by 2^64, unlike Raydium's unscaled liquidity.
//! Time schedulers and stored volatility fees are supported. Rate limiters,
//! market-cap schedulers and compounding mode remain explicit unsupported cases.
use crate::{bytes, u128_at, u64_at, Error, Result, TransferFee};
use ethnum::U256;
const Q: u128 = 1u128 << 64;
const DEN: u128 = 1_000_000_000;
fn fee_amount(q: u64, rate: u64) -> Result<u64> {
    q.checked_sub((u128::from(q) * u128::from(rate)).div_ceil(DEN) as u64)
        .ok_or(Error::Arithmetic)
}
fn narrowed(x: U256) -> Result<u128> {
    u128::try_from(x).map_err(|_| Error::Arithmetic)
}
fn product(a: u128, b: u128) -> U256 {
    U256::from(a) * U256::from(b)
}
fn div_up(n: U256, d: U256) -> Result<U256> {
    if d == U256::ZERO {
        return Err(Error::Arithmetic);
    }
    let q = n / d;
    if n % d == U256::ZERO {
        Ok(q)
    } else {
        q.checked_add(U256::ONE).ok_or(Error::Arithmetic)
    }
}
fn scheduled_fee(p: &[u8], point: u64, activation: u64) -> Result<u64> {
    let initial = u64_at(p, 8)?;
    let mode = p[16];
    if mode > 1 {
        return Err(Error::Unsupported);
    }
    let frequency = u64_at(p, 24)?;
    if frequency == 0 {
        return Ok(initial);
    }
    let periods = ((point.checked_sub(activation).ok_or(Error::Capacity)?) / frequency)
        .min(u64::from(u16::from_le_bytes(bytes(p, 22)?)));
    let reduction = u64_at(p, 32)?;
    if mode == 0 {
        return initial
            .checked_sub(periods.checked_mul(reduction).ok_or(Error::Arithmetic)?)
            .ok_or(Error::Arithmetic);
    }
    let mut base = Q
        .checked_sub((u128::from(reduction) << 64) / 10_000)
        .ok_or(Error::Arithmetic)?;
    let mut factor = Q;
    let mut n = periods;
    while n > 0 {
        if n & 1 != 0 {
            factor = narrowed(product(factor, base) >> 64)?;
        }
        n >>= 1;
        if n > 0 {
            base = narrowed(product(base, base) >> 64)?;
        }
    }
    if factor == 0 {
        return Err(Error::Arithmetic);
    }
    u64::try_from((factor * u128::from(initial)) >> 64).map_err(|_| Error::Arithmetic)
}
#[derive(Clone, Debug)]
pub struct DammCurve {
    sqrt: u128,
    min: u128,
    max: u128,
    liquidity: u128,
    fee: u64,
    mode: u8,
    fees: [TransferFee; 2],
}
impl DammCurve {
    pub fn decode(pool: &[u8], fees: [TransferFee; 2], slot: u64, time: u64) -> Result<Self> {
        if pool.len() != 1112 || bytes::<8>(pool, 0)? != [241, 154, 109, 4, 17, 177, 109, 188] {
            return Err(Error::Layout);
        }
        if pool[480] > 1
            || pool[481] != 0
            || pool[484] > 1
            || pool[485] > 1
            || pool[486] > 1
            || pool[56] > 1
        {
            return Err(Error::Unsupported);
        }
        let activation = u64_at(pool, 472)?;
        let point = if pool[480] == 0 { slot } else { time };
        if point < activation {
            return Err(Error::Capacity);
        }
        let mut fee = u128::from(scheduled_fee(pool, point, activation)?);
        // The published program updates the reference before swapping but updates
        // its accumulator after swapping. Current fee uses the stored accumulator.
        if pool[56] == 1 {
            if time < u64_at(pool, 80)? {
                return Err(Error::Stale);
            }
            let volatility = u128_at(pool, 120)?;
            if volatility > u128::from(u32::from_le_bytes(bytes(pool, 64)?)) {
                return Err(Error::Layout);
            }
            let x = volatility
                .checked_mul(u128::from(u16::from_le_bytes(bytes(pool, 72)?)))
                .ok_or(Error::Arithmetic)?;
            let variable = x
                .checked_mul(x)
                .and_then(|x| {
                    x.checked_mul(u128::from(u32::from_le_bytes(bytes::<4>(pool, 68).ok()?)))
                })
                .ok_or(Error::Arithmetic)?
                .div_ceil(100_000_000_000);
            fee = fee.checked_add(variable).ok_or(Error::Arithmetic)?;
        }
        let fee = fee.min(if pool[486] == 0 {
            500_000_000
        } else {
            990_000_000
        }) as u64;
        let min = u128_at(pool, 424)?;
        let max = u128_at(pool, 440)?;
        let sqrt = u128_at(pool, 456)?;
        let liquidity = u128_at(pool, 360)?;
        if min == 0 || min >= max || sqrt < min || sqrt > max || liquidity == 0 {
            return Err(Error::Layout);
        }
        Ok(Self {
            sqrt,
            min,
            max,
            liquidity,
            fee,
            mode: pool[484],
            fees,
        })
    }
    pub fn quote(&self, input: u64, a_to_b: bool) -> Result<u64> {
        if input == 0 || input > stocklana_adapters::MAX_INPUT {
            return Err(Error::Capacity);
        }
        let mut net = self.fees[0].net(input)?;
        let fee_input = self.mode == 1 && !a_to_b;
        if fee_input {
            net = fee_amount(net, self.fee)?;
        }
        let l = self.liquidity;
        let p = self.sqrt;
        let next = if a_to_b {
            narrowed(div_up(
                product(l, p),
                U256::from(l)
                    .checked_add(product(u128::from(net), p))
                    .ok_or(Error::Arithmetic)?,
            )?)?
        } else {
            p.checked_add(narrowed((U256::from(net) << 128) / U256::from(l))?)
                .ok_or(Error::Arithmetic)?
        };
        if next < self.min || next > self.max {
            return Err(Error::Capacity);
        }
        let out = if a_to_b {
            product(l, p.checked_sub(next).ok_or(Error::Arithmetic)?) >> 128
        } else {
            product(l, next.checked_sub(p).ok_or(Error::Arithmetic)?) / product(p, next)
        };
        let mut out = u64::try_from(out).map_err(|_| Error::Arithmetic)?;
        if !fee_input {
            out = fee_amount(out, self.fee)?;
        }
        out = self.fees[1].net(out)?;
        if out == 0 {
            Err(Error::Capacity)
        } else {
            Ok(out)
        }
    }
}
