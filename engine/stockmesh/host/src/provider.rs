//! A run-scoped, shared admission boundary for hosted state readers.
//! This limits our traffic, not the provider's billing or monthly invoice.
use crate::{rpc::Rpc, Result};
use crate::provider_quota::{DurableQuota, QuotaConfig, QuotaMetrics};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub provider_id: String,
    pub endpoint_file: String,
    pub expected_genesis: String,
    pub requests_per_second: u32,
    pub max_requests: u64,
    pub max_in_flight: u32,
    pub max_response_bytes: usize,
    pub max_total_response_bytes: u64,
    #[serde(default)]
    pub durable_quota: Option<QuotaConfig>,
}

impl ProviderConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = read_bounded(path, 16 * 1024)?;
        let config: Self =
            serde_json::from_slice(&bytes).map_err(|_| "invalid provider profile")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.provider_id.is_empty()
            || self.provider_id.len() > 32
            || !self
                .provider_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            || !(1..=100).contains(&self.requests_per_second)
            || !(1..=1_000_000).contains(&self.max_requests)
            || !(1..=16).contains(&self.max_in_flight)
            || !(1..=32 * 1024 * 1024).contains(&self.max_response_bytes)
            || self.max_total_response_bytes < self.max_response_bytes as u64 + 1
            || self.max_total_response_bytes > 64 * 1024 * 1024 * 1024
            || bs58::decode(&self.expected_genesis)
                .into_vec()
                .map_or(true, |v| v.len() != 32)
            || !Path::new(&self.endpoint_file).is_absolute()
        {
            return Err("provider profile bounds".into());
        }
        if let Some(quota) = &self.durable_quota {
            quota.validate()?;
        }
        Ok(())
    }

    pub fn connect(&self) -> Result<Rpc> {
        self.validate()?;
        let path = Path::new(&self.endpoint_file);
        let metadata =
            std::fs::symlink_metadata(path).map_err(|_| "provider endpoint unavailable")?;
        if !metadata.file_type().is_file() {
            return Err("provider endpoint must be a private regular file".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(
                    "provider endpoint file permissions must exclude group and others".into(),
                );
            }
        }
        let bytes = read_bounded(path, 4096)?;
        let endpoint = std::str::from_utf8(&bytes)
            .map_err(|_| "provider endpoint encoding")?
            .trim();
        Rpc::pinned_provider(
            endpoint.into(),
            self.expected_genesis.clone(),
            Budget::new(self.clone())?,
        )
    }
}

fn read_bounded(path: &Path, max: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| "provider file unavailable")?
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "provider file read failed")?;
    if bytes.len() > max {
        return Err("provider file size bound".into());
    }
    Ok(bytes)
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Metrics {
    pub provider_id: String,
    pub requests: u64,
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub rejected: u64,
    pub completed: u64,
    pub failed: u64,
    pub in_flight: u32,
    pub cooldown_remaining_ms: u64,
    pub max_requests: u64,
    pub max_total_response_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub durable_quota: Option<QuotaMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub durable_quota_error: Option<String>,
}

struct State {
    metrics: Metrics,
    window: Instant,
    window_requests: u32,
    reserved_bytes: u64,
    cooldown: Option<Instant>,
}

pub struct Budget {
    config: ProviderConfig,
    durable_quota: Option<DurableQuota>,
    state: Mutex<State>,
}

impl Budget {
    pub fn new(config: ProviderConfig) -> Result<Arc<Self>> {
        config.validate()?;
        let durable_quota = config.durable_quota.clone()
            .map(|quota| DurableQuota::new(quota, &config.provider_id, &config.expected_genesis))
            .transpose()?;
        if let Some(quota) = &durable_quota {
            quota.snapshot()?;
        }
        let metrics = Metrics {
            provider_id: config.provider_id.clone(),
            max_requests: config.max_requests,
            max_total_response_bytes: config.max_total_response_bytes,
            ..Metrics::default()
        };
        Ok(Arc::new(Self {
            config,
            durable_quota,
            state: Mutex::new(State {
                metrics,
                window: Instant::now(),
                window_requests: 0,
                reserved_bytes: 0,
                cooldown: None,
            }),
        }))
    }

