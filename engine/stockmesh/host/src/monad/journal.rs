//! Private, single-writer append journal. A torn final frame may be truncated;
//! a complete corrupt frame fails closed. A successful method return implies
//! the event has been synced to disk.
use super::{orders::{AttemptKind, Order}, receipt::{self, Observation}};
use super::monday_receipts::SubmittedOrder;
use crate::{journal::open_private_regular, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs::File, io::{Read, Seek, SeekFrom, Write}, path::{Path, PathBuf}};

const RECORD_BOUND: usize = 8 * 1024;
const MAX_FILE: u64 = 64 * 1024 * 1024;
const MAX_ORDERS: usize = 65_536;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Frame { seq: u64, order: Order }

pub struct MonadJournal {
    file: File,
    path: PathBuf,
    _lock: File,
    orders: BTreeMap<String, Order>,
    idempotency: BTreeMap<String, String>,
    tx_hashes: BTreeMap<String, String>,
    wallet_nonces: BTreeMap<(String, u64), String>,
    seq: u64,
    bytes: u64,
    poisoned: bool,
}

impl MonadJournal {
    pub fn open(path: &Path) -> Result<Self> {
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock = open_private_regular(&PathBuf::from(lock_path), false)?;
        lock.try_lock_exclusive().map_err(|_| "Monad journal already owned")?;
        let mut file = open_private_regular(path, false)?;
        file.try_lock_exclusive().map_err(|_| "Monad journal already owned")?;
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        File::open(parent).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
        let size = file.metadata().map_err(|e| e.to_string())?.len();
        if size > MAX_FILE { return Err("Monad journal budget exceeded".into()); }
        let mut data = Vec::with_capacity(size as usize);
        file.read_to_end(&mut data).map_err(|e| e.to_string())?;
        let (mut offset, mut seq) = (0usize, 0u64);
        let mut orders: BTreeMap<String, Order> = BTreeMap::new();
        let mut idempotency = BTreeMap::new();
        let mut tx_hashes = BTreeMap::new();
        let mut wallet_nonces = BTreeMap::new();
        while offset + 8 <= data.len() {
            let length = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
            let crc = u32::from_le_bytes(data[offset + 4..offset + 8].try_into().unwrap());
            if length == 0 || length > RECORD_BOUND { return Err("Monad journal frame size".into()); }
            if offset + 8 + length > data.len() { break; }
            let body = &data[offset + 8..offset + 8 + length];
            if crc32fast::hash(body) != crc { return Err("Monad journal checksum corruption".into()); }
            let frame: Frame = serde_json::from_slice(body).map_err(|e| e.to_string())?;
            if frame.seq != seq + 1 { return Err("Monad journal sequence corruption".into()); }
            frame.order.validate()?;
            let order = &frame.order;
            if let Some(existing) = idempotency.insert(order.intent.idempotency_key.clone(), order.intent.order_id.clone()) {
                if existing != order.intent.order_id { return Err("Monad journal idempotency collision".into()); }
            }
            for attempt in order.attempts() {
                if let Some(existing) = tx_hashes.insert(attempt.hash, order.intent.order_id.clone()) {
                    if existing != order.intent.order_id { return Err("Monad transaction assigned twice".into()); }
                }
            }
            if let Some(nonce) = order.tx_nonce {
                let key = (order.intent.owner.to_ascii_lowercase(), nonce);
                if let Some(existing) = wallet_nonces.insert(key, order.intent.order_id.clone()) {
                    if existing != order.intent.order_id {
                        return Err("Monad wallet nonce assigned twice".into());
                    }
                }
            }
            if let Some(existing) = orders.get(&order.intent.order_id) {
                if existing.intent != order.intent || existing.market_binding != order.market_binding {
                    return Err("Monad order identity changed".into());
                }
                if existing.tx_nonce.is_some() && existing.tx_nonce != order.tx_nonce
                    || existing.tx_hash.is_some() && existing.tx_hash != order.tx_hash
                    || existing.finalized_block_hash.is_some()
                        && existing.finalized_block_hash != order.finalized_block_hash
                    || existing.market_issuer_order_id.is_some()
                        && existing.market_issuer_order_id != order.market_issuer_order_id {
                    return Err("Monad order immutable binding changed".into());
                }
                let previous = existing.attempts();
                let next = order.attempts();
                if !next.starts_with(&previous) {
                    return Err("Monad transaction history changed".into());
                }
            }
            orders.insert(order.intent.order_id.clone(), order.clone());
            if orders.len() > MAX_ORDERS { return Err("Monad journal order capacity".into()); }
            seq = frame.seq;
            offset += 8 + length;
        }
        if offset < data.len() {
            file.set_len(offset as u64).map_err(|e| e.to_string())?;
            file.sync_data().map_err(|e| e.to_string())?;
        }
        file.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
        Ok(Self { file, path: path.to_path_buf(), _lock: lock,
            orders, idempotency, tx_hashes, wallet_nonces,
            seq, bytes: offset as u64, poisoned: false })
    }

