//! Independent checked DLMM pricing from published account ABI and fee equations.
//! Decode once; per-bin arithmetic uses Q64/U256. Caller binds account ownership.
use crate::{bytes, u128_at, u64_at, Error, Result, TransferFee};
use ethnum::U256;
const DEN: u128 = 1_000_000_000;
const MAX_BINS: usize = 420;
fn u16_at(d: &[u8], o: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(bytes(d, o)?))
}
fn u32_at(d: &[u8], o: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(bytes(d, o)?))
}
fn i32_at(d: &[u8], o: usize) -> Result<i32> {
    Ok(i32::from_le_bytes(bytes(d, o)?))
}
fn narrow(v: U256) -> Result<u64> {
    u64::try_from(v).map_err(|_| Error::Arithmetic)
}
fn ratio(n: U256, d: U256, ceil: bool) -> Result<U256> {
    if d == U256::ZERO {
        return Err(Error::Arithmetic);
    }
    let v = n / d;
    if ceil && n % d != U256::ZERO {
        v.checked_add(U256::ONE).ok_or(Error::Arithmetic)
    } else {
        Ok(v)
    }
}
fn convert(q: u64, price: u128, x_to_y: bool, ceil: bool) -> Result<u64> {
    if price == 0 {
        return Err(Error::Layout);
    }
    if x_to_y {
        narrow(ratio(
            U256::from(q) * U256::from(price),
            U256::ONE << 64,
            ceil,
        )?)
    } else {
        narrow(ratio(U256::from(q) << 64, U256::from(price), ceil)?)
    }
}
#[derive(Clone, Debug)]
struct Bin {
    id: i32,
    price: u128,
    mm: [u64; 2],
    order: [u64; 2],
    ask: bool,
}
#[derive(Clone, Debug)]
pub struct DlmmCurve {
    bins: Vec<Bin>,
    active: i32,
    reference: i32,
    volatility_reference: u64,
    max_volatility: u64,
    variable: u64,
    step: u64,
    base: u64,
    fee_mode: u8,
    fees: [TransferFee; 2],
}
impl DlmmCurve {
    pub fn decode(
        pool_key: [u8; 32],
        pool: &[u8],
        arrays: &[&[u8]],
        fees: [TransferFee; 2],
        slot: u64,
        time: u64,
    ) -> Result<Self> {
        if pool.len() != 904
            || bytes::<8>(pool, 0)? != [33, 11, 49, 98, 181, 101, 177, 13]
            || arrays.is_empty()
            || arrays.len() > 6
        {
            return Err(Error::Layout);
        }
        if pool[82] != 0 || pool[35] > 2 || pool[36] > 1 || pool[75] > 3 || pool[86] > 1 {
            return Err(Error::Unsupported);
        }
        if pool[75] != 0 && (if pool[86] == 0 { slot } else { time }) < u64_at(pool, 816)? {
            return Err(Error::Capacity);
        }
        let active = i32_at(pool, 76)?;
        if active < i32_at(pool, 24)? || active > i32_at(pool, 28)? {
            return Err(Error::Layout);
        }
        let step = u64::from(u16_at(pool, 80)?);
        if step == 0 {
            return Err(Error::Layout);
        }
        let power = 10u64
            .checked_pow(u32::from(pool[34]))
            .ok_or(Error::Arithmetic)?;
        let base = u64::from(u16_at(pool, 8)?)
            .checked_mul(step)
            .and_then(|v| v.checked_mul(10))
            .and_then(|v| v.checked_mul(power))
            .ok_or(Error::Arithmetic)?;
        // FunctionType::Undetermined pools support limit orders only while both
        // reward mints are unset. This matches the deployed program's backward
        // compatibility rule; treating only FunctionType::LimitOrder as enabled
        // silently drops executable order liquidity from older pools.
        let supports_limit_orders = match pool[35] {
            0 => bytes::<32>(pool, 264)? == [0; 32] && bytes::<32>(pool, 408)? == [0; 32],
            1 => false,
            2 => true,
            _ => return Err(Error::Unsupported),
        };
        let mut reference = i32_at(pool, 48)?;
        let mut volatility_reference = u64::from(u32_at(pool, 44)?);
        let last = u64_at(pool, 56)?;
        if time < last {
            return Err(Error::Stale);
        }
        let elapsed = time - last;
        if elapsed >= u64::from(u16_at(pool, 10)?) {
            reference = active;
            volatility_reference = if elapsed < u64::from(u16_at(pool, 12)?) {
                u64::from(u32_at(pool, 40)?) * u64::from(u16_at(pool, 14)?) / 10_000
            } else {
                0
            };
        }
        let mut bins = Vec::with_capacity(arrays.len() * 70);
        let mut starts = Vec::new();
        for a in arrays {
            if a.len() != 10136
                || bytes::<8>(a, 0)? != [92, 142, 92, 220, 5, 148, 70, 181]
                || bytes::<32>(a, 24)? != pool_key
                || a[16] > 2
            {
                return Err(Error::Layout);
            }
            let index = i64::from_le_bytes(bytes(a, 8)?);
            let start = i32::try_from(index.checked_mul(70).ok_or(Error::Arithmetic)?)
                .map_err(|_| Error::Layout)?;
            starts.push(start);
            for i in 0..70 {
                let o = 56 + i * 144;
                let mm = [u64_at(a, o)?, u64_at(a, o + 8)?];
                let order = if supports_limit_orders {
                    [u64_at(a, o + 128)?, u64_at(a, o + 112)?]
                } else {
                    [0, 0]
                };
                if mm == [0, 0] && order == [0, 0] {
                    continue;
                }
                let price = u128_at(a, o + 16)?;
                if price == 0 || a[o + 140] > 1 {
                    return Err(Error::Unsupported);
                }
                bins.push(Bin {
                    id: start.checked_add(i as i32).ok_or(Error::Arithmetic)?,
                    price,
                    mm,
                    order,
                    ask: a[o + 140] != 0,
                });
            }
        }
        starts.sort_unstable();
        bins.sort_unstable_by_key(|b| b.id);
        if bins.len() > MAX_BINS
            || starts
                .windows(2)
                .any(|p| p[1].checked_sub(p[0]) != Some(70))
            || active < starts[0]
            || i64::from(active) >= i64::from(*starts.last().unwrap()) + 70
        {
            return Err(Error::Unsupported);
        }
        Ok(Self {
            bins,
            active,
            reference,
            volatility_reference,
            max_volatility: u64::from(u32_at(pool, 20)?),
            variable: u64::from(u32_at(pool, 16)?),
            step,
            base,
            fee_mode: pool[36],
            fees,
        })
    }
    fn fee_rate(&self, id: i32) -> Result<u128> {
        let v = (self.volatility_reference + u64::from(id.abs_diff(self.reference)) * 10_000)
            .min(self.max_volatility);
        let x = u128::from(v) * u128::from(self.step);
        let variable = x
            .checked_mul(x)
            .and_then(|x| x.checked_mul(u128::from(self.variable)))
            .ok_or(Error::Arithmetic)?
            .div_ceil(100_000_000_000);
        Ok((u128::from(self.base) + variable).min(100_000_000))
    }
    pub fn quote(&self, input: u64, x_to_y: bool) -> Result<u64> {
        if input == 0 || input > stocklana_adapters::MAX_INPUT {
            return Err(Error::Capacity);
        }
        let mut remaining = self.fees[0].net(input)?;
        let mut output = 0u64;
        let fee_input = self.fee_mode == 0 || !x_to_y;
        let cursor = self.bins.partition_point(|b| b.id < self.active);
        let mut index = if x_to_y {
            self.bins.partition_point(|b| b.id <= self.active) as isize - 1
        } else {
            cursor as isize
        };
        while remaining > 0 && index >= 0 && (index as usize) < self.bins.len() {
            let b = &self.bins[index as usize];
            let rate = self.fee_rate(b.id)?;
            let excluded = if fee_input {
                remaining
                    .checked_sub((u128::from(remaining) * rate).div_ceil(DEN) as u64)
                    .ok_or(Error::Arithmetic)?
            } else {
                remaining
            };
            let mut net_left = excluded;
            let mut out = 0u64;
            let matching_order = x_to_y != b.ask;
            let liquidity = [
                b.mm[usize::from(x_to_y)],
                if matching_order { b.order[0] } else { 0 },
                if matching_order { b.order[1] } else { 0 },
            ];
            for amount in liquidity {
                if amount == 0 || net_left == 0 {
                    continue;
                }
                let need = convert(amount, b.price, !x_to_y, true)?;
                let (spent, fill) = if net_left >= need {
                    (need, amount)
                } else {
                    (net_left, convert(net_left, b.price, x_to_y, false)?)
                };
                net_left -= spent;
                out = out.checked_add(fill).ok_or(Error::Arithmetic)?;
            }
            let used = excluded - net_left;
            let consumed = if net_left == 0 {
                remaining
            } else if fee_input {
                u64::try_from((u128::from(used) * DEN).div_ceil(DEN - rate))
                    .map_err(|_| Error::Arithmetic)?
            } else {
                used
            };
            remaining = remaining.checked_sub(consumed).ok_or(Error::Arithmetic)?;
            if !fee_input {
                out = out
                    .checked_sub((u128::from(out) * rate).div_ceil(DEN) as u64)
                    .ok_or(Error::Arithmetic)?;
            }
            output = output.checked_add(out).ok_or(Error::Arithmetic)?;
            index += if x_to_y { -1 } else { 1 };
        }
        if remaining != 0 || output == 0 {
            return Err(Error::Capacity);
        }
        self.fees[1].net(output)
    }
}
