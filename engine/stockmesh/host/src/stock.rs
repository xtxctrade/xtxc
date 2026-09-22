//! Versioned stock intent admission. The configured attestor is a trust root,
//! not an oracle inferred from a ticker. These checks never confer RFQ access.
use crate::Result;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
const DOMAIN: &[u8] = b"SKEW_STOCK_STATE_V1\0";
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Product {
    pub mint: String,
    pub issuer: String,
    pub primary_is_synchronous: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub instrument: String,
    pub version: u64,
    pub attestor: String,
    pub quote_mint: String,
    pub products: Vec<Product>,
    pub max_state_lag_slots: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductState {
    pub mint: String,
    pub secondary: bool,
    pub rfq: bool,
    pub primary: bool,
    pub corporate_action_halt: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub instrument: String,
    pub version: u64,
    pub sequence: u64,
    pub slot: u64,
    pub expires_slot: u64,
    pub underlying_open: bool,
    pub products: Vec<ProductState>,
}
impl State {
    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        let mut b = DOMAIN.to_vec();
        b.extend(serde_json::to_vec(self).map_err(|e| e.to_string())?);
        Ok(b)
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StockContext {
    pub policy: Policy,
    pub state: State,
    pub signature: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EconomicIntent {
    pub instrument: String,
    pub version: u64,
    pub input_mint: String,
    pub input_atoms: u64,
    pub output_mint: String,
    pub issuer: String,
    pub min_output_atoms: u64,
    pub deadline_slot: u64,
    pub allow_underlying_closed: bool,
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Secondary,
    Rfq,
    Primary,
}
fn key(s: &str) -> Result<[u8; 32]> {
    bs58::decode(s)
        .into_vec()
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "invalid public key".into())
}
impl StockContext {
    pub fn verify(&self, slot: u64) -> Result<[u8; 32]> {
        let p = &self.policy;
        let s = &self.state;
        if p.instrument.is_empty()
            || p.instrument.len() > 32
            || p.version == 0
            || p.products.is_empty()
            || p.products.len() > 8
            || p.max_state_lag_slots == 0
            || p.max_state_lag_slots > 150
            || s.instrument != p.instrument
            || s.version != p.version
            || s.sequence == 0
            || s.slot > slot
            || s.expires_slot < slot
            || s.expires_slot < s.slot
            || slot - s.slot > p.max_state_lag_slots
            || s.products.len() != p.products.len()
        {
            return Err("stock state identity/version/freshness".into());
        }
        key(&p.quote_mint)?;
        let mut seen = std::collections::BTreeSet::new();
        for product in &p.products {
            key(&product.mint)?;
            if product.issuer.is_empty()
                || product.issuer.len() > 64
                || product.mint == p.quote_mint
                || !seen.insert(&product.mint)
                || s.products.iter().filter(|x| x.mint == product.mint).count() != 1
            {
                return Err("stock product binding".into());
            }
        }
        let bytes = s.signing_bytes()?;
        let public = VerifyingKey::from_bytes(&key(&p.attestor)?).map_err(|e| e.to_string())?;
        let sig = Signature::from_slice(
            &bs58::decode(&self.signature)
                .into_vec()
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        public
            .verify_strict(&bytes, &sig)
            .map_err(|_| "invalid stock attestation")?;
        let mut hash = Sha256::new();
        hash.update(serde_json::to_vec(p).map_err(|e| e.to_string())?);
        hash.update(bytes);
        Ok(hash.finalize().into())
    }
    pub fn admit(&self, intent: &EconomicIntent, slot: u64, kind: Kind) -> Result<[u8; 32]> {
        let state_hash = self.verify(slot)?;
        let p = &self.policy;
        if intent.instrument != p.instrument
            || intent.version != p.version
            || intent.input_mint != p.quote_mint
            || intent.input_atoms == 0
            || intent.input_atoms > 500_000_000_000
            || intent.min_output_atoms == 0
            || intent.deadline_slot < slot
            || intent.deadline_slot > self.state.expires_slot
            || (!self.state.underlying_open && !intent.allow_underlying_closed)
        {
            return Err("economic intent constraints".into());
        }
        let product = p
            .products
            .iter()
            .find(|p| p.mint == intent.output_mint && p.issuer == intent.issuer)
            .ok_or("issuer/mint not approved")?;
        let state = self
            .state
            .products
            .iter()
            .find(|s| s.mint == product.mint)
            .ok_or("product state missing")?;
        let available = match kind {
            Kind::Secondary => state.secondary,
            Kind::Rfq => state.rfq,
            Kind::Primary => state.primary && product.primary_is_synchronous,
        };
        if state.corporate_action_halt || !available {
            return Err("stock action unavailable".into());
        }
        let mut hash = Sha256::new();
        hash.update(state_hash);
        hash.update(serde_json::to_vec(intent).map_err(|e| e.to_string())?);
        hash.update([match kind {
            Kind::Secondary => 0,
            Kind::Rfq => 1,
            Kind::Primary => 2,
        }]);
        Ok(hash.finalize().into())
    }
}

/// A discrete RFQ ticket is not an arbitrarily divisible AMM curve. A quote
/// backend must separately verify the maker signature, funds and execution ABI.
#[derive(Clone, Serialize, Deserialize)]
pub struct TicketCapacity {
    pub minimum_input: u64,
    pub maximum_input: u64,
    pub lot: u64,
    pub partial_fill: bool,
    pub expires_slot: u64,
}
impl TicketCapacity {
    pub fn check(&self, input: u64, slot: u64) -> Result<()> {
        if self.minimum_input == 0
            || self.maximum_input < self.minimum_input
            || self.lot == 0
            || input < self.minimum_input
            || input > self.maximum_input
            || !input.is_multiple_of(self.lot)
            || (!self.partial_fill && input != self.maximum_input)
            || slot > self.expires_slot
        {
            return Err("RFQ quantity/expiry constraint".into());
        }
        Ok(())
    }
}