    pub fn get(&self, order_id: &str, owner: &str) -> Option<&Order> {
        self.orders.get(order_id).filter(|order| order.intent.owner.eq_ignore_ascii_case(owner))
    }
    pub fn list(&self, owner: &str) -> Vec<&Order> {
        self.orders.values().filter(|order| order.intent.owner.eq_ignore_ascii_case(owner)).collect()
    }
    /// Atomic checkpoint of the latest complete order states. The stable
    /// sidecar lock still owns the journal during the inode replacement.
    /// UNKNOWN orders and every competing transaction hash are retained.
    pub fn compact(&mut self) -> Result<u64> {
        if self.poisoned { return Err("Monad journal requires restart after I/O failure".into()); }
        let mut name = self.path.as_os_str().to_os_string();
        name.push(".compact");
        let temporary = PathBuf::from(name);
        let mut file = open_private_regular(&temporary, true)?;
        file.try_lock_exclusive().map_err(|e| e.to_string())?;
        let (mut bytes, mut seq) = (0u64, 0u64);
        for order in self.orders.values() {
            seq = seq.checked_add(1).ok_or("Monad sequence overflow")?;
            let body = serde_json::to_vec(&Frame { seq, order: order.clone() })
                .map_err(|e| e.to_string())?;
            bytes = bytes.checked_add(8 + body.len() as u64)
                .ok_or("Monad journal size overflow")?;
            if body.len() > RECORD_BOUND || bytes > MAX_FILE {
                return Err("Monad compaction budget exceeded".into());
            }
            file.write_all(&(body.len() as u32).to_le_bytes())
                .and_then(|_| file.write_all(&crc32fast::hash(&body).to_le_bytes()))
                .and_then(|_| file.write_all(&body))
                .map_err(|e| e.to_string())?;
        }
        file.sync_all().map_err(|e| e.to_string())?;
        self.poisoned = true;
        std::fs::rename(&temporary, &self.path).map_err(|e| e.to_string())?;
        let parent = self.path.parent().filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
        let saved = self.bytes.saturating_sub(bytes);
        self.file = file;
        self.bytes = bytes;
        self.seq = seq;
        self.poisoned = false;
        Ok(saved)
    }
    pub fn prepare(&mut self, order: Order) -> Result<&Order> {
        order.validate()?;
        if let Some(id) = self.idempotency.get(&order.intent.idempotency_key) {
            let existing = self.orders.get(id).ok_or("Monad journal index corruption")?;
            if existing.intent != order.intent { return Err("idempotency key reused with different intent".into()); }
            return Ok(self.orders.get(id).unwrap());
        }
        if self.orders.contains_key(&order.intent.order_id) || self.orders.len() >= MAX_ORDERS {
            return Err("Monad order identifier or capacity conflict".into());
        }
        let id = order.intent.order_id.clone();
        self.append(&order)?;
        self.idempotency.insert(order.intent.idempotency_key.clone(), id.clone());
        self.orders.insert(id.clone(), order);
        Ok(self.orders.get(&id).unwrap())
    }
    pub fn report_submission(&mut self, id: &str, owner: &str, tx_hash: &str, nonce: u64) -> Result<&Order> {
        let mut next = self.get(id, owner).ok_or("Monad order not found")?.clone();
        let key = (next.intent.owner.to_ascii_lowercase(), nonce);
        if self.wallet_nonces.get(&key).is_some_and(|existing| existing != id) {
            return Err("wallet nonce already reserved by another order".into());
        }
        if let Some(existing) = self.tx_hashes.get(&tx_hash.to_ascii_lowercase()) {
            if existing != id { return Err("transaction already bound to another order".into()); }
        }
        next.report_submission(tx_hash, nonce)?;
        if self.orders.get(id) != Some(&next) {
            self.append(&next)?;
            self.orders.insert(id.into(), next.clone());
            self.tx_hashes.insert(next.tx_hash.clone().unwrap(), id.into());
            self.wallet_nonces.insert(key, id.into());
        }
        Ok(self.orders.get(id).unwrap())
    }
    pub fn report_replacement(&mut self, id: &str, owner: &str, tx_hash: &str,
        nonce: u64, kind: AttemptKind) -> Result<&Order> {
        let hash = tx_hash.to_ascii_lowercase();
        if let Some(existing) = self.tx_hashes.get(&hash) {
            if existing != id { return Err("transaction already bound to another order".into()); }
        }
        let mut next = self.get(id, owner).ok_or("Monad order not found")?.clone();
        next.report_replacement(&hash, nonce, kind)?;
        next.validate()?;
        if self.orders.get(id) != Some(&next) {
            self.append(&next)?;
            self.orders.insert(id.into(), next);
            self.tx_hashes.insert(hash, id.into());
        }
        Ok(self.orders.get(id).unwrap())
    }
    pub fn mark_unknown(&mut self, id: &str, owner: &str) -> Result<&Order> {
        let mut next = self.get(id, owner).ok_or("Monad order not found")?.clone();
        next.mark_unknown()?;
        if self.orders.get(id) != Some(&next) { self.append(&next)?; self.orders.insert(id.into(), next); }
        Ok(self.orders.get(id).unwrap())
    }
    /// Chain-observer-only terminal transition. The inclusion and finality
    /// records are committed in one frame so a crash cannot expose a half
    /// resolved cancellation or reverted wallet transaction.
    pub(crate) fn resolve_attempt(&mut self, id: &str, hash: &str,
        block_hash: &str, success: bool, kind: AttemptKind) -> Result<&Order> {
        let mut next = self.orders.get(id).ok_or("Monad order not found")?.clone();
        if next.market_binding.is_some() && kind == AttemptKind::Execution && success {
            return Err("issuer execution needs an exact submission event".into());
        }
        let attempt = next.attempts().into_iter().find(|attempt| attempt.hash == hash)
            .ok_or("unregistered Monad transaction")?;
        if attempt.kind != kind { return Err("Monad attempt kind changed".into()); }
        if next.finalized_block_hash.as_deref() == Some(block_hash)
            && next.included_tx_hash.as_deref().unwrap_or(next.tx_hash.as_deref().unwrap_or("")) == hash {
            let expected = if !success { super::orders::Phase::Reverted }
                else if kind == AttemptKind::Cancellation { super::orders::Phase::Cancelled }
                else { super::orders::Phase::Finalized };
            return if next.phase == expected { Ok(self.orders.get(id).unwrap()) }
                else { Err("conflicting Monad terminal outcome".into()) };
        }
        if next.included_block_hash.is_none() {
            receipt::apply(&mut next, &Observation::Included { tx_hash: hash.into(),
                block_hash: block_hash.into(), success })?;
        } else if next.included_block_hash.as_deref() != Some(block_hash)
            || next.included_tx_hash.as_deref().unwrap_or(next.tx_hash.as_deref().unwrap_or("")) != hash {
            return Err("conflicting Monad canonical inclusion".into());
        }
        if kind == AttemptKind::Cancellation && success {
            receipt::apply(&mut next, &Observation::Cancelled { tx_hash: hash.into(),
                block_hash: block_hash.into() })?;
            next.failure_code = Some("WALLET_CANCELLED".into());
        } else {
            receipt::apply(&mut next, &Observation::Finalized { tx_hash: hash.into(),
                block_hash: block_hash.into(), success })?;
            if !success { next.failure_code = Some("TRANSACTION_REVERTED".into()); }
        }
        next.validate()?;
        self.append(&next)?;
        self.orders.insert(id.into(), next);
        Ok(self.orders.get(id).unwrap())
    }
    /// Only a canonical chain observer may call this; a browser cannot
    /// invent the issuer order ID or turn submission into stock delivery.
    pub(crate) fn observe_market_submission(&mut self, id: &str,
        submitted: &SubmittedOrder) -> Result<&Order> {
        let mut next = self.orders.get(id).ok_or("Monad order not found")?.clone();
        let binding = next.market_binding.as_ref().ok_or("not a direct market order")?;
        let expected_amount = i128::try_from(binding.request.order_amount_atoms)
            .map_err(|_| "issuer amount overflow")?;
        let (expected_side, expected_amount) = match binding.request.side {
            super::monday_public::MarketSide::Buy => (super::monday_receipts::Side::Buy, expected_amount),
            super::monday_public::MarketSide::Sell => (super::monday_receipts::Side::Sell, -expected_amount),
        };
        if !next.attempts().iter().any(|attempt| attempt.hash == submitted.submission_tx
            && attempt.kind == AttemptKind::Execution)
            || !next.intent.owner.eq_ignore_ascii_case(&submitted.owner)
            || !binding.call.stock_token.eq_ignore_ascii_case(&submitted.stock_token)
            || submitted.side != expected_side
            || submitted.input_atoms != binding.request.wallet_debit_atoms
            || submitted.requested_amount != expected_amount {
            return Err("issuer submission does not bind prepared order".into());
        }
        if next.market_issuer_order_id.as_deref() == Some(submitted.order_id.as_str())
            && next.phase == super::orders::Phase::Finalized {
            return Ok(self.orders.get(id).unwrap());
        }
        if next.market_issuer_order_id.is_some() { return Err("issuer order ID changed".into()); }
        receipt::apply(&mut next, &Observation::Included {
            tx_hash: submitted.submission_tx.clone(), block_hash: submitted.submission_block_hash.clone(), success: true })?;
        receipt::apply(&mut next, &Observation::Finalized {
            tx_hash: submitted.submission_tx.clone(), block_hash: submitted.submission_block_hash.clone(), success: true })?;
        next.market_issuer_order_id = Some(submitted.order_id.clone());
        next.validate()?;
        self.append(&next)?;
        self.orders.insert(id.into(), next);
        Ok(self.orders.get(id).unwrap())
    }
    pub fn cancel_unsigned(&mut self, id: &str, owner: &str) -> Result<&Order> {
        let mut next = self.get(id, owner).ok_or("Monad order not found")?.clone();
        next.cancel_unsigned()?;
        if self.orders.get(id) != Some(&next) { self.append(&next)?; self.orders.insert(id.into(), next); }
        Ok(self.orders.get(id).unwrap())
    }
    fn append(&mut self, order: &Order) -> Result<()> {
        if self.poisoned { return Err("Monad journal requires restart after I/O failure".into()); }
        if self.seq == u64::MAX { return Err("Monad journal sequence exhausted".into()); }
        let body = serde_json::to_vec(&Frame { seq: self.seq + 1, order: order.clone() }).map_err(|e| e.to_string())?;
        if body.len() > RECORD_BOUND || self.bytes + 8 + body.len() as u64 > MAX_FILE {
            return Err("Monad journal budget exceeded".into());
        }
        let mut record = Vec::with_capacity(8 + body.len());
        record.extend_from_slice(&(body.len() as u32).to_le_bytes());
        record.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        record.extend_from_slice(&body);
        let start = self.bytes;
        if let Err(error) = self.file.write_all(&record).and_then(|()| self.file.sync_data()) {
            // No further acknowledgment from an uncertain or torn append.
            self.poisoned = true;
            let _ = self.file.set_len(start);
            let _ = self.file.sync_data();
            return Err(error.to_string());
        }
        self.bytes += record.len() as u64;
        self.seq += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monad::{monday_public::{encode_market_call, MarketRequest, MarketSide},
        orders::{fixture_intent, MarketBinding, Phase}};
    use crate::monad_contract::Catalog;
    fn path() -> PathBuf {
        std::env::temp_dir().join(format!("xtxc-monad-journal-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()))
    }
    #[test]
    fn idempotency_restart_unknown_and_owner_isolation() {
        let path = path();
        let intent = fixture_intent("mon_one", "idempotency_key_0001");
        let hash = format!("0x{}", "b".repeat(64));
        { let mut j = MonadJournal::open(&path).unwrap();
          j.prepare(Order::new(intent.clone()).unwrap()).unwrap();
          j.prepare(Order::new(intent.clone()).unwrap()).unwrap();
          assert!(j.get("mon_one", "0x3333333333333333333333333333333333333333").is_none());
          j.report_submission("mon_one", &intent.owner, &hash, 7).unwrap();
          j.mark_unknown("mon_one", &intent.owner).unwrap();
          assert!(j.report_submission("mon_one", &intent.owner, &format!("0x{}", "c".repeat(64)), 8).is_err());
        }
        { let j = MonadJournal::open(&path).unwrap();
          let found = j.get("mon_one", &intent.owner).unwrap();
          assert_eq!(found.phase, Phase::Unknown);
          assert_eq!(found.tx_nonce, Some(7));
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(format!("{}.lock", path.display())).unwrap();
    }
    #[test]
    fn torn_tail_recovers_but_corrupt_full_frame_fails() {
        let path = path();
        { let mut j = MonadJournal::open(&path).unwrap();
          j.prepare(Order::new(fixture_intent("mon_two", "idempotency_key_0002")).unwrap()).unwrap(); }
        { let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap(); f.write_all(&[3, 0, 0]).unwrap(); }
        { let j = MonadJournal::open(&path).unwrap(); assert_eq!(j.list("0x1111111111111111111111111111111111111111").len(), 1); }
        { let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
          f.seek(SeekFrom::Start(4)).unwrap(); f.write_all(&[1, 2, 3, 4]).unwrap(); }
        assert!(MonadJournal::open(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(format!("{}.lock", path.display())).unwrap();
    }
    #[test]
    fn one_tx_cannot_be_claimed_by_two_orders() {
        let path = path();
        let mut j = MonadJournal::open(&path).unwrap();
        let one = fixture_intent("mon_one", "idempotency_key_0001");
        let two = fixture_intent("mon_two", "idempotency_key_0002");
        j.prepare(Order::new(one.clone()).unwrap()).unwrap();
        j.prepare(Order::new(two.clone()).unwrap()).unwrap();
        let tx = format!("0x{}", "b".repeat(64));
        j.report_submission("mon_one", &one.owner, &tx, 2).unwrap();
        assert!(j.report_submission("mon_two", &two.owner, &tx, 3).is_err());
        assert!(j.report_submission("mon_two", &two.owner,
            &format!("0x{}", "c".repeat(64)), 2).is_err());
        assert_eq!(j.get("mon_two", &two.owner).unwrap().phase, Phase::Prepared);
        drop(j);
        { let mut reopened = MonadJournal::open(&path).unwrap();
          assert!(reopened.report_submission("mon_two", &two.owner,
              &format!("0x{}", "d".repeat(64)), 2).is_err()); }
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(format!("{}.lock", path.display())).unwrap();
    }
    #[test]
    fn direct_market_binding_survives_restart_without_float_atoms() {
        let path = path();
        let catalog: Catalog = serde_json::from_str(include_str!("../../../monad/catalog/registry.v1.json")).unwrap();
        let token = &catalog.token_observations[0];
        let request = MarketRequest {
            owner: "0x1111111111111111111111111111111111111111".into(),
            asset_id: format!("eip155:143:erc20:{}:{}:{}", token.token_address,
                token.issuer, token.issuer_product_id), side: MarketSide::Buy,
            wallet_debit_atoms: 10_000_000, order_amount_atoms: 9_970_000_000_000_000_000,
            deadline_secs: 1_800_000_100,
        };
        let binding = MarketBinding { call: encode_market_call(&catalog, &request, 1_800_000_000).unwrap(),
            request: request.clone(), simulated_block_hash: format!("0x{}", "a".repeat(64)),
            router_implementation_sha256: "b".repeat(64),
            stock_implementation_sha256: "c".repeat(64) };
        let mut intent = fixture_intent("mon_market", "idempotency_market_0001");
        intent.asset_id = request.asset_id.clone();
        intent.quantity_atoms = request.order_amount_atoms.to_string();
        intent.max_input_atoms = request.wallet_debit_atoms.to_string();
        let order = Order::new_market(intent, binding).unwrap();
        let json = serde_json::to_string(&order).unwrap();
        assert!(json.contains("\"orderAmountAtoms\":\"9970000000000000000\""));
        let tx_hash = format!("0x{}", "d".repeat(64));
        let issuer_id = format!("0x{}", "e".repeat(64));
        let block_hash = format!("0x{}", "f".repeat(64));
        { let mut journal = MonadJournal::open(&path).unwrap();
          journal.prepare(order.clone()).unwrap();
          journal.report_submission("mon_market", &request.owner, &tx_hash, 2).unwrap();
          let submitted = SubmittedOrder { order_id: issuer_id.clone(), owner: request.owner.clone(),
              stock_token: token.token_address.clone(), side: super::super::monday_receipts::Side::Buy,
              input_atoms: request.wallet_debit_atoms, requested_amount: request.order_amount_atoms as i128,
              submission_tx: tx_hash.clone(), submission_block_hash: block_hash.clone(),
              submission_block_number: 100 };
          let mut wrong = submitted.clone(); wrong.input_atoms += 1;
          assert!(journal.observe_market_submission("mon_market", &wrong).is_err());
          assert_eq!(journal.observe_market_submission("mon_market", &submitted).unwrap().phase, Phase::Finalized);
          assert_eq!(journal.observe_market_submission("mon_market", &submitted).unwrap().phase, Phase::Finalized);
        }
        { let journal = MonadJournal::open(&path).unwrap();
          let found = journal.get("mon_market", &request.owner).unwrap();
          assert_eq!(found.market_issuer_order_id.as_deref(), Some(issuer_id.as_str()));
          assert_eq!(found.phase, Phase::Finalized);
          assert!(found.market_binding.is_some()); }
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(format!("{}.lock", path.display())).unwrap();
    }
    #[test]
    fn replacement_cancel_and_reverted_attempt_are_durable_and_idempotent() {
        let path = path();
        let intent = fixture_intent("mon_recovery", "idempotency_recovery_0001");
        let original = format!("0x{}", "a".repeat(64));
        let repriced = format!("0x{}", "b".repeat(64));
        let cancel = format!("0x{}", "c".repeat(64));
        let block = format!("0x{}", "d".repeat(64));
        { let mut journal = MonadJournal::open(&path).unwrap();
          journal.prepare(Order::new(intent.clone()).unwrap()).unwrap();
          journal.report_submission("mon_recovery", &intent.owner, &original, 17).unwrap();
          journal.report_replacement("mon_recovery", &intent.owner, &repriced,
              17, AttemptKind::Execution).unwrap();
          journal.report_replacement("mon_recovery", &intent.owner, &cancel,
              17, AttemptKind::Cancellation).unwrap();
          assert!(journal.report_replacement("mon_recovery", &intent.owner,
              &format!("0x{}", "e".repeat(64)), 18, AttemptKind::Execution).is_err());
        }
        { let mut journal = MonadJournal::open(&path).unwrap();
          let order = journal.get("mon_recovery", &intent.owner).unwrap();
          assert_eq!(order.phase, Phase::Unknown);
          assert_eq!(order.attempts().len(), 3);
          journal.resolve_attempt("mon_recovery", &cancel, &block,
              true, AttemptKind::Cancellation).unwrap();
          journal.resolve_attempt("mon_recovery", &cancel, &block,
              true, AttemptKind::Cancellation).unwrap();
          assert_eq!(journal.get("mon_recovery", &intent.owner).unwrap().phase,
              Phase::Cancelled);
          assert!(journal.resolve_attempt("mon_recovery", &original, &block,
              true, AttemptKind::Execution).is_err());
        }
        { let journal = MonadJournal::open(&path).unwrap();
          let order = journal.get("mon_recovery", &intent.owner).unwrap();
          assert_eq!(order.phase, Phase::Cancelled);
          assert_eq!(order.finalized_block_hash.as_deref(), Some(block.as_str()));
          assert_eq!(order.failure_code.as_deref(), Some("WALLET_CANCELLED"));
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(format!("{}.lock", path.display())).unwrap();
    }
    #[test]
    fn failed_finalized_tx_consumes_nonce_without_economic_success() {
        let path = path();
        let intent = fixture_intent("mon_failed", "idempotency_failed_0001");
        let hash = format!("0x{}", "a".repeat(64));
        let block = format!("0x{}", "b".repeat(64));
        { let mut journal = MonadJournal::open(&path).unwrap();
          journal.prepare(Order::new(intent.clone()).unwrap()).unwrap();
          journal.report_submission("mon_failed", &intent.owner, &hash, 7).unwrap();
          let order = journal.resolve_attempt("mon_failed", &hash, &block,
              false, AttemptKind::Execution).unwrap();
          assert_eq!(order.phase, Phase::Reverted);
          assert_eq!(order.finalized_block_hash.as_deref(), Some(block.as_str()));
          assert!(journal.report_replacement("mon_failed", &intent.owner,
              &format!("0x{}", "c".repeat(64)), 7,
              AttemptKind::Execution).is_err());
        }
        { let journal = MonadJournal::open(&path).unwrap();
          assert_eq!(journal.get("mon_failed", &intent.owner).unwrap().phase, Phase::Reverted); }
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(format!("{}.lock", path.display())).unwrap();
    }
    #[test]
    fn checkpoint_retains_unknown_nonce_and_hash_ownership() {
        let path = path();
        let intent = fixture_intent("mon_checkpoint", "idempotency_checkpoint_0001");
        let hash = format!("0x{}", "a".repeat(64));
        let replacement = format!("0x{}", "b".repeat(64));
        { let mut journal = MonadJournal::open(&path).unwrap();
          journal.prepare(Order::new(intent.clone()).unwrap()).unwrap();
          journal.report_submission("mon_checkpoint", &intent.owner, &hash, 9).unwrap();
          journal.report_replacement("mon_checkpoint", &intent.owner,
              &replacement, 9, AttemptKind::Execution).unwrap();
          journal.compact().unwrap();
        }
        { let mut journal = MonadJournal::open(&path).unwrap();
          let recovered = journal.get("mon_checkpoint", &intent.owner).unwrap();
          assert_eq!(recovered.phase, Phase::Unknown);
          assert_eq!(recovered.attempts().len(), 2);
          assert!(journal.report_submission("mon_checkpoint", &intent.owner,
              &format!("0x{}", "c".repeat(64)), 10).is_err());
          let other = fixture_intent("mon_other", "idempotency_other_0001");
          journal.prepare(Order::new(other.clone()).unwrap()).unwrap();
          assert!(journal.report_submission("mon_other", &other.owner,
              &replacement, 1).is_err());
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(format!("{}.lock", path.display())).unwrap();
    }
}
