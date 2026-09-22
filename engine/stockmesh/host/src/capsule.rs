//! Flow Capsules turn one large economic intent into a sequence of bounded,
//! owner-signed transactions. Each capsule keeps a conservative pro-rata
//! cumulative output floor. This is resumable execution, not cross-transaction
//! atomicity; every capsule must still pass the normal SBF and receipt gates.
use crate::Result;
use serde::{Deserialize, Serialize};

pub const MAX_PLAN_INPUT_ATOMS: u64 = 10_000_000_000 * 1_000_000;
pub const MAX_CAPSULE_INPUT_ATOMS: u64 = 500_000 * 1_000_000;
pub const MAX_CAPSULES: u64 = 20_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub id: [u8; 32],
    pub total_input: u64,
    pub total_minimum_output: u64,
    pub maximum_capsule_input: u64,
    pub settled_input: u64,
    pub settled_output: u64,
    pub next_sequence: u64,
    pub expires_slot: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Capsule {
    pub plan_id: [u8; 32],
    pub sequence: u64,
    pub input: u64,
    pub minimum_output: u64,
    pub required_cumulative_output: u64,
    pub prior_settled_input: u64,
    pub prior_settled_output: u64,
    pub expires_slot: u64,
}

impl Plan {
    pub fn new(
        id: [u8; 32],
        total_input: u64,
        total_minimum_output: u64,
        maximum_capsule_input: u64,
        first_sequence: u64,
        expires_slot: u64,
        current_slot: u64,
    ) -> Result<Self> {
        if id == [0; 32]
            || total_input == 0
            || total_input > MAX_PLAN_INPUT_ATOMS
            || total_minimum_output == 0
            || maximum_capsule_input == 0
            || maximum_capsule_input > MAX_CAPSULE_INPUT_ATOMS
            || first_sequence == 0
            || expires_slot < current_slot
            || total_input.div_ceil(maximum_capsule_input) > MAX_CAPSULES
        {
            return Err("capsule plan bounds".into());
        }
        Ok(Self {
            id,
            total_input,
            total_minimum_output,
            maximum_capsule_input,
            settled_input: 0,
            settled_output: 0,
            next_sequence: first_sequence,
            expires_slot,
        })
    }

    pub fn next(&self, slot: u64) -> Result<Option<Capsule>> {
        self.validate()?;
        if slot > self.expires_slot {
            return Err("capsule plan expired".into());
        }
        let remaining = self
            .total_input
            .checked_sub(self.settled_input)
            .ok_or("capsule settled input")?;
        if remaining == 0 {
            return Ok(None);
        }
        let input = remaining.min(self.maximum_capsule_input);
        let cumulative_input = self
            .settled_input
            .checked_add(input)
            .ok_or("capsule input overflow")?;
        let required = ceil_div(
            u128::from(cumulative_input)
                .checked_mul(u128::from(self.total_minimum_output))
                .ok_or("capsule floor overflow")?,
            u128::from(self.total_input),
        )?;
        let required = u64::try_from(required).map_err(|_| "capsule floor range")?;
        // The typed graph requires a positive floor. If earlier capsules have
        // already overperformed the final pro-rata target, retain a one-atom
        // postcondition rather than weakening execution to an unconstrained leg.
        let minimum_output = required.saturating_sub(self.settled_output).max(1);
        Ok(Some(Capsule {
            plan_id: self.id,
            sequence: self.next_sequence,
            input,
            minimum_output,
            required_cumulative_output: required,
            prior_settled_input: self.settled_input,
            prior_settled_output: self.settled_output,
            expires_slot: self.expires_slot,
        }))
    }

    /// Apply only a finalized, exact-wire receipt for this capsule.
    pub fn apply(
        &mut self,
        capsule: Capsule,
        actual_input: u64,
        actual_output: u64,
        slot: u64,
    ) -> Result<bool> {
        self.validate()?;
        if slot > self.expires_slot
            || capsule.plan_id != self.id
            || capsule.sequence != self.next_sequence
            || capsule.prior_settled_input != self.settled_input
            || capsule.prior_settled_output != self.settled_output
            || capsule.expires_slot != self.expires_slot
            || actual_input != capsule.input
            || actual_output < capsule.minimum_output
        {
            return Err("capsule receipt binding".into());
        }
        let next_input = self
            .settled_input
            .checked_add(actual_input)
            .ok_or("capsule input overflow")?;
        let next_output = self
            .settled_output
            .checked_add(actual_output)
            .ok_or("capsule output overflow")?;
        if next_input > self.total_input || next_output < capsule.required_cumulative_output {
            return Err("capsule cumulative floor".into());
        }
        self.settled_input = next_input;
        self.settled_output = next_output;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or("capsule sequence overflow")?;
        let complete = self.settled_input == self.total_input;
        if complete && self.settled_output < self.total_minimum_output {
            return Err("capsule final floor".into());
        }
        Ok(complete)
    }

    fn validate(&self) -> Result<()> {
        if self.id == [0; 32]
            || self.total_input == 0
            || self.total_input > MAX_PLAN_INPUT_ATOMS
            || self.total_minimum_output == 0
            || self.maximum_capsule_input == 0
            || self.maximum_capsule_input > MAX_CAPSULE_INPUT_ATOMS
            || self.settled_input > self.total_input
            || self.next_sequence == 0
        {
            return Err("capsule state bounds".into());
        }
        Ok(())
    }
}

fn ceil_div(numerator: u128, denominator: u128) -> Result<u128> {
    if denominator == 0 {
        return Err("capsule division".into());
    }
    Ok(numerator / denominator + u128::from(!numerator.is_multiple_of(denominator)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_billion_usdc_plan_closes_without_rounding_away_the_floor() {
        let total = MAX_PLAN_INPUT_ATOMS;
        let floor = 53_333_333_333_333;
        let mut plan =
            Plan::new([7; 32], total, floor, MAX_CAPSULE_INPUT_ATOMS, 1, 50_000, 1).unwrap();
        let mut count = 0;
        while let Some(capsule) = plan.next(10).unwrap() {
            let output = capsule.minimum_output;
            let done = plan.apply(capsule, capsule.input, output, 10).unwrap();
            count += 1;
            assert_eq!(done, count == MAX_CAPSULES);
        }
        assert_eq!(count, MAX_CAPSULES);
        assert_eq!(plan.settled_input, total);
        assert!(plan.settled_output >= floor);
    }

    #[test]
    fn stale_duplicate_partial_and_below_floor_receipts_are_rejected() {
        let mut plan = Plan::new([9; 32], 1_000, 333, 400, 8, 100, 1).unwrap();
        let first = plan.next(2).unwrap().unwrap();
        assert!(plan
            .apply(first, first.input - 1, first.minimum_output, 2)
            .is_err());
        assert!(plan
            .apply(first, first.input, first.minimum_output - 1, 2)
            .is_err());
        plan.apply(first, first.input, first.minimum_output, 2)
            .unwrap();
        assert!(plan
            .apply(first, first.input, first.minimum_output, 2)
            .is_err());
        let second = plan.next(2).unwrap().unwrap();
        assert!(plan
            .apply(second, second.input, second.minimum_output, 101)
            .is_err());
    }
}
