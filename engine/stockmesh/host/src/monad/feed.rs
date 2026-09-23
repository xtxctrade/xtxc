//! Read-only, bounded Monad state feed. A token identity is never a quote.
use crate::{
    monad_contract::{Catalog, MAINNET_CHAIN_ID},
    Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};
use tungstenite::{connect, Message};

const MAX_BACKFILL: u64 = 64;
const MAX_TOKENS: usize = 128;

pub trait Rpc {
    fn call(&mut self, method: &str, params: Value) -> Result<Value>;
    fn used(&self) -> u32;
}

/// Endpoint is supplied at runtime, never embedded in evidence or errors.
pub struct BoundedRpc {
    endpoint: String,
    client: reqwest::blocking::Client,
    used: u32,
    limit: u32,
}

impl BoundedRpc {
    pub fn new(endpoint: String, limit: u32) -> Result<Self> {
        if !endpoint.starts_with("https://") || endpoint.len() > 512 || !(1..=512).contains(&limit)
        {
            return Err("invalid Monad provider configuration".into());
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(8))
            .build()
            .map_err(|_| "Monad provider client")?;
        Ok(Self {
            endpoint,
            client,
            used: 0,
            limit,
        })
    }
}

impl Rpc for BoundedRpc {
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        if self.used >= self.limit {
            return Err("Monad provider call budget exhausted".into());
        }
        if !matches!(
            method,
            "eth_chainId"
                | "eth_getBlockByNumber"
                | "eth_getBlockByHash"
                | "eth_getCode"
                | "eth_getStorageAt"
                | "eth_call"
                | "eth_getLogs"
        ) {
            return Err("Monad read-only method required".into());
        }
        self.used += 1;
        let body = json!({"jsonrpc":"2.0","id":self.used,"method":method,"params":params});
        let response = self
            .client
            .post(&self.endpoint)
            .json(&body)
            .send()
            .map_err(|_| "Monad provider unavailable")?;
        if !response.status().is_success() {
            return Err("Monad provider HTTP failure".into());
        }
        let value: Value = response
            .json()
            .map_err(|_| "Monad provider malformed JSON")?;
        if value.get("id") != Some(&json!(self.used)) || value.get("error").is_some() {
            return Err("Monad provider JSON-RPC failure".into());
        }
        value
            .get("result")
            .cloned()
            .ok_or_else(|| "Monad provider missing result".into())
    }
    fn used(&self) -> u32 {
        self.used
    }
}

