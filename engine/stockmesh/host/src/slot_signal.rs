//! Solana PubSub is a wake-up signal only. Account bytes still come from one
//! bounded `getMultipleAccounts` response and retain the existing publication
//! fences in `feed`/`stockmesh_api`.
use crate::Result;
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tungstenite::{connect, Message};

#[derive(Clone, Debug)]
pub struct SlotSignalTelemetry {
    pub connected: bool,
    pub slot: u64,
    pub parent: u64,
    pub root: u64,
    pub age_ms: Option<u64>,
    pub notifications: u64,
    pub lineage_gaps: u64,
    pub rejected: u64,
    pub reconnects: u64,
}

pub struct SlotSignal {
    sequence: AtomicU64,
    connected: AtomicBool,
    slot: AtomicU64,
    parent: AtomicU64,
    root: AtomicU64,
    last_event_ms: AtomicU64,
    notifications: AtomicU64,
    lineage_gaps: AtomicU64,
    rejected: AtomicU64,
    reconnects: AtomicU64,
    wake: (Mutex<u64>, Condvar),
}

impl SlotSignal {
    pub fn start(http_rpc_url: &str) -> Result<Arc<Self>> {
        let ws_url = websocket_url(http_rpc_url)?;
        let state = Arc::new(Self {
            sequence: AtomicU64::new(0),
            connected: AtomicBool::new(false),
            slot: AtomicU64::new(0),
            parent: AtomicU64::new(0),
            root: AtomicU64::new(0),
            last_event_ms: AtomicU64::new(0),
            notifications: AtomicU64::new(0),
            lineage_gaps: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            wake: (Mutex::new(0), Condvar::new()),
        });
        let runner = Arc::clone(&state);
        std::thread::Builder::new()
            .name("stockmesh-slot-signal".into())
            .spawn(move || runner.run(&ws_url))
            .map_err(|error| error.to_string())?;
        Ok(state)
    }

    /// Wake the state loop for a newly selected instrument without waiting for
    /// another slot notification. This never fabricates a slot.
    pub fn poke(&self) {
        self.advance_sequence();
    }

    pub fn sequence(&self) -> u64 {
        self.sequence.load(Ordering::Acquire)
    }

    pub fn wait_for_change(&self, observed: u64, timeout: Duration) -> u64 {
        if self.sequence() != observed {
            return self.sequence();
        }
        let (lock, wake) = &self.wake;
        let Ok(guard) = lock.lock() else {
            return self.sequence();
        };
        let _ = wake.wait_timeout_while(guard, timeout, |_| self.sequence() == observed);
        self.sequence()
    }

    pub fn telemetry(&self) -> SlotSignalTelemetry {
        let event_ms = self.last_event_ms.load(Ordering::Acquire);
        SlotSignalTelemetry {
            connected: self.connected.load(Ordering::Acquire),
            slot: self.slot.load(Ordering::Acquire),
            parent: self.parent.load(Ordering::Acquire),
            root: self.root.load(Ordering::Acquire),
            age_ms: (event_ms != 0).then(|| now_ms().unwrap_or(0).saturating_sub(event_ms)),
            notifications: self.notifications.load(Ordering::Relaxed),
            lineage_gaps: self.lineage_gaps.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
        }
    }

    fn run(self: Arc<Self>, url: &str) {
        let mut failures = 0u32;
        loop {
            let result = self.run_connection(url);
            self.connected.store(false, Ordering::Release);
            self.advance_sequence();
            failures = failures.saturating_add(1);
            self.reconnects.fetch_add(1, Ordering::Relaxed);
            if let Err(error) = result {
                if failures == 1 || failures.is_power_of_two() {
                    eprintln!(
                        "StockMesh slot WebSocket unavailable ({failures} consecutive): {error}"
                    );
                }
            }
            let delay = 250u64.saturating_mul(1u64 << failures.saturating_sub(1).min(5));
            std::thread::sleep(Duration::from_millis(delay.min(8_000)));
        }
    }

