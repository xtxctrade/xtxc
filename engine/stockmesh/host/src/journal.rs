use crate::{exposure_receipt::ExpectedExposure, receipt::Expected, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Phase {
    Prepared,
    Submitted,
    Unknown,
    Finalized,
    Failed,
    Reconciled,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    /// Set by the StockMesh API only after current-state authorization. This
    /// durable binding permits observation of a retried submit after the quote
    /// cache disappears. It never permits fresh authorization or a new wire.
    /// Legacy/generic records remain unbound and are observation-only by ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote_id: Option<String>,
    pub signature: String,
    pub wire: Vec<u8>,
    pub message_hash: [u8; 32],
    pub last_valid_height: u64,
    pub resources: Vec<[u8; 32]>,
    /// Trusted compiler output carried with the exact signed wire. Public
    /// callers can never supply or replace this value. Older generic sender
    /// records deserialize as `None` and retain their previous behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_exposure: Option<ExpectedExposure>,
    /// Exact product->cash balance postcondition produced by the trusted sell
    /// compiler. It is mutually exclusive with `expected_exposure`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_swap: Option<Expected>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_basket: Option<crate::basket_wire::ExpectedBasket>,
    pub phase: Phase,
    pub attempts: u32,
}
impl Entry {
    fn validate_economic_type(&self)->Result<()>{
        if let Some(b)=&self.expected_basket{b.validate_investment()?;}
        if usize::from(self.expected_exposure.is_some())+usize::from(self.expected_swap.is_some())+usize::from(self.expected_basket.is_some())>1{
            return Err("journal ambiguous economic type".into());}
        Ok(())
    }
    /// Exactly one economic type owns a trade. Legacy generic journal records
    /// remain readable, but cannot impersonate an owner-scoped stock order.
    pub(crate) fn wallet_owner(&self) -> Option<&str> {
        match (&self.expected_exposure,&self.expected_swap,&self.expected_basket) {
            (Some(e),None,None)=>Some(e.wallet_owner()),
            (None,Some(e),None)=>Some(&e.owner),
            (None,None,Some(e))=>Some(e.wallet_owner()),
            _=>None,
        }
    }
}
#[derive(Serialize, Deserialize)]
struct Record {
    seq: u64,
    entry: Entry,
}
pub struct Journal {
    file: File,
    path: PathBuf,
    _lock: File,
    entries: BTreeMap<String, Entry>,
    signatures: BTreeMap<String, String>,
    reservations: BTreeMap<[u8; 32], String>,
    seq: u64,
    bytes: u64,
    max_bytes: u64,
    capacity: usize,
    poisoned: bool,
}

pub(crate) fn open_private_regular(path: &Path, truncate: bool) -> Result<File> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if !metadata.file_type().is_file()
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err("journal path must be a private regular file".into());
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(truncate)
        .mode(0o600)
        .open(path)
        .map_err(|e| e.to_string())?;
    let descriptor = file.metadata().map_err(|e| e.to_string())?;
    let pathname = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !pathname.file_type().is_file()
        || pathname.permissions().mode() & 0o077 != 0
        || pathname.nlink() != 1
        || descriptor.dev() != pathname.dev()
        || descriptor.ino() != pathname.ino()
    {
        return Err("journal path changed or is not private".into());
    }
    Ok(file)
}

