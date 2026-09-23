use crate::{monad_contract::MAINNET_CHAIN_ID, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Side { Buy, Sell }

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Phase {
    Prepared, Submitted, Unknown, Included, Finalized, Reconciled,
    Reverted, Replaced, Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Intent {
    pub order_id: String,
    pub idempotency_key: String,
    pub owner: String,
    pub chain_id: u64,
    pub asset_id: String,
    pub side: Side,
    pub quantity_atoms: String,
    pub max_input_atoms: String,
    pub quote_digest: String,
    pub quote_expires_at_ms: u64,
}

impl Intent {
    pub fn validate(&self) -> Result<()> {
        if !self.order_id.starts_with("mon_") || self.order_id.len() > 80
            || !self.order_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || self.idempotency_key.len() < 16 || self.idempotency_key.len() > 128
            || !self.idempotency_key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || self.chain_id != MAINNET_CHAIN_ID || !address(&self.owner)
            || !self.asset_id.starts_with("eip155:143:erc20:")
            || self.asset_id.len() > 256
            // Product::asset_id uses catalog labels verbatim. An issuer such
            // as "Anchored Finance" is valid, so the order grammar must not
            // reject the whole observed cohort before catalog matching.
            || !self.asset_id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'-' | b'_' | b'.' | b' '))
            || !atoms(&self.quantity_atoms) || !atoms(&self.max_input_atoms)
            || !digest(&self.quote_digest) || self.quote_expires_at_ms == 0
        { return Err("invalid Monad intent".into()); }
        Ok(())
    }
}

pub(crate) fn address(value: &str) -> bool {
    value.len() == 42 && value.starts_with("0x") && value[2..].bytes().all(|b| b.is_ascii_hexdigit())
}
pub(crate) fn digest(value: &str) -> bool {
    value.len() == 66 && value.starts_with("0x") && value[2..].bytes().all(|b| b.is_ascii_hexdigit())
}
fn atoms(value: &str) -> bool {
    !value.is_empty() && value.len() <= 39 && !value.starts_with('0')
        && value.bytes().all(|b| b.is_ascii_digit()) && value.parse::<u128>().is_ok()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Order {
    pub intent: Intent,
    pub phase: Phase,
    pub tx_hash: Option<String>,
    pub tx_nonce: Option<u64>,
    pub included_block_hash: Option<String>,
    pub finalized_block_hash: Option<String>,
    pub failure_code: Option<String>,
}

impl Order {
    pub fn new(intent: Intent) -> Result<Self> {
        intent.validate()?;
        Ok(Self { intent, phase: Phase::Prepared, tx_hash: None, tx_nonce: None,
            included_block_hash: None, finalized_block_hash: None, failure_code: None })
    }
    pub fn report_submission(&mut self, tx_hash: &str, tx_nonce: u64) -> Result<()> {
        if !digest(tx_hash) { return Err("invalid Monad transaction hash".into()); }
        if let Some(existing) = &self.tx_hash {
            return if existing.eq_ignore_ascii_case(tx_hash) && self.tx_nonce == Some(tx_nonce) {
                Ok(())
            } else { Err("order already bound to another transaction".into()) };
        }
        if self.phase != Phase::Prepared { return Err("order is not prepared".into()); }
        self.tx_hash = Some(tx_hash.to_ascii_lowercase());
        self.tx_nonce = Some(tx_nonce);
        self.phase = Phase::Submitted;
        Ok(())
    }
    pub fn mark_unknown(&mut self) -> Result<()> {
        if !matches!(self.phase, Phase::Submitted | Phase::Unknown) || self.tx_hash.is_none() {
            return Err("unknown requires an existing transaction hash".into());
        }
        self.phase = Phase::Unknown;
        Ok(())
    }
    pub fn cancel_unsigned(&mut self) -> Result<()> {
        if self.phase != Phase::Prepared { return Err("submitted order cannot be cancelled locally".into()); }
        self.phase = Phase::Cancelled;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn fixture_intent(id: &str, key: &str) -> Intent {
    Intent { order_id: id.into(), idempotency_key: key.into(),
        owner: "0x1111111111111111111111111111111111111111".into(), chain_id: MAINNET_CHAIN_ID,
        asset_id: "eip155:143:erc20:0x2222222222222222222222222222222222222222:Issuer:NVDA".into(),
        side: Side::Buy, quantity_atoms: "100".into(), max_input_atoms: "1000000".into(),
        quote_digest: format!("0x{}", "a".repeat(64)), quote_expires_at_ms: 2_000_000_000_000 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monad_contract::Catalog;

    #[test]
    fn every_observed_asset_identity_passes_order_grammar() {
        let catalog: Catalog = serde_json::from_str(include_str!("../../../monad/catalog/registry.v1.json")).unwrap();
        assert_eq!(catalog.token_observations.len(), 112);
        for token in &catalog.token_observations {
            let mut intent = fixture_intent("mon_identity", "idempotency_identity_0001");
            intent.asset_id = format!("eip155:{}:erc20:{}:{}:{}", token.chain_id, token.token_address, token.issuer, token.issuer_product_id);
            Order::new(intent).unwrap();
        }
    }
}
