//! Private, single-writer append journal. A torn final frame may be truncated;
//! a complete corrupt frame fails closed. A successful method return implies
//! the event has been synced to disk.
use super::{orders::Order, receipt::{self, Observation}};
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
    _lock: File,
    orders: BTreeMap<String, Order>,
    idempotency: BTreeMap<String, String>,
    tx_hashes: BTreeMap<String, String>,
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
            if let Some(hash) = &order.tx_hash {
                if let Some(existing) = tx_hashes.insert(hash.clone(), order.intent.order_id.clone()) {
                    if existing != order.intent.order_id { return Err("Monad transaction assigned twice".into()); }
                }
            }
            if let Some(existing) = orders.get(&order.intent.order_id) {
                if existing.intent != order.intent { return Err("Monad order identity changed".into()); }
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
        Ok(Self { file, _lock: lock, orders, idempotency, tx_hashes, seq, bytes: offset as u64, poisoned: false })
    }

    pub fn get(&self, order_id: &str, owner: &str) -> Option<&Order> {
        self.orders.get(order_id).filter(|order| order.intent.owner.eq_ignore_ascii_case(owner))
    }
    pub fn list(&self, owner: &str) -> Vec<&Order> {
        self.orders.values().filter(|order| order.intent.owner.eq_ignore_ascii_case(owner)).collect()
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
        if let Some(existing) = self.tx_hashes.get(&tx_hash.to_ascii_lowercase()) {
            if existing != id { return Err("transaction already bound to another order".into()); }
        }
        next.report_submission(tx_hash, nonce)?;
        if self.orders.get(id) != Some(&next) {
            self.append(&next)?;
            self.orders.insert(id.into(), next.clone());
            self.tx_hashes.insert(next.tx_hash.clone().unwrap(), id.into());
        }
        Ok(self.orders.get(id).unwrap())
    }
    pub fn mark_unknown(&mut self, id: &str, owner: &str) -> Result<&Order> {
        let mut next = self.get(id, owner).ok_or("Monad order not found")?.clone();
        next.mark_unknown()?;
        if self.orders.get(id) != Some(&next) { self.append(&next)?; self.orders.insert(id.into(), next); }
        Ok(self.orders.get(id).unwrap())
    }
    /// Caller must be an authenticated chain observer. Never expose as a public HTTP command.
    pub(crate) fn observe(&mut self, id: &str, observation: &Observation) -> Result<&Order> {
        let mut next = self.orders.get(id).ok_or("Monad order not found")?.clone();
        receipt::apply(&mut next, observation)?;
        if self.orders.get(id) != Some(&next) { self.append(&next)?; self.orders.insert(id.into(), next); }
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
        if next.tx_hash.as_deref() != Some(submitted.submission_tx.as_str())
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
        assert_eq!(j.get("mon_two", &two.owner).unwrap().phase, Phase::Prepared);
        drop(j);
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
            router_implementation_sha256: format!("0x{}", "b".repeat(64)),
            stock_implementation_sha256: format!("0x{}", "c".repeat(64)) };
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
}
