//! Raydium concentrated-liquidity quote, using checked integer math.
//! Tick constants/ABI are adapted from raydium-io/raydium-clmm (Apache-2.0).
//! Dynamic pools require a coherent snapshot Clock; limit orders remain unsupported.
use crate::{bytes, u128_at, u64_at, Error, Result, TransferFee};
use crate::clmm_dynamic::DynamicFee;
use ethnum::U256;
const Q: u128 = 1u128 << 64;
const FACTORS: [u128; 19] = [
    0xfffcb933bd6fb800,
    0xfff97272373d4000,
    0xfff2e50f5f657000,
    0xffe5caca7e10f000,
    0xffcb9843d60f7000,
    0xff973b41fa98e800,
    0xff2ea16466c9b000,
    0xfe5dee046a9a3800,
    0xfcbe86c7900bb000,
    0xf987a7253ac65800,
    0xf3392b0822bb6000,
    0xe7159475a2caf000,
    0xd097f3bdfd2f2000,
    0xa9f746462d9f8000,
    0x70d869a156f31c00,
    0x31be135f97ed3200,
    0x9aa508b5b85a500,
    0x5d6af8dedc582c,
    0x2216e584f5fa,
];
pub fn sqrt_tick(tick: i32) -> Result<u128> {
    if !(-443636..=443636).contains(&tick) {
        return Err(Error::Layout);
    }
    let n = tick.unsigned_abs();
    let mut x = Q;
    for (i, f) in FACTORS.iter().enumerate() {
        if n & (1 << i) != 0 {
            x = x.checked_mul(*f).ok_or(Error::Arithmetic)? >> 64;
        }
    }
    Ok(if tick > 0 { u128::MAX / x } else { x })
}
pub(crate) fn mul(a: U256, b: U256) -> Result<U256> {
    a.checked_mul(b).ok_or(Error::Arithmetic)
}
pub(crate) fn div(n: U256, d: U256, up: bool) -> Result<U256> {
    if d == U256::ZERO {
        return Err(Error::Arithmetic);
    }
    let q = n / d;
    if up && n % d != U256::ZERO {
        q.checked_add(U256::ONE).ok_or(Error::Arithmetic)
    } else {
        Ok(q)
    }
}
pub(crate) fn small(x: U256) -> Result<u128> {
    u128::try_from(x).map_err(|_| Error::Arithmetic)
}
pub(crate) fn delta0(lo: u128, hi: u128, l: u128, up: bool) -> Result<U256> {
    let n = mul(
        U256::from(l) << 64,
        U256::from(hi.checked_sub(lo).ok_or(Error::Arithmetic)?),
    )?;
    div(div(n, U256::from(hi), up)?, U256::from(lo), up)
}
pub(crate) fn delta1(lo: u128, hi: u128, l: u128, up: bool) -> Result<U256> {
    div(
        mul(
            U256::from(l),
            U256::from(hi.checked_sub(lo).ok_or(Error::Arithmetic)?),
        )?,
        U256::from(Q),
        up,
    )
}
#[derive(Clone, Debug)]
struct Tick {
    index: i32,
    #[cfg(feature = "lazy-ticks")]
    step_generation: core::cell::Cell<u32>,
    #[cfg(feature = "lazy-ticks")]
    step_debit: core::cell::Cell<u64>,
    #[cfg(feature = "lazy-ticks")]
    step_output: core::cell::Cell<u64>,
    #[cfg(not(feature = "lazy-ticks"))]
    sqrt: u128,
    #[cfg(feature = "lazy-ticks")]
    sqrt: core::cell::Cell<u128>,
    net: i128,
}
impl Tick {
    fn price(&self) -> Result<u128> {
        #[cfg(not(feature = "lazy-ticks"))]
        {
            Ok(self.sqrt)
        }
        #[cfg(feature = "lazy-ticks")]
        {
            let p = self.sqrt.get();
            if p != 0 {
                return Ok(p);
            }
            let p = sqrt_tick(self.index)?;
            self.sqrt.set(p);
            Ok(p)
        }
    }
}
#[derive(Clone, Debug)]
pub struct ClmmCurve {
    sqrt: u128,
    liquidity: u128,
    current: i32,
    fee: u32,
    fee_on: u8,
    ticks: Vec<Tick>,
    input_fee: TransferFee,
    output_fee: TransferFee,
    byreal: bool,
    lower: i32,
    upper: i32,
    spacing: i32,
    dynamic: Option<DynamicFee>,
    #[cfg(feature = "lazy-ticks")]
    cache_generation: core::cell::Cell<u32>,
    #[cfg(feature = "lazy-ticks")]
    cache_direction: core::cell::Cell<bool>,
}
impl ClmmCurve {
    pub fn decode_at(pool_key: [u8;32], pool: &[u8], config: &[u8], arrays: &[&[u8]], fees: [TransferFee;2], timestamp: u64) -> Result<Self> {
        Self::decode_inner(pool_key,pool,config,arrays,fees,false,timestamp)
    }
    pub fn decode_byreal(
        pool_key: [u8; 32],
        pool: &[u8],
        config: &[u8],
        arrays: &[&[u8]],
        fees: [TransferFee; 2],
        timestamp: u64,
    ) -> Result<Self> {
        Self::decode_inner(pool_key, pool, config, arrays, fees, true, timestamp)
    }

