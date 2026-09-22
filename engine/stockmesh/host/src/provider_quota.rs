//! Durable, cross-process admission for bounded hosted-reader runs.
//! Reserve worst-case body bytes before dispatch, never refund ambiguous work.
//! Not a provider invoice: headers, credits and unrelated clients are excluded.
use crate::Result;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_RECORD_BYTES: u64 = 8192;
const SCHEMA: &str = "skew.provider-quota/v1";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QuotaConfig {
    pub ledger_file: String,
    pub budget_id: String,
    pub period_start_unix: u64,
    pub period_end_unix: u64,
    pub max_requests: u64,
    pub max_reserved_body_bytes: u64,
}

impl QuotaConfig {
    pub fn validate(&self) -> Result<()> {
        let path = Path::new(&self.ledger_file);
        if !path.is_absolute()
            || path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir))
            || path.file_name().is_none()
            || self.budget_id.is_empty()
            || self.budget_id.len() > 64
            || !self.budget_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || self.period_start_unix >= self.period_end_unix
            || self.period_end_unix - self.period_start_unix > 32 * 86400
            || !(1..=1_000_000).contains(&self.max_requests)
            || !(1..=64 * 1024 * 1024 * 1024).contains(&self.max_reserved_body_bytes)
        {
            return Err("provider durable quota bounds".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Terms {
    schema: String,
    provider_id: String,
    genesis: String,
    budget_id: String,
    period_start_unix: u64,
    period_end_unix: u64,
    max_requests: u64,
    max_reserved_body_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Usage {
    terms: Terms,
    requests: u64,
    reserved_body_bytes: u64,
    last_reservation_unix: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    usage: Usage,
    checksum: String,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaMetrics {
    pub budget_id: String,
    pub requests: u64,
    pub reserved_body_bytes: u64,
    pub max_requests: u64,
    pub max_reserved_body_bytes: u64,
    pub period_end_unix: u64,
}

pub struct DurableQuota {
    config: QuotaConfig,
    terms: Terms,
}

// Explicit unlock matters when another thread forks while this file is open:
// an inherited descriptor must not pin the lock until that child execs/exits.
struct LockedFile(File);
impl std::ops::Deref for LockedFile {
    type Target = File;
    fn deref(&self) -> &File { &self.0 }
}
impl std::ops::DerefMut for LockedFile {
    fn deref_mut(&mut self) -> &mut File { &mut self.0 }
}
impl Drop for LockedFile {
    fn drop(&mut self) { let _ = FileExt::unlock(&self.0); }
}

impl DurableQuota {
    pub fn new(config: QuotaConfig, provider_id: &str, genesis: &str) -> Result<Self> {
        config.validate()?;
        if provider_id.is_empty() || provider_id.len() > 32 || genesis.len() > 64 {
            return Err("provider durable identity bounds".into());
        }
        let terms = Terms {
            schema: SCHEMA.into(), provider_id: provider_id.into(), genesis: genesis.into(),
            budget_id: config.budget_id.clone(), period_start_unix: config.period_start_unix,
            period_end_unix: config.period_end_unix, max_requests: config.max_requests,
            max_reserved_body_bytes: config.max_reserved_body_bytes,
        };
        Ok(Self { config, terms })
    }

    /// Explicit one-time creation; connect/admit never create or reset a ledger.
    pub fn initialize(&self) -> Result<()> {
        let now = unix_now()?;
        self.check_time(now, 0)?;
        self.check_parent()?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)] {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&self.config.ledger_file)
            .map_err(|_| "provider quota initialization requires an absent file")?;
        FileExt::try_lock_exclusive(&file).map_err(|_| "provider quota busy")?;
        let mut file = LockedFile(file);
        self.write(&mut file, Usage {
            terms: self.terms.clone(), requests: 0, reserved_body_bytes: 0,
            last_reservation_unix: now,
        })?;
        // A crash before either sync can leave a missing/corrupt file. It must
        // be treated as unavailable, not recreated with zero usage.
        File::open(Path::new(&self.config.ledger_file).parent().ok_or("quota parent")?)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| "provider quota directory sync failed")?;
        Ok(())
    }

    pub fn snapshot(&self) -> Result<QuotaMetrics> {
        let mut file = self.open_locked()?;
        let usage = self.read(&mut file)?;
        self.check_time(unix_now()?, usage.last_reservation_unix)?;
        Ok(metrics(&usage))
    }

    /// A reservation is durable before the caller may contact the provider.
    /// No finish/refund: a killed process cannot prove the upstream did no work.
    pub fn reserve(&self, body_bytes: u64) -> Result<()> {
        let mut file = self.open_locked()?;
        let mut usage = self.read(&mut file)?;
        let now = unix_now()?;
        self.check_time(now, usage.last_reservation_unix)?;
        let requests = usage.requests.checked_add(1).ok_or("provider quota arithmetic")?;
        let bytes = usage.reserved_body_bytes.checked_add(body_bytes).ok_or("provider quota arithmetic")?;
        if body_bytes == 0 || requests > self.terms.max_requests || bytes > self.terms.max_reserved_body_bytes {
            return Err("provider durable quota exhausted".into());
        }
        usage.requests = requests;
        usage.reserved_body_bytes = bytes;
        usage.last_reservation_unix = now;
        self.write(&mut file, usage)
    }

    fn check_time(&self, now: u64, previous: u64) -> Result<()> {
        if now < self.terms.period_start_unix || now >= self.terms.period_end_unix || now < previous {
            return Err("provider quota period invalid, expired or clock regressed".into());
        }
        Ok(())
    }

    fn check_parent(&self) -> Result<()> {
        let path = Path::new(&self.config.ledger_file);
        let parent = path.parent().ok_or("provider quota parent")?;
        if std::fs::canonicalize(parent).map_err(|_| "provider quota parent unavailable")? != parent {
            return Err("provider quota parent must be canonical".into());
        }
        let meta = std::fs::symlink_metadata(parent).map_err(|_| "provider quota parent unavailable")?;
        if !meta.is_dir() { return Err("provider quota parent is not a directory".into()); }
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o077 != 0 {
                return Err("provider quota parent must be private".into());
            }
        }
        Ok(())
    }

    fn open_locked(&self) -> Result<LockedFile> {
        self.check_parent()?;
        let path = Path::new(&self.config.ledger_file);
        let before = std::fs::symlink_metadata(path).map_err(|_| "provider quota unavailable; initialization is explicit")?;
        if !before.file_type().is_file() { return Err("provider quota must be a regular file".into()); }
        let file = OpenOptions::new().read(true).write(true).open(path).map_err(|_| "provider quota open failed")?;
        FileExt::try_lock_exclusive(&file).map_err(|_| "provider quota busy")?;
        let file = LockedFile(file);
        let after = file.metadata().map_err(|_| "provider quota metadata failed")?;
        #[cfg(unix)] {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if before.dev() != after.dev() || before.ino() != after.ino() || after.nlink() != 1
                || after.permissions().mode() & 0o077 != 0 {
                return Err("provider quota identity or permissions changed".into());
            }
        }
        if after.len() == 0 || after.len() > MAX_RECORD_BYTES {
            return Err("provider quota record size invalid".into());
        }
        Ok(file)
    }

    fn read(&self, file: &mut File) -> Result<Usage> {
        let mut bytes = Vec::new();
        file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes).map_err(|_| "provider quota read failed")?;
        if bytes.len() as u64 > MAX_RECORD_BYTES { return Err("provider quota record size invalid".into()); }
        let record: Record = serde_json::from_slice(&bytes).map_err(|_| "provider quota corrupt")?;
        if record.usage.terms != self.terms || checksum(&record.usage)? != record.checksum {
            return Err("provider quota terms or checksum mismatch".into());
        }
        if record.usage.requests > self.terms.max_requests
            || record.usage.reserved_body_bytes > self.terms.max_reserved_body_bytes
            || record.usage.last_reservation_unix < self.terms.period_start_unix
            || record.usage.last_reservation_unix >= self.terms.period_end_unix {
            return Err("provider quota usage invalid".into());
        }
        Ok(record.usage)
    }

    fn write(&self, file: &mut File, usage: Usage) -> Result<()> {
        let record = Record { checksum: checksum(&usage)?, usage };
        let bytes = serde_json::to_vec(&record).map_err(|_| "provider quota encoding failed")?;
        if bytes.len() as u64 > MAX_RECORD_BYTES { return Err("provider quota record size invalid".into()); }
        file.seek(SeekFrom::Start(0)).and_then(|_| file.write_all(&bytes))
            .and_then(|_| file.set_len(bytes.len() as u64)).and_then(|_| file.sync_all())
            .map_err(|_| "provider quota durable reservation failed".into())
    }
}

fn checksum(usage: &Usage) -> Result<String> {
    let bytes = serde_json::to_vec(usage).map_err(|_| "provider quota encoding failed")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn unix_now() -> Result<u64> {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).map_err(|_| "provider quota clock unavailable".into())
}

fn metrics(usage: &Usage) -> QuotaMetrics {
    QuotaMetrics { budget_id: usage.terms.budget_id.clone(), requests: usage.requests,
        reserved_body_bytes: usage.reserved_body_bytes, max_requests: usage.terms.max_requests,
        max_reserved_body_bytes: usage.terms.max_reserved_body_bytes, period_end_unix: usage.terms.period_end_unix }
}
