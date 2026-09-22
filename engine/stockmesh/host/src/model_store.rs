//! Atomic persistence for replay watermarks and conservative receipt feedback.
//! The configured policy and signing keys are reconstructed independently;
//! snapshots carry state only and fail closed on corruption. Generation is
//! monotonic while one store owns the lock, but rollback resistance requires
//! an external durable high-water mark (for example a signed receipt ledger).
use crate::{fair::Watermarks, receipt::CostState, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

const MAGIC: &[u8; 8] = b"SKWMODL1";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelState {
    pub generation: u64,
    pub fair: Watermarks,
    pub cost: CostState,
}

pub struct ModelStore {
    path: PathBuf,
    _lock: File,
    generation: u64,
    max_bytes: usize,
}

impl ModelStore {
    pub fn open(path: &Path, max_bytes: usize) -> Result<(Self, Option<ModelState>)> {
        if !(4096..=4 * 1024 * 1024).contains(&max_bytes) {
            return Err("model store bounds".into());
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut lock_name = path.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(PathBuf::from(lock_name))
            .map_err(|e| e.to_string())?;
        lock.try_lock_exclusive()
            .map_err(|_| "model store already owned")?;
        File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(|e| e.to_string())?;
        let state = if path.exists() {
            Some(read(path, max_bytes)?)
        } else {
            None
        };
        let generation = state.as_ref().map_or(0, |value| value.generation);
        Ok((
            Self {
                path: path.to_path_buf(),
                _lock: lock,
                generation,
                max_bytes,
            },
            state,
        ))
    }

    pub fn save(&mut self, fair: Watermarks, cost: CostState) -> Result<ModelState> {
        let state = ModelState {
            generation: self
                .generation
                .checked_add(1)
                .ok_or("model generation overflow")?,
            fair,
            cost,
        };
        let body = serde_json::to_vec(&state).map_err(|e| e.to_string())?;
        if body.len() + 16 > self.max_bytes {
            return Err("model snapshot disk budget".into());
        }
        let mut bytes = Vec::with_capacity(body.len() + 16);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        bytes.extend_from_slice(&body);
        let mut next_name = self.path.as_os_str().to_os_string();
        next_name.push(".next");
        let next = PathBuf::from(next_name);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&next)
            .map_err(|e| e.to_string())?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        std::fs::rename(&next, &self.path).map_err(|e| e.to_string())?;
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)
            .and_then(|parent| parent.sync_all())
            .map_err(|e| e.to_string())?;
        self.generation = state.generation;
        Ok(state)
    }
}

fn read(path: &Path, max_bytes: usize) -> Result<ModelState> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(false)
        .mode(0o600)
        .open(path)
        .map_err(|e| e.to_string())?;
    let length = usize::try_from(file.metadata().map_err(|e| e.to_string())?.len())
        .map_err(|_| "model snapshot size")?;
    if length < 16 || length > max_bytes {
        return Err("model snapshot size".into());
    }
    let mut bytes = Vec::with_capacity(length);
    file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if &bytes[..8] != MAGIC {
        return Err("model snapshot magic".into());
    }
    let size = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let checksum = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    if size != bytes.len() - 16 || crc32fast::hash(&bytes[16..]) != checksum {
        return Err("model snapshot corruption".into());
    }
    let state: ModelState = serde_json::from_slice(&bytes[16..]).map_err(|e| e.to_string())?;
    if state.generation == 0 {
        return Err("model snapshot generation".into());
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    fn path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("skew-model-{name}-{}", std::process::id()))
    }
    fn state() -> (Watermarks, CostState) {
        (
            Watermarks {
                policy_hash: [7; 32],
                latest: BTreeMap::from([(1, (9, [8; 32]))]),
                last_clock: Some((100, 1000)),
            },
            CostState {
                epoch: 1,
                seen: BTreeSet::from(["signature".into()]),
                routes: vec![([6; 32], 1, 20)],
            },
        )
    }
    #[test]
    fn atomic_snapshot_recovers_and_detects_corruption() {
        let path = path("recover");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("lock"));
        let (fair, cost) = state();
        {
            let (mut store, prior) = ModelStore::open(&path, 65536).unwrap();
            assert!(prior.is_none());
            assert_eq!(
                store.save(fair.clone(), cost.clone()).unwrap().generation,
                1
            );
            assert_eq!(
                store.save(fair.clone(), cost.clone()).unwrap().generation,
                2
            );
        }
        {
            let (_store, recovered) = ModelStore::open(&path, 65536).unwrap();
            let recovered = recovered.unwrap();
            assert_eq!(recovered.generation, 2);
            assert_eq!(recovered.fair, fair);
            assert_eq!(recovered.cost, cost);
        }
        let mut bytes = std::fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(&path, bytes).unwrap();
        assert!(ModelStore::open(&path, 65536).is_err());
        let _ = std::fs::remove_file(&path);
        let mut lock = path.as_os_str().to_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(PathBuf::from(lock));
    }
}
