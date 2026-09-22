//! Integer exchange refinement against an exact quote oracle.
//!
//! A quote must describe one immutable state generation. Edges must have disjoint
//! writable market state (wallet accounts excepted). Non-concave or rounded
//! curves are admitted, so this returns a best observed allocation, NOT a global
//! optimality certificate. Every request and accepted exchange is bounded.
use crate::{Error, Result, MAX_ATOMS, MAX_EDGES, MAX_LEGS};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExactPlan {
    pub inputs: [u64; MAX_EDGES],
    pub outputs: [u64; MAX_EDGES],
    pub output: u64,
    pub costs: [u64; MAX_EDGES],
    pub cost: u64,
    pub oracle_calls: u32,
    pub exchanges: u32,
    pub final_step: u64,
    pub exhausted: bool,
}

fn quote<F: FnMut(usize, u64) -> Result<(u64, u64)>>(
    oracle: &mut F,
    calls: &mut u32,
    limit: u32,
    edge: usize,
    amount: u64,
) -> Result<(u64, u64)> {
    if amount == 0 {
        return Ok((0, 0));
    }
    if *calls == limit {
        return Err(Error::WorkLimit);
    }
    *calls += 1;
    oracle(edge, amount)
}

pub fn refine<F: FnMut(usize, u64) -> Result<u64>>(
    edges: usize,
    amount: u64,
    limit: u32,
    mut oracle: F,
) -> Result<ExactPlan> {
    refine_bounded(edges, amount, limit, u64::MAX, |i, q| {
        oracle(i, q).map(|out| (out, 0))
    })
}

/// Costs are conservative resource estimates supplied by the oracle. A final
/// runtime simulation remains mandatory; estimates never override a CU limit.
pub fn refine_bounded<F: FnMut(usize, u64) -> Result<(u64, u64)>>(
    edges: usize,
    amount: u64,
    limit: u32,
    cost_limit: u64,
    oracle: F,
) -> Result<ExactPlan> {
    refine_bounded_legs(edges, amount, limit, cost_limit, MAX_LEGS, oracle)
}

pub fn refine_bounded_legs<F: FnMut(usize, u64) -> Result<(u64, u64)>>(
    edges: usize,
    amount: u64,
    limit: u32,
    cost_limit: u64,
    max_legs: usize,
    mut oracle: F,
) -> Result<ExactPlan> {
    if edges == 0 || edges > MAX_EDGES || amount == 0 || amount > MAX_ATOMS {
        return Err(Error::Bounds);
    }
    if max_legs == 0 || max_legs > MAX_LEGS {
        return Err(Error::Bounds);
    }
    // Full-size single venues and evenly funded subsets are feasibility seeds.
    // Refinement below is in token atoms, not a fixed percentage candidate grid.
    let mut best = ExactPlan::default();
    let mut calls = 0;
    for mask in 1u16..(1u16 << edges) {
        let count = mask.count_ones() as usize;
        if count > max_legs || amount < count as u64 {
            continue;
        }
        let mut candidate = ExactPlan::default();
        let mut left = amount;
        let mut remaining = count;
        let mut valid = true;
        for i in 0..edges {
            if mask & (1 << i) == 0 {
                continue;
            }
            let input = left / remaining as u64;
            candidate.inputs[i] = input;
            match quote(&mut oracle, &mut calls, limit, i, input) {
                Ok((output, cost)) => {
                    candidate.costs[i] = cost;
                    candidate.cost = candidate.cost.checked_add(cost).ok_or(Error::Arithmetic)?;
                    candidate.outputs[i] = output;
                    candidate.output = candidate
                        .output
                        .checked_add(output)
                        .ok_or(Error::Arithmetic)?;
                }
                Err(_) => {
                    valid = false;
                    break;
                }
            }
            left -= input;
            remaining -= 1;
        }
        if valid && candidate.cost <= cost_limit && candidate.output > best.output {
            best = candidate;
        }
        if calls == limit {
            break;
        }
    }
    if best.output == 0 {
        return Err(if calls == limit {
            Error::WorkLimit
        } else {
            Error::Capacity
        });
    }
    let mut step = (amount / 2).max(1);
    'refine: loop {
        best.final_step = step;
        // Prevent an adversarial oracle from keeping an exchange scale alive.
        for _ in 0..32 {
            let mut next = best;
            for from in 0..edges {
                if best.inputs[from] == 0 {
                    continue;
                }
                let take = best.inputs[from].min(step);
                for to in 0..edges {
                    if from == to {
                        continue;
                    }
                    if best.inputs[to] == 0
                        && take < best.inputs[from]
                    && best.inputs.iter().filter(|x| **x > 0).count() == max_legs
                    {
                        continue;
                    }
                    if calls.saturating_add(2) > limit {
                        break 'refine;
                    }
                    let a = best.inputs[from] - take;
                    let b = best.inputs[to].checked_add(take).ok_or(Error::Arithmetic)?;
                    let qa = quote(&mut oracle, &mut calls, limit, from, a);
                    let qb = quote(&mut oracle, &mut calls, limit, to, b);
                    if let (Ok((oa, ca)), Ok((ob, cb))) = (qa, qb) {
                        let cost = best.cost - best.costs[from] - best.costs[to];
                        let cost = cost
                            .checked_add(ca)
                            .and_then(|v| v.checked_add(cb))
                            .ok_or(Error::Arithmetic)?;
                        if cost > cost_limit {
                            continue;
                        }
                        let output = best.output - best.outputs[from] - best.outputs[to];
                        let output = output
                            .checked_add(oa)
                            .and_then(|v| v.checked_add(ob))
                            .ok_or(Error::Arithmetic)?;
                        if output > next.output {
                            next = best;
                            next.inputs[from] = a;
                            next.inputs[to] = b;
                            next.outputs[from] = oa;
                            next.outputs[to] = ob;
                            next.costs[from] = ca;
                            next.costs[to] = cb;
                            next.cost = cost;
                            next.output = output;
                            next.exchanges += 1;
                        }
                    }
                }
            }
            if next.output == best.output {
                break;
            }
            best = next;
        }
        if step == 1 {
            best.final_step = 0;
            break;
        }
        step /= 2;
    }
    best.oracle_calls = calls;
    best.exhausted = best.final_step != 0;
    Ok(best)
}
