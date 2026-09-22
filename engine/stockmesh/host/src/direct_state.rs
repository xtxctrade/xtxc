//! Bounded local frozen-bank transport for the StockMesh quote hot path.
//!
//! The producer is expected to live beside an Agave/Frankendancer replay
//! process and resolve every requested account against one frozen Bank.  This
//! module deliberately has no network client and no signing capability.  A
//! malformed, stale, gapped or overloaded stream becomes unavailable. Legacy
//! `primary` callers may use their separately pinned mainnet RPC fallback;
//! `required` callers must fail closed and never construct one.
use crate::Result;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    os::unix::{fs::FileTypeExt, fs::MetadataExt, fs::PermissionsExt, net::UnixStream},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc, Mutex, RwLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const REQUEST_MAGIC: &[u8; 8] = b"SKWDREQ1";
const FRAME_MAGIC: &[u8; 8] = b"SKWDBNK1";
const WIRE_VERSION: u16 = 1;
const REQUEST_HEADER_BYTES: usize = 48;
const FRAME_HEADER_BYTES: usize = 216;
const ACCOUNT_HEADER_BYTES: usize = 80;
const MAX_ACCOUNTS: usize = 256;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_ACCOUNT_BYTES: usize = 4 * 1024 * 1024;
const MAX_CLOCK_SKEW_MS: u64 = 5_000;
const FLAG_FROZEN: u32 = 1;
const FLAG_CONFIRMED: u32 = 2;
const REQUIRED_FLAGS: u32 = FLAG_FROZEN | FLAG_CONFIRMED;
const KIND_BANK: u16 = 1;
const KIND_OVERLOAD: u16 = 2;
const KIND_GAP: u16 = 3;
const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectMode {
    Shadow,
    Primary,
    Required,
}

impl DirectMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "shadow" => Ok(Self::Shadow),
            "primary" => Ok(Self::Primary),
            "required" => Ok(Self::Required),
            _ => Err("direct state mode must be shadow, primary, or required".into()),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shadow => "shadow",
            Self::Primary => "primary",
            Self::Required => "required",
        }
    }

    pub fn uses_direct_quotes(self) -> bool {
        matches!(self, Self::Primary | Self::Required)
    }

    pub fn forbids_rpc(self) -> bool {
        self == Self::Required
    }
}

#[derive(Clone, Debug)]
pub struct DirectConfig {
    pub socket: PathBuf,
    pub mode: DirectMode,
    pub max_age: Duration,
    pub promotion_frames: u8,
}

