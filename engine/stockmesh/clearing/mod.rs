//! Flow Folding: bounded maximum-cost circulation at a supplied common price
//! vector. Residual reverse edges can UNDO earlier crossings. A pair-first greedy
//! matcher cannot do that and can strand a more valuable multi-asset cycle.
//!
//! Optimality is for integer value lots at these fixed prices, not joint price
//! discovery plus external routing. Hitting the pivot bound returns a feasible
//! plan with `optimal_at_fixed_prices = false`.
use crate::{as_u64, Error, Key, Result, WorkMeter, MAX_ATOMS, MAX_EDGES};

pub const MAX_INTENTS: usize = 32;
pub const MAX_ASSETS: usize = 8;
pub const MAX_PIVOTS: usize = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlowIntent {
    pub owner: Key,
    pub nonce: u64,
    pub sell: u8,
    pub buy: u8,
    pub amount: u64,
    pub min_out: u64,
    pub expires_at_slot: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FoldFill {
    pub input: u64,
    pub output: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FoldReport {
    pub fills: [FoldFill; MAX_INTENTS],
    pub value_quantum: u128,
    pub requested_value: u128,
    pub internally_cleared_value: u128,
    pub pivots: u16,
    pub reverse_edge_pivots: u16,
    pub optimal_at_fixed_prices: bool,
}

/// Flow Folding over caller-supplied per-asset token lots. One lot of every
/// asset is a bounded approximation of the same authenticated numeraire value.
/// Unlike [`fold`], this representation does not take the LCM of unrelated
/// prices, so a five-stock bank cannot become unmatchable merely because its
/// price numerators are co-prime. Token conservation is still exact: every
/// residual-graph unit debits and credits the same number of asset-specific
/// lots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LotFoldReport {
    pub fills: [FoldFill; MAX_INTENTS],
    pub lot_sizes: [u64; MAX_ASSETS],
    pub requested_lots: u128,
    pub internally_cleared_lots: u128,
    pub pivots: u16,
    pub reverse_edge_pivots: u16,
    pub optimal_at_fixed_lots: bool,
}

#[derive(Clone, Copy)]
struct Arc {
    from: usize,
    to: usize,
    order: usize,
    reverse: bool,
    cap: u128,
    cost: i64,
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

fn arc(
    orders: &[FlowIntent],
    capacities: &[u128; MAX_INTENTS],
    flows: &[u128; MAX_INTENTS],
    index: usize,
) -> Arc {
    let i = index / 2;
    let reverse = index % 2 == 1;
    let o = orders[i];
    if reverse {
        Arc {
            from: o.buy as usize,
            to: o.sell as usize,
            order: i,
            reverse,
            cap: flows[i],
            cost: -1,
        }
    } else {
        Arc {
            from: o.sell as usize,
            to: o.buy as usize,
            order: i,
            reverse,
            cap: capacities[i] - flows[i],
            cost: 1,
        }
    }
}

// Bellman-Ford over the residual graph, with positive cost = cleared gross
// notional. No positive-cost residual cycle is the optimality certificate.
fn improving_cycle(
    orders: &[FlowIntent],
    capacities: &[u128; MAX_INTENTS],
    flows: &[u128; MAX_INTENTS],
    assets: usize,
    meter: &mut WorkMeter,
) -> Result<Option<([usize; MAX_ASSETS], usize)>> {
    let mut distance = [0i64; MAX_ASSETS];
    let mut previous = [usize::MAX; MAX_ASSETS];
    let mut updated = None;
    for _ in 0..assets {
        updated = None;
        for j in 0..orders.len() * 2 {
            meter.charge(1)?;
            let e = arc(orders, capacities, flows, j);
            if e.cap > 0 && distance[e.to] < distance[e.from] + e.cost {
                distance[e.to] = distance[e.from] + e.cost;
                previous[e.to] = j;
                updated = Some(e.to);
            }
        }
        if updated.is_none() {
            return Ok(None);
        }
    }
    let mut vertex = updated.ok_or(Error::Arithmetic)?;
    for _ in 0..assets {
        let j = previous[vertex];
        if j == usize::MAX {
            return Err(Error::Arithmetic);
        }
        vertex = arc(orders, capacities, flows, j).from;
    }
    let start = vertex;
    let mut cycle = [0usize; MAX_ASSETS];
    let mut n = 0;
    loop {
        if n >= assets {
            return Err(Error::Arithmetic);
        }
        let j = previous[vertex];
        if j == usize::MAX {
            return Err(Error::Arithmetic);
        }
        cycle[n] = j;
        n += 1;
        vertex = arc(orders, capacities, flows, j).from;
        if vertex == start {
            break;
        }
    }
    Ok(Some((cycle, n)))
}

/// `prices[a]` is common-numeraire value per atomic unit, already normalized
/// for mint decimals. The caller is responsible for authenticated price sourcing.
/// Limits protect each intent even if the proposed clearing prices are poor.
pub fn fold(
    orders: &[FlowIntent],
    prices: &[u64],
    slot: u64,
    max_pivots: usize,
    meter: &mut WorkMeter,
) -> Result<FoldReport> {
    if orders.is_empty()
        || orders.len() > MAX_INTENTS
        || prices.len() < 2
        || prices.len() > MAX_ASSETS
        || max_pivots > MAX_PIVOTS
    {
        return Err(Error::Bounds);
    }
    let mut quantum = 1u128;
    for &p in prices {
        if p == 0 || p > 1_000_000_000_000 {
            return Err(Error::InvalidAmount);
        }
        quantum = (quantum / gcd(quantum, u128::from(p)))
            .checked_mul(u128::from(p))
            .ok_or(Error::Arithmetic)?;
    }
    let mut capacities = [0u128; MAX_INTENTS];
    let mut flows = [0u128; MAX_INTENTS];
    let mut report = FoldReport {
        value_quantum: quantum,
        ..FoldReport::default()
    };
    for (i, o) in orders.iter().enumerate() {
        if o.sell == o.buy
            || o.sell as usize >= prices.len()
            || o.buy as usize >= prices.len()
            || o.amount == 0
            || o.amount > MAX_ATOMS
            || o.min_out > MAX_ATOMS
        {
            return Err(Error::InvalidAmount);
        }
        if o.expires_at_slot < slot {
            return Err(Error::Expired);
        }
        if orders[..i]
            .iter()
            .any(|a| a.owner == o.owner && a.nonce == o.nonce)
        {
            return Err(Error::Duplicate);
        }
        let value = u128::from(o.amount) * u128::from(prices[o.sell as usize]);
        report.requested_value += value;
        // Only cross at a price meeting the full order's proportional limit.
        if value >= u128::from(o.min_out) * u128::from(prices[o.buy as usize]) {
            capacities[i] = value / quantum;
        }
    }
    for _ in 0..max_pivots {
        let Some((cycle, len)) = improving_cycle(orders, &capacities, &flows, prices.len(), meter)?
        else {
            report.optimal_at_fixed_prices = true;
            break;
        };
        let mut delta = u128::MAX;
        let mut cost = 0;
        let mut reverse = false;
        for &j in &cycle[..len] {
            let e = arc(orders, &capacities, &flows, j);
            delta = delta.min(e.cap);
            cost += e.cost;
            reverse |= e.reverse;
        }
        if cost <= 0 || delta == 0 {
            return Err(Error::Arithmetic);
        }
        for &j in &cycle[..len] {
            let e = arc(orders, &capacities, &flows, j);
            if e.reverse {
                flows[e.order] -= delta;
            } else {
                flows[e.order] += delta;
            }
        }
        report.pivots += 1;
        report.reverse_edge_pivots += u16::from(reverse);
    }
    if !report.optimal_at_fixed_prices {
        report.optimal_at_fixed_prices =
            improving_cycle(orders, &capacities, &flows, prices.len(), meter)?.is_none();
    }
    let mut debit = [0u128; MAX_ASSETS];
    let mut credit = [0u128; MAX_ASSETS];
    for (i, o) in orders.iter().enumerate() {
        let value = flows[i].checked_mul(quantum).ok_or(Error::Arithmetic)?;
        let fill = FoldFill {
            input: as_u64(value / u128::from(prices[o.sell as usize]))?,
            output: as_u64(value / u128::from(prices[o.buy as usize]))?,
        };
        if fill.input > o.amount
            || u128::from(fill.output) * u128::from(o.amount)
                < u128::from(fill.input) * u128::from(o.min_out)
        {
            return Err(Error::BalanceInvariant);
        }
        report.fills[i] = fill;
        report.internally_cleared_value += value;
        debit[o.sell as usize] += u128::from(fill.input);
        credit[o.buy as usize] += u128::from(fill.output);
    }
    if debit != credit {
        return Err(Error::BalanceInvariant);
    }
    Ok(report)
}

/// Clear a bounded intent graph on an explicit asset-lot lattice.
///
/// `lot_sizes[a]` is the number of atomic token units in one clearing lot for
/// asset `a`. The host must derive these lots from one authenticated, coherent
/// market snapshot and enforce its maximum value-error policy. This kernel only
/// proves integer feasibility, user limits, bounded work and exact per-token
/// conservation.
pub fn fold_lots(
    orders: &[FlowIntent],
    lot_sizes: &[u64],
    slot: u64,
    max_pivots: usize,
    meter: &mut WorkMeter,
) -> Result<LotFoldReport> {
    if orders.is_empty()
        || orders.len() > MAX_INTENTS
        || lot_sizes.len() < 2
        || lot_sizes.len() > MAX_ASSETS
        || max_pivots > MAX_PIVOTS
        || lot_sizes.iter().any(|&lot| lot == 0 || lot > MAX_ATOMS)
    {
        return Err(Error::Bounds);
    }
    let mut capacities = [0u128; MAX_INTENTS];
    let mut flows = [0u128; MAX_INTENTS];
    let mut report = LotFoldReport::default();
    report.lot_sizes[..lot_sizes.len()].copy_from_slice(lot_sizes);
    for (i, o) in orders.iter().enumerate() {
        if o.sell == o.buy
            || o.sell as usize >= lot_sizes.len()
            || o.buy as usize >= lot_sizes.len()
            || o.amount == 0
            || o.amount > MAX_ATOMS
            || o.min_out > MAX_ATOMS
        {
            return Err(Error::InvalidAmount);
        }
        if o.expires_at_slot < slot {
            return Err(Error::Expired);
        }
        if orders[..i]
            .iter()
            .any(|a| a.owner == o.owner && a.nonce == o.nonce)
        {
            return Err(Error::Duplicate);
        }
        let sell_lot = lot_sizes[o.sell as usize];
        let buy_lot = lot_sizes[o.buy as usize];
        let requested = o.amount / sell_lot;
        report.requested_lots = report
            .requested_lots
            .checked_add(u128::from(requested))
            .ok_or(Error::Arithmetic)?;
        // Every filled lot has this exact proportional rate. An intent whose
        // signed floor is stricter remains entirely residual.
        if u128::from(buy_lot) * u128::from(o.amount)
            >= u128::from(sell_lot) * u128::from(o.min_out)
        {
            capacities[i] = u128::from(requested);
        }
    }
    for _ in 0..max_pivots {
        let Some((cycle, len)) =
            improving_cycle(orders, &capacities, &flows, lot_sizes.len(), meter)?
        else {
            report.optimal_at_fixed_lots = true;
            break;
        };
        let mut delta = u128::MAX;
        let mut cost = 0;
        let mut reverse = false;
        for &j in &cycle[..len] {
            let e = arc(orders, &capacities, &flows, j);
            delta = delta.min(e.cap);
            cost += e.cost;
            reverse |= e.reverse;
        }
        if cost <= 0 || delta == 0 {
            return Err(Error::Arithmetic);
        }
        for &j in &cycle[..len] {
            let e = arc(orders, &capacities, &flows, j);
            if e.reverse {
                flows[e.order] -= delta;
            } else {
                flows[e.order] += delta;
            }
        }
        report.pivots += 1;
        report.reverse_edge_pivots += u16::from(reverse);
    }
    if !report.optimal_at_fixed_lots {
        report.optimal_at_fixed_lots =
            improving_cycle(orders, &capacities, &flows, lot_sizes.len(), meter)?.is_none();
    }
    let mut debit = [0u128; MAX_ASSETS];
    let mut credit = [0u128; MAX_ASSETS];
    for (i, o) in orders.iter().enumerate() {
        let lots = flows[i];
        let input = lots
            .checked_mul(u128::from(lot_sizes[o.sell as usize]))
            .ok_or(Error::Arithmetic)?;
        let output = lots
            .checked_mul(u128::from(lot_sizes[o.buy as usize]))
            .ok_or(Error::Arithmetic)?;
        let fill = FoldFill {
            input: as_u64(input)?,
            output: as_u64(output)?,
        };
        if fill.input > o.amount
            || u128::from(fill.output) * u128::from(o.amount)
                < u128::from(fill.input) * u128::from(o.min_out)
        {
            return Err(Error::BalanceInvariant);
        }
        report.fills[i] = fill;
        report.internally_cleared_lots = report
            .internally_cleared_lots
            .checked_add(lots)
            .ok_or(Error::Arithmetic)?;
        debit[o.sell as usize] += input;
        credit[o.buy as usize] += output;
    }
    if debit != credit {
        return Err(Error::BalanceInvariant);
    }
    Ok(report)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResidualGroup {
    pub sell: u8,
    pub buy: u8,
    pub amount_in: u64,
    pub min_out: u64,
    pub members: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Residuals {
    pub groups: [ResidualGroup; MAX_EDGES],
    pub len: usize,
}

pub fn residuals(orders: &[FlowIntent], report: &FoldReport) -> Result<Residuals> {
    if orders.len() > MAX_INTENTS {
        return Err(Error::Bounds);
    }
    validate_fold(orders, report)?;
    build_residuals(orders, &report.fills)
}

pub fn residuals_lots(orders: &[FlowIntent], report: &LotFoldReport) -> Result<Residuals> {
    if orders.len() > MAX_INTENTS {
        return Err(Error::Bounds);
    }
    validate_lot_fold(orders, report)?;
    build_residuals(orders, &report.fills)
}

fn build_residuals(orders: &[FlowIntent], fills: &[FoldFill; MAX_INTENTS]) -> Result<Residuals> {
    let mut out = Residuals::default();
    for (i, o) in orders.iter().enumerate() {
        let amount = o
            .amount
            .checked_sub(fills[i].input)
            .ok_or(Error::BalanceInvariant)?;
        if amount == 0 {
            if fills[i].output < o.min_out {
                return Err(Error::MinOut);
            }
            continue;
        }
        let j = match out.groups[..out.len]
            .iter()
            .position(|g| g.sell == o.sell && g.buy == o.buy)
        {
            Some(j) => j,
            None => {
                if out.len == MAX_EDGES {
                    return Err(Error::Bounds);
                }
                out.groups[out.len] = ResidualGroup {
                    sell: o.sell,
                    buy: o.buy,
                    ..ResidualGroup::default()
                };
                out.len += 1;
                out.len - 1
            }
        };
        let group = &mut out.groups[j];
        group.amount_in = group
            .amount_in
            .checked_add(amount)
            .ok_or(Error::Arithmetic)?;
        if group.amount_in > MAX_ATOMS {
            return Err(Error::InvalidAmount);
        }
        group.min_out = group
            .min_out
            .checked_add(o.min_out.saturating_sub(fills[i].output))
            .ok_or(Error::Arithmetic)?;
        group.members |= 1 << i;
    }
    Ok(out)
}

pub fn validate_lot_fold(orders: &[FlowIntent], report: &LotFoldReport) -> Result<()> {
    if orders.len() > MAX_INTENTS {
        return Err(Error::Bounds);
    }
    let mut debit = [0u128; MAX_ASSETS];
    let mut credit = [0u128; MAX_ASSETS];
    let mut cleared_lots = 0u128;
    for (i, o) in orders.iter().enumerate() {
        let f = report.fills[i];
        if o.sell as usize >= MAX_ASSETS
            || o.buy as usize >= MAX_ASSETS
            || o.sell == o.buy
            || o.amount == 0
        {
            return Err(Error::BalanceInvariant);
        }
        let sell_lot = report.lot_sizes[o.sell as usize];
        let buy_lot = report.lot_sizes[o.buy as usize];
        if sell_lot == 0
            || buy_lot == 0
            || f.input > o.amount
            || (f.input == 0 && f.output != 0)
            || f.input % sell_lot != 0
            || f.output % buy_lot != 0
            || f.input / sell_lot != f.output / buy_lot
            || u128::from(f.output) * u128::from(o.amount)
                < u128::from(f.input) * u128::from(o.min_out)
        {
            return Err(Error::BalanceInvariant);
        }
        cleared_lots = cleared_lots
            .checked_add(u128::from(f.input / sell_lot))
            .ok_or(Error::Arithmetic)?;
        debit[o.sell as usize] += u128::from(f.input);
        credit[o.buy as usize] += u128::from(f.output);
    }
    if debit != credit || cleared_lots != report.internally_cleared_lots {
        return Err(Error::BalanceInvariant);
    }
    Ok(())
}

pub fn validate_fold(orders: &[FlowIntent], report: &FoldReport) -> Result<()> {
    if orders.len() > MAX_INTENTS {
        return Err(Error::Bounds);
    }
    let mut debit = [0u128; MAX_ASSETS];
    let mut credit = [0u128; MAX_ASSETS];
    for (i, o) in orders.iter().enumerate() {
        let f = report.fills[i];
        if o.sell as usize >= MAX_ASSETS
            || o.buy as usize >= MAX_ASSETS
            || o.sell == o.buy
            || o.amount == 0
            || f.input > o.amount
            || (f.input == 0 && f.output != 0)
            || u128::from(f.output) * u128::from(o.amount)
                < u128::from(f.input) * u128::from(o.min_out)
        {
            return Err(Error::BalanceInvariant);
        }
        debit[o.sell as usize] += u128::from(f.input);
        credit[o.buy as usize] += u128::from(f.output);
    }
    if debit != credit {
        return Err(Error::BalanceInvariant);
    }
    Ok(())
}

/// Final per-intent postconditions and per-mint conservation. `external_delta`
/// must come from trusted before/after venue settlement balances, not solver
/// claims. Positive is net tokens received from external venues by this cell.
pub fn verify_settlement(
    orders: &[FlowIntent],
    debits: &[u64],
    credits: &[u64],
    external_delta: [i128; MAX_ASSETS],
) -> Result<()> {
    if orders.len() > MAX_INTENTS || orders.len() != debits.len() || orders.len() != credits.len() {
        return Err(Error::Bounds);
    }
    let mut net = external_delta;
    for (i, o) in orders.iter().enumerate() {
        if o.sell as usize >= MAX_ASSETS
            || o.buy as usize >= MAX_ASSETS
            || o.sell == o.buy
            || debits[i] != o.amount
        {
            return Err(Error::BalanceInvariant);
        }
        if credits[i] < o.min_out {
            return Err(Error::MinOut);
        }
        net[o.sell as usize] = net[o.sell as usize]
            .checked_add(i128::from(debits[i]))
            .ok_or(Error::Arithmetic)?;
        net[o.buy as usize] = net[o.buy as usize]
            .checked_sub(i128::from(credits[i]))
            .ok_or(Error::Arithmetic)?;
    }
    if net != [0; MAX_ASSETS] {
        return Err(Error::BalanceInvariant);
    }
    Ok(())
}

/// Distribute *observed* external output: satisfy every remaining minimum first,
/// then allocate surplus proportional to residual input. Dust goes to the lowest
/// (owner, nonce), making permutation of the submission array irrelevant.
pub fn distribute(
    orders: &[FlowIntent],
    folded: &FoldReport,
    group: ResidualGroup,
    observed_output: u64,
) -> Result<[u64; MAX_INTENTS]> {
    let expected = residuals(orders, folded)?;
    if !expected.groups[..expected.len].contains(&group) {
        return Err(Error::Identity);
    }
    if observed_output < group.min_out {
        return Err(Error::MinOut);
    }
    let surplus = observed_output - group.min_out;
    let mut out = [0u64; MAX_INTENTS];
    let mut allocated = 0u64;
    let mut first = None;
    for (i, o) in orders.iter().enumerate() {
        if group.members & (1 << i) == 0 {
            continue;
        }
        let residual = o.amount - folded.fills[i].input;
        out[i] = o
            .min_out
            .saturating_sub(folded.fills[i].output)
            .checked_add(as_u64(
                u128::from(surplus) * u128::from(residual) / u128::from(group.amount_in),
            )?)
            .ok_or(Error::Arithmetic)?;
        allocated = allocated.checked_add(out[i]).ok_or(Error::Arithmetic)?;
        if first.is_none_or(|j: usize| (o.owner, o.nonce) < (orders[j].owner, orders[j].nonce)) {
            first = Some(i);
        }
    }
    let dust = observed_output
        .checked_sub(allocated)
        .ok_or(Error::BalanceInvariant)?;
    out[first.ok_or(Error::Bounds)?] += dust;
    Ok(out)
}