impl Journal {
    pub fn open(path: &Path, capacity: usize, max_bytes: u64) -> Result<Self> {
        if capacity == 0 || capacity > 4096 || !(4096..=64 * 1024 * 1024).contains(&max_bytes) {
            return Err("journal bounds".into());
        }
        // A stable sidecar inode protects both the current log and a replacement
        // during atomic compaction. Locking only the renamed data inode is unsafe.
        let mut name = path.as_os_str().to_os_string();
        name.push(".lock");
        let lock = open_private_regular(&PathBuf::from(name), false)?;
        lock.try_lock_exclusive()
            .map_err(|_| "journal already owned")?;
        let mut file = open_private_regular(path, false)?;
        file.try_lock_exclusive()
            .map_err(|_| "journal already owned")?;
        // Persist creation of the journal directory entry before any durable acknowledgement.
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        let length = file.metadata().map_err(|e| e.to_string())?.len();
        if length > max_bytes {
            return Err("journal disk budget".into());
        }
        let mut data = Vec::with_capacity(length as usize);
        file.read_to_end(&mut data).map_err(|e| e.to_string())?;
        let (mut p, mut seq) = (0usize, 0u64);
        let mut entries = BTreeMap::new();
        while p < data.len() {
            if data.len() - p < 8 {
                break;
            }
            let n = u32::from_le_bytes(data[p..p + 4].try_into().unwrap()) as usize;
            let checksum = u32::from_le_bytes(data[p + 4..p + 8].try_into().unwrap());
            if n > 16 * 1024 {
                return Err("invalid journal record size".into());
            }
            if data.len() - p - 8 < n {
                break;
            }
            let body = &data[p + 8..p + 8 + n];
            if crc32fast::hash(body) != checksum {
                return Err("journal checksum corruption".into());
            }
            let r: Record = serde_json::from_slice(body).map_err(|e| e.to_string())?;
            r.entry.validate_economic_type()?;
            if r.seq != seq + 1 {
                return Err("journal sequence corruption".into());
            }
            seq = r.seq;
            entries.insert(r.entry.id.clone(), r.entry);
            if entries.len() > capacity {
                return Err("journal capacity".into());
            }
            p += 8 + n;
        }
        // Only an incomplete final record is a recoverable torn append. CRC corruption is fatal.
        if p < data.len() {
            file.set_len(p as u64).map_err(|e| e.to_string())?;
            file.sync_data().map_err(|e| e.to_string())?;
        }
        file.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
        let mut signatures = BTreeMap::new();
        let mut reservations = BTreeMap::new();
        // Reopening/compaction must preserve the same complete prefix. Do not
        // accept a later tranche whose predecessor disappeared or is UNKNOWN.
        let mut plans=BTreeSet::new();
        for e in entries.values(){if let Some(t)=e.expected_basket.as_ref().and_then(crate::basket_wire::ExpectedBasket::investment){
            if plans.insert(t.plan.id.clone()){t.plan.progress(entries.values())?;}
        }}
        for e in entries.values() {
            if signatures
                .insert(e.signature.clone(), e.id.clone())
                .is_some()
            {
                return Err("one signature assigned to multiple intents".into());
            }
            if !matches!(e.phase, Phase::Failed | Phase::Reconciled) {
                for resource in &e.resources {
                    if reservations
                        .insert(*resource, e.id.clone())
                        .is_some_and(|id| id != e.id)
                    {
                        return Err("overlapping durable reservations".into());
                    }
                }
            }
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
            _lock: lock,
            entries,
            signatures,
            reservations,
            seq,
            bytes: p as u64,
            max_bytes,
            capacity,
            poisoned: false,
        })
    }
    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.get(id)
    }
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }
    /// Remove superseded lifecycle records, retaining every intent ID, exact wire
    /// and terminal receipt state. This reclaims bytes, not deduplication capacity.
    /// No uncertain or completed intent silently disappears to admit a new one.
    pub fn compact(&mut self) -> Result<u64> {
        if self.poisoned {
            return Err("journal requires recovery after I/O failure".into());
        }
        let mut name = self.path.as_os_str().to_os_string();
        name.push(".compact");
        let path = PathBuf::from(name);
        let mut file = open_private_regular(&path, true)?;
        file.try_lock_exclusive().map_err(|e| e.to_string())?;
        let mut bytes = 0u64;
        let mut seq = 0u64;
        for entry in self.entries.values() {
            seq = seq.checked_add(1).ok_or("sequence overflow")?;
            let body = serde_json::to_vec(&Record {
                seq,
                entry: entry.clone(),
            })
            .map_err(|e| e.to_string())?;
            bytes = bytes
                .checked_add(8 + body.len() as u64)
                .ok_or("size overflow")?;
            if body.len() > 16 * 1024 || bytes > self.max_bytes {
                return Err("compaction bounds".into());
            }
            file.write_all(&(body.len() as u32).to_le_bytes())
                .and_then(|_| file.write_all(&crc32fast::hash(&body).to_le_bytes()))
                .and_then(|_| file.write_all(&body))
                .map_err(|e| e.to_string())?;
        }
        file.sync_all().map_err(|e| e.to_string())?;
        self.poisoned = true;
        std::fs::rename(&path, &self.path).map_err(|e| e.to_string())?;
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        let saved = self.bytes.saturating_sub(bytes);
        self.file = file;
        self.bytes = bytes;
        self.seq = seq;
        self.poisoned = false;
        Ok(saved)
    }
    pub fn insert(&mut self, e: Entry) -> Result<()> {
        self.insert_batch(vec![e])
    }
    /// Acknowledge only after a durable group flush. A crash may retain an
    /// unacknowledged prefix, recoverable by the original intent IDs.
    pub fn insert_batch(&mut self, pending: Vec<Entry>) -> Result<()> {
        if pending.is_empty() || pending.len() > 64 {
            return Err("batch bounds".into());
        }
        if self.entries.len() + pending.len() > self.capacity {
            return Err("admission full".into());
        }
        let mut ids = BTreeSet::new();
        let mut signatures = BTreeSet::new();
        let mut resources = BTreeSet::new();
        for e in &pending {
            e.validate_economic_type()?;
            crate::investment::admission(e,self.entries.values().chain(pending.iter().filter(|other|other.id!=e.id)))?;
            if self.entries.contains_key(&e.id) || !ids.insert(&e.id) {
                return Err("duplicate intent".into());
            }
            if self.signatures.contains_key(&e.signature) || !signatures.insert(&e.signature) {
                return Err("signed transaction already belongs to an intent".into());
            }
            if e.phase != Phase::Prepared || e.attempts != 0 {
                return Err("new entry must be prepared without attempts".into());
            }
            for r in &e.resources {
                if self.reservations.contains_key(r) || !resources.insert(r) {
                    return Err("resource conflict".into());
                }
            }
        }
        self.append_batch(pending)
    }
    pub fn update(&mut self, id: &str, phase: Phase, attempt: bool) -> Result<()> {
        self.update_batch(&[(id, phase, attempt)])
    }
    pub fn update_batch(&mut self, updates: &[(&str, Phase, bool)]) -> Result<()> {
        if updates.is_empty() || updates.len() > 64 {
            return Err("batch bounds".into());
        }
        let mut ids = BTreeSet::new();
        let mut pending = Vec::with_capacity(updates.len());
        for (id, phase, attempt) in updates {
            if !ids.insert(id) {
                return Err("duplicate update".into());
            }
            let mut e = self.entries.get(*id).ok_or("unknown intent")?.clone();
            let allowed = match e.phase {
                Phase::Prepared | Phase::Submitted | Phase::Unknown => matches!(
                    *phase,
                    Phase::Submitted | Phase::Unknown | Phase::Finalized | Phase::Failed
                ),
                Phase::Finalized => *phase == Phase::Reconciled,
                _ => false,
            };
            if !allowed {
                return Err("invalid phase transition".into());
            }
            e.phase = phase.clone();
            if *attempt {
                e.attempts = e.attempts.checked_add(1).ok_or("attempt overflow")?;
            }
            pending.push(e);
        }
        self.append_batch(pending)
    }
    fn append_batch(&mut self, pending: Vec<Entry>) -> Result<()> {
        if self.poisoned {
            return Err("journal requires recovery after I/O failure".into());
        }
        let mut seq = self.seq;
        let mut record = Vec::new();
        for e in &pending {
            seq = seq.checked_add(1).ok_or("sequence overflow")?;
            let body = serde_json::to_vec(&Record {
                seq,
                entry: e.clone(),
            })
            .map_err(|e| e.to_string())?;
            if body.len() > 16 * 1024
                || self.bytes + record.len() as u64 + 8 + body.len() as u64 > self.max_bytes
            {
                return Err("journal disk budget".into());
            }
            record.extend_from_slice(&(body.len() as u32).to_le_bytes());
            record.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
            record.extend_from_slice(&body);
        }
        self.poisoned = true;
        self.file.write_all(&record).map_err(|e| e.to_string())?;
        self.file.sync_data().map_err(|e| e.to_string())?;
        self.poisoned = false;
        self.bytes += record.len() as u64;
        self.seq = seq;
        // Index changes follow the same durable append as the lifecycle transition.
        // Admission is O(resources * log(capacity)), not a scan of all retained intents.
        for e in pending {
            self.signatures.insert(e.signature.clone(), e.id.clone());
            for resource in &e.resources {
                if matches!(e.phase, Phase::Failed | Phase::Reconciled) {
                    if self.reservations.get(resource) == Some(&e.id) {
                        self.reservations.remove(resource);
                    }
                } else {
                    self.reservations.insert(*resource, e.id.clone());
                }
            }
            self.entries.insert(e.id.clone(), e);
        }
        Ok(())
    }
}
