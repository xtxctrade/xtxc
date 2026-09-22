//! Fixture-only open-loop sender load against a synthetic loopback RPC.
//! No real chain, keys, blockhashes, financial approvals or settlements.
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use sha2::{Digest, Sha256};
use skew_execution_host::{
    journal::{Journal, Phase},
    rpc::Rpc,
    sender::{authorize, Authorization, Sender},
};
use std::{
    path::PathBuf,
    sync::{
        mpsc::{sync_channel, TrySendError},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
const GENESIS: &str = "SkewSyntheticSenderLoadNotAChain";
struct Request {
    id: String,
    wire: Vec<u8>,
    hash: [u8; 32],
    scheduled: Instant,
}
fn stats(mut values: Vec<u64>) -> serde_json::Value {
    values.sort_unstable();
    if values.is_empty() {
        return json!(null);
    }
    let q = |n| values[(values.len() - 1) * n / 100];
    json!({"n":values.len(),"p50":q(50),"p95":q(95),"p99":q(99),"max":values.last()})
}
fn main() {
    assert_eq!(std::env::consts::OS, "linux");
    let root =
        PathBuf::from("/srv/skew/stocklana-engine-20260912/artifacts/native-search/sender-load");
    std::fs::create_dir_all(&root).unwrap();
    let key = SigningKey::from_bytes(&[11; 32]); // Public deterministic fixture. Never usable on a chain.
    let mut rows = Vec::new();
    let mut seq = 0u64;
    for rate in [32u64, 128, 512, 2048] {
        let mut prepared = Vec::new();
        for _ in 0..rate * 2 {
            seq += 1;
            let mut message = vec![1, 0, 0, 1];
            message.extend_from_slice(key.verifying_key().as_bytes());
            message.extend_from_slice(&<[u8; 32]>::from(Sha256::digest(seq.to_le_bytes())));
            message.push(0); // Empty, no-op fixture message with a deliberately fake blockhash.
            let mut wire = vec![1];
            wire.extend_from_slice(&key.sign(&message).to_bytes());
            wire.extend_from_slice(&message);
            prepared.push((
                format!("load-{seq}"),
                wire,
                <[u8; 32]>::from(Sha256::digest(message)),
            ));
        }
        let (tx, rx) = sync_channel::<Request>(64);
        let rx = Arc::new(Mutex::new(rx));
        let results = Arc::new(Mutex::new(Vec::<(bool, u64, u64)>::new()));
        let mut threads = Vec::new();
        for worker in 0..2 {
            let rx = rx.clone();
            let results = results.clone();
            let file = root.join(format!("{rate}-{worker}.wal"));
            assert!(!file.exists(), "fresh proof journal required");
            let journal = Journal::open(&file, 4096, 64 * 1024 * 1024).unwrap();
            let rpc = Rpc::pinned("http://127.0.0.1:19201".into(), GENESIS.into()).unwrap();
            threads.push(std::thread::spawn(move || {
                let mut sender = Sender {
                    journal,
                    rpc,
                    max_attempts: 3,
                };
                let mut completed = 0;
                loop {
                    let batch = {
                        let guard = rx.lock().unwrap();
                        let Ok(first) = guard.recv() else { break };
                        let mut batch = vec![first];
                        while batch.len() < 32 {
                            match guard.try_recv() {
                                Ok(r) => batch.push(r),
                                Err(_) => break,
                            }
                        }
                        batch
                    };
                    let began = Instant::now();
                    let result = (|| {
                        let pending = batch
                            .iter()
                            .map(|r| {
                                authorize(
                                    &r.wire,
                                    Authorization {
                                        intent_id: r.id.clone(),
                                        message_hash: r.hash,
                                        last_valid_height: 1000,
                                        resources: vec![r.hash],
                                    },
                                )
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        sender.journal.insert_batch(pending)?;
                        let ids: Vec<_> = batch.iter().map(|r| r.id.as_str()).collect();
                        let first = sender.step_batch(&ids)?;
                        if !first
                            .iter()
                            .all(|p| matches!(p, Phase::Submitted | Phase::Unknown))
                        {
                            return Err("initial phase".to_string());
                        }
                        if !sender
                            .step_batch(&ids)?
                            .iter()
                            .all(|p| *p == Phase::Finalized)
                        {
                            return Err("fixture status lookup".into());
                        }
                        // Synthetic RPC asserts only lifecycle mechanics, not economic receipts.
                        let updates: Vec<_> = ids
                            .iter()
                            .map(|id| (*id, Phase::Reconciled, false))
                            .collect();
                        sender.journal.update_batch(&updates)?;
                        if !ids
                            .iter()
                            .all(|id| sender.journal.get(id).unwrap().attempts == 1)
                        {
                            return Err("duplicate send".into());
                        }
                        Ok::<_, String>(())
                    })();
                    if result.is_ok() {
                        completed += batch.len();
                    }
                    for r in batch {
                        results.lock().unwrap().push((
                            result.is_ok(),
                            began.elapsed().as_nanos() as u64,
                            r.scheduled.elapsed().as_nanos() as u64,
                        ));
                    }
                }
                drop(sender);
                let reopened = Journal::open(&file, 4096, 64 * 1024 * 1024).unwrap();
                assert_eq!(
                    reopened
                        .entries()
                        .filter(|e| e.phase == Phase::Reconciled && e.attempts == 1)
                        .count(),
                    completed
                );
                completed
            }));
        }
        let start = Instant::now();
        let mut accepted = 0;
        let mut rejected = 0;
        let mut lag = Vec::new();
        for (i, (id, wire, hash)) in prepared.into_iter().enumerate() {
            let scheduled = start + Duration::from_nanos(i as u64 * 1_000_000_000 / rate);
            if let Some(delay) = scheduled.checked_duration_since(Instant::now()) {
                std::thread::sleep(delay);
            }
            lag.push(scheduled.elapsed().as_nanos() as u64);
            match tx.try_send(Request {
                id,
                wire,
                hash,
                scheduled,
            }) {
                Ok(()) => accepted += 1,
                Err(TrySendError::Full(_)) => rejected += 1,
                Err(TrySendError::Disconnected(_)) => panic!("worker unavailable"),
            }
        }
        drop(tx);
        for t in threads {
            t.join().unwrap();
        }
        let elapsed = start.elapsed().as_nanos() as u64;
        let results = results.lock().unwrap();
        let good: Vec<_> = results.iter().filter(|r| r.0).collect();
        let row = json!({"target_requests_per_second":rate,"scheduled":rate*2,"accepted":accepted,"queue_rejected":rejected,"completed":good.len(),"errors":results.len()-good.len(),"elapsed_ns":elapsed,"completed_per_second_including_drain":good.len() as f64*1e9/elapsed as f64,"service_ns":stats(good.iter().map(|r|r.1).collect()),"scheduled_to_completion_ns":stats(good.iter().map(|r|r.2).collect()),"generator_lag_ns":stats(lag),"restart_reconciled_records_verified":true});
        println!("{row}");
        rows.push(row);
    }
    std::fs::write(root.join("proof.json"),serde_json::to_vec_pretty(&json!({"scope":"synthetic loopback RPC lifecycle load; valid fixture signatures, durable journal, bounded queue, response loss, restart; NOT SBF or landed throughput","workers":2,"queue_capacity":64,"rows":rows})).unwrap()).unwrap();
}