impl DirectConfig {
    pub fn validate(&self) -> Result<()> {
        if !self.socket.is_absolute()
            || !self.socket.starts_with("/run/stockmesh")
            || self
                .socket
                .components()
                .any(|component| matches!(component, Component::ParentDir))
            || self.socket.as_os_str().as_encoded_bytes().len() > 100
            || !(Duration::from_millis(250)..=Duration::from_millis(1_500)).contains(&self.max_age)
            || !(2..=32).contains(&self.promotion_frames)
        {
            return Err("direct state configuration bounds".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct DirectAccount {
    key: String,
    owner: String,
    executable: bool,
    lamports: u64,
    data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct DirectSnapshot {
    pub sequence: u64,
    pub slot: u64,
    parent_slot: u64,
    produced_ms: u64,
    watch_hash: [u8; 32],
    bank_hash: [u8; 32],
    parent_bank_hash: [u8; 32],
    accounts: BTreeMap<String, DirectAccount>,
    received: Instant,
}

impl DirectSnapshot {
    pub fn response_for(&self, keys: &[String]) -> Result<Value> {
        if keys.is_empty() || keys.len() > MAX_ACCOUNTS {
            return Err("direct projection bounds".into());
        }
        let mut seen = BTreeSet::new();
        let values = keys
            .iter()
            .map(|key| {
                if !seen.insert(key) {
                    return Err("direct projection duplicate".into());
                }
                let account = self
                    .accounts
                    .get(key)
                    .ok_or_else(|| "direct projection account missing".to_string())?;
                Ok(json!({
                    "data":[STANDARD.encode(&account.data),"base64"],
                    "executable":account.executable,
                    "lamports":account.lamports,
                    "owner":account.owner
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(json!({"context":{"slot":self.slot},"value":values}))
    }
}

#[derive(Clone, Debug)]
pub struct DirectTelemetry {
    pub mode: &'static str,
    pub connected: bool,
    pub promoted: bool,
    pub active_source: &'static str,
    pub latest_slot: u64,
    pub latest_age_ms: Option<u64>,
    pub frames: u64,
    pub rejected: u64,
    pub overloads: u64,
    pub gaps: u64,
    pub disconnects: u64,
    pub direct_publishes: u64,
    pub rpc_fallbacks: u64,
    pub direct_unavailable: u64,
    pub shadow_matches: u64,
    pub shadow_mismatches: u64,
    pub last_fallback: String,
    pub last_unavailable: String,
}

pub struct DirectState {
    config: DirectConfig,
    latest: RwLock<Option<Arc<DirectSnapshot>>>,
    connected: AtomicBool,
    promoted: AtomicBool,
    active_source: AtomicU8,
    frames: AtomicU64,
    rejected: AtomicU64,
    overloads: AtomicU64,
    gaps: AtomicU64,
    disconnects: AtomicU64,
    direct_publishes: AtomicU64,
    rpc_fallbacks: AtomicU64,
    direct_unavailable: AtomicU64,
    shadow_matches: AtomicU64,
    shadow_mismatches: AtomicU64,
    last_fallback: Mutex<String>,
    last_unavailable: Mutex<String>,
}

impl DirectState {
    pub fn start<F>(config: DirectConfig, watch_keys: F) -> Result<Arc<Self>>
    where
        F: Fn() -> Result<Vec<String>> + Send + Sync + 'static,
    {
        config.validate()?;
        let state = Arc::new(Self {
            config,
            latest: RwLock::new(None),
            connected: AtomicBool::new(false),
            promoted: AtomicBool::new(false),
            active_source: AtomicU8::new(0),
            frames: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            overloads: AtomicU64::new(0),
            gaps: AtomicU64::new(0),
            disconnects: AtomicU64::new(0),
            direct_publishes: AtomicU64::new(0),
            rpc_fallbacks: AtomicU64::new(0),
            direct_unavailable: AtomicU64::new(0),
            shadow_matches: AtomicU64::new(0),
            shadow_mismatches: AtomicU64::new(0),
            last_fallback: Mutex::new("startup".into()),
            last_unavailable: Mutex::new("startup".into()),
        });
        let runner = Arc::clone(&state);
        let watch_keys = Arc::new(watch_keys);
        std::thread::Builder::new()
            .name("stockmesh-direct-state".into())
            .spawn(move || runner.run(watch_keys))
            .map_err(|error| error.to_string())?;
        Ok(state)
    }

    pub fn mode(&self) -> DirectMode {
        self.config.mode
    }

    pub fn snapshot(&self, watch_keys: &[String]) -> Result<Arc<DirectSnapshot>> {
        if !self.connected.load(Ordering::Acquire) {
            return Err("direct state disconnected".into());
        }
        if !self.promoted.load(Ordering::Acquire) {
            return Err("direct state warming".into());
        }
        let expected = watch_hash(watch_keys)?;
        let snapshot = self
            .latest
            .read()
            .map_err(|_| "direct state poisoned")?
            .clone()
            .ok_or("direct state empty")?;
        if snapshot.watch_hash != expected {
            return Err("direct state watch set changed".into());
        }
        if snapshot.received.elapsed() > self.config.max_age {
            return Err("direct state stale".into());
        }
        let now = now_ms()?;
        if snapshot.produced_ms > now.saturating_add(MAX_CLOCK_SKEW_MS)
            || now.saturating_sub(snapshot.produced_ms)
                > u64::try_from(self.config.max_age.as_millis()).unwrap_or(u64::MAX)
        {
            return Err("direct state producer clock stale".into());
        }
        Ok(snapshot)
    }

    pub fn record_direct_publish(&self) {
        self.active_source.store(1, Ordering::Release);
        self.direct_publishes.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_rpc_fallback(&self, reason: &str) {
        debug_assert!(!self.config.mode.forbids_rpc());
        self.active_source.store(2, Ordering::Release);
        self.rpc_fallbacks.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut last) = self.last_fallback.lock() {
            *last = reason.chars().take(160).collect();
        }
    }

    pub fn record_direct_unavailable(&self, reason: &str) {
        self.active_source.store(3, Ordering::Release);
        self.direct_unavailable.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut last) = self.last_unavailable.lock() {
            *last = reason.chars().take(160).collect();
        }
    }

    pub fn record_shadow(&self, matches: bool) {
        if matches {
            self.shadow_matches.fetch_add(1, Ordering::Relaxed);
        } else {
            self.shadow_mismatches.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn telemetry(&self) -> DirectTelemetry {
        let latest = self.latest.read().ok().and_then(|value| value.clone());
        DirectTelemetry {
            mode: self.config.mode.as_str(),
            connected: self.connected.load(Ordering::Acquire),
            promoted: self.promoted.load(Ordering::Acquire),
            active_source: match self.active_source.load(Ordering::Acquire) {
                1 => "direct_node",
                2 => "mainnet_rpc_fallback",
                3 => "direct_unavailable",
                _ if self.config.mode == DirectMode::Shadow => "mainnet_rpc_shadow",
                _ if self.config.mode == DirectMode::Required => "direct_required_warming",
                _ => "initializing",
            },
            latest_slot: latest.as_ref().map_or(0, |snapshot| snapshot.slot),
            latest_age_ms: latest.map(|snapshot| {
                u64::try_from(snapshot.received.elapsed().as_millis()).unwrap_or(u64::MAX)
            }),
            frames: self.frames.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            overloads: self.overloads.load(Ordering::Relaxed),
            gaps: self.gaps.load(Ordering::Relaxed),
            disconnects: self.disconnects.load(Ordering::Relaxed),
            direct_publishes: self.direct_publishes.load(Ordering::Relaxed),
            rpc_fallbacks: self.rpc_fallbacks.load(Ordering::Relaxed),
            direct_unavailable: self.direct_unavailable.load(Ordering::Relaxed),
            shadow_matches: self.shadow_matches.load(Ordering::Relaxed),
            shadow_mismatches: self.shadow_mismatches.load(Ordering::Relaxed),
            last_fallback: self
                .last_fallback
                .lock()
                .map(|value| value.clone())
                .unwrap_or_else(|_| "telemetry poisoned".into()),
            last_unavailable: self
                .last_unavailable
                .lock()
                .map(|value| value.clone())
                .unwrap_or_else(|_| "telemetry poisoned".into()),
        }
    }

    fn run<F>(self: Arc<Self>, watch_keys: Arc<F>)
    where
        F: Fn() -> Result<Vec<String>> + Send + Sync + 'static,
    {
        let mut failures = 0u32;
        loop {
            let result = (|| -> Result<()> {
                validate_socket(&self.config.socket)?;
                let mut stream = UnixStream::connect(&self.config.socket)
                    .map_err(|error| format!("direct socket connect: {error}"))?;
                stream
                    .set_read_timeout(Some(self.config.max_age))
                    .map_err(|error| error.to_string())?;
                stream
                    .set_write_timeout(Some(Duration::from_millis(250)))
                    .map_err(|error| error.to_string())?;
                let keys = normalized_watch_keys((watch_keys)()?)?;
                let expected_watch = watch_hash(&keys)?;
                stream
                    .write_all(&watch_request(&keys, expected_watch)?)
                    .map_err(|error| format!("direct watch request: {error}"))?;
                self.connected.store(true, Ordering::Release);
                failures = 0;
                let mut streak = 0u8;
                let mut previous: Option<Arc<DirectSnapshot>> = None;
                loop {
                    let current_keys = normalized_watch_keys((watch_keys)()?)?;
                    if watch_hash(&current_keys)? != expected_watch {
                        return Err("direct watch set rotated".into());
                    }
                    match read_frame(&mut stream, expected_watch, &keys)? {
                        Frame::Bank(snapshot) => {
                            if let Some(prior) = previous.as_ref() {
                                if snapshot.sequence != prior.sequence.saturating_add(1)
                                    || snapshot.parent_slot != prior.slot
                                    || snapshot.parent_bank_hash != prior.bank_hash
                                {
                                    self.gaps.fetch_add(1, Ordering::Relaxed);
                                    return Err("direct sequence or bank lineage gap".into());
                                }
                            }
                            let snapshot = Arc::new(snapshot);
                            previous = Some(Arc::clone(&snapshot));
                            if let Ok(mut latest) = self.latest.write() {
                                *latest = Some(snapshot);
                            }
                            self.frames.fetch_add(1, Ordering::Relaxed);
                            streak = streak.saturating_add(1);
                            if streak >= self.config.promotion_frames {
                                self.promoted.store(true, Ordering::Release);
                            }
                        }
                        Frame::Overload => {
                            self.overloads.fetch_add(1, Ordering::Relaxed);
                            return Err("direct producer overloaded".into());
                        }
                        Frame::Gap => {
                            self.gaps.fetch_add(1, Ordering::Relaxed);
                            return Err("direct producer reported replay gap".into());
                        }
                    }
                }
            })();
            self.connected.store(false, Ordering::Release);
            self.promoted.store(false, Ordering::Release);
            if let Ok(mut latest) = self.latest.write() {
                *latest = None;
            }
            self.disconnects.fetch_add(1, Ordering::Relaxed);
            if let Err(error) = result {
                self.rejected.fetch_add(1, Ordering::Relaxed);
                failures = failures.saturating_add(1);
                if failures == 1 || failures.is_power_of_two() {
                    eprintln!(
                        "StockMesh direct state unavailable ({failures} consecutive): {error}"
                    );
                }
            }
            let delay = 100u64.saturating_mul(1u64 << failures.saturating_sub(1).min(5));
            std::thread::sleep(Duration::from_millis(delay.min(3_200)));
        }
    }
}

enum Frame {
    Bank(DirectSnapshot),
    Overload,
    Gap,
}

fn validate_socket(path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("direct socket metadata: {error}"))?;
    if !metadata.file_type().is_socket()
        || metadata.file_type().is_symlink()
        || metadata.uid() != 0
        || metadata.permissions().mode() & 0o007 != 0
        || metadata.nlink() != 1
    {
        return Err("direct socket type or permissions".into());
    }
    Ok(())
}

fn normalized_watch_keys(mut keys: Vec<String>) -> Result<Vec<String>> {
    if keys.is_empty() || keys.len() > MAX_ACCOUNTS {
        return Err("direct watch account bound".into());
    }
    keys.sort();
    keys.dedup();
    if keys.is_empty() || keys.len() > MAX_ACCOUNTS {
        return Err("direct watch account bound".into());
    }
    for key in &keys {
        decode_pubkey(key)?;
    }
    Ok(keys)
}

fn watch_hash(keys: &[String]) -> Result<[u8; 32]> {
    let keys = normalized_watch_keys(keys.to_vec())?;
    let mut hasher = Sha256::new();
    hasher.update(b"skew.stockmesh.direct-watch/v1");
    hasher.update((keys.len() as u16).to_le_bytes());
    for key in keys {
        hasher.update(decode_pubkey(&key)?);
    }
    Ok(hasher.finalize().into())
}

fn watch_request(keys: &[String], hash: [u8; 32]) -> Result<Vec<u8>> {
    let keys = normalized_watch_keys(keys.to_vec())?;
    let payload_len = keys.len().checked_mul(32).ok_or("direct request length")?;
    let mut request = Vec::with_capacity(REQUEST_HEADER_BYTES + payload_len);
    request.extend_from_slice(REQUEST_MAGIC);
    request.extend_from_slice(&WIRE_VERSION.to_le_bytes());
    request.extend_from_slice(&(keys.len() as u16).to_le_bytes());
    request.extend_from_slice(&(payload_len as u32).to_le_bytes());
    request.extend_from_slice(&hash);
    for key in keys {
        request.extend_from_slice(&decode_pubkey(&key)?);
    }
    Ok(request)
}

fn read_frame(
    reader: &mut impl Read,
    expected_watch: [u8; 32],
    expected_keys: &[String],
) -> Result<Frame> {
    let mut header = [0u8; FRAME_HEADER_BYTES];
    reader
        .read_exact(&mut header)
        .map_err(|error| format!("direct frame header: {error}"))?;
    if &header[0..8] != FRAME_MAGIC
        || u16_at(&header, 8)? != WIRE_VERSION
        || u16_at(&header, 22)? != 0
    {
        return Err("direct frame header identity".into());
    }
    let kind = u16_at(&header, 10)?;
    let flags = u32_at(&header, 12)?;
    let frame_len = usize::try_from(u32_at(&header, 16)?).map_err(|_| "direct frame length")?;
    let account_count = usize::from(u16_at(&header, 20)?);
    if !(FRAME_HEADER_BYTES..=MAX_FRAME_BYTES).contains(&frame_len) || account_count > MAX_ACCOUNTS
    {
        return Err("direct frame bounds".into());
    }
    let mut watch = [0u8; 32];
    watch.copy_from_slice(&header[56..88]);
    if watch != expected_watch {
        return Err("direct frame watch hash".into());
    }
    let mut genesis = [0u8; 32];
    genesis.copy_from_slice(&header[88..120]);
    if genesis != decode_pubkey(MAINNET_GENESIS)? {
        return Err("direct frame genesis".into());
    }
    let payload_len = frame_len - FRAME_HEADER_BYTES;
    let mut payload = vec![0u8; payload_len];
    reader
        .read_exact(&mut payload)
        .map_err(|error| format!("direct frame payload: {error}"))?;
    let expected_payload_hash: [u8; 32] = Sha256::digest(&payload).into();
    if header[184..216] != expected_payload_hash {
        return Err("direct frame payload hash".into());
    }
    if matches!(kind, KIND_OVERLOAD | KIND_GAP) {
        if account_count != 0 || !payload.is_empty() {
            return Err("direct control frame payload".into());
        }
        return Ok(if kind == KIND_OVERLOAD {
            Frame::Overload
        } else {
            Frame::Gap
        });
    }
    if kind != KIND_BANK
        || flags & REQUIRED_FLAGS != REQUIRED_FLAGS
        || account_count != expected_keys.len()
    {
        return Err("direct bank frame flags or account count".into());
    }
    let sequence = u64_at(&header, 24)?;
    let slot = u64_at(&header, 32)?;
    let parent_slot = u64_at(&header, 40)?;
    let produced_ms = u64_at(&header, 48)?;
    let mut bank_hash = [0u8; 32];
    bank_hash.copy_from_slice(&header[120..152]);
    let mut parent_bank_hash = [0u8; 32];
    parent_bank_hash.copy_from_slice(&header[152..184]);
    if sequence == 0
        || slot == 0
        || parent_slot >= slot
        || bank_hash == [0; 32]
        || parent_bank_hash == [0; 32]
    {
        return Err("direct bank lineage fields".into());
    }
    let now = now_ms()?;
    if produced_ms > now.saturating_add(MAX_CLOCK_SKEW_MS) {
        return Err("direct producer clock future".into());
    }
    let mut cursor = 0usize;
    let mut accounts = BTreeMap::new();
    let mut total_data = 0usize;
    for _ in 0..account_count {
        let end = cursor
            .checked_add(ACCOUNT_HEADER_BYTES)
            .ok_or("direct account header overflow")?;
        let fixed = payload
            .get(cursor..end)
            .ok_or("direct account header truncated")?;
        let key = bs58::encode(&fixed[0..32]).into_string();
        let owner = bs58::encode(&fixed[32..64]).into_string();
        let lamports = u64_at(fixed, 64)?;
        let data_len =
            usize::try_from(u32_at(fixed, 72)?).map_err(|_| "direct account data length")?;
        let account_flags = u32_at(fixed, 76)?;
        if data_len > MAX_ACCOUNT_BYTES || account_flags & !1 != 0 {
            return Err("direct account bounds or flags".into());
        }
        total_data = total_data
            .checked_add(data_len)
            .ok_or("direct account total overflow")?;
        if total_data > MAX_FRAME_BYTES - FRAME_HEADER_BYTES {
            return Err("direct account total bound".into());
        }
        cursor = end;
        let data_end = cursor
            .checked_add(data_len)
            .ok_or("direct account data overflow")?;
        let data = payload
            .get(cursor..data_end)
            .ok_or("direct account data truncated")?
            .to_vec();
        cursor = data_end;
        if accounts
            .insert(
                key.clone(),
                DirectAccount {
                    key,
                    owner,
                    executable: account_flags & 1 == 1,
                    lamports,
                    data,
                },
            )
            .is_some()
        {
            return Err("direct duplicate account".into());
        }
    }
    if cursor != payload.len()
        || accounts.keys().ne(expected_keys.iter())
        || accounts.values().any(|account| account.key.is_empty())
    {
        return Err("direct account set or trailing bytes".into());
    }
    Ok(Frame::Bank(DirectSnapshot {
        sequence,
        slot,
        parent_slot,
        produced_ms,
        watch_hash: watch,
        bank_hash,
        parent_bank_hash,
        accounts,
        received: Instant::now(),
    }))
}

fn decode_pubkey(value: &str) -> Result<[u8; 32]> {
    bs58::decode(value)
        .into_vec()
        .map_err(|_| "direct public key encoding".to_string())?
        .try_into()
        .map_err(|_| "direct public key length".into())
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    bytes
        .get(offset..offset + 2)
        .ok_or_else(|| "direct u16 bounds".to_string())?
        .try_into()
        .map(u16::from_le_bytes)
        .map_err(|_| "direct u16 decode".into())
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "direct u32 bounds".to_string())?
        .try_into()
        .map(u32::from_le_bytes)
        .map_err(|_| "direct u32 decode".into())
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64> {
    bytes
        .get(offset..offset + 8)
        .ok_or_else(|| "direct u64 bounds".to_string())?
        .try_into()
        .map(u64::from_le_bytes)
        .map_err(|_| "direct u64 decode".into())
}

fn now_ms() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_millis()
        .try_into()
        .map_err(|_| "clock overflow".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn required_mode_is_explicitly_rpc_free() {
        let mode = DirectMode::parse("required").unwrap();
        assert!(mode.uses_direct_quotes());
        assert!(mode.forbids_rpc());
        assert_eq!(mode.as_str(), "required");
        assert!(DirectMode::parse("fallback").is_err());
    }

    fn bank_frame(keys: &[String], sequence: u64, slot: u64, byte: u8) -> Vec<u8> {
        let watch = watch_hash(keys).unwrap();
        let key = decode_pubkey(&keys[0]).unwrap();
        let owner = decode_pubkey("11111111111111111111111111111111").unwrap();
        let mut payload = Vec::new();
        payload.extend_from_slice(&key);
        payload.extend_from_slice(&owner);
        payload.extend_from_slice(&1u64.to_le_bytes());
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.push(byte);
        let payload_hash: [u8; 32] = Sha256::digest(&payload).into();
        let mut frame = Vec::new();
        frame.extend_from_slice(FRAME_MAGIC);
        frame.extend_from_slice(&WIRE_VERSION.to_le_bytes());
        frame.extend_from_slice(&KIND_BANK.to_le_bytes());
        frame.extend_from_slice(&REQUIRED_FLAGS.to_le_bytes());
        frame.extend_from_slice(&((FRAME_HEADER_BYTES + payload.len()) as u32).to_le_bytes());
        frame.extend_from_slice(&1u16.to_le_bytes());
        frame.extend_from_slice(&0u16.to_le_bytes());
        frame.extend_from_slice(&sequence.to_le_bytes());
        frame.extend_from_slice(&slot.to_le_bytes());
        frame.extend_from_slice(&(slot - 1).to_le_bytes());
        frame.extend_from_slice(&now_ms().unwrap().to_le_bytes());
        frame.extend_from_slice(&watch);
        frame.extend_from_slice(&decode_pubkey(MAINNET_GENESIS).unwrap());
        frame.extend_from_slice(&[byte; 32]);
        frame.extend_from_slice(&[byte.saturating_sub(1).max(1); 32]);
        frame.extend_from_slice(&payload_hash);
        frame.extend_from_slice(&payload);
        frame
    }

    #[test]
    fn request_is_bounded_and_commits_exact_watch_set() {
        let keys = vec!["So11111111111111111111111111111111111111112".to_string()];
        let hash = watch_hash(&keys).unwrap();
        let request = watch_request(&keys, hash).unwrap();
        assert_eq!(request.len(), REQUEST_HEADER_BYTES + 32);
        assert_eq!(&request[..8], REQUEST_MAGIC);
        assert_eq!(&request[16..48], &hash);
    }

    #[test]
    fn production_sized_watch_set_fits_one_coherent_request() {
        let keys = (0u16..189)
            .map(|index| {
                let mut key = [0u8; 32];
                key[..2].copy_from_slice(&index.to_le_bytes());
                bs58::encode(key).into_string()
            })
            .collect::<Vec<_>>();
        let hash = watch_hash(&keys).unwrap();
        let request = watch_request(&keys, hash).unwrap();
        assert_eq!(request.len(), REQUEST_HEADER_BYTES + 189 * 32);
    }

    #[test]
    fn bank_frame_projects_rpc_compatible_account_values() {
        let keys = vec!["So11111111111111111111111111111111111111112".to_string()];
        let bytes = bank_frame(&keys, 7, 20, 9);
        let snapshot =
            match read_frame(&mut Cursor::new(bytes), watch_hash(&keys).unwrap(), &keys).unwrap() {
                Frame::Bank(snapshot) => snapshot,
                _ => panic!("bank frame expected"),
            };
        let response = snapshot.response_for(&keys).unwrap();
        assert_eq!(response["context"]["slot"], 20);
        assert_eq!(response["value"][0]["data"][0], STANDARD.encode([9]));
    }

    #[test]
    fn corrupted_payload_fails_closed() {
        let keys = vec!["So11111111111111111111111111111111111111112".to_string()];
        let mut bytes = bank_frame(&keys, 7, 20, 9);
        *bytes.last_mut().unwrap() ^= 1;
        assert!(read_frame(&mut Cursor::new(bytes), watch_hash(&keys).unwrap(), &keys).is_err());
    }
}
