//! Exact-unit ETF funding from an explicitly selected wallet balance and
//! executable USDC buy quotes. This never infers permission from allowance.
use super::definition::{AssetAddress, Definition, DefinitionError};

pub const FEE_DENOMINATOR: u128 = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegOffer {
    pub asset: AssetAddress,
    pub wallet_balance: u128,
    pub use_from_wallet: u128,
    pub cash_in: u128,
    pub min_bought: u128,
    pub venue: Option<AssetAddress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FundedLeg {
    pub asset: AssetAddress,
    pub required: u128,
    pub use_from_wallet: u128,
    pub shortage: u128,
    pub cash_in: u128,
    pub min_bought: u128,
    pub venue: Option<AssetAddress>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingPlan {
    pub shares: u128,
    pub legs: Vec<FundedLeg>,
    pub usdc_spent: u128,
    pub platform_fee: u128,
    pub max_usdc_debit: u128,
    pub usdc_refund: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FundingError {
    Definition(DefinitionError),
    WrongLegCount,
    WrongAsset,
    WalletBalance,
    MissingExecutableQuote,
    ExcessiveWalletUse,
    Budget,
    FeeCap,
    Overflow,
}

pub fn prepare(def: &Definition, shares: u128, offers: &[LegOffer],
    max_usdc_debit: u128, fee_cap: u128) -> Result<FundingPlan, FundingError>
{
    let required = def.preview_claim(shares).map_err(FundingError::Definition)?;
    if offers.len() != required.len() { return Err(FundingError::WrongLegCount); }
    let mut legs = Vec::with_capacity(offers.len());
    let mut usdc_spent = 0u128;
    for ((definition_leg, &need), offer) in def.legs.iter().zip(&required).zip(offers) {
        if offer.asset != definition_leg.asset { return Err(FundingError::WrongAsset); }
        if offer.use_from_wallet > offer.wallet_balance { return Err(FundingError::WalletBalance); }
        if offer.use_from_wallet > need { return Err(FundingError::ExcessiveWalletUse); }
        let shortage = need - offer.use_from_wallet;
        if shortage == 0 {
            if offer.cash_in != 0 || offer.min_bought != 0 || offer.venue.is_some() {
                return Err(FundingError::MissingExecutableQuote);
            }
        } else if offer.cash_in == 0 || offer.min_bought < shortage || offer.venue.is_none() {
            return Err(FundingError::MissingExecutableQuote);
        }
        usdc_spent = usdc_spent.checked_add(offer.cash_in).ok_or(FundingError::Overflow)?;
        legs.push(FundedLeg { asset: offer.asset, required: need,
            use_from_wallet: offer.use_from_wallet, shortage,
            cash_in: offer.cash_in, min_bought: offer.min_bought, venue: offer.venue });
    }
    let platform_fee = usdc_spent / FEE_DENOMINATOR;
    if platform_fee > fee_cap { return Err(FundingError::FeeCap); }
    let debit = usdc_spent.checked_add(platform_fee).ok_or(FundingError::Overflow)?;
    if debit > max_usdc_debit { return Err(FundingError::Budget); }
    Ok(FundingPlan { shares, legs, usdc_spent, platform_fee, max_usdc_debit,
        usdc_refund: max_usdc_debit - debit })
}

/// Select the largest granularity-aligned share amount under a fixed-state,
/// monotone executable quote source. The caller must re-quote/simulate at
/// signing time; this number is not an execution guarantee.
pub fn max_affordable<F>(def: &Definition, max_shares: u128,
    max_usdc_debit: u128, fee_cap: u128, mut quotes: F)
    -> Result<Option<FundingPlan>, FundingError>
where F: FnMut(u128) -> Result<Vec<LegOffer>, FundingError>
{
    def.validate().map_err(FundingError::Definition)?;
    let mut lo = 0u128;
    let mut hi = max_shares / def.share_granularity;
    let mut best = None;
    while lo < hi {
        let mid = lo + (hi - lo + 1) / 2;
        let shares = mid.checked_mul(def.share_granularity).ok_or(FundingError::Overflow)?;
        match prepare(def, shares, &quotes(shares)?, max_usdc_debit, fee_cap) {
            Ok(plan) => { lo = mid; best = Some(plan); }
            Err(FundingError::Budget | FundingError::FeeCap) => { hi = mid - 1; }
            Err(error) => return Err(error),
        }
    }
    if lo == 0 { return Ok(None); }
    let shares = lo.checked_mul(def.share_granularity).ok_or(FundingError::Overflow)?;
    if best.as_ref().is_some_and(|plan| plan.shares == shares) { return Ok(best); }
    Ok(Some(prepare(def, shares, &quotes(shares)?, max_usdc_debit, fee_cap)?))
}

/// A staged purchase never claims to have minted. Purchases remain in the
/// owner's wallet until an independently observed balance covers every leg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedFunding {
    pub investment_id: [u8; 32],
    pub plan: FundingPlan,
    pub confirmed_wallet_balances: Vec<u128>,
}

impl StagedFunding {
    pub fn mint_ready(&self) -> bool {
        self.investment_id != [0; 32]
            && self.confirmed_wallet_balances.len() == self.plan.legs.len()
            && self.confirmed_wallet_balances.iter().zip(&self.plan.legs)
                .all(|(&held, leg)| held >= leg.required)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monad::etf::definition::{Leg, SHARE_SCALE};
    fn address(n: u8) -> AssetAddress { AssetAddress([n; 20]) }
    fn definition() -> Definition { Definition { version: 1,
        share_granularity: 1_000_000_000_000,
        legs: vec![Leg { asset: address(1), units_per_share: 1_000_000 },
            Leg { asset: address(2), units_per_share: 2_000_000 }] } }
    fn offers() -> Vec<LegOffer> { vec![
        LegOffer { asset: address(1), wallet_balance: 900_000,
            use_from_wallet: 500_000, cash_in: 12_000_000,
            min_bought: 500_000, venue: Some(address(3)) },
        LegOffer { asset: address(2), wallet_balance: 0,
            use_from_wallet: 0, cash_in: 44_000_000,
            min_bought: 2_000_000, venue: Some(address(4)) },
    ] }
    #[test]
    fn exact_user_selected_partial_funding_and_refund() {
        let plan = prepare(&definition(), SHARE_SCALE, &offers(),
            60_000_000, 2_800).unwrap();
        assert_eq!(plan.legs[0].shortage, 500_000);
        assert_eq!(plan.legs[1].shortage, 2_000_000);
        assert_eq!(plan.usdc_spent, 56_000_000);
        assert_eq!(plan.platform_fee, 2_800);
        assert_eq!(plan.usdc_refund, 3_997_200);
        let mut staged = StagedFunding { investment_id: [1; 32], plan,
            confirmed_wallet_balances: vec![999_999, 2_000_000] };
        assert!(!staged.mint_ready());
        staged.confirmed_wallet_balances[0] = 1_000_000;
        assert!(staged.mint_ready());
    }
    #[test]
    fn user_permission_and_budget_are_fail_closed() {
        let mut o = offers();
        o[0].wallet_balance = 1_000_000;
        o[0].use_from_wallet = 1_000_000;
        assert_eq!(prepare(&definition(), SHARE_SCALE, &o, 60_000_000, 3_000),
            Err(FundingError::MissingExecutableQuote));
        let o = offers();
        assert_eq!(prepare(&definition(), SHARE_SCALE, &o, 56_000_000, 3_000),
            Err(FundingError::Budget));
        assert_eq!(prepare(&definition(), SHARE_SCALE, &o, 60_000_000, 2_799),
            Err(FundingError::FeeCap));
        let mut o = offers(); o[0].use_from_wallet = 900_001;
        assert_eq!(prepare(&definition(), SHARE_SCALE, &o, 60_000_000, 3_000),
            Err(FundingError::WalletBalance));
        let mut o = offers(); o[1].min_bought = 1_999_999;
        assert_eq!(prepare(&definition(), SHARE_SCALE, &o, 60_000_000, 3_000),
            Err(FundingError::MissingExecutableQuote));
    }
    #[test]
    fn picks_budget_bounded_granularity_without_omitting_assets() {
        let d = definition();
        let plan = max_affordable(&d, 10 * SHARE_SCALE, 10_000_500, 2_000, |shares| {
            let need = d.preview_claim(shares).map_err(FundingError::Definition)?;
            Ok(vec![
                LegOffer { asset: address(1), wallet_balance: 0, use_from_wallet: 0,
                    cash_in: need[0] * 2, min_bought: need[0],
                    venue: Some(address(3)) },
                LegOffer { asset: address(2), wallet_balance: 0, use_from_wallet: 0,
                    cash_in: need[1] * 3 / 2, min_bought: need[1],
                    venue: Some(address(4)) },
            ])
        }).unwrap().unwrap();
        assert_eq!(plan.shares, 2 * SHARE_SCALE);
        assert_eq!(plan.legs.len(), 2);
        assert_eq!(plan.usdc_spent, 10_000_000);
        assert_eq!(plan.platform_fee, 500);
        assert_eq!(plan.usdc_refund, 0);
    }
}
