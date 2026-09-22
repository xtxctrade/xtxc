//! On-chain residual oracle over the signed, predeclared independent candidates.
//! Tick liquidity nets are compiled once; swap-mutated heads are reread per leg.
use super::*;
use dex::graph::{Graph, Venue, MAX_CANDIDATES};
use skew_native::{
    clmm::ClmmCurve, dlmm::DlmmCurve, orca::OrcaCurve, TransferFee,
};

fn native<T>(x: skew_native::Result<T>) -> Result<T, ProgramError> {
    x.map_err(|_| err(ADAPTER))
}
enum Curve {
    Clmm(ClmmCurve),
    Orca(OrcaCurve),
    Dlmm(DlmmCurve),
}

pub(super) struct Oracle {
    curves: Vec<Option<Curve>>,
    direction: [bool; MAX_CANDIDATES],
    active: [bool; MAX_CANDIDATES],
    fees: [[TransferFee; 2]; MAX_CANDIDATES],
    observed_used: [u64; MAX_CANDIDATES],
}
impl Oracle {
    #[inline(never)]
    pub fn new(a: &[AccountInfo], g: &Graph, clock: &Clock) -> Result<Self, ProgramError> {
        let mut me = Self {
            curves: Vec::with_capacity(g.leg_count),
            direction: [false; MAX_CANDIDATES],
            active: [true; MAX_CANDIDATES],
            fees: [[TransferFee::default(); 2]; MAX_CANDIDATES],
            observed_used: [0; MAX_CANDIDATES],
        };
        // The default SBF heap is bounded. Reject before allocating tick curves.
        let mut array_count = 0;
        for leg in g.legs[..g.leg_count].iter().flatten() {
            need(
                matches!(
                    leg.venue,
                    Venue::RaydiumClmm
                        | Venue::ByrealClmm
                        | Venue::RaydiumCpmm
                        | Venue::Phoenix
                        | Venue::RaydiumAmmV4
                        | Venue::MeteoraDammV2
                        | Venue::OrcaWhirlpool
                        | Venue::MeteoraDlmm
                ),
                ADAPTER,
            )?;
            if matches!(
                leg.venue,
                Venue::RaydiumClmm | Venue::ByrealClmm | Venue::OrcaWhirlpool | Venue::MeteoraDlmm
            ) {
                let indices = match leg.venue {
                    Venue::RaydiumClmm | Venue::ByrealClmm => &leg.accounts[13..],
                    Venue::OrcaWhirlpool => &leg.accounts[11..14],
                    Venue::MeteoraDlmm => &leg.accounts[16..],
                    _ => &[],
                };
                for idx in indices {
                    let x = &a[*idx as usize];
                    if x.owner == a[leg.program as usize].key
                        && matches!(x.data_len(), 9988 | 10004 | 10136 | 10240)
                    {
                        array_count += 1;
                    }
                }
            }
        }
        need(array_count <= 6, BOUNDS)?;
        for (i, leg) in g.legs[..g.leg_count].iter().flatten().enumerate() {
            let im = a[g.assets[leg.source].mint as usize].key.to_bytes();
            let om = a[g.assets[leg.destination].mint as usize].key.to_bytes();
            me.fees[i] = [
                native(TransferFee::decode(
                    &a[g.assets[leg.source].mint as usize].try_borrow_data()?,
                    clock.epoch,
                ))?,
                native(TransferFee::decode(
                    &a[g.assets[leg.destination].mint as usize].try_borrow_data()?,
                    clock.epoch,
                ))?,
            ];
            let (pi, _, _, _) = leg.venue.bindings(leg.direction);
            let pool = &a[leg.accounts[pi] as usize];
            let p = pool.try_borrow_data()?;
            let offsets = match leg.venue {
                Venue::RaydiumClmm | Venue::ByrealClmm => Some([73, 105]),
                Venue::MeteoraDammV2 => Some([168, 200]),
                Venue::Phoenix => Some([48, 128]),
                Venue::OrcaWhirlpool => Some([101, 181]),
                Venue::MeteoraDlmm => Some([88, 120]),
                _ => None,
            };
            me.direction[i] = leg.direction;
            if let Some([x, y]) = offsets {
                let ma = ad(dex::key(&p, x))?;
                let mb = ad(dex::key(&p, y))?;
                need((im == ma && om == mb) || (im == mb && om == ma), IDENTITY)?;
                me.direction[i] = im == ma;
                if leg.venue == Venue::Phoenix {
                    need(leg.direction == me.direction[i], IDENTITY)?;
                }
            }
            let curve = if matches!(leg.venue, Venue::RaydiumClmm | Venue::ByrealClmm) {
                let config = &a[leg.accounts[1] as usize];
                need(
                    config.owner == pool.owner && ad(dex::key(&p, 9))? == config.key.to_bytes(),
                    IDENTITY,
                )?;
                let mut arrays = Vec::with_capacity(3);
                for idx in &leg.accounts[13..] {
                    let x = &a[*idx as usize];
                    if x.owner == pool.owner && x.data_len() != 1832 {
                        arrays.push(x.try_borrow_data()?);
                    }
                }
                let refs: Vec<&[u8]> = arrays.iter().map(|x| &x[..]).collect();
                let c = if leg.venue == Venue::ByrealClmm {
                    ClmmCurve::decode_byreal(
                        pool.key.to_bytes(),
                        &p,
                        &config.try_borrow_data()?,
                        &refs,
                        me.fees[i],
                        clock.unix_timestamp as u64,
                    )
                } else {
                    ClmmCurve::decode(
                        pool.key.to_bytes(),
                        &p,
                        &config.try_borrow_data()?,
                        &refs,
                        me.fees[i][0],
                        me.fees[i][1],
                    )
                };
                Some(Curve::Clmm(native(c)?))
            } else if leg.venue == Venue::OrcaWhirlpool {
                let mut arrays = Vec::with_capacity(3);
                for idx in &leg.accounts[11..14] {
                    let x = &a[*idx as usize];
                    if !arrays.iter().any(|prior: &&AccountInfo| prior.key == x.key) {
                        arrays.push(x);
                    }
                }
                let borrowed = arrays
                    .iter()
                    .map(|account| account.try_borrow_data())
                    .collect::<Result<Vec<_>, _>>()?;
                let refs = borrowed.iter().map(|data| &data[..]).collect::<Vec<_>>();
                let oracle = a[leg.accounts[14] as usize].try_borrow_data()?;
                Some(Curve::Orca(native(OrcaCurve::decode(
                    pool.key.to_bytes(),
                    &p,
                    &refs,
                    Some(&oracle),
                    me.fees[i],
                    clock.unix_timestamp as u64,
                    me.direction[i],
                ))?))
            } else if leg.venue == Venue::MeteoraDlmm {
                let borrowed = leg.accounts[16..]
                    .iter()
                    .map(|idx| a[*idx as usize].try_borrow_data())
                    .collect::<Result<Vec<_>, _>>()?;
                let refs = borrowed.iter().map(|data| &data[..]).collect::<Vec<_>>();
                Some(Curve::Dlmm(native(DlmmCurve::decode(
                    pool.key.to_bytes(),
                    &p,
                    &refs,
                    me.fees[i],
                    clock.slot,
                    clock.unix_timestamp as u64,
                ))?))
            } else {
                None
            };
            me.curves.push(curve);
        }
        // The optimizer sums independent output curves. Shared wallet input and
        // output are expected; all other writable CPI state must be disjoint.
        for i in 0..g.leg_count {
            let x = g.legs[i].as_ref().ok_or(err(BOUNDS))?;
            for j in 0..i {
                let y = g.legs[j].as_ref().ok_or(err(BOUNDS))?;
                for (xp, xi) in x.accounts.iter().enumerate() {
                    let xa = &a[*xi as usize];
                    if !x
                        .venue
                        .writable_with_owner(xp, xa.owner == a[x.program as usize].key)
                        || xa.key == a[0].key
                        || g.assets[..g.asset_count]
                            .iter()
                            .any(|asset| xa.key == a[asset.token as usize].key)
                        || xa.executable
                    {
                        continue;
                    }
                    for (yp, yi) in y.accounts.iter().enumerate() {
                        let ya = &a[*yi as usize];
                        if y.venue
                            .writable_with_owner(yp, ya.owner == a[y.program as usize].key)
                            && !ya.executable
                        {
                            need(xa.key != ya.key, IDENTITY)?;
                        }
                    }
                }
            }
        }
        Ok(me)
    }
    pub fn refresh(
        &mut self,
        a: &[AccountInfo],
        g: &Graph,
        clock: &Clock,
        used: &[u64; MAX_CANDIDATES],
    ) -> ProgramResult {
        for (i, c) in self.curves.iter_mut().enumerate() {
            if used[i] == self.observed_used[i] {
                continue;
            }
            if let Some(Curve::Clmm(c)) = c {
                let leg = g.legs[i].as_ref().ok_or(err(BOUNDS))?;
                self.active[i] = c
                    .refresh_after_swap(
                        &a[leg.accounts[2] as usize].try_borrow_data()?,
                        &a[leg.accounts[1] as usize].try_borrow_data()?,
                        clock.unix_timestamp as u64,
                    )
                    .is_ok();
            } else {
                // Whirlpool and DLMM compiled curves contain interval/bin
                // prefixes. Until their bounded in-place refresh is proven in
                // SBF, a mutated candidate cannot be selected twice; disjoint
                // unexecuted candidates remain available for the next Reflow.
                self.active[i] = false;
            }
            self.observed_used[i] = used[i];
        }
        Ok(())
    }
    #[inline(never)]
    pub(super) fn quote(
        &self,
        a: &[AccountInfo],
        g: &Graph,
        i: usize,
        q: u64,
        clock: &Clock,
        exact: bool,
    ) -> Result<u64, ProgramError> {
        need(self.active[i], ADAPTER)?;
        let leg = g.legs[i].as_ref().ok_or(err(BOUNDS))?;
        let fees = self.fees[i];
        if let Some(c) = &self.curves[i] {
            return match c {
                Curve::Clmm(curve) => native(curve.quote(q, self.direction[i])),
                Curve::Orca(curve) => native(curve.quote(q)),
                Curve::Dlmm(curve) => native(curve.quote(q, self.direction[i])),
            };
        }
        let (pi, _, _, _) = leg.venue.bindings(leg.direction);
        let p = a[leg.accounts[pi] as usize].try_borrow_data()?;
        match leg.venue {
            Venue::MeteoraDammV2 => native(
                native(skew_native::damm::DammCurve::decode(
                    &p,
                    fees,
                    clock.slot,
                    clock.unix_timestamp as u64,
                ))?
                .quote(q, self.direction[i]),
            ),
            Venue::Phoenix => {
                need(fees == [TransferFee::default(); 2], ADAPTER)?;
                let f = ad(ad(PhoenixBook::decode(&p))?.quote(
                    &a[0].key.to_bytes(),
                    leg.direction,
                    q,
                    dex::MAX_MATCHES,
                    clock.slot,
                    clock.unix_timestamp as u64,
                ))?;
                need(
                    f.output > 0 && f.input > 0 && (!exact || f.input == q),
                    ADAPTER,
                )?;
                Ok(f.output)
            }
            Venue::RaydiumCpmm => {
                let pool = ad(RaydiumPool::decode(&p, clock.unix_timestamp as u64))?;
                let config = &a[leg.accounts[2] as usize];
                need(
                    pool.config == config.key.to_bytes()
                        && config.owner == a[leg.program as usize].key,
                    IDENTITY,
                )?;
                let mut balances = [0; 2];
                for (j, v) in pool.vaults.iter().enumerate() {
                    let acc = a
                        .iter()
                        .find(|x| x.key.to_bytes() == *v)
                        .ok_or(err(IDENTITY))?;
                    balances[j] = ad(dex::u64_at(&acc.try_borrow_data()?, 64))?;
                }
                let im = a[g.assets[leg.source].mint as usize].key.to_bytes();
                let om = a[g.assets[leg.destination].mint as usize].key.to_bytes();
                let zero = im == pool.mints[0];
                need(
                    (zero || im == pool.mints[1]) && om == pool.mints[usize::from(zero)],
                    IDENTITY,
                )?;
                native(fees[1].net(ad(pool.quote(
                    &config.try_borrow_data()?,
                    balances,
                    zero,
                    native(fees[0].net(q))?,
                ))?))
            }
            Venue::RaydiumAmmV4 => {
                need(
                    p.len() == 752
                        && [1, 6].contains(&ad(dex::u64_at(&p, 0))?)
                        && fees == [TransferFee::default(); 2],
                    ADAPTER,
                )?;
                let mut reserves = [0u64; 2];
                for j in 0..2 {
                    let va = &a[leg.accounts[3 + j] as usize];
                    need(
                        ad(dex::key(&p, 336 + j * 32))? == va.key.to_bytes(),
                        IDENTITY,
                    )?;
                    reserves[j] = ad(dex::u64_at(&va.try_borrow_data()?, 64))?
                        .checked_sub(ad(dex::u64_at(&p, 192 + j * 8))?)
                        .ok_or(err(ADAPTER))?;
                }
                let zero = ad(dex::key(&a[leg.accounts[3] as usize].try_borrow_data()?, 0))?
                    == a[g.assets[leg.source].mint as usize].key.to_bytes();
                let n = ad(dex::u64_at(&p, 176))?;
                let d = ad(dex::u64_at(&p, 184))?;
                need(d > n, ADAPTER)?;
                let net = q
                    .checked_sub((u128::from(q) * u128::from(n)).div_ceil(u128::from(d)) as u64)
                    .ok_or(err(ADAPTER))?;
                let input = usize::from(!zero);
                let out = (u128::from(net) * u128::from(reserves[1 - input])
                    / (u128::from(reserves[input]) + u128::from(net)))
                    as u64;
                need(out > 0, ADAPTER)?;
                Ok(out)
            }
            _ => Err(err(ADAPTER)),
        }
    }
    /// Returns one next CPI from a fresh bounded allocation of the entire residual.
    /// No global optimum is claimed. Final step must consume the residual exactly.
    #[inline(never)]
    pub fn next(
        &mut self,
        a: &[AccountInfo],
        g: &Graph,
        clock: &Clock,
        remaining: u64,
        used: &[u64; MAX_CANDIDATES],
        executed: usize,
    ) -> Result<(usize, u64, u32), ProgramError> {
        self.next_normalized(a, g, clock, remaining, used, executed, 4, |_, output| {
            Ok(output)
        })
    }

