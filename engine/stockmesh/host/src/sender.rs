//! Exact approved wire, no signing or blockhash replacement. Bounded group commit
//! amortizes status lookups and fsync; uncertain results retain reservations.
use crate::{
    journal::{Entry, Journal, Phase},
    rpc::Rpc,
    Result,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use bincode::Options;
use ed25519_dalek::{Signature, VerifyingKey};
use serde_json::json;
use sha2::{Digest, Sha256};
use solana_message::VersionedMessage;

pub struct Authorization {
    pub intent_id: String,
    pub message_hash: [u8; 32],
    pub last_valid_height: u64,
    pub resources: Vec<[u8; 32]>,
}
pub fn authorize(wire: &[u8], a: Authorization) -> Result<Entry> {
    if wire.len() > 1232
        || wire.len() < 65
        || a.intent_id.is_empty()
        || a.intent_id.len() > 128
        || a.resources.len() > 64
        || a.last_valid_height == 0
    {
        return Err("authorization bounds".into());
    }
    let n = wire[0] as usize;
    if !(1..=4).contains(&n) || wire.len() < 1 + n * 64 {
        return Err("signature count".into());
    }
    let message = &wire[1 + n * 64..];
    if <[u8; 32]>::from(Sha256::digest(message)) != a.message_hash {
        return Err("message differs from approved plan".into());
    }
    let msg: VersionedMessage = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(1232)
        .reject_trailing_bytes()
        .deserialize(message)
        .map_err(|e| e.to_string())?;
    if msg.serialize() != message
        || msg.header().num_required_signatures as usize != n
        || msg.static_account_keys().len() < n
    {
        return Err("noncanonical or invalid message".into());
    }
    for i in 0..n {
        let key = VerifyingKey::from_bytes(&msg.static_account_keys()[i].to_bytes())
            .map_err(|e| e.to_string())?;
        let sig = Signature::from_slice(&wire[1 + i * 64..1 + (i + 1) * 64])
            .map_err(|e| e.to_string())?;
        key.verify_strict(message, &sig)
            .map_err(|_| "invalid transaction signature")?;
    }
    Ok(Entry {
        id: a.intent_id,
        quote_id: None,
        signature: bs58::encode(&wire[1..65]).into_string(),
        wire: wire.to_vec(),
        message_hash: a.message_hash,
        last_valid_height: a.last_valid_height,
        resources: a.resources,
        expected_exposure: None,
        expected_swap: None,
        expected_basket: None,
        phase: Phase::Prepared,
        attempts: 0,
    })
}
pub struct Sender {
    pub journal: Journal,
    pub rpc: Rpc,
    pub max_attempts: u32,
}
impl Sender {
    pub fn reconcile_basket(&mut self,id:&str)->Result<serde_json::Value>{
        let entry=self.journal.get(id).ok_or("unknown intent")?.clone();
        if !matches!(entry.phase,Phase::Finalized|Phase::Reconciled)||entry.expected_swap.is_some()||entry.expected_exposure.is_some(){return Err("basket receipt not finalized or ambiguous".into());}
        let expected=entry.expected_basket.as_ref().ok_or("basket expectation missing")?;
        let receipt=expected.fetch(&self.rpc,&entry)?;
        if entry.phase!=Phase::Reconciled{self.journal.update(id,Phase::Reconciled,false)?;}
        Ok(receipt)
    }
    pub fn step(&mut self, id: &str) -> Result<Phase> {
        Ok(self.step_batch(&[id])?.remove(0))
    }

    /// Convert a finalized exact wire into its economic receipt and only then
    /// release its durable resource reservation. The expectation was produced
    /// by the trusted compiler before the wallet signed; it is never accepted
    /// from a submit request.
    pub fn reconcile_exposure(&mut self, id: &str) -> Result<serde_json::Value> {
        let entry = self.journal.get(id).ok_or("unknown intent")?.clone();
        if entry.phase == Phase::Reconciled {
            return entry
                .expected_exposure
                .as_ref()
                .ok_or_else(|| "reconciled intent lacks economic expectation".to_string())?
                .fetch(&self.rpc, &entry)
                .map(|receipt| receipt.summary());
        }
        if entry.phase != Phase::Finalized {
            return Err("economic receipt not finalized".into());
        }
        let expected = entry
            .expected_exposure
            .clone()
            .ok_or("finalized intent lacks economic expectation")?;
        let receipt = expected.fetch(&self.rpc, &entry)?.summary();
        self.journal.update(id, Phase::Reconciled, false)?;
        Ok(receipt)
    }

    pub fn reconcile_swap(&mut self, id: &str) -> Result<serde_json::Value> {
        let entry = self.journal.get(id).ok_or("unknown intent")?.clone();
        if !matches!(entry.phase, Phase::Finalized | Phase::Reconciled) {
            return Err("swap receipt not finalized".into());
        }
        let expected = entry
            .expected_swap
            .clone()
            .ok_or("finalized intent lacks swap expectation")?;
        let receipt = crate::receipt::fetch(&self.rpc, &entry, &expected)?.summary();
        if entry.phase != Phase::Reconciled {
            self.journal.update(id, Phase::Reconciled, false)?;
        }
        Ok(receipt)
    }
    /// At most 64 intents, one status lookup and at most four simultaneous sends.
    /// Group commit changes durability scheduling, never transaction atomicity.
    pub fn step_batch(&mut self, ids: &[&str]) -> Result<Vec<Phase>> {
        self.progress_batch(ids, true)
    }

    /// Observation never transmits a transaction or consumes a send attempt.
    /// In particular, an absent expired signature remains UNKNOWN, not FAILED.
    pub fn observe_batch(&mut self, ids: &[&str]) -> Result<Vec<Phase>> {
        self.progress_batch(ids, false)
    }

    fn progress_batch(&mut self, ids: &[&str], allow_send: bool) -> Result<Vec<Phase>> {
        if ids.is_empty() || ids.len() > 64 {
            return Err("sender batch bounds".into());
        }
        let mut unique = std::collections::BTreeSet::new();
        let mut entries = Vec::with_capacity(ids.len());
        for id in ids {
            if !unique.insert(id) {
                return Err("duplicate sender intent".into());
            }
            entries.push(self.journal.get(id).ok_or("unknown intent")?.clone());
        }
        let mut phases: Vec<_> = entries.iter().map(|e| e.phase.clone()).collect();
        let active: Vec<_> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                !matches!(
                    e.phase,
                    Phase::Finalized | Phase::Failed | Phase::Reconciled
                )
            })
            .map(|(i, _)| i)
            .collect();
        if active.is_empty() {
            return Ok(phases);
        }
        for &i in &active {
            let e = &entries[i];
            let checked = authorize(
                &e.wire,
                Authorization {
                    intent_id: e.id.clone(),
                    message_hash: e.message_hash,
                    last_valid_height: e.last_valid_height,
                    resources: e.resources.clone(),
                },
            )?;
            if checked.signature != e.signature {
                return Err("persisted signature/wire mismatch".into());
            }
        }
        self.rpc.check_genesis()?;
        let signatures: Vec<_> = active.iter().map(|i| &entries[*i].signature).collect();
        let response = self.rpc.call(
            "getSignatureStatuses",
            json!([signatures,{"searchTransactionHistory":true}]),
        )?;
        let statuses = response["value"].as_array().ok_or("missing status array")?;
        if statuses.len() != active.len() {
            return Err("incomplete status array".into());
        }
        let mut updates = Vec::new();
        let mut absent = Vec::new();
        for (&i, status) in active.iter().zip(statuses) {
            if status.is_null() {
                absent.push(i);
                continue;
            }
            let confirmation = status["confirmationStatus"]
                .as_str()
                .ok_or("missing confirmation status")?;
            let err = status.get("err").ok_or("missing execution status")?;
            let phase = match confirmation {
                "finalized" => {
                    if err.is_null() {
                        Phase::Finalized
                    } else {
                        Phase::Failed
                    }
                }
                "processed" | "confirmed" => Phase::Submitted,
                _ => return Err("invalid confirmation status".into()),
            };
            if phase != entries[i].phase {
                updates.push((ids[i], phase.clone(), false));
            }
            phases[i] = phase;
        }
        let mut sends = Vec::new();
        if !absent.is_empty() {
            let height = if allow_send { self
                .rpc
                .call("getBlockHeight", json!([{"commitment":"finalized"}]))?
                .as_u64()
                .ok_or("missing block height")? } else { u64::MAX };
            for i in absent {
                let attempt = allow_send && height <= entries[i].last_valid_height
                    && entries[i].attempts < self.max_attempts;
                if attempt || entries[i].phase != Phase::Unknown {
                    updates.push((ids[i], Phase::Unknown, attempt));
                }
                phases[i] = Phase::Unknown;
                if attempt {
                    sends.push(i);
                }
            }
        }
        // All send attempts become durable uncertainty before any RPC transmission.
        if !updates.is_empty() {
            self.journal.update_batch(&updates)?;
        }
        let mut acknowledged = Vec::new();
        for chunk in sends.chunks(4) {
            let results = std::thread::scope(|scope| {
                let rpc = &self.rpc;
                let handles:Vec<_>=chunk.iter().map(|&i| {
                    let e=&entries[i];
                    (i,scope.spawn(move || {
                        let sent=rpc.call("sendTransaction",json!([STANDARD.encode(&e.wire),{"encoding":"base64","skipPreflight":false,"preflightCommitment":"confirmed","maxRetries":0}]));
                        sent.as_ref().ok().and_then(|v|v.as_str())==Some(e.signature.as_str())
                    }))
                }).collect();
                handles
                    .into_iter()
                    .map(|(i, h)| (i, h.join().unwrap_or(false)))
                    .collect::<Vec<_>>()
            });
            for (i, ok) in results {
                if ok {
                    acknowledged.push((ids[i], Phase::Submitted, false));
                    phases[i] = Phase::Submitted;
                }
            }
        }
        if !acknowledged.is_empty() {
            self.journal.update_batch(&acknowledged)?;
        }
        Ok(phases)
    }
}