    pub fn decode(
        pool_key: [u8; 32],
        pool: &[u8],
        config: &[u8],
        arrays: &[&[u8]],
        input_fee: TransferFee,
        output_fee: TransferFee,
    ) -> Result<Self> {
        Self::decode_inner(
            pool_key,
            pool,
            config,
            arrays,
            [input_fee, output_fee],
            false,
            0,
        )
    }
    fn decode_inner(
        pool_key: [u8; 32],
        pool: &[u8],
        config: &[u8],
        arrays: &[&[u8]],
        fees: [TransferFee; 2],
        byreal: bool,
        timestamp: u64,
    ) -> Result<Self> {
        if pool.len() != 1544
            || bytes::<8>(pool, 0)? != [247, 237, 227, 245, 215, 195, 222, 70]
            || config.len() != 117
            || bytes::<8>(config, 0)? != [218, 244, 33, 104, 203, 203, 43, 111]
        {
            return Err(Error::Layout);
        }
        if pool[389] & 16 != 0
            || pool[390] > 2
            || (byreal
                && (pool[1096] & !8 != 0
                    || u64_at(pool, 1080)? > timestamp
                    || pool[390..393].iter().any(|x| *x != 0)))
        {
            return Err(Error::Unsupported);
        }
        let override_fee = if byreal {
            u32::from_le_bytes(bytes(pool, 393)?)
        } else {
            0
        };
        let fee = if override_fee > 0 {
            override_fee
        } else {
            u32::from_le_bytes(bytes(config, 47)?)
        };
        let spacing = i32::from(u16::from_le_bytes(bytes(pool, 235)?));
        if fee >= 1_000_000 || spacing == 0 || arrays.is_empty() || arrays.len() > 8 {
            return Err(Error::Layout);
        }
        let mut ticks = Vec::with_capacity(arrays.len() * 60);
        let mut starts = Vec::with_capacity(arrays.len());
        for d in arrays {
            let dynamic = byreal && bytes::<8>(d, 0)? == [106, 139, 152, 36, 117, 153, 184, 56];
            let count = if dynamic {
                *d.get(108).ok_or(Error::Layout)? as usize
            } else {
                60
            };
            if (dynamic && (count > 60 || d.len() != 216 + count * 168))
                || (!dynamic
                    && (d.len() != 10240
                        || bytes::<8>(d, 0)? != [192, 155, 85, 205, 49, 249, 129, 42]))
                || bytes::<32>(d, 8)? != pool_key
            {
                return Err(Error::Unsupported);
            }
            let start = i32::from_le_bytes(bytes(d, 40)?);
            if start.rem_euclid(60 * spacing) != 0 {
                return Err(Error::Layout);
            }
            starts.push(start);
            let mut used = 0u64;
            for i in 0..60 {
                let o = if dynamic {
                    let position = d[48 + i] as usize;
                    if position == 0 {
                        continue;
                    }
                    if position > count || used & (1u64 << (position - 1)) != 0 {
                        return Err(Error::Layout);
                    }
                    used |= 1u64 << (position - 1);
                    216 + (position - 1) * 168
                } else {
                    44 + i * 168
                };
                if u64_at(d, o + 124)? != 0 || u64_at(d, o + 132)? != 0 {
                    return Err(Error::Unsupported);
                }
                if u128_at(d, o + 20)? == 0 {
                    continue;
                }
                let index = i32::from_le_bytes(bytes(d, o)?);
                if index != start + i as i32 * spacing {
                    return Err(Error::Layout);
                }
                ticks.push(Tick {
                    index,
                    #[cfg(feature = "lazy-ticks")]
                    step_generation: core::cell::Cell::new(0),
                    #[cfg(feature = "lazy-ticks")]
                    step_debit: core::cell::Cell::new(0),
                    #[cfg(feature = "lazy-ticks")]
                    step_output: core::cell::Cell::new(0),
                    #[cfg(not(feature = "lazy-ticks"))]
                    sqrt: sqrt_tick(index)?,
                    #[cfg(feature = "lazy-ticks")]
                    sqrt: core::cell::Cell::new(0),
                    net: i128::from_le_bytes(bytes(d, o + 4)?),
                });
            }
            if dynamic && used.count_ones() as usize != count {
                return Err(Error::Layout);
            }
        }
        starts.sort_unstable();
        if starts.windows(2).any(|s| s[1] - s[0] != 60 * spacing) {
            return Err(Error::Unsupported);
        }
        ticks.sort_unstable_by_key(|t| t.index);
        let current = i32::from_le_bytes(bytes(pool, 269)?);
        let dynamic = if byreal { None } else { DynamicFee::decode(pool, spacing, current, timestamp)? };
        let lower = starts[0];
        let upper = starts[starts.len() - 1] + 60 * spacing;
        if current < lower || current >= upper || lower < -443636 || upper > 443636 {
            return Err(Error::Unsupported);
        }
        let sqrt = u128_at(pool, 253)?;
        let liquidity = u128_at(pool, 237)?;
        if sqrt < sqrt_tick(current)? || sqrt > sqrt_tick(current + 1)? {
            return Err(Error::Layout);
        }
        Ok(Self {
            sqrt,
            liquidity,
            current,
            fee,
            fee_on: pool[390],
            ticks,
            input_fee: fees[0],
            output_fee: fees[1],
            byreal,
            lower,
            upper,
            spacing,
            dynamic,
            #[cfg(feature = "lazy-ticks")]
            cache_generation: core::cell::Cell::new(1),
            #[cfg(feature = "lazy-ticks")]
            cache_direction: core::cell::Cell::new(false),
        })
    }
    /// Reuse ticks only inside a transaction restricted to swaps. Swaps change
    /// price/active liquidity/fees, not initialized ticks' liquidity nets. Outside
    /// that boundary any tick dependency update requires a full decode.
    pub fn refresh_after_swap(&mut self, pool: &[u8], config: &[u8], timestamp: u64) -> Result<()> {
        if pool.len() != 1544
            || config.len() != 117
            || pool[389] & 16 != 0
            || i32::from(u16::from_le_bytes(bytes(pool, 235)?)) != self.spacing
            || (self.byreal
                && (pool[1096] & !8 != 0
                    || u64_at(pool, 1080)? > timestamp
                    || pool[390..393].iter().any(|x| *x != 0)))
        {
            return Err(Error::Unsupported);
        }
        let current = i32::from_le_bytes(bytes(pool, 269)?);
        let sqrt = u128_at(pool, 253)?;
        if current < self.lower
            || current >= self.upper
            || sqrt < sqrt_tick(current)?
            || sqrt > sqrt_tick(current + 1)?
        {
            return Err(Error::Capacity);
        }
        let override_fee = if self.byreal {
            u32::from_le_bytes(bytes(pool, 393)?)
        } else {
            0
        };
        let fee = if override_fee > 0 {
            override_fee
        } else {
            u32::from_le_bytes(bytes(config, 47)?)
        };
        if fee >= 1_000_000 || pool[390] > 2 {
            return Err(Error::Unsupported);
        }
        let dynamic = if self.byreal {None} else {DynamicFee::decode(pool,self.spacing,current,timestamp)?};
        #[cfg(feature = "lazy-ticks")]
        if self.current != current
            || self.sqrt != sqrt
            || self.liquidity != u128_at(pool, 237)?
            || self.fee != fee
            || self.fee_on != pool[390]
        {
            self.cache_generation.set(
                self.cache_generation
                    .get()
                    .checked_add(1)
                    .ok_or(Error::Arithmetic)?,
            );
        }
        self.current = current;
        self.sqrt = sqrt;
        self.liquidity = u128_at(pool, 237)?;
        self.fee = fee;
        self.fee_on = pool[390];
        self.dynamic = dynamic;
        Ok(())
    }
    // Dynamic fees change at each tick-spacing boundary, not only at initialized
    // liquidity ticks. Keep this path separate from the static segment cache.
    fn quote_dynamic(&self, input: u64, zero: bool, mut dynamic: DynamicFee) -> Result<u64> {
        let mut remain=self.input_fee.net(input)?;
        let (mut sqrt,mut liq,mut output)=(self.sqrt,self.liquidity,0u64);
        let mut index=self.current.div_euclid(self.spacing);
        let mut cursor=self.ticks.partition_point(|t|t.index<=self.current);
        let fee_input=self.fee_on==0 || self.fee_on==if zero {1} else {2};
        let mut steps=0usize;
        while remain>0 {
            let tick=if zero {
                cursor=cursor.checked_sub(1).ok_or(Error::Capacity)?;
                self.ticks.get(cursor)
            } else {let tick=self.ticks.get(cursor);cursor+=1;tick}.ok_or(Error::Capacity)?;
            let target=tick.price()?;
            if zero && target>sqrt || !zero && target<sqrt {return Err(Error::Layout);}
            loop {
                steps+=1;if steps>16_384 {return Err(Error::Capacity);}
                dynamic.update(index)?;
                let fee=dynamic.total(self.fee,self.spacing)?;
                let skipped=liq==0 || dynamic.capped();
                let bound=if skipped {target} else {
                    let group=if zero {index} else {index.checked_add(1).ok_or(Error::Arithmetic)?};
                    let boundary=group.checked_mul(self.spacing).ok_or(Error::Arithmetic)?.clamp(-443636,443636);
                    let price=sqrt_tick(boundary)?;
                    if zero {target.max(price)}else{target.min(price)}
                };
                if zero && bound>sqrt || !zero && bound<sqrt {return Err(Error::Layout);}
                if sqrt!=bound {
                    let available=if fee_input {((u128::from(remain)*(1_000_000-u128::from(fee)))/1_000_000) as u64}else{remain};
                    let needed=if zero {delta0(bound,sqrt,liq,true)?}else{delta1(sqrt,bound,liq,true)?};
                    let reached=U256::from(available)>=needed;
                    let next=if reached {bound}else if zero {
                        let l=U256::from(liq)<<64;
                        small(div(mul(l,U256::from(sqrt))?,l.checked_add(mul(U256::from(available),U256::from(sqrt))?).ok_or(Error::Arithmetic)?,true)?)?
                    }else {sqrt.checked_add((u128::from(available)<<64)/liq).ok_or(Error::Arithmetic)?};
                    let spent=u64::try_from(if reached {needed}else if zero {delta0(next,sqrt,liq,true)?}else{delta1(sqrt,next,liq,true)?}).map_err(|_|Error::Arithmetic)?;
                    let gross=u64::try_from(if zero {delta1(next,sqrt,liq,false)?}else{delta0(sqrt,next,liq,false)?}).map_err(|_|Error::Arithmetic)?;
                    let out=if fee_input {gross}else {gross.checked_sub((u128::from(gross)*u128::from(fee)).div_ceil(1_000_000) as u64).ok_or(Error::Arithmetic)?};
                    let debit=if !reached {remain}else if fee_input {
                        spent.checked_add((u128::from(spent)*u128::from(fee)).div_ceil(1_000_000-u128::from(fee)) as u64).ok_or(Error::Arithmetic)?
                    }else{spent};
                    remain=remain.checked_sub(debit).ok_or(Error::Arithmetic)?;
                    output=output.checked_add(out).ok_or(Error::Arithmetic)?;
                    sqrt=next;
                }
                // For skipped groups a continuing swap reached the initialized
                // tick. A partial final segment exits, so no inverse-price search
                // or mutable state is required to return its exact output.
                if skipped && remain>0 {
                    index=tick.index.div_euclid(self.spacing);
                    if !zero && tick.index%self.spacing==0 {index-=1;}
                }
                index=index.checked_add(if zero {-1}else{1}).ok_or(Error::Arithmetic)?;
                if remain==0 || sqrt==target {break;}
            }
            if sqrt==target {liq=change(liq,tick.net,zero)?;}
        }
        if output==0 {return Err(Error::Capacity);}
        self.output_fee.net(output)
    }
    pub fn quote(&self, input: u64, zero: bool) -> Result<u64> {
        if input == 0 || input > stocklana_adapters::MAX_INPUT {
            return Err(Error::Capacity);
        }
        if let Some(dynamic)=self.dynamic { return self.quote_dynamic(input,zero,dynamic); }
        let mut remain = self.input_fee.net(input)?;
        #[cfg(feature = "lazy-ticks")]
        if self.cache_direction.replace(zero) != zero {
            self.cache_generation.set(
                self.cache_generation
                    .get()
                    .checked_add(1)
                    .ok_or(Error::Arithmetic)?,
            );
        }
        let mut output = 0u64;
        let (mut sqrt, mut liq) = (self.sqrt, self.liquidity);
        let fee_input = self.fee_on == 0 || self.fee_on == if zero { 1 } else { 2 };
        let mut cursor = self.ticks.partition_point(|t| t.index <= self.current);
        for _ in 0..=self.ticks.len() {
            if remain == 0 {
                break;
            }
            let tick = if zero {
                if cursor > 0 {
                    cursor -= 1;
                    Some(&self.ticks[cursor])
                } else {
                    None
                }
            } else if cursor < self.ticks.len() {
                let t = &self.ticks[cursor];
                cursor += 1;
                Some(t)
            } else {
                None
            };
            // The DEX needs the next initialized tick account before executing the step;
            // mathematical liquidity beyond the last supplied tick is not executable.
            let target = tick.ok_or(Error::Capacity)?.price()?;
            if zero && target > sqrt || !zero && target < sqrt {
                return Err(Error::Layout);
            }
            // A full tick segment has exactly the same integer debit/output for
            // every quote from this head and direction. Reuse only full steps;
            // the final partial step always uses the exact arithmetic below.
            // Changed head/liquidity/fee or direction revokes the generation.
            #[cfg(feature = "lazy-ticks")]
            if let Some(t) = tick {
                if t.step_generation.get() == self.cache_generation.get()
                    && remain >= t.step_debit.get()
                {
                    remain = remain
                        .checked_sub(t.step_debit.get())
                        .ok_or(Error::Arithmetic)?;
                    output = output
                        .checked_add(t.step_output.get())
                        .ok_or(Error::Arithmetic)?;
                    sqrt = target;
                    liq = change(liq, t.net, zero)?;
                    continue;
                }
            }
            if liq == 0 {
                if let Some(t) = tick {
                    liq = change(liq, t.net, zero)?;
                    sqrt = target;
                    continue;
                } else {
                    return Err(Error::Capacity);
                }
            }
            let available = if fee_input {
                ((u128::from(remain) * (1_000_000 - u128::from(self.fee))) / 1_000_000) as u64
            } else {
                remain
            };
            let needed = if zero {
                delta0(target, sqrt, liq, true)?
            } else {
                delta1(sqrt, target, liq, true)?
            };
            let reached = U256::from(available) >= needed;
            let next = if reached {
                target
            } else if zero {
                let l = U256::from(liq) << 64;
                small(div(
                    mul(l, U256::from(sqrt))?,
                    l.checked_add(mul(U256::from(available), U256::from(sqrt))?)
                        .ok_or(Error::Arithmetic)?,
                    true,
                )?)?
            } else {
                sqrt.checked_add((u128::from(available) << 64) / liq)
                    .ok_or(Error::Arithmetic)?
            };
            let spent = if reached {
                u64::try_from(needed).map_err(|_| Error::Arithmetic)?
            } else if zero {
                u64::try_from(delta0(next, sqrt, liq, true)?).map_err(|_| Error::Arithmetic)?
            } else {
                u64::try_from(delta1(sqrt, next, liq, true)?).map_err(|_| Error::Arithmetic)?
            };
            let gross = u64::try_from(if zero {
                delta1(next, sqrt, liq, false)?
            } else {
                delta0(sqrt, next, liq, false)?
            })
            .map_err(|_| Error::Arithmetic)?;
            let out = if fee_input {
                gross
            } else {
                gross
                    .checked_sub(
                        (u128::from(gross) * u128::from(self.fee)).div_ceil(1_000_000) as u64,
                    )
                    .ok_or(Error::Arithmetic)?
            };
            let debit = if !reached {
                remain
            } else if fee_input {
                spent
                    .checked_add(
                        (u128::from(spent) * u128::from(self.fee))
                            .div_ceil(1_000_000 - u128::from(self.fee))
                            as u64,
                    )
                    .ok_or(Error::Arithmetic)?
            } else {
                spent
            };
            remain = remain.checked_sub(debit).ok_or(Error::Arithmetic)?;
            output = output.checked_add(out).ok_or(Error::Arithmetic)?;
            sqrt = next;
            if reached {
                if let Some(t) = tick {
                    #[cfg(feature = "lazy-ticks")]
                    {
                        t.step_debit.set(debit);
                        t.step_output.set(out);
                        t.step_generation.set(self.cache_generation.get());
                    }
                    liq = change(liq, t.net, zero)?;
                } else if remain != 0 {
                    return Err(Error::Capacity);
                }
            }
        }
        if remain != 0 || output == 0 {
            return Err(Error::Capacity);
        }
        self.output_fee.net(output)
    }
}
pub(crate) fn change(l: u128, net: i128, zero: bool) -> Result<u128> {
    let subtract = (net < 0) ^ zero;
    if subtract {
        l.checked_sub(net.unsigned_abs())
    } else {
        l.checked_add(net.unsigned_abs())
    }
    .ok_or(Error::Arithmetic)
}
