//! Exact-claim accounting for in-kind redemption and a staged cash exit.
//! A stage is settled only with observed wallet balances/confirmed receipts.
use super::definition::{Definition, DefinitionError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitError { Definition(DefinitionError), WrongLegCount, ExcessSale, Overflow }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitPlan { pub shares: u128, pub claims: Vec<u128> }

pub fn prepare_in_kind(def: &Definition, shares: u128) -> Result<ExitPlan, ExitError> {
    Ok(ExitPlan { shares, claims: def.preview_claim(shares).map_err(ExitError::Definition)? })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedCashExit {
    pub exit_id: [u8; 32],
    pub plan: ExitPlan,
    pub confirmed_received: Vec<u128>,
    pub confirmed_sold: Vec<u128>,
    pub confirmed_usdc_atoms: u128,
}

impl StagedCashExit {
    pub fn unsold(&self) -> Result<Vec<u128>, ExitError> {
        if self.confirmed_received.len() != self.plan.claims.len()
            || self.confirmed_sold.len() != self.plan.claims.len() {
            return Err(ExitError::WrongLegCount);
        }
        self.plan.claims.iter().zip(&self.confirmed_received)
            .zip(&self.confirmed_sold).map(|((&owed, &received), &sold)| {
                if received > owed || sold > received { return Err(ExitError::ExcessSale); }
                Ok(received - sold)
            }).collect()
    }

    pub fn completed(&self) -> bool {
        self.exit_id != [0; 32] && self.unsold().map(|unsold| {
            self.confirmed_received == self.plan.claims && unsold.iter().all(|&x| x == 0)
        }).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monad::etf::definition::{AssetAddress, Leg, SHARE_SCALE};
    fn def() -> Definition { Definition { version: 1, share_granularity: 1_000_000_000_000,
        legs: vec![Leg { asset: AssetAddress([1; 20]), units_per_share: 1_000_000 },
            Leg { asset: AssetAddress([2; 20]), units_per_share: 2_000_000 }] } }
    #[test]
    fn partial_exit_keeps_real_unsold_assets() {
        let plan = prepare_in_kind(&def(), SHARE_SCALE / 2).unwrap();
        assert_eq!(plan.claims, vec![500_000, 1_000_000]);
        let mut exit = StagedCashExit { exit_id: [7; 32], plan,
            confirmed_received: vec![500_000, 1_000_000],
            confirmed_sold: vec![500_000, 200_000], confirmed_usdc_atoms: 9_000_000 };
        assert_eq!(exit.unsold().unwrap(), vec![0, 800_000]);
        assert!(!exit.completed());
        exit.confirmed_sold[1] = 1_000_000;
        assert!(exit.completed());
        exit.confirmed_sold[1] = 1_000_001;
        assert_eq!(exit.unsold(), Err(ExitError::ExcessSale));
    }
}