    fn run_connection(&self, url: &str) -> Result<()> {
        let (mut socket, response) = connect(url).map_err(|error| error.to_string())?;
        if response.status().as_u16() != 101 {
            return Err("slot WebSocket upgrade status".into());
        }
        socket
            .send(Message::Text(
                json!({"jsonrpc":"2.0","id":1,"method":"slotSubscribe"})
                    .to_string()
                    .into(),
            ))
            .map_err(|error| error.to_string())?;
        socket
            .send(Message::Text(
                json!({"jsonrpc":"2.0","id":2,"method":"rootSubscribe"})
                    .to_string()
                    .into(),
            ))
            .map_err(|error| error.to_string())?;
        self.connected.store(true, Ordering::Release);
        loop {
            match socket.read().map_err(|error| error.to_string())? {
                Message::Text(text) => self.accept_json(&text)?,
                Message::Ping(bytes) => socket
                    .send(Message::Pong(bytes))
                    .map_err(|error| error.to_string())?,
                Message::Close(_) => return Err("slot WebSocket closed".into()),
                Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
    }

    fn accept_json(&self, text: &str) -> Result<()> {
        let value: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
        match value["method"].as_str() {
            Some("slotNotification") => {
                let result = &value["params"]["result"];
                let slot = result["slot"].as_u64().ok_or("slot notification slot")?;
                let parent = result["parent"]
                    .as_u64()
                    .ok_or("slot notification parent")?;
                let root = result["root"].as_u64().ok_or("slot notification root")?;
                self.accept_slot(slot, parent, root)
            }
            Some("rootNotification") => {
                let root = value["params"]["result"]
                    .as_u64()
                    .ok_or("root notification slot")?;
                let prior = self.root.load(Ordering::Acquire);
                if prior != 0 && root < prior {
                    self.rejected.fetch_add(1, Ordering::Relaxed);
                    return Err("root notification regressed".into());
                }
                self.root.store(root, Ordering::Release);
                self.last_event_ms
                    .store(now_ms().unwrap_or(0), Ordering::Release);
                Ok(())
            }
            None if value.get("result").is_some() && value.get("id").is_some() => Ok(()),
            _ => {
                self.rejected.fetch_add(1, Ordering::Relaxed);
                Err("unexpected slot WebSocket message".into())
            }
        }
    }

    fn accept_slot(&self, slot: u64, parent: u64, root: u64) -> Result<()> {
        if slot == 0 || parent >= slot || root > slot {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            return Err("slot notification bounds".into());
        }
        let prior_slot = self.slot.load(Ordering::Acquire);
        let prior_root = self.root.load(Ordering::Acquire);
        if prior_root != 0 && root < prior_root {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            return Err("slot notification root regressed".into());
        }
        if prior_slot != 0 && slot <= prior_slot {
            // Processed notifications may describe competing forks. They are
            // useful as a wake-up but must not replace the monotonic trigger.
            self.lineage_gaps.fetch_add(1, Ordering::Relaxed);
            self.note_event();
            return Ok(());
        }
        if prior_slot != 0 && parent != prior_slot {
            self.lineage_gaps.fetch_add(1, Ordering::Relaxed);
        }
        self.parent.store(parent, Ordering::Release);
        self.root.store(root, Ordering::Release);
        self.slot.store(slot, Ordering::Release);
        self.notifications.fetch_add(1, Ordering::Relaxed);
        self.note_event();
        Ok(())
    }

    fn note_event(&self) {
        self.last_event_ms
            .store(now_ms().unwrap_or(0), Ordering::Release);
        self.advance_sequence();
    }

    fn advance_sequence(&self) {
        self.sequence.fetch_add(1, Ordering::AcqRel);
        let (_, wake) = &self.wake;
        wake.notify_all();
    }
}

fn websocket_url(http_rpc_url: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(http_rpc_url).map_err(|error| error.to_string())?;
    let ws_scheme = match url.scheme() {
        "https" => "wss",
        "http" if matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")) => "ws",
        _ => return Err("slot WebSocket requires HTTPS RPC or loopback".into()),
    };
    url.set_scheme(ws_scheme)
        .map_err(|_| "slot WebSocket scheme")?;
    Ok(url.to_string())
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

    fn signal() -> SlotSignal {
        SlotSignal {
            sequence: AtomicU64::new(0),
            connected: AtomicBool::new(false),
            slot: AtomicU64::new(0),
            parent: AtomicU64::new(0),
            root: AtomicU64::new(0),
            last_event_ms: AtomicU64::new(0),
            notifications: AtomicU64::new(0),
            lineage_gaps: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            wake: (Mutex::new(0), Condvar::new()),
        }
    }

    #[test]
    fn rpc_url_maps_only_to_secure_or_loopback_websocket() {
        assert_eq!(
            websocket_url("https://api.mainnet-beta.solana.com").unwrap(),
            "wss://api.mainnet-beta.solana.com/"
        );
        assert_eq!(
            websocket_url("http://127.0.0.1:8899").unwrap(),
            "ws://127.0.0.1:8899/"
        );
        assert!(websocket_url("http://rpc.example.com").is_err());
    }

    #[test]
    fn slot_and_root_are_monotonic_wake_signals() {
        let signal = signal();
        signal
            .accept_json(
                r#"{"jsonrpc":"2.0","method":"slotNotification","params":{"result":{"parent":9,"root":8,"slot":10},"subscription":1}}"#,
            )
            .unwrap();
        signal
            .accept_json(
                r#"{"jsonrpc":"2.0","method":"rootNotification","params":{"result":9,"subscription":2}}"#,
            )
            .unwrap();
        assert_eq!(signal.slot.load(Ordering::Acquire), 10);
        assert_eq!(signal.root.load(Ordering::Acquire), 9);
        assert_eq!(signal.notifications.load(Ordering::Acquire), 1);
        assert_eq!(signal.sequence(), 1);
        assert!(signal.accept_slot(11, 10, 7).is_err());
    }

    #[test]
    fn competing_processed_fork_wakes_without_regressing_slot() {
        let signal = signal();
        signal.accept_slot(20, 19, 18).unwrap();
        signal.accept_slot(20, 17, 18).unwrap();
        assert_eq!(signal.slot.load(Ordering::Acquire), 20);
        assert_eq!(signal.lineage_gaps.load(Ordering::Acquire), 1);
    }
}
