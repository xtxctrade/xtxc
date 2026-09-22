//! Explicitly synthetic execution host; native copy-on-commit rollback.
//! Real DEX adapters and SVM rollback must be verified separately.
use crate::compiler::{Model, Segment, Snapshot};
use crate::reflow::{execute, Intent, Limits, Receipt};
use crate::runtime::{Balances, EdgePin, ExecutionHost};
use crate::{Error, Result, MAX_EDGES, MAX_SEGMENTS, SCALE};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixtureHost {
    pub venues: [Snapshot; MAX_EDGES],
    pub len: usize,
    pub balances: Balances,
    pub slot: u64,
    /// Successful partial execution, distinct from a failing CPI.
    pub settlement_caps: [u64; MAX_EDGES],
    pub fatal: u8,
    pub calls: u8,
}

impl FixtureHost {
    pub fn new(venues: &[Snapshot], amount: u64, slot: u64) -> Result<Self> {
        if venues.is_empty() || venues.len() > MAX_EDGES {
            return Err(Error::Bounds);
        }
        let mut all = [venues[0]; MAX_EDGES];
        all[..venues.len()].copy_from_slice(venues);
        Ok(Self {
            venues: all,
            len: venues.len(),
            balances: Balances {
                input: amount,
                output: 0,
            },
            slot,
            settlement_caps: [u64::MAX; MAX_EDGES],
            fatal: 0,
            calls: 0,
        })
    }

    pub fn atomic(&mut self, pins: &[EdgePin], intent: Intent, limits: Limits) -> Result<Receipt> {
        let mut staged = *self;
        let receipt = execute(&mut staged, pins, intent, limits)?;
        *self = staged;
        Ok(receipt)
    }
}

impl ExecutionHost for FixtureHost {
    fn slot(&self) -> u64 {
        self.slot
    }
    fn snapshot(&self, index: usize) -> Result<Snapshot> {
        if index >= self.len {
            return Err(Error::Bounds);
        }
        Ok(self.venues[index])
    }
    fn balances(&self) -> Result<Balances> {
        Ok(self.balances)
    }
    fn invoke(&mut self, index: usize, input: u64) -> Result<()> {
        if index >= self.len {
            return Err(Error::Bounds);
        }
        self.calls += 1;
        if self.fatal & (1 << index) != 0 {
            return Err(Error::FatalCpi);
        }
        let s = &mut self.venues[index];
        if !s.enabled || s.expires_at_slot < self.slot {
            return Err(Error::FatalCpi);
        }
        let spent = input.min(self.settlement_caps[index]).min(s.capacity);
        let output = s.exact_quote(spent)?;
        self.balances.input = self
            .balances
            .input
            .checked_sub(spent)
            .ok_or(Error::BalanceInvariant)?;
        self.balances.output = self
            .balances
            .output
            .checked_add(output)
            .ok_or(Error::Arithmetic)?;
        match &mut s.model {
            Model::ConstantProduct {
                reserve_in,
                reserve_out,
                ..
            } => {
                *reserve_in = reserve_in.checked_add(spent).ok_or(Error::Arithmetic)?;
                *reserve_out -= output;
            }
            Model::Book { levels, len } => {
                let mut remaining = spent;
                let mut new = [Segment::default(); MAX_SEGMENTS];
                let mut n = 0;
                for level in &levels[..*len as usize] {
                    let take = remaining.min(level.input_capacity);
                    remaining -= take;
                    if take < level.input_capacity {
                        new[n] = Segment {
                            input_capacity: level.input_capacity - take,
                            ..*level
                        };
                        n += 1;
                    }
                }
                *levels = new;
                *len = n as u8;
            }
        }
        s.capacity -= spent;
        s.generation += 1;
        Ok(())
    }
}

pub fn book(id: u8, levels: &[(u64, u64)], slot: u64) -> Snapshot {
    let mut segments = [Segment::default(); MAX_SEGMENTS];
    assert!(!levels.is_empty() && levels.len() <= MAX_SEGMENTS);
    for (i, &(capacity, rate)) in levels.iter().enumerate() {
        segments[i] = Segment {
            input_capacity: capacity,
            marginal_q32: rate,
        };
    }
    Snapshot {
        market: [id; 32],
        program: [200; 32],
        liquidity_id: [id; 32],
        input_mint: [1; 32],
        output_mint: [2; 32],
        writable_resources: 1u64 << (id % 64),
        slot,
        generation: 1,
        expires_at_slot: slot + 100,
        capacity: levels.iter().map(|l| l.0).sum(),
        atomic: true,
        enabled: true,
        model: Model::Book {
            levels: segments,
            len: levels.len() as u8,
        },
    }
}

pub fn rate(numerator: u64, denominator: u64) -> u64 {
    (u128::from(numerator) * SCALE / u128::from(denominator)) as u64
}
