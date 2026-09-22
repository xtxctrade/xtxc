//! One response publishes one immutable bank view. No slot-stitching across calls.
use crate::{rpc::Rpc, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};
#[derive(Clone, Debug)]
pub struct Account {
    pub key: String,
    pub owner: String,
    pub executable: bool,
    pub lamports: u64,
    pub data: Vec<u8>,
}
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub slot: u64,
    pub generation: u64,
    pub hash: [u8; 32],
    pub accounts: Vec<Account>,
    pub observed: Instant,
    /// Repeated replies from a frozen RPC bank must not reset freshness.
    pub slot_advanced: Instant,
    pub revision: u64,
}
impl Snapshot {
    /// Project a market view from one complete execution bank. This never
    /// fetches, stitches slots or refreshes the original freshness timestamps.
    /// The key order is the trusted market feed order, not executor input.
    pub fn project(&self, keys: &[String]) -> Result<Self> {
        if keys.is_empty() || keys.len() > 100 {
            return Err("snapshot projection bounds".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut accounts = Vec::with_capacity(keys.len());
        let mut hash = Sha256::new();
        for key in keys {
            if !seen.insert(key) {
                return Err("duplicate projection key".into());
            }
            let mut rows = self.accounts.iter().filter(|row| &row.key == key);
            let row = rows.next().ok_or("projection account missing")?;
            if rows.next().is_some() {
                return Err("ambiguous projection account".into());
            }
            hash_account(&mut hash, row)?;
            accounts.push(row.clone());
        }
        Ok(Self {
            slot: self.slot,
            generation: self.generation,
            hash: hash.finalize().into(),
            accounts,
            observed: self.observed,
            slot_advanced: self.slot_advanced,
            revision: self.revision,
        })
    }
}

fn hash_account(hash: &mut Sha256, row: &Account) -> Result<()> {
    for key in [&row.key, &row.owner] {
        let bytes = bs58::decode(key)
            .into_vec()
            .map_err(|_| "snapshot public key")?;
        if bytes.len() != 32 {
            return Err("snapshot public key length".into());
        }
        hash.update(bytes);
    }
    hash.update(row.lamports.to_le_bytes());
    hash.update([u8::from(row.executable)]);
    hash.update((row.data.len() as u64).to_le_bytes());
    hash.update(&row.data);
    Ok(())
}
pub struct Feed {
    keys: Vec<String>,
    explicit_absence: std::collections::BTreeSet<String>,
    current: RwLock<Option<Arc<Snapshot>>>,
    max_age: Duration,
    max_bytes: usize,
    healthy: AtomicBool,
    revision: AtomicU64,
}
impl Feed {
    pub fn new(keys: Vec<String>, max_age: Duration, max_bytes: usize) -> Result<Self> {
        Self::new_execution_bank(keys, std::collections::BTreeSet::new(), max_age, max_bytes)
    }

    /// A wallet compiler may predeclare canonical ATA/nonce addresses that are
    /// allowed to be absent in a successful complete RPC response. Transport
    /// failure, an omitted response row, or absence of any other key still
    /// fails closed. Normal market feeds always use `new` and admit no absence.
    pub fn new_execution_bank(
        keys: Vec<String>,
        explicit_absence: std::collections::BTreeSet<String>,
        max_age: Duration,
        max_bytes: usize,
    ) -> Result<Self> {
        if keys.is_empty()
            || keys.len() > 100
            || max_age.is_zero()
            || max_bytes > 4 * 1024 * 1024
            || max_bytes == 0
        {
            return Err("feed bounds".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for key in &keys {
            if bs58::decode(key)
                .into_vec()
                .map_err(|e| e.to_string())?
                .len()
                != 32
                || !seen.insert(key)
            {
                return Err("invalid/duplicate account key".into());
            }
        }
        if !explicit_absence.iter().all(|key| seen.contains(key)) {
            return Err("explicit absence outside execution bank".into());
        }
        Ok(Self {
            keys,
            explicit_absence,
            current: RwLock::new(None),
            max_age,
            max_bytes,
            healthy: AtomicBool::new(false),
            revision: AtomicU64::new(0),
        })
    }
    pub fn refresh(&self, rpc: &Rpc) -> Result<Arc<Snapshot>> {
        let previous = self.current.read().map_err(|_| "feed poisoned")?.clone();
        let min = previous.as_ref().map_or(0, |s| s.slot);
        let response = match rpc.call(
            "getMultipleAccounts",
            json!([self.keys,{"encoding":"base64","commitment":"confirmed","minContextSlot":min}]),
        ) {
            Ok(x) => x,
            Err(e) => {
                self.invalidate()?;
                return Err(e);
            }
        };
        self.publish(&response)
    }
    pub(crate) fn keys(&self) -> &[String] {
        &self.keys
    }
    pub fn publish(&self, response: &serde_json::Value) -> Result<Arc<Snapshot>> {
        let next = match self.prepare(response) {
            Ok(next) => next,
            Err(error) => {
                self.invalidate()?;
                return Err(error);
            }
        };
        if let Err(error) = self.install_prepared(next.clone()) {
            self.invalidate()?;
            return Err(error);
        }
        Ok(next)
    }
    /// Decode and validate the next snapshot without disturbing the currently
    /// readable one. Bank publication compiles every dependent World from this
    /// value before committing it under its own short write barrier.
    pub(crate) fn prepare(&self, response: &serde_json::Value) -> Result<Arc<Snapshot>> {
        let previous = self.current.read().map_err(|_| "feed poisoned")?.clone();
        let revision = self
            .revision
            .load(Ordering::Acquire)
            .checked_add(1)
            .ok_or("feed revision exhausted")?;
        let slot = response["context"]["slot"].as_u64().ok_or("missing slot")?;
        let values = response["value"]
            .as_array()
            .ok_or("missing account values")?;
        if values.len() != self.keys.len() {
            return Err("incomplete snapshot".into());
        }
        let mut accounts = Vec::with_capacity(values.len());
        let mut total = 0;
        let mut hasher = Sha256::new();
        for (k, v) in self.keys.iter().zip(values) {
            if v.is_null() {
                if !self.explicit_absence.contains(k) {
                    return Err("required account missing/deleted".into());
                }
                let account = Account {
                    key: k.clone(),
                    owner: "11111111111111111111111111111111".into(),
                    executable: false,
                    lamports: 0,
                    data: Vec::new(),
                };
                hash_account(&mut hasher, &account)?;
                accounts.push(account);
                continue;
            }
            if v["data"][1].as_str() != Some("base64") {
                return Err("snapshot account encoding".into());
            }
            let data = STANDARD
                .decode(v["data"][0].as_str().ok_or("missing base64")?)
                .map_err(|e| e.to_string())?;
            total += data.len();
            if total > self.max_bytes {
                return Err("snapshot byte budget".into());
            }
            let owner = v["owner"].as_str().ok_or("owner")?.to_string();
            if bs58::decode(&owner)
                .into_vec()
                .map_err(|e| e.to_string())?
                .len()
                != 32
            {
                return Err("bad owner".into());
            }
            let lamports = v["lamports"].as_u64().ok_or("lamports")?;
            let executable = v["executable"].as_bool().ok_or("executable")?;
            let account = Account {
                key: k.clone(),
                owner,
                executable,
                lamports,
                data,
            };
            hash_account(&mut hasher, &account)?;
            accounts.push(account);
        }
        let hash = hasher.finalize().into();
        if previous
            .as_ref()
            .is_some_and(|snapshot| slot < snapshot.slot)
        {
            return Err("out-of-order snapshot".into());
        }
        let generation = previous.as_ref().map_or(Ok(1), |s| {
            if s.hash == hash {
                Ok(s.generation)
            } else {
                s.generation.checked_add(1).ok_or("generation overflow")
            }
        })?;
        let now = Instant::now();
        let slot_advanced = previous
            .as_ref()
            .filter(|s| s.slot == slot)
            .map_or(now, |s| s.slot_advanced);
        Ok(Arc::new(Snapshot {
            slot,
            generation,
            hash,
            accounts,
            observed: now,
            slot_advanced,
            revision,
        }))
    }
    pub(crate) fn install_prepared(&self, next: Arc<Snapshot>) -> Result<()> {
        let expected = next
            .revision
            .checked_sub(1)
            .ok_or("prepared revision underflow")?;
        let mut guard = self.current.write().map_err(|_| "feed poisoned")?;
        self.revision
            .compare_exchange(expected, next.revision, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "publication superseded by invalidation")?;
        *guard = Some(next.clone());
        self.healthy.store(true, Ordering::Release);
        Ok(())
    }
    pub fn read(&self) -> Result<Arc<Snapshot>> {
        if !self.healthy.load(Ordering::Acquire) {
            return Err("feed unavailable".into());
        }
        let value = self
            .current
            .read()
            .map_err(|_| "feed poisoned")?
            .clone()
            .ok_or("feed not ready")?;
        // A repeated response can prove transport liveness but not advancing
        // chain state. Both the observation and its slot must stay inside the
        // same strict freshness horizon.
        if value.observed.elapsed() > self.max_age || value.slot_advanced.elapsed() > self.max_age {
            return Err("stale snapshot".into());
        }
        if !self.healthy.load(Ordering::Acquire)
            || self.revision.load(Ordering::Acquire) != value.revision
        {
            return Err("state invalidated during read".into());
        }
        Ok(value)
    }
    /// A disconnect/gap revokes existing work even if recovery returns identical
    /// bytes. A generation/hash check alone cannot detect that interruption.
    pub fn invalidate(&self) -> Result<u64> {
        let _guard = self.current.write().map_err(|_| "feed poisoned")?;
        self.healthy.store(false, Ordering::Release);
        self.revision
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_add(1))
            .map(|v| v + 1)
            .map_err(|_| "feed revision exhausted".into())
    }
    /// Call after expensive planning/simulation and immediately before handing an
    /// unsigned message to the signer. Final on-chain limits still govern execution.
    pub fn validate_fence(&self, observed: &Snapshot) -> Result<()> {
        let now = self.read()?;
        if now.revision != observed.revision
            || now.hash != observed.hash
            || now.slot != observed.slot
        {
            return Err("discard result from superseded state".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(slot: u64, byte: u8) -> serde_json::Value {
        json!({
            "context":{"slot":slot},
            "value":[{
                "data":[STANDARD.encode([byte]),"base64"],
                "executable":false,
                "lamports":1,
                "owner":"11111111111111111111111111111111"
            }]
        })
    }

    #[test]
    fn prepared_snapshot_does_not_interrupt_current_readers() {
        let feed = Feed::new(
            vec!["So11111111111111111111111111111111111111112".into()],
            Duration::from_secs(60),
            1024,
        )
        .unwrap();
        let first = feed.publish(&response(10, 1)).unwrap();
        let next = feed.prepare(&response(11, 2)).unwrap();
        let during_compile = feed.read().unwrap();
        assert_eq!(during_compile.slot, first.slot);
        assert_eq!(during_compile.revision, first.revision);
        feed.install_prepared(next.clone()).unwrap();
        let installed = feed.read().unwrap();
        assert_eq!(installed.slot, 11);
        assert_eq!(installed.revision, next.revision);
        assert_eq!(installed.accounts[0].data, vec![2]);
    }

    #[test]
    fn only_predeclared_wallet_absence_becomes_a_canonical_empty_account() {
        let key = "So11111111111111111111111111111111111111112".to_string();
        let response = json!({"context":{"slot":10},"value":[null]});
        let required = Feed::new(vec![key.clone()], Duration::from_secs(60), 1024).unwrap();
        assert!(required.publish(&response).is_err());
        let optional = Feed::new_execution_bank(
            vec![key.clone()],
            [key.clone()].into_iter().collect(),
            Duration::from_secs(60),
            1024,
        )
        .unwrap();
        let snapshot = optional.publish(&response).unwrap();
        assert_eq!(snapshot.accounts[0].key, key);
        assert_eq!(
            snapshot.accounts[0].owner,
            "11111111111111111111111111111111"
        );
        assert_eq!(snapshot.accounts[0].lamports, 0);
        assert!(!snapshot.accounts[0].executable);
        assert!(snapshot.accounts[0].data.is_empty());
        assert!(Feed::new_execution_bank(
            vec!["So11111111111111111111111111111111111111112".into()],
            ["11111111111111111111111111111111".into()]
                .into_iter()
                .collect(),
            Duration::from_secs(60),
            1024,
        )
        .is_err());
    }
}
