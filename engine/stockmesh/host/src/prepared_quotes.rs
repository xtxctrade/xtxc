//! Bounded handoff from verified executor candidates to the existing swap API.
//! Public callers provide quote ID and owner only. They cannot install a wire,
//! policy, balance expectation, simulation flag or executor score in this store.
use crate::{
    exposure_pipeline::PreparedExposure,
    feed::{Feed, Snapshot},
    journal::Entry,
    mesh::{ExecutorBid, FrozenIntent},
    Result,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Arc, time::Instant};

#[derive(Clone)]
pub struct Candidate {
    pub intent: FrozenIntent,
    pub feed: Arc<Feed>,
    pub snapshot: Arc<Snapshot>,
    pub prepared: PreparedExposure,
    pub bid: ExecutorBid,
    pub last_valid_block_height: u64,
}

struct Stored {
    candidate: Candidate,
    expires: Instant,
    expires_at: String,
}

#[derive(Default)]
pub struct PreparedQuotes {
    entries: BTreeMap<(String, String), Stored>,
}

impl PreparedQuotes {
    pub fn admit(
        &mut self,
        quote_id: &str,
        candidate: Candidate,
        expires: Instant,
        expires_at: String,
    ) -> Result<()> {
        if !valid_quote(quote_id) || expires <= Instant::now() || expires_at.is_empty() {
            return Err("prepared quote expiry/identity".into());
        }
        candidate.prepared.validate_state(
            &candidate.feed,
            &candidate.snapshot,
            &candidate.intent,
        )?;
        candidate.prepared.validate_bid(
            &candidate.bid,
            &candidate.intent,
            candidate.snapshot.slot,
        )?;
        if candidate.last_valid_block_height == 0 {
            return Err("prepared candidate block height".into());
        }
        self.entries.retain(|_, row| row.expires > Instant::now());
        let key = (quote_id.to_string(), candidate.intent.owner.clone());
        if let Some(old) = self.entries.get(&key) {
            let a = &old.candidate;
            // Selection never compares bids for different signed intentions.
            if a.intent.commitment(candidate.snapshot.slot)?
                != candidate.intent.commitment(candidate.snapshot.slot)?
            {
                return Err("prepared candidate intent mismatch; refresh quote".into());
            }
            if candidate.bid.executor == a.bid.executor && candidate.bid.sequence <= a.bid.sequence
            {
                return Err("prepared executor replay/equivocation".into());
            }
            if a.prepared
                .validate_state(&a.feed, &a.snapshot, &a.intent)
                .is_ok()
            {
                let bids = [a.bid.clone(), candidate.bid.clone()];
                let best = crate::mesh::select(&candidate.intent, &bids, candidate.snapshot.slot)?;
                if best != &bids[1] {
                    return Err("prepared candidate does not improve admitted outcome".into());
                }
            }
        } else if self.entries.len() >= 256 {
            return Err("prepared quote capacity".into());
        }
        self.entries.insert(
            key,
            Stored {
                candidate,
                expires,
                expires_at,
            },
        );
        Ok(())
    }

    pub fn review(
        &self,
        quote_id: &str,
        owner: &str,
        market_hash: [u8; 32],
        market_slot: u64,
    ) -> Result<Option<Value>> {
        let Some(row) = self.checked(quote_id, owner, market_hash, market_slot)? else {
            return Ok(None);
        };
        let c = &row.candidate;
        let prepared_id = prepared_id(quote_id, c)?;
        Ok(Some(json!({
            "schema":"skew.stocklana.prepared/v1", "preparedId":prepared_id,
            "quoteId":quote_id,"owner":owner,"expiresAt":row.expires_at,
            "estimatedCu":c.prepared.summary()["simulationCU"],
            "lastValidBlockHeight":c.last_valid_block_height.to_string(),
            "transactionBase64":STANDARD.encode(c.prepared.unsigned_wire()),
            "submitAllowed":false,"economicExecution":c.prepared.summary(),
            "executor":c.bid.executor,"guaranteedExposureQ32":c.bid.guaranteed_exposure_q32.to_string()
        })))
    }

    /// Bind wallet signatures to the exact cached candidate selected by the
    /// trusted executor. A submit request supplies only identity and signed
    /// bytes; it cannot supply policy, resources, receipt expectations or a
    /// different message.
    pub fn authorize_submission(
        &self,
        quote_id: &str,
        owner: &str,
        supplied_prepared_id: &str,
        market_hash: [u8; 32],
        market_slot: u64,
        signed_wire: &[u8],
    ) -> Result<Entry> {
        let row = self
            .checked(quote_id, owner, market_hash, market_slot)?
            .ok_or("prepared quote not found")?;
        let candidate = &row.candidate;
        let expected_id = prepared_id(quote_id, candidate)?;
        if supplied_prepared_id != expected_id {
            return Err("prepared identity differs from cached candidate".into());
        }
        candidate.prepared.authorize(
            &candidate.feed,
            &candidate.snapshot,
            &candidate.intent,
            signed_wire,
            expected_id,
            candidate.last_valid_block_height,
        )
    }

    fn checked(
        &self,
        quote_id: &str,
        owner: &str,
        market_hash: [u8; 32],
        market_slot: u64,
    ) -> Result<Option<&Stored>> {
        let Some(row) = self.entries.get(&(quote_id.into(), owner.into())) else {
            return Ok(None);
        };
        if row.expires <= Instant::now() {
            return Err("prepared quote expired".into());
        }
        let c = &row.candidate;
        if c.intent.world_generation_hash != market_hash
            || market_slot < c.snapshot.slot
            || market_slot > c.intent.deadline_slot
        {
            return Err("prepared market generation changed".into());
        }
        c.prepared.validate_state(&c.feed, &c.snapshot, &c.intent)?;
        c.prepared.validate_bid(&c.bid, &c.intent, market_slot)?;
        Ok(Some(row))
    }
}

fn prepared_id(quote_id: &str, candidate: &Candidate) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(b"SKEW_PREPARED_QUOTE_V1\0");
    hash.update(quote_id.as_bytes());
    hash.update(candidate.intent.commitment(candidate.snapshot.slot)?);
    hash.update(candidate.prepared.unsigned_wire());
    let digest = hash.finalize();
    Ok(format!(
        "stkp_{}",
        digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn valid_quote(id: &str) -> bool {
    id.len() == 37
        && id.starts_with("stkq_")
        && id[5..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
