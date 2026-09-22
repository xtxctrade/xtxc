//! StockMesh identity and executor-bid boundary.
//!
//! A ticker, issuer product, mint, executable market and executor are distinct
//! identities. Executors may race to prepare a transaction, but they cannot
//! change the frozen economic intent or claim a different state generation.
use crate::Result;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

const PRODUCT_DOMAIN: &[u8] = b"SKEW_STOCKMESH_PRODUCT_V1\0";
const INTENT_DOMAIN: &[u8] = b"SKEW_STOCKMESH_INTENT_V1\0";
const BID_DOMAIN: &[u8] = b"SKEW_STOCKMESH_EXECUTOR_BID_V1\0";
const MAX_PRODUCTS: usize = 8;
const MAX_INPUT_ATOMS: u64 = 10_000_000_000_000_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProductIdentity {
    pub instrument: String,
    pub issuer: String,
    pub mint: String,
    pub token_program: String,
    pub rights_hash: [u8; 32],
    pub raw_decimals: u8,
}

impl ProductIdentity {
    pub fn id(&self) -> Result<[u8; 32]> {
        self.validate()?;
        let mut bytes = PRODUCT_DOMAIN.to_vec();
        field(&mut bytes, self.instrument.as_bytes())?;
        field(&mut bytes, self.issuer.as_bytes())?;
        bytes.extend_from_slice(&key(&self.mint)?);
        bytes.extend_from_slice(&key(&self.token_program)?);
        bytes.extend_from_slice(&self.rights_hash);
        bytes.push(self.raw_decimals);
        Ok(Sha256::digest(bytes).into())
    }

