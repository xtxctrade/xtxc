use riptide_amm::{quote_exact_in_with_guards, GuardParams, Market};
use sha2::{Digest, Sha256};

/// A caller must bind the penalty to the actual planned execution context.
/// We deliberately do not infer Jupiter's terms for a direct Skew instruction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Context {
    pub state_slot: u64,
    pub execution_slot: u64,
    pub state_sha256: [u8; 32],
    pub execution_fingerprint: [u8; 32],
    pub penalty_per_million: u32,
}

/// One decoded 1024-byte market. No RPC, heap allocation, floating point or
/// account decode on the quote path. Context/guard errors fail closed.
pub struct RiptideCurve {
    market: Market,
    guards: GuardParams,
    context: Context,
}

impl RiptideCurve {
    pub fn decode(data: &[u8], context: Context) -> Result<Self, &'static str> {
        if data.len() != Market::LEN
            || context.execution_slot < context.state_slot
            || context.penalty_per_million > 1_000_000
        {
            return Err("invalid quote context");
        }
        if <[u8; 32]>::from(Sha256::digest(data)) != context.state_sha256 {
            return Err("snapshot hash mismatch");
        }
        let market = Market::from_bytes(data).map_err(|_| "invalid Riptide market")?;
        if market.discriminator != riptide_amm::MARKET_DISCRIMINATOR {
            return Err("wrong market discriminator");
        }
        let guards = GuardParams::from_market_fields(
            market.max_inventory_imbalance_guard_per_cent,
            market.max_a_inventory_per_m,
            market.max_b_inventory_per_m,
            market.min_spread_guard_per_m,
            market.min_oracle_price_guard,
            market.max_oracle_price_guard,
            market.valid_until,
        );
        Ok(Self {
            market,
            guards,
            context,
        })
    }

    pub fn quote(&self, input: u64, a_to_b: bool, context: Context) -> Result<u64, &'static str> {
        if context != self.context || input == 0 {
            return Err("stale or different execution context");
        }
        let q = quote_exact_in_with_guards(
            input,
            a_to_b,
            &self.market.oracle,
            self.market.reserves_a,
            self.market.reserves_b,
            self.market.skew_cliff_min_per_m,
            self.market.skew_cliff_max_per_m,
            context.penalty_per_million,
            context.execution_slot,
            &self.guards,
        )
        .map_err(|_| "quote guard or capacity rejected")?
        .quote;
        if q.amount_in != input || q.amount_out == 0 {
            return Err("partial or empty quote");
        }
        Ok(q.amount_out)
    }
}