    pub fn max_response_bytes(&self) -> usize {
        self.config.max_response_bytes
    }

    pub fn snapshot(&self) -> Result<Metrics> {
        let state = self
            .state
            .lock()
            .map_err(|_| "provider budget unavailable")?;
        let mut metrics = state.metrics.clone();
        metrics.cooldown_remaining_ms = state
            .cooldown
            .and_then(|until| until.checked_duration_since(Instant::now()))
            .map_or(0, |duration| duration.as_millis() as u64);
        // Accounting unavailability must never erase the bounded-provider mode.
        // Admissions still fail closed; status preserves the local counters.
        if let Some(quota) = &self.durable_quota {
            match quota.snapshot() {
                Ok(snapshot) => metrics.durable_quota = Some(snapshot),
                Err(error) => metrics.durable_quota_error = Some(error),
            }
        }
        Ok(metrics)
    }

    pub fn admit(self: &Arc<Self>, method: &str, request_bytes: usize) -> Result<Permit> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "provider budget unavailable")?;
        let now = Instant::now();
        if now.duration_since(state.window) >= Duration::from_secs(1) {
            state.window = now;
            state.window_requests = 0;
        }
        let reservation = self.config.max_response_bytes as u64 + 1;
        let rejection = if !matches!(
            method,
            "getGenesisHash"
                | "getSlot"
                | "getBlockHeight"
                | "getLatestBlockhash"
                | "getAccountInfo"
                | "getMultipleAccounts"
                | "getBalance"
                | "getMinimumBalanceForRentExemption"
                | "getTokenAccountsByOwner"
                | "getSignatureStatuses"
        ) {
            Some("provider read-only method denied")
        } else if request_bytes > 1024 * 1024 {
            Some("provider request byte budget exceeded")
        } else if state.metrics.requests >= self.config.max_requests {
            Some("provider run request budget exhausted")
        } else if state.metrics.response_bytes + state.reserved_bytes + reservation
            > self.config.max_total_response_bytes
        {
            Some("provider run response budget exhausted")
        } else if state.cooldown.is_some_and(|until| until > now) {
            Some("provider cooling down")
        } else if state.metrics.in_flight >= self.config.max_in_flight {
            Some("provider in-flight limit reached")
        } else if state.window_requests >= self.config.requests_per_second {
            Some("provider rate limit reached")
        } else {
            None
        };
        if let Some(reason) = rejection {
            state.metrics.rejected += 1;
            return Err(reason.into());
        }
        if let Some(quota) = &self.durable_quota {
            if let Err(error) = quota.reserve(reservation + request_bytes as u64) {
                state.metrics.rejected += 1;
                return Err(error);
            }
        }
        state.metrics.requests += 1;
        state.metrics.request_bytes += request_bytes as u64;
        state.metrics.in_flight += 1;
        state.window_requests += 1;
        state.reserved_bytes += reservation;
        Ok(Permit {
            budget: Arc::clone(self),
            bytes: 0,
            success: false,
        })
    }
}

pub struct Permit {
    budget: Arc<Budget>,
    bytes: u64,
    success: bool,
}

impl Permit {
    pub fn received(&mut self, bytes: usize) {
        self.bytes = bytes as u64;
    }
    pub fn complete(&mut self) {
        self.success = true;
    }
    pub fn backoff(&self, seconds: u64) {
        if let Ok(mut state) = self.budget.state.lock() {
            let until = Instant::now() + Duration::from_secs(seconds.clamp(1, 30));
            state.cooldown = Some(state.cooldown.map_or(until, |previous| previous.max(until)));
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.budget.state.lock() {
            state.metrics.in_flight -= 1;
            state.reserved_bytes -= self.budget.config.max_response_bytes as u64 + 1;
            state.metrics.response_bytes += self.bytes;
            if self.success {
                state.metrics.completed += 1;
            } else {
                state.metrics.failed += 1;
            }
        }
    }
}
