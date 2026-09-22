use crate::provider::{Budget, Metrics};
use crate::Result;
use serde_json::{json, Value};
use std::{io::Read, sync::Arc, time::Duration};
#[derive(Clone)]
pub struct Rpc {
    client: reqwest::blocking::Client,
    url: String,
    genesis: String,
    maximum_response_bytes: usize,
    budget: Option<Arc<Budget>>,
}
impl Rpc {
    pub fn provider_metrics(&self) -> Option<Metrics> {
        self.budget
            .as_ref()
            .and_then(|budget| budget.snapshot().ok())
    }

    pub(crate) fn pinned_provider(
        url: String,
        genesis: String,
        budget: Arc<Budget>,
    ) -> Result<Self> {
        Self::build(
            url,
            genesis,
            budget.max_response_bytes(),
            Duration::from_millis(500),
            Duration::from_millis(1500),
            Some(budget),
        )
    }
    pub fn genesis(&self) -> &str {
        &self.genesis
    }
    pub fn pinned(url: String, genesis: String) -> Result<Self> {
        Self::pinned_with_response_budget(url, genesis, 8 * 1024 * 1024)
    }
    pub fn pinned_with_response_budget(
        url: String,
        genesis: String,
        maximum_response_bytes: usize,
    ) -> Result<Self> {
        Self::pinned_with_limits(
            url,
            genesis,
            maximum_response_bytes,
            Duration::from_secs(2),
            Duration::from_secs(5),
        )
    }

    /// Quote-state polling must fail fast enough to retry before its coherent
    /// bank ages out. Preparation and submission keep the wider default RPC
    /// deadline because they do not serve the hot quote bank.
    pub fn pinned_live_feed(url: String, genesis: String) -> Result<Self> {
        Self::pinned_with_limits(
            url,
            genesis,
            8 * 1024 * 1024,
            // Public endpoints often answer coherent account reads in more
            // than 500 ms. This client runs only in the background publisher,
            // so a wider bounded deadline improves availability without
            // adding latency to cached user quotes.
            Duration::from_millis(500),
            Duration::from_millis(1_500),
        )
    }

    /// Reuse an already verified endpoint and genesis pin with the wider read
    /// deadline needed by wallet portfolio scans. This creates no additional
    /// RPC request, so bringing up the public portfolio path cannot consume a
    /// second genesis-check allowance or restart the hot quote feed.
    pub fn relaxed_read_clone(&self) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            url: self.url.clone(),
            genesis: self.genesis.clone(),
            maximum_response_bytes: self.maximum_response_bytes,
            budget: self.budget.clone(),
        })
    }

    fn pinned_with_limits(
        url: String,
        genesis: String,
        maximum_response_bytes: usize,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self> {
        Self::build(
            url,
            genesis,
            maximum_response_bytes,
            connect_timeout,
            request_timeout,
            None,
        )
    }

    fn build(
        url: String,
        genesis: String,
        maximum_response_bytes: usize,
        connect_timeout: Duration,
        request_timeout: Duration,
        budget: Option<Arc<Budget>>,
    ) -> Result<Self> {
        let parsed = reqwest::Url::parse(&url).map_err(|_| "invalid RPC endpoint")?;
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.fragment().is_some()
        {
            return Err(
                "RPC endpoint credentials must use provider query or path, not userinfo".into(),
            );
        }
        if parsed.scheme() != "https"
            && !(parsed.scheme() == "http"
                && matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")))
        {
            return Err("RPC must be HTTPS or loopback".into());
        }
        if !(1..=32 * 1024 * 1024).contains(&maximum_response_bytes) {
            return Err("RPC response budget bounds".into());
        }
        if connect_timeout.is_zero()
            || request_timeout.is_zero()
            || connect_timeout > request_timeout
            || request_timeout > Duration::from_secs(5)
        {
            return Err("RPC timeout bounds".into());
        }
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        let r = Self {
            client,
            url,
            genesis,
            maximum_response_bytes,
            budget,
        };
        r.check_genesis()?;
        Ok(r)
    }
    pub fn check_genesis(&self) -> Result<()> {
        if self.call("getGenesisHash", json!([]))?.as_str() != Some(&self.genesis) {
            Err("wrong genesis".into())
        } else {
            Ok(())
        }
    }

    /// A later blockhash cannot be paired with an earlier captured execution
    /// bank. The caller retries the whole prepare pipeline when the node has
    /// already advanced.
    pub fn latest_blockhash_at(&self, slot: u64) -> Result<([u8; 32], u64)> {
        let result = self.call(
            "getLatestBlockhash",
            json!([{"commitment":"confirmed","minContextSlot":slot}]),
        )?;
        if result["context"]["slot"].as_u64() != Some(slot) {
            return Err("blockhash bank changed; rebuild".into());
        }
        let blockhash = result["value"]["blockhash"]
            .as_str()
            .ok_or("latest blockhash missing")?;
        let blockhash: [u8; 32] = bs58::decode(blockhash)
            .into_vec()
            .map_err(|_| "latest blockhash encoding")?
            .try_into()
            .map_err(|_| "latest blockhash length")?;
        let last_valid_height = result["value"]["lastValidBlockHeight"]
            .as_u64()
            .ok_or("last valid block height missing")?;
        if blockhash == [0; 32] || last_valid_height == 0 {
            return Err("latest blockhash invalid".into());
        }
        Ok((blockhash, last_valid_height))
    }
    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
                .map_err(|_| "RPC request encoding")?;
        let mut permit = self
            .budget
            .as_ref()
            .map(|budget| budget.admit(method, body.len()))
            .transpose()?;
        let response = self
            .client
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .map_err(|error| {
                if let Some(permit) = &permit {
                    permit.backoff(1);
                }
                if error.is_timeout() {
                    "RPC request timed out"
                } else {
                    "RPC transport failed"
                }
            })?;
        let status = response.status();
        if !status.is_success() {
            if let Some(permit) = &permit {
                if status.as_u16() == 429 || status.is_server_error() {
                    let seconds = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(2);
                    permit.backoff(seconds);
                }
            }
            // Never forward provider text or reqwest errors containing the endpoint.
            return Err(format!("RPC HTTP status {}", status.as_u16()));
        }
        let mut bytes = Vec::new();
        let read = response
            .take(self.maximum_response_bytes as u64 + 1)
            .read_to_end(&mut bytes);
        if let Some(permit) = &mut permit {
            permit.received(bytes.len());
        }
        read.map_err(|_| "RPC response read failed")?;
        if bytes.len() > self.maximum_response_bytes {
            return Err("RPC response budget exceeded".into());
        }
        let v: Value = serde_json::from_slice(&bytes).map_err(|_| "RPC response JSON invalid")?;
        if v["jsonrpc"] != "2.0" || v["id"] != 1 {
            return Err("RPC response identity mismatch".into());
        }
        if let Some(error) = v.get("error").filter(|error| !error.is_null()) {
            return Err(format!(
                "RPC rejected (code {})",
                error["code"].as_i64().unwrap_or(0)
            ));
        }
        let result = v.get("result").cloned().ok_or("missing RPC result")?;
        if let Some(permit) = &mut permit {
            permit.complete();
        }
        Ok(result)
    }
}