    /// Select one candidate after translating every raw mint output into one
    /// comparable economic unit.  `maximum_executions` lets a parent reserve
    /// CPI slots for a preceding SOL->cash funding graph.
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    pub fn next_normalized<F>(
        &mut self,
        a: &[AccountInfo],
        g: &Graph,
        clock: &Clock,
        remaining: u64,
        used: &[u64; MAX_CANDIDATES],
        executed: usize,
        maximum_executions: usize,
        mut normalize: F,
    ) -> Result<(usize, u64, u32), ProgramError>
    where
        F: FnMut(usize, u64) -> Result<u64, ProgramError>,
    {
        need(
            (1..=4).contains(&maximum_executions) && executed < maximum_executions,
            BOUNDS,
        )?;
        self.refresh(a, g, clock, used)?;
        let mut cache = [(usize::MAX, 0u64, None); 16];
        let mut cursor = 0;
        let mut quote = |i: usize, q: u64| {
            if let Some((_, _, v)) = cache
                .iter()
                .find(|(edge, amount, _)| *edge == i && *amount == q)
            {
                return v.ok_or(err(ADAPTER));
            }
            let cap = g.legs[i]
                .as_ref()
                .ok_or(err(BOUNDS))?
                .budget
                .saturating_sub(used[i]);
            need(q <= cap, BOUNDS)?;
            let value = self
                .quote(
                    a,
                    g,
                    i,
                    q,
                    clock,
                    executed + 1 == maximum_executions,
                )
                .and_then(|output| normalize(i, output));
            cache[cursor] = (i, q, value.as_ref().ok().copied());
            cursor = (cursor + 1) % cache.len();
            value
        };
        if executed + 1 == maximum_executions {
            let mut best = None;
            for i in 0..g.leg_count {
                if let Ok(out) = quote(i, remaining) {
                    if best.is_none_or(|(_, old)| out > old) {
                        best = Some((i, out));
                    }
                }
            }
            return best
                .map(|(i, _)| (i, remaining, g.leg_count as u32))
                .ok_or(err(ADAPTER));
        }
        let plan = skew_engine::optimizer::oracle::refine(
            g.leg_count,
            remaining,
            u32::from(g.reflow_calls),
            |i, q| quote(i, q).map_err(|_| skew_engine::Error::Capacity),
        )
        .map_err(|_| err(ADAPTER))?;
        let i = (0..g.leg_count)
            .max_by_key(|i| (plan.inputs[*i], core::cmp::Reverse(*i)))
            .ok_or(err(ADAPTER))?;
        need(plan.inputs[i] > 0, ADAPTER)?;
        Ok((i, plan.inputs[i], plan.oracle_calls))
    }
}
