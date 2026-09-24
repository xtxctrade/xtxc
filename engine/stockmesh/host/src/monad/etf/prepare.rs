//! Dispatch boundary for ETF funding. Only all-synchronous, admitted and
//! gas-bounded legs may become the wallet-owned atomic position-flow call.
//! Anything else remains a staged user-wallet order, never a fake atomic mint.
use super::definition::AssetAddress;
use super::funding::FundingPlan;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegClass { Atomic, IssuerAsync, CrossChainContinuation }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatch {
    Atomic { gas_limit: u64, plan: FundingPlan },
    Staged { investment_id: [u8; 32], plan: FundingPlan },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareError { InvalidIdentity, WrongLegCount, UnadmittedVenue,
    MissingInvestmentId, EmptyGasEstimate }

pub fn dispatch(owner: AssetAddress, receiver: AssetAddress,
    investment_id: [u8; 32], plan: FundingPlan, classes: &[LegClass],
    admitted_venues: &[bool], measured_gas: Option<u64>, gas_ceiling: u64)
    -> Result<Dispatch, PrepareError>
{
    if owner.0 == [0; 20] || receiver != owner { return Err(PrepareError::InvalidIdentity); }
    if classes.len() != plan.legs.len() || admitted_venues.len() != plan.legs.len() {
        return Err(PrepareError::WrongLegCount);
    }
    for (leg, admitted) in plan.legs.iter().zip(admitted_venues) {
        if leg.shortage != 0 && (!admitted || leg.venue.is_none()) {
            return Err(PrepareError::UnadmittedVenue);
        }
    }
    let all_atomic = classes.iter().all(|&class| class == LegClass::Atomic);
    if all_atomic && measured_gas.is_some_and(|gas| gas <= gas_ceiling) {
        let gas_limit = measured_gas.ok_or(PrepareError::EmptyGasEstimate)?;
        if gas_limit == 0 { return Err(PrepareError::EmptyGasEstimate); }
        return Ok(Dispatch::Atomic { gas_limit, plan });
    }
    if investment_id == [0; 32] { return Err(PrepareError::MissingInvestmentId); }
    Ok(Dispatch::Staged { investment_id, plan })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monad::etf::funding::FundedLeg;
    fn address(n: u8) -> AssetAddress { AssetAddress([n; 20]) }
    fn plan() -> FundingPlan { FundingPlan { shares: 1_000_000_000_000_000_000,
        legs: vec![FundedLeg { asset: address(1), required: 1_000_000,
            use_from_wallet: 0, shortage: 1_000_000, cash_in: 20_000_000,
            min_bought: 1_000_000, venue: Some(address(3)) },
            FundedLeg { asset: address(2), required: 1_000_000,
                use_from_wallet: 1_000_000, shortage: 0, cash_in: 0,
                min_bought: 0, venue: None }],
        usdc_spent: 20_000_000, platform_fee: 1_000,
        max_usdc_debit: 21_000_000, usdc_refund: 999_000 } }
    #[test]
    fn atomic_only_when_both_routes_and_gas_are_admitted() {
        let ready = dispatch(address(9), address(9), [1; 32], plan(),
            &[LegClass::Atomic, LegClass::Atomic], &[true, true], Some(590_000), 800_000);
        assert!(matches!(ready, Ok(Dispatch::Atomic { gas_limit: 590_000, .. })));
        let staged = dispatch(address(9), address(9), [1; 32], plan(),
            &[LegClass::IssuerAsync, LegClass::Atomic], &[true, true], Some(590_000), 800_000);
        assert!(matches!(staged, Ok(Dispatch::Staged { .. })));
        let gas_staged = dispatch(address(9), address(9), [1; 32], plan(),
            &[LegClass::Atomic, LegClass::Atomic], &[true, true], Some(900_000), 800_000);
        assert!(matches!(gas_staged, Ok(Dispatch::Staged { .. })));
        assert_eq!(dispatch(address(9), address(8), [1; 32], plan(),
            &[LegClass::Atomic, LegClass::Atomic], &[true, true], Some(590_000), 800_000),
            Err(PrepareError::InvalidIdentity));
        assert_eq!(dispatch(address(9), address(9), [1; 32], plan(),
            &[LegClass::Atomic, LegClass::Atomic], &[false, true], Some(590_000), 800_000),
            Err(PrepareError::UnadmittedVenue));
    }
}