    fn validate(&self) -> Result<()> {
        if self.instrument.is_empty()
            || self.instrument.len() > 32
            || !self
                .instrument
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'.')
            || self.issuer.is_empty()
            || self.issuer.len() > 64
            || self.issuer.trim() != self.issuer
            || !self
                .issuer
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
            || self.rights_hash == [0; 32]
            || self.raw_decimals > 12
            || key(&self.mint)? == key(&self.token_program)?
        {
            return Err("invalid StockMesh product identity".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FrozenIntent {
    pub intent_id: [u8; 32],
    pub owner: String,
    pub owner_nonce: u64,
    pub instrument: String,
    pub input_mint: String,
    pub input_atoms: u64,
    /// Conservative underlying-share exposure in Q32 units.
    pub minimum_exposure_q32: u128,
    pub admitted_product_ids: Vec<[u8; 32]>,
    pub product_policy_hash: [u8; 32],
    pub world_generation_hash: [u8; 32],
    pub deadline_slot: u64,
}

impl FrozenIntent {
    pub fn commitment(&self, current_slot: u64) -> Result<[u8; 32]> {
        if self.intent_id == [0; 32]
            || self.owner_nonce == u64::MAX
            || self.instrument.is_empty()
            || self.instrument.len() > 32
            || self.input_atoms == 0
            || self.input_atoms > MAX_INPUT_ATOMS
            || self.minimum_exposure_q32 == 0
            || self.admitted_product_ids.is_empty()
            || self.admitted_product_ids.len() > MAX_PRODUCTS
            || self.product_policy_hash == [0; 32]
            || self.world_generation_hash == [0; 32]
            || self.deadline_slot < current_slot
            || key(&self.owner)? == key(&self.input_mint)?
        {
            return Err("invalid frozen StockMesh intent".into());
        }
        let mut products = BTreeSet::new();
        if self
            .admitted_product_ids
            .iter()
            .any(|id| *id == [0; 32] || !products.insert(*id))
        {
            return Err("duplicate or empty admitted product".into());
        }
        let mut bytes = INTENT_DOMAIN.to_vec();
        bytes.extend_from_slice(&self.intent_id);
        bytes.extend_from_slice(&key(&self.owner)?);
        bytes.extend_from_slice(&self.owner_nonce.to_le_bytes());
        field(&mut bytes, self.instrument.as_bytes())?;
        bytes.extend_from_slice(&key(&self.input_mint)?);
        bytes.extend_from_slice(&self.input_atoms.to_le_bytes());
        bytes.extend_from_slice(&self.minimum_exposure_q32.to_le_bytes());
        bytes.push(self.admitted_product_ids.len() as u8);
        // Preserve the owner's signed order. Product IDs are required to be
        // unique but are not resorted behind the signer's back.
        for id in &self.admitted_product_ids {
            bytes.extend_from_slice(id);
        }
        bytes.extend_from_slice(&self.product_policy_hash);
        bytes.extend_from_slice(&self.world_generation_hash);
        bytes.extend_from_slice(&self.deadline_slot.to_le_bytes());
        Ok(Sha256::digest(bytes).into())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutorBid {
    pub executor: String,
    pub sequence: u64,
    pub intent_commitment: [u8; 32],
    pub world_generation_hash: [u8; 32],
    pub transaction_message_hash: [u8; 32],
    pub guaranteed_exposure_q32: u128,
    pub executor_fee_input_atoms: u64,
    pub predicted_compute_units: u32,
    pub expires_slot: u64,
    pub signature: String,
}

impl ExecutorBid {
    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        if self.sequence == 0
            || self.intent_commitment == [0; 32]
            || self.world_generation_hash == [0; 32]
            || self.transaction_message_hash == [0; 32]
            || self.guaranteed_exposure_q32 == 0
            || self.executor_fee_input_atoms > MAX_INPUT_ATOMS
            || self.predicted_compute_units == 0
            || self.predicted_compute_units > 1_400_000
        {
            return Err("invalid executor bid".into());
        }
        let mut bytes = BID_DOMAIN.to_vec();
        bytes.extend_from_slice(&key(&self.executor)?);
        bytes.extend_from_slice(&self.sequence.to_le_bytes());
        bytes.extend_from_slice(&self.intent_commitment);
        bytes.extend_from_slice(&self.world_generation_hash);
        bytes.extend_from_slice(&self.transaction_message_hash);
        bytes.extend_from_slice(&self.guaranteed_exposure_q32.to_le_bytes());
        bytes.extend_from_slice(&self.executor_fee_input_atoms.to_le_bytes());
        bytes.extend_from_slice(&self.predicted_compute_units.to_le_bytes());
        bytes.extend_from_slice(&self.expires_slot.to_le_bytes());
        Ok(bytes)
    }

    pub fn verify(&self, intent: &FrozenIntent, current_slot: u64) -> Result<[u8; 32]> {
        let intent_commitment = intent.commitment(current_slot)?;
        if self.intent_commitment != intent_commitment
            || self.world_generation_hash != intent.world_generation_hash
            || self.expires_slot < current_slot
            || self.expires_slot > intent.deadline_slot
            || self.guaranteed_exposure_q32 < intent.minimum_exposure_q32
        {
            return Err("executor bid does not bind the frozen intent".into());
        }
        let bytes = self.signing_bytes()?;
        let public = VerifyingKey::from_bytes(&key(&self.executor)?)
            .map_err(|_| "invalid executor public key")?;
        let signature = bs58::decode(&self.signature)
            .into_vec()
            .map_err(|_| "invalid executor signature encoding")?;
        public
            .verify_strict(
                &bytes,
                &Signature::from_slice(&signature)
                    .map_err(|_| "invalid executor signature length")?,
            )
            .map_err(|_| "invalid executor bid signature")?;
        Ok(Sha256::digest(bytes).into())
    }
}

/// Pick the best valid prepared outcome deterministically. Guaranteed exposure
/// dominates; fees, compute and executor key are tie breakers. This comparison
/// does not authorize the message: exact simulation and wallet review still do.
pub fn select<'a>(
    intent: &FrozenIntent,
    bids: &'a [ExecutorBid],
    current_slot: u64,
) -> Result<&'a ExecutorBid> {
    if bids.is_empty() || bids.len() > 32 {
        return Err("executor bid count".into());
    }
    let mut identities = BTreeSet::new();
    let mut best: Option<(&ExecutorBid, [u8; 32])> = None;
    for bid in bids {
        bid.verify(intent, current_slot)?;
        let executor = key(&bid.executor)?;
        if !identities.insert((executor, bid.sequence)) {
            return Err("duplicate executor sequence".into());
        }
        if best.is_none_or(|(winner, winner_executor)| {
            (
                bid.guaranteed_exposure_q32,
                std::cmp::Reverse(bid.executor_fee_input_atoms),
                std::cmp::Reverse(bid.predicted_compute_units),
                std::cmp::Reverse(executor),
            ) > (
                winner.guaranteed_exposure_q32,
                std::cmp::Reverse(winner.executor_fee_input_atoms),
                std::cmp::Reverse(winner.predicted_compute_units),
                std::cmp::Reverse(winner_executor),
            )
        }) {
            best = Some((bid, executor));
        }
    }
    best.map(|(bid, _)| bid)
        .ok_or_else(|| "no valid executor bid".into())
}

fn key(value: &str) -> Result<[u8; 32]> {
    bs58::decode(value)
        .into_vec()
        .map_err(|_| "invalid base58 identity")?
        .try_into()
        .map_err(|_| "identity must be 32 bytes".into())
}

fn field(bytes: &mut Vec<u8>, value: &[u8]) -> Result<()> {
    let len = u8::try_from(value.len()).map_err(|_| "identity field too long")?;
    bytes.push(len);
    bytes.extend_from_slice(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn intent(owner: &SigningKey) -> FrozenIntent {
        FrozenIntent {
            intent_id: [1; 32],
            owner: bs58::encode(owner.verifying_key().to_bytes()).into_string(),
            owner_nonce: 7,
            instrument: "NVDA".into(),
            input_mint: bs58::encode([9; 32]).into_string(),
            input_atoms: 1_000_000_000,
            minimum_exposure_q32: 20 * (1u128 << 32),
            admitted_product_ids: vec![[3; 32], [4; 32]],
            product_policy_hash: [5; 32],
            world_generation_hash: [6; 32],
            deadline_slot: 120,
        }
    }

    fn bid(executor: &SigningKey, intent: &FrozenIntent, exposure: u128, fee: u64) -> ExecutorBid {
        let mut bid = ExecutorBid {
            executor: bs58::encode(executor.verifying_key().to_bytes()).into_string(),
            sequence: 1,
            intent_commitment: intent.commitment(100).unwrap(),
            world_generation_hash: intent.world_generation_hash,
            transaction_message_hash: [8; 32],
            guaranteed_exposure_q32: exposure,
            executor_fee_input_atoms: fee,
            predicted_compute_units: 800_000,
            expires_slot: 110,
            signature: String::new(),
        };
        bid.signature =
            bs58::encode(executor.sign(&bid.signing_bytes().unwrap()).to_bytes()).into_string();
        bid
    }

    #[test]
    fn issuer_product_identity_changes_with_rights() {
        let mut product = ProductIdentity {
            instrument: "NVDA".into(),
            issuer: "ISSUER-A".into(),
            mint: bs58::encode([1; 32]).into_string(),
            token_program: bs58::encode([2; 32]).into_string(),
            rights_hash: [3; 32],
            raw_decimals: 8,
        };
        let first = product.id().unwrap();
        product.rights_hash = [4; 32];
        assert_ne!(first, product.id().unwrap());
    }

    #[test]
    fn product_identity_matches_manifest_builder_encoding() {
        let product = ProductIdentity {
            instrument: "NVDA".into(),
            issuer: "Backed Assets (JE) Limited".into(),
            mint: "Xsc9qvGR1efVDFGLrVsmkzv3qi45LTBjeUKSPmx9qEh".into(),
            token_program: "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".into(),
            rights_hash: [3; 32],
            raw_decimals: 8,
        };
        let id = product.id().unwrap();
        let prefix = id[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(prefix, "caa557997fadc8be43d41082e65318fa");
    }

    #[test]
    fn signed_executor_competition_is_intent_and_generation_bound() {
        let owner = SigningKey::from_bytes(&[11; 32]);
        let intent = intent(&owner);
        let first = SigningKey::from_bytes(&[12; 32]);
        let second = SigningKey::from_bytes(&[13; 32]);
        let bids = [
            bid(&first, &intent, 21 * (1u128 << 32), 10),
            bid(&second, &intent, 22 * (1u128 << 32), 20),
        ];
        assert_eq!(
            select(&intent, &bids, 100).unwrap().executor,
            bids[1].executor
        );
        let mut replay = bids[1].clone();
        replay.world_generation_hash = [7; 32];
        assert!(replay.verify(&intent, 100).is_err());
        let mut tampered = bids[1].clone();
        tampered.transaction_message_hash = [9; 32];
        assert!(tampered.verify(&intent, 100).is_err());
    }
}