fn hex_u64(value: &str) -> Result<u64> {
    u64::from_str_radix(value.strip_prefix("0x").ok_or("missing hex prefix")?, 16)
        .map_err(|_| "invalid hexadecimal integer".into())
}
fn hash(value: &str) -> Result<String> {
    if value.len() != 66
        || !value.starts_with("0x")
        || !value[2..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("invalid block hash".into());
    }
    Ok(value.to_ascii_lowercase())
}
fn block_number(number: u64) -> String {
    format!("0x{number:x}")
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BlockRef {
    pub number: u64,
    pub hash: String,
    pub parent_hash: String,
    pub timestamp: u64,
}

impl BlockRef {
    fn parse(value: Value) -> Result<Self> {
        Ok(Self {
            number: hex_u64(value["number"].as_str().ok_or("block number absent")?)?,
            hash: hash(value["hash"].as_str().ok_or("block hash absent")?)?,
            parent_hash: hash(value["parentHash"].as_str().ok_or("parent hash absent")?)?,
            timestamp: hex_u64(value["timestamp"].as_str().ok_or("timestamp absent")?)?,
        })
    }
}

pub fn chain_guard(rpc: &mut impl Rpc) -> Result<()> {
    let chain = rpc.call("eth_chainId", json!([]))?;
    if hex_u64(chain.as_str().ok_or("chain ID absent")?)? != MAINNET_CHAIN_ID {
        return Err("Monad chain mismatch".into());
    }
    Ok(())
}
pub fn read_block(rpc: &mut impl Rpc, number: u64) -> Result<BlockRef> {
    BlockRef::parse(rpc.call("eth_getBlockByNumber", json!([block_number(number), false]))?)
}
pub fn read_latest(rpc: &mut impl Rpc) -> Result<BlockRef> {
    BlockRef::parse(rpc.call("eth_getBlockByNumber", json!(["latest", false]))?)
}

/// A gap, changed parent or duplicate height with different hash invalidates
/// every quote derived from the old head. Only contiguous replay makes it ready.
#[derive(Default)]
pub struct HeadTracker {
    chain: VecDeque<BlockRef>,
    ready: bool,
}
impl HeadTracker {
    pub fn head(&self) -> Option<&BlockRef> {
        self.chain.back()
    }
    pub fn ready(&self) -> bool {
        self.ready
    }
    pub fn invalidate(&mut self) {
        self.ready = false;
    }
    pub fn reset(&mut self, anchor: BlockRef) {
        self.chain.clear();
        self.chain.push_back(anchor);
        self.ready = true;
    }
    pub fn push(&mut self, next: BlockRef) -> Result<()> {
        let Some(last) = self.head() else {
            self.reset(next);
            return Ok(());
        };
        if let Some(known) = self.chain.iter().find(|b| b.number == next.number) {
            if known.hash == next.hash {
                return Ok(());
            }
            self.invalidate();
            return Err("Monad known height changed: snapshot invalidated".into());
        }
        if next.number < last.number {
            return Ok(());
        } // older finalized update outside the retained window
        if last.number.checked_add(1) != Some(next.number) || last.hash != next.parent_hash {
            self.invalidate();
            return Err("Monad chain gap or reorg: snapshot invalidated".into());
        }
        self.chain.push_back(next);
        while self.chain.len() > MAX_BACKFILL as usize + 1 {
            self.chain.pop_front();
        }
        self.ready = true;
        Ok(())
    }
    /// Reconnect from a retained ancestor; never jump over an unknown block.
    pub fn recover(&mut self, rpc: &mut impl Rpc, target: u64) -> Result<()> {
        let mut ancestor = None;
        for known in self.chain.iter().rev() {
            if known.number > target {
                continue;
            }
            let canonical = read_block(rpc, known.number)?;
            if known.hash == canonical.hash {
                ancestor = Some(known.clone());
                break;
            }
        }
        let anchor = ancestor.ok_or("Monad common ancestor not retained")?;
        if target.saturating_sub(anchor.number) > MAX_BACKFILL {
            self.invalidate();
            return Err("Monad backfill bound exceeded".into());
        }
        self.reset(anchor.clone());
        for height in anchor.number + 1..=target {
            let block = read_block(rpc, height)?;
            self.push(block)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitState {
    Proposed,
    Voted,
    Finalized,
    Verified,
}
impl CommitState {
    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "Proposed" => Ok(Self::Proposed),
            "Voted" => Ok(Self::Voted),
            "Finalized" => Ok(Self::Finalized),
            "Verified" => Ok(Self::Verified),
            _ => Err("unknown Monad commitment state".into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadEvent {
    pub block: BlockRef,
    pub block_id: String,
    pub commit_state: CommitState,
}
impl HeadEvent {
    pub fn from_notification(value: &Value) -> Result<Self> {
        if value["method"] != "eth_subscription" {
            return Err("Monad head subscription required".into());
        }
        let body = &value["params"]["result"];
        let block_id = body["blockId"].as_str().ok_or("Monad block ID absent")?;
        if block_id.is_empty() || block_id.len() > 132 {
            return Err("invalid Monad block ID".into());
        }
        Ok(Self {
            block: BlockRef::parse(body.clone())?,
            block_id: block_id.into(),
            commit_state: CommitState::parse(
                body["commitState"].as_str().ok_or("commit state absent")?,
            )?,
        })
    }
}

impl HeadTracker {
    pub fn accept_event(&mut self, event: &HeadEvent) -> Result<()> {
        if self.head().is_none() && event.commit_state != CommitState::Proposed {
            return Ok(()); // initial older commitment is not an executable head
        }
        self.push(event.block.clone())
    }
}

/// One bounded WS observation. The caller owns reconnect policy and must
/// invalidate quotes if the connection closes or a fork/gap is observed.
pub fn observe_ws_heads(
    url: &str,
    max_events: u8,
    tracker: &mut HeadTracker,
) -> Result<Vec<HeadEvent>> {
    if !url.starts_with("wss://") || url.len() > 512 || !(1..=32).contains(&max_events) {
        return Err("invalid Monad WS configuration".into());
    }
    let outcome = (|| {
        let (mut socket, response) = connect(url).map_err(|_| "Monad WS unavailable")?;
        if response.status().as_u16() != 101 {
            return Err("Monad WS upgrade failed".into());
        }
        socket
            .send(Message::Text(
                json!({"jsonrpc":"2.0","id":1,"method":"eth_subscribe",
            "params":["monadNewHeads"]})
                .to_string()
                .into(),
            ))
            .map_err(|_| "Monad WS subscribe send failed")?;
        let ack = match socket
            .read()
            .map_err(|_| "Monad WS subscribe reply missing")?
        {
            Message::Text(text) => serde_json::from_str::<Value>(&text)
                .map_err(|_| "Monad WS subscribe reply malformed")?,
            _ => return Err("Monad WS subscribe reply unexpected".into()),
        };
        let subscription = ack["result"]
            .as_str()
            .ok_or("Monad WS subscription denied")?;
        if ack["id"] != 1 || subscription.len() > 132 || subscription.is_empty() {
            return Err("Monad WS subscription invalid".into());
        }
        let mut events = Vec::with_capacity(max_events as usize);
        while events.len() < max_events as usize {
            match socket.read().map_err(|_| "Monad WS stream interrupted")? {
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(&text)
                        .map_err(|_| "Monad WS notification malformed")?;
                    if value["params"]["subscription"] != subscription {
                        return Err("Monad WS subscription mismatch".into());
                    }
                    let event = HeadEvent::from_notification(&value)?;
                    tracker.accept_event(&event)?;
                    events.push(event);
                }
                Message::Ping(bytes) => socket
                    .send(Message::Pong(bytes))
                    .map_err(|_| "Monad WS pong failed")?,
                Message::Close(_) => return Err("Monad WS stream closed".into()),
                Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
        let _ = socket.close(None);
        Ok(events)
    })();
    if outcome.is_err() {
        tracker.invalidate();
    }
    outcome
}

/// One shared hot set per process. A selected cold asset warms once; separate
/// viewers do not open separate provider subscriptions.
pub struct WatchSet {
    hot: BTreeMap<String, u32>,
    max_hot: usize,
    provider_calls: u64,
}
impl WatchSet {
    pub fn new(max_hot: usize) -> Result<Self> {
        if !(1..=32).contains(&max_hot) {
            return Err("invalid hot asset capacity".into());
        }
        Ok(Self {
            hot: BTreeMap::new(),
            max_hot,
            provider_calls: 0,
        })
    }
    pub fn watch(&mut self, asset_id: &str) -> Result<bool> {
        if asset_id.len() > 256 || !asset_id.starts_with("eip155:143:erc20:") {
            return Err("invalid Monad watched asset".into());
        }
        if let Some(viewers) = self.hot.get_mut(asset_id) {
            *viewers = viewers.checked_add(1).ok_or("watcher overflow")?;
            return Ok(false);
        }
        if self.hot.len() == self.max_hot {
            return Err("hot asset capacity reached".into());
        }
        self.hot.insert(asset_id.to_owned(), 1);
        Ok(true)
    }
    pub fn unwatch(&mut self, asset_id: &str) {
        if let Some(viewers) = self.hot.get_mut(asset_id) {
            if *viewers > 1 {
                *viewers -= 1;
                return;
            }
        }
        self.hot.remove(asset_id);
    }
    pub fn charge_provider_calls(&mut self, calls: u32) -> Result<()> {
        self.provider_calls = self
            .provider_calls
            .checked_add(u64::from(calls))
            .ok_or("provider call overflow")?;
        Ok(())
    }
    pub fn hot_count(&self) -> usize {
        self.hot.len()
    }
    pub fn provider_calls(&self) -> u64 {
        self.provider_calls
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenState {
    pub instrument_id: String,
    pub issuer: String,
    pub token_address: String,
    pub deployed: bool,
    pub code_sha256: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MarketSnapshot {
    pub schema: String,
    pub chain_id: u64,
    pub block: BlockRef,
    pub tokens: Vec<TokenState>,
    pub provider_calls: u32,
    pub executable_buy: u32,
    pub executable_sell: u32,
    pub wallet_delivery: u32,
    pub etf_eligible: u32,
}

/// Reads only deployed bytecode at one numbered block. The final header check
/// rejects a mixed-fork scan; this never grants product/trading admission.
pub fn scan_catalog(
    rpc: &mut impl Rpc,
    catalog: &Catalog,
    block: BlockRef,
) -> Result<MarketSnapshot> {
    catalog.validate()?;
    if catalog.token_observations.len() > MAX_TOKENS {
        return Err("token scan bound exceeded".into());
    }
    let before = rpc.used();
    let mut tokens = Vec::with_capacity(catalog.token_observations.len());
    for token in &catalog.token_observations {
        let code = rpc.call(
            "eth_getCode",
            json!([token.token_address, block_number(block.number)]),
        )?;
        let code = code.as_str().ok_or("invalid token bytecode")?;
        let bytes = code.strip_prefix("0x").ok_or("invalid token bytecode")?;
        if bytes.len() % 2 != 0 || !bytes.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid token bytecode".into());
        }
        let deployed = !bytes.is_empty();
        let code_sha256 = if deployed {
            let decoded = (0..bytes.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&bytes[i..i + 2], 16).unwrap())
                .collect::<Vec<_>>();
            Some(format!("{:x}", Sha256::digest(decoded)))
        } else {
            None
        };
        tokens.push(TokenState {
            instrument_id: token.instrument_id.clone(),
            issuer: token.issuer.clone(),
            token_address: token.token_address.clone(),
            deployed,
            code_sha256,
        });
    }
    let after = read_block(rpc, block.number)?;
    if after.hash != block.hash {
        return Err("Monad block changed during token scan".into());
    }
    Ok(MarketSnapshot {
        schema: "xtxc.monad.market-state/v1".into(),
        chain_id: MAINNET_CHAIN_ID,
        block,
        tokens,
        provider_calls: rpc.used() - before,
        executable_buy: 0,
        executable_sell: 0,
        wallet_delivery: 0,
        etf_eligible: 0,
    })
}

/// A bounded log range is accepted only if its canonical block references are
/// known to the tracker. Empty results do not prove that a market is liquid.
pub fn read_logs(
    rpc: &mut impl Rpc,
    tracker: &mut HeadTracker,
    from: u64,
    to: u64,
    address: &str,
) -> Result<Vec<Value>> {
    let result = (|| {
        if from > to
            || to - from >= MAX_BACKFILL
            || address.len() != 42
            || !address.starts_with("0x")
            || !address[2..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("invalid bounded log request".into());
        }
        let logs = rpc.call(
            "eth_getLogs",
            json!([{"fromBlock":block_number(from),"toBlock":block_number(to),"address":address}]),
        )?;
        let logs = logs.as_array().ok_or("invalid logs")?;
        for log in logs {
            let number = hex_u64(
                log["blockNumber"]
                    .as_str()
                    .ok_or("log block number absent")?,
            )?;
            let fingerprint = hash(log["blockHash"].as_str().ok_or("log block hash absent")?)?;
            if number < from
                || number > to
                || !tracker
                    .chain
                    .iter()
                    .any(|b| b.number == number && b.hash == fingerprint)
            {
                return Err("Monad log references unknown fork".into());
            }
        }
        Ok(logs.clone())
    })();
    if result.is_err() {
        tracker.invalidate();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        replies: VecDeque<Value>,
        calls: u32,
    }
    impl Fake {
        fn new(values: Vec<Value>) -> Self {
            Self {
                replies: values.into(),
                calls: 0,
            }
        }
    }
    impl Rpc for Fake {
        fn call(&mut self, _: &str, _: Value) -> Result<Value> {
            self.calls += 1;
            self.replies.pop_front().ok_or("fake exhausted".into())
        }
        fn used(&self) -> u32 {
            self.calls
        }
    }
    fn block(n: u64, h: char, p: char) -> BlockRef {
        BlockRef {
            number: n,
            hash: format!("0x{}", h.to_string().repeat(64)),
            parent_hash: format!("0x{}", p.to_string().repeat(64)),
            timestamp: n,
        }
    }
    fn wire(b: &BlockRef) -> Value {
        json!({"number":block_number(b.number),"hash":b.hash,"parentHash":b.parent_hash,"timestamp":block_number(b.timestamp)})
    }
    #[test]
    fn gap_and_reorg_fail_closed_then_recover() {
        let mut tracker = HeadTracker::default();
        tracker.reset(block(7, 'a', '0'));
        assert!(tracker.push(block(9, 'c', 'b')).is_err());
        assert!(!tracker.ready());
        let mut rpc = Fake::new(vec![
            wire(&block(7, 'a', '0')),
            wire(&block(8, 'b', 'a')),
            wire(&block(9, 'c', 'b')),
        ]);
        tracker.recover(&mut rpc, 9).unwrap();
        assert!(tracker.ready());
        assert_eq!(tracker.head().unwrap().number, 9);
        assert!(tracker.push(block(10, 'd', 'e')).is_err());
        assert!(!tracker.ready());
    }
    #[test]
    fn older_commit_update_does_not_invalidate_newer_proposal() {
        let mut tracker = HeadTracker::default();
        let old = HeadEvent {
            block: block(7, 'a', '0'),
            block_id: "old".into(),
            commit_state: CommitState::Finalized,
        };
        tracker.accept_event(&old).unwrap();
        assert!(!tracker.ready());
        tracker
            .accept_event(&HeadEvent {
                block: block(8, 'b', 'a'),
                block_id: "new".into(),
                commit_state: CommitState::Proposed,
            })
            .unwrap();
        tracker.accept_event(&old).unwrap();
        assert!(tracker.ready());
    }
    #[test]
    fn block_change_rejects_snapshot() {
        let mut catalog: Catalog =
            serde_json::from_str(include_str!("../../../monad/catalog/registry.v1.json")).unwrap();
        catalog.token_observations.truncate(1);
        let mut rpc = Fake::new(vec![json!("0x6001"), wire(&block(7, 'b', '0'))]);
        assert!(scan_catalog(&mut rpc, &catalog, block(7, 'a', '0')).is_err());
    }
    #[test]
    fn foreign_log_invalidates_tracker() {
        let mut tracker = HeadTracker::default();
        tracker.reset(block(7, 'a', '0'));
        let mut rpc = Fake::new(vec![
            json!([{"blockNumber":"0x7","blockHash":format!("0x{}","b".repeat(64))}]),
        ]);
        assert!(read_logs(
            &mut rpc,
            &mut tracker,
            7,
            7,
            "0x1111111111111111111111111111111111111111"
        )
        .is_err());
        assert!(!tracker.ready());
    }
    #[test]
    fn shared_hot_set_is_bounded() {
        let mut set = WatchSet::new(1).unwrap();
        assert!(set.watch("eip155:143:erc20:a").unwrap());
        assert!(!set.watch("eip155:143:erc20:a").unwrap());
        assert!(set.watch("eip155:143:erc20:b").is_err());
        set.unwatch("eip155:143:erc20:a");
        assert_eq!(set.hot_count(), 1);
        set.unwatch("eip155:143:erc20:a");
        assert_eq!(set.hot_count(), 0);
        set.charge_provider_calls(3).unwrap();
        assert_eq!(set.provider_calls(), 3);
    }
    #[test]
    fn commitment_and_head_identity_are_required() {
        let value = json!({"method":"eth_subscription","params":{"result":{
            "number":"0x7","hash":format!("0x{}","a".repeat(64)),
            "parentHash":format!("0x{}","0".repeat(64)),"timestamp":"0x7",
            "blockId":"0xabc","commitState":"Finalized"}}});
        assert_eq!(
            HeadEvent::from_notification(&value).unwrap().commit_state,
            CommitState::Finalized
        );
        let mut invalid = value;
        invalid["params"]["result"]["commitState"] = json!("unknown");
        assert!(HeadEvent::from_notification(&invalid).is_err());
    }
}
