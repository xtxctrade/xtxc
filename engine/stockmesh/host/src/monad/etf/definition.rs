//! Conservative host-side fixed-unit ETF arithmetic. Contract arithmetic is
//! authoritative; this module rejects any amount it cannot represent exactly.

use std::collections::HashSet;

pub const SHARE_SCALE: u128 = 1_000_000_000_000_000_000;
pub const MAX_UNIT_ATOMS: u128 = 100_000_000_000_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AssetAddress(pub [u8; 20]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leg {
    pub asset: AssetAddress,
    /// Underlying token atoms claimable by one whole ETF share (1e18 atoms).
    pub units_per_share: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub version: u64,
    pub share_granularity: u128,
    pub legs: Vec<Leg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefinitionError {
    InvalidDefinition,
    DuplicateAsset,
    InvalidShareAmount,
    Overflow,
    BackingDeficit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reserve {
    pub held: u128,
    pub owed: u128,
    pub surplus: u128,
}

impl Definition {
    pub fn validate(&self) -> Result<(), DefinitionError> {
        if self.version == 0 || !(2..=16).contains(&self.legs.len())
            || self.share_granularity == 0
            || self.share_granularity > SHARE_SCALE
            || SHARE_SCALE % self.share_granularity != 0
        {
            return Err(DefinitionError::InvalidDefinition);
        }
        let mut seen = HashSet::with_capacity(self.legs.len());
        for leg in &self.legs {
            if leg.asset.0 == [0; 20]
                || leg.units_per_share == 0
                || leg.units_per_share > MAX_UNIT_ATOMS
            {
                return Err(DefinitionError::InvalidDefinition);
            }
            if !seen.insert(leg.asset) {
                return Err(DefinitionError::DuplicateAsset);
            }
            let at_granularity = leg.units_per_share
                .checked_mul(self.share_granularity)
                .ok_or(DefinitionError::Overflow)?;
            if at_granularity % SHARE_SCALE != 0 {
                return Err(DefinitionError::InvalidDefinition);
            }
        }
        Ok(())
    }

    pub fn preview_claim(&self, share_atoms: u128) -> Result<Vec<u128>, DefinitionError> {
        self.validate()?;
        if share_atoms == 0 || share_atoms % self.share_granularity != 0 {
            return Err(DefinitionError::InvalidShareAmount);
        }
        self.legs.iter().map(|leg| {
            leg.units_per_share.checked_mul(share_atoms)
                .map(|product| product / SHARE_SCALE)
                .ok_or(DefinitionError::Overflow)
        }).collect()
    }

    pub fn reserve_state(&self, total_share_atoms: u128, held: &[u128])
        -> Result<Vec<Reserve>, DefinitionError>
    {
        self.validate()?;
        if total_share_atoms % self.share_granularity != 0
            || held.len() != self.legs.len()
        {
            return Err(DefinitionError::InvalidShareAmount);
        }
        self.legs.iter().zip(held).map(|(leg, &balance)| {
            let owed = leg.units_per_share.checked_mul(total_share_atoms)
                .ok_or(DefinitionError::Overflow)? / SHARE_SCALE;
            if balance < owed {
                return Err(DefinitionError::BackingDeficit);
            }
            Ok(Reserve { held: balance, owed, surplus: balance - owed })
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(n: usize) -> Definition {
        Definition {
            version: 1,
            share_granularity: 1_000_000_000_000,
            legs: (1..=n).map(|i| Leg {
                asset: AssetAddress([i as u8; 20]),
                units_per_share: 1_000_000,
            }).collect(),
        }
    }

    #[test]
    fn exact_claims_and_donations_for_all_supported_basket_sizes() {
        for n in [2, 3, 8, 16] {
            let d = definition(n);
            assert_eq!(d.preview_claim(2 * SHARE_SCALE).unwrap(), vec![2_000_000; n]);
            let mut held = vec![2_000_000; n];
            held[0] += 17;
            let reserves = d.reserve_state(2 * SHARE_SCALE, &held).unwrap();
            assert_eq!(reserves[0].surplus, 17);
            assert_eq!(reserves[1].surplus, 0);
            assert_eq!(d.reserve_state(0, &held).unwrap()[0].surplus, 2_000_017);
        }
    }

    #[test]
    fn invalid_and_insolvent_states_fail_closed() {
        let mut d = definition(2);
        assert_eq!(d.preview_claim(1), Err(DefinitionError::InvalidShareAmount));
        assert_eq!(d.reserve_state(SHARE_SCALE, &[1_000_000, 999_999]),
            Err(DefinitionError::BackingDeficit));
        d.legs[1].asset = d.legs[0].asset;
        assert_eq!(d.validate(), Err(DefinitionError::DuplicateAsset));
        let d = definition(2);
        assert_eq!(d.preview_claim(u128::MAX), Err(DefinitionError::InvalidShareAmount));
        assert_eq!(d.preview_claim(SHARE_SCALE * 100_000_000_000_000_000),
            Err(DefinitionError::Overflow));
    }
}
