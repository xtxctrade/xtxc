//! Independent native Whirlpool interval compiler from public ABI/fee equations.
//! Q64 tick constants below derive from the pre-2025 Apache reference (NOTICE).
//! No current Orca SDK dependency. Missing arrays and unknown state fail closed.
use crate::clmm::{change, delta0, delta1, div, mul, small};
use crate::{bytes, u128_at, u64_at, Error, Result, TransferFee};
use ethnum::U256;
const POS: [u128; 19] = [
    79232123823359799118286999567,
    79236085330515764027303304731,
    79244008939048815603706035061,
    79259858533276714757314932305,
    79291567232598584799939703904,
    79355022692464371645785046466,
    79482085999252804386437311141,
    79736823300114093921829183326,
    80248749790819932309965073892,
    81282483887344747381513967011,
    83390072131320151908154831281,
    87770609709833776024991924138,
    97234110755111693312479820773,
    119332217159966728226237229890,
    179736315981702064433883588727,
    407748233172238350107850275304,
    2098478828474011932436660412517,
    55581415166113811149459800483533,
    38992368544603139932233054999993551,
];
const NEG: [u128; 19] = [
    18445821805675392311,
    18444899583751176498,
    18443055278223354162,
    18439367220385604838,
    18431993317065449817,
    18417254355718160513,
    18387811781193591352,
    18329067761203520168,
    18212142134806087854,
    17980523815641551639,
    17526086738831147013,
    16651378430235024244,
    15030750278693429944,
    12247334978882834399,
    8131365268884726200,
    3584323654723342297,
    696457651847595233,
    26294789957452057,
    37481735321082,
];
pub fn sqrt_tick(tick: i32) -> Result<u128> {
    if !(-443636..=443636).contains(&tick) {
        return Err(Error::Layout);
    }
    let n = tick.unsigned_abs();
    let shift = if tick >= 0 { 96 } else { 64 };
    let mut p = U256::ONE << shift;
    for (i, f) in (if tick >= 0 { &POS } else { &NEG }).iter().enumerate() {
        if n & (1 << i) != 0 {
            p = mul(p, U256::from(*f))? >> shift;
        }
    }
    small(if tick >= 0 { p >> 32 } else { p })
}
fn u16_at(d: &[u8], o: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(bytes(d, o)?))
}
fn u32_at(d: &[u8], o: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(bytes(d, o)?))
}
fn i32_at(d: &[u8], o: usize) -> Result<i32> {
    Ok(i32::from_le_bytes(bytes(d, o)?))
}
fn narrow(n: U256) -> Result<u64> {
    u64::try_from(n).map_err(|_| Error::Arithmetic)
}
#[derive(Clone, Debug)]
struct Tick {
    index: i32,
    net: i128,
}
#[derive(Clone, Debug)]
struct Adaptive {
    group: i32,
    reference: i32,
    volatility: u64,
    maximum: u64,
    control: u64,
}
impl Adaptive {
    fn decode(key: [u8; 32], d: &[u8], now: u64, current: i32) -> Result<Self> {
        if d.len() != 254
            || bytes::<8>(d, 0)? != [139, 194, 131, 179, 140, 179, 229, 244]
            || bytes::<32>(d, 8)? != key
            || u64_at(d, 40)? > now
            || d[66..82]
                .iter()
                .chain(d[110..126].iter())
                .chain(d[126..].iter())
                .any(|b| *b != 0)
        {
            return Err(Error::Unsupported);
        }
        let filter = u64::from(u16_at(d, 48)?);
        let decay = u64::from(u16_at(d, 50)?);
        let reduction = u64::from(u16_at(d, 52)?);
        let control = u64::from(u32_at(d, 54)?);
        let maximum = u64::from(u32_at(d, 58)?);
        let group = i32::from(u16_at(d, 62)?);
        let last = u64_at(d, 82)?;
        let major = u64_at(d, 90)?;
        let mut volatility = u64::from(u32_at(d, 98)?);
        let mut reference = i32_at(d, 102)?;
        let accumulator = u64::from(u32_at(d, 106)?);
        if group == 0
            || decay < filter
            || reduction > 10000
            || last > now
            || major > now
            || volatility > maximum
            || accumulator > maximum
        {
            return Err(Error::Layout);
        }
        if now - last >= 3600 || now - last.max(major) >= decay {
            volatility = 0;
            reference = current.div_euclid(group);
        } else if now - last.max(major) >= filter {
            volatility = accumulator * reduction / 10000;
            reference = current.div_euclid(group);
        }
        Ok(Self {
            group,
            reference,
            volatility,
            maximum,
            control,
        })
    }
    fn rate(&self, index: i32, base: u32) -> Result<(u32, bool)> {
        let distance = u64::from(index.abs_diff(self.reference));
        let v = self
            .volatility
            .checked_add(distance.checked_mul(10000).ok_or(Error::Arithmetic)?)
            .ok_or(Error::Arithmetic)?
            .min(self.maximum);
        let scaled = u128::from(v) * self.group as u128;
        let variable = scaled
            .checked_mul(scaled)
            .and_then(|s| s.checked_mul(u128::from(self.control)))
            .ok_or(Error::Arithmetic)?
            .div_ceil(10_000_000_000_000);
        Ok((
            (u128::from(base) + variable).min(100000) as u32,
            v == self.maximum,
        ))
    }
}
#[derive(Clone, Debug)]
struct Segment {
    sqrt: u128,
    liquidity: u128,
    fee: u32,
    before_input: u64,
    before_output: u64,
    end_input: u64,
    end_output: u64,
}
#[derive(Clone, Debug)]
pub struct OrcaCurve {
    segments: Vec<Segment>,
    zero: bool,
    fees: [TransferFee; 2],
}
impl OrcaCurve {
    /// Compile one direction once per complete bank view. Prefix sums contain
    /// exact per-step fee rounding; quoting needs binary search + one exact step.
    pub fn decode(
        key: [u8; 32],
        pool: &[u8],
        arrays: &[&[u8]],
        oracle: Option<&[u8]>,
        fees: [TransferFee; 2],
        time: u64,
        zero: bool,
    ) -> Result<Self> {
        if pool.len() != 653
            || bytes::<8>(pool, 0)? != [63, 149, 209, 12, 225, 128, 99, 9]
            || arrays.is_empty()
            || arrays.len() > 6
        {
            return Err(Error::Layout);
        }
        let spacing = i32::from(u16_at(pool, 41)?);
        let fee = u32::from(u16_at(pool, 45)?);
        let mut current = i32_at(pool, 81)?;
        let mut sqrt = u128_at(pool, 65)?;
        let mut liquidity = u128_at(pool, 49)?;
        if spacing == 0
            || !(-443636..443636).contains(&current)
            || sqrt < sqrt_tick(current)?
            || sqrt > sqrt_tick(current + 1)?
        {
            return Err(Error::Layout);
        }
        let adaptive = if u16_at(pool, 43)? != spacing as u16 {
            Some(Adaptive::decode(
                key,
                oracle.ok_or(Error::Unsupported)?,
                time,
                current,
            )?)
        } else {
            None
        };
        let mut ticks = Vec::new();
        let mut starts = Vec::new();
        for d in arrays {
            let dynamic = bytes::<8>(d, 0)? == [17, 216, 246, 142, 225, 199, 218, 56];
            let start = i32_at(d, 8)?;
            if start.rem_euclid(88 * spacing) != 0
                || start < -443636 - 88 * spacing
                || start > 443636
            {
                return Err(Error::Layout);
            }
            let (mut cursor, bitmap) = if dynamic {
                if d.len() < 148 || d.len() > 10004 || bytes::<32>(d, 12)? != key {
                    return Err(Error::Layout);
                }
                let bitmap = u128_at(d, 44)?;
                if bitmap >> 88 != 0 {
                    return Err(Error::Layout);
                }
                (60, bitmap)
            } else {
                if d.len() != 9988
                    || bytes::<8>(d, 0)? != [69, 97, 189, 190, 110, 7, 66, 187]
                    || bytes::<32>(d, 9956)? != key
                {
                    return Err(Error::Unsupported);
                }
                (12, 0)
            };
            for i in 0..88 {
                let tag = *d.get(cursor).ok_or(Error::Layout)?;
                if tag > 1 || (dynamic && ((bitmap >> i) & 1) != u128::from(tag)) {
                    return Err(Error::Layout);
                }
                if tag == 1 {
                    let net = i128::from_le_bytes(bytes(d, cursor + 1)?);
                    let gross = u128_at(d, cursor + 17)?;
                    if gross == 0 || net.unsigned_abs() > gross {
                        return Err(Error::Layout);
                    }
                    ticks.push(Tick {
                        index: start + i * spacing,
                        net,
                    });
                }
                cursor += if dynamic && tag == 0 { 1 } else { 113 };
            }
            if dynamic && cursor != d.len() {
                return Err(Error::Layout);
            }
            starts.push(start);
        }
        starts.sort_unstable();
        if starts.windows(2).any(|s| s[1] - s[0] != 88 * spacing) {
            return Err(Error::Unsupported);
        }
        let lower = starts[0].max(-443636);
        let upper = (starts[starts.len() - 1] + 88 * spacing - 1).min(443636);
        if current < lower || current > upper {
            return Err(Error::Capacity);
        }
        ticks.sort_unstable_by_key(|t| t.index);
        let boundary = if zero { lower } else { upper };
        if !ticks.iter().any(|t| t.index == boundary) {
            ticks.push(Tick {
                index: boundary,
                net: 0,
            });
            ticks.sort_unstable_by_key(|t| t.index);
        }
        let mut segments = Vec::new();
        let (mut spent, mut output) = (0u64, 0u64);
        for _ in 0..2048 {
            let tick = if zero {
                ticks.iter().rev().find(|t| t.index <= current)
            } else {
                ticks.iter().find(|t| t.index > current)
            };
            let Some(tick) = tick else {
                break;
            };
            let mut target = tick.index;
            let rate = if let Some(a) = &adaptive {
                let group = current.div_euclid(a.group);
                let (rate, maxed) = a.rate(group, fee)?;
                let moving_away = (if zero {
                    group < a.reference
                } else {
                    group > a.reference
                }) && u64::from(group.abs_diff(a.reference))
                    > (a.maximum - a.volatility).div_ceil(10000);
                if liquidity > 0 && a.control > 0 && !(maxed && moving_away) {
                    let group_boundary = if zero {
                        group * a.group
                    } else {
                        (group + 1) * a.group
                    };
                    target = if zero {
                        target.max(group_boundary)
                    } else {
                        target.min(group_boundary)
                    };
                }
                rate
            } else {
                fee
            };
            let price = sqrt_tick(target)?;
            if (zero && price > sqrt) || (!zero && price < sqrt) {
                return Err(Error::Layout);
            }
            let needed = if zero {
                delta0(price, sqrt, liquidity, true)?
            } else {
                delta1(sqrt, price, liquidity, true)?
            };
            let debit = needed
                .checked_add(div(
                    mul(needed, U256::from(rate))?,
                    U256::from(1_000_000 - rate),
                    true,
                )?)
                .ok_or(Error::Arithmetic)?;
            let out = if zero {
                delta1(price, sqrt, liquidity, false)?
            } else {
                delta0(sqrt, price, liquidity, false)?
            };
            if debit > U256::from(u64::MAX - spent) || out > U256::from(u64::MAX - output) {
                // The bounded order cannot reach this end. Keep its exact head
                // for the final partial step without narrowing an overflowing sum.
                segments.push(Segment {
                    sqrt,
                    liquidity,
                    fee: rate,
                    before_input: spent,
                    before_output: output,
                    end_input: u64::MAX,
                    end_output: output,
                });
                break;
            }
            let end_input = spent + narrow(debit)?;
            let end_output = output + narrow(out)?;
            if end_input > spent {
                segments.push(Segment {
                    sqrt,
                    liquidity,
                    fee: rate,
                    before_input: spent,
                    before_output: output,
                    end_input,
                    end_output,
                });
            }
            spent = end_input;
            output = end_output;
            sqrt = price;
            if target == tick.index {
                liquidity = change(liquidity, tick.net, zero)?;
            }
            current = if zero { target - 1 } else { target };
            if target == boundary || spent > stocklana_adapters::MAX_INPUT {
                break;
            }
        }
        if segments.is_empty() || segments.len() >= 2048 {
            return Err(Error::Capacity);
        }
        Ok(Self {
            segments,
            zero,
            fees,
        })
    }
    pub fn quote(&self, input: u64) -> Result<u64> {
        if input == 0 || input > stocklana_adapters::MAX_INPUT {
            return Err(Error::Capacity);
        }
        let available = self.fees[0].net(input)?;
        let pos = self.segments.partition_point(|s| s.end_input < available);
        let s = self.segments.get(pos).ok_or(Error::Capacity)?;
        if s.end_input == available {
            return self.fees[1].net(s.end_output);
        }
        if available < s.before_input || s.liquidity == 0 {
            return Err(Error::Capacity);
        }
        let remaining = available - s.before_input;
        let net = (u128::from(remaining) * u128::from(1_000_000 - s.fee) / 1_000_000) as u64;
        let next = if self.zero {
            let l = U256::from(s.liquidity) << 64;
            small(div(
                mul(l, U256::from(s.sqrt))?,
                l.checked_add(mul(U256::from(net), U256::from(s.sqrt))?)
                    .ok_or(Error::Arithmetic)?,
                true,
            )?)?
        } else {
            s.sqrt
                .checked_add((u128::from(net) << 64) / s.liquidity)
                .ok_or(Error::Arithmetic)?
        };
        let out = if self.zero {
            delta1(next, s.sqrt, s.liquidity, false)?
        } else {
            delta0(s.sqrt, next, s.liquidity, false)?
        };
        let result = s
            .before_output
            .checked_add(narrow(out)?)
            .ok_or(Error::Arithmetic)?;
        if result == 0 {
            return Err(Error::Capacity);
        }
        self.fees[1].net(result)
    }
}
