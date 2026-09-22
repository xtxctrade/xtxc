//! Captured-account differential publication / halt / invalidation proof.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use skew_execution_host::{feed::Feed, world::WorldConfig};
use std::{fs, path::PathBuf, time::Duration};
fn mutate(j: &mut Value, i: usize, f: impl FnOnce(&mut Vec<u8>)) {
    let mut b = STANDARD
        .decode(j["value"][i]["data"][0].as_str().unwrap())
        .unwrap();
    f(&mut b);
    j["value"][i]["data"][0] = json!(STANDARD.encode(b));
}
fn main() {
    let root = PathBuf::from(if std::env::args().any(|s| s == "--fair-execution") {
        "/srv/skew/stocklana-engine-20260912/artifacts/fair-execution"
    } else {
        "/srv/skew/stocklana-engine-20260912/artifacts/native-expansion"
    });
    let c: WorldConfig =
        serde_json::from_slice(&fs::read(root.join("world-config.json")).unwrap()).unwrap();
    let count = c.markets.len() as u64;
    let keys = c.keys().unwrap();
    let ci = keys
        .iter()
        .position(|k| k == "SysvarC1ock11111111111111111111111111111111")
        .unwrap();
    let mut data: Value =
        serde_json::from_slice(&fs::read(root.join("world-snapshot.json")).unwrap()).unwrap();
    let feed = Feed::new(keys.clone(), Duration::from_secs(60), 4 * 1024 * 1024).unwrap();
    let first = feed.publish(&data).unwrap();
    let world = c.compile(&first, None).unwrap();
    assert_eq!(world.metrics["native_edges"], count);
    let mut rows = Vec::new();
    for usd in [100u64, 1000, 10000, 50000, 100000, 500000] {
        let r = world.quote(usd * 1_000_000).unwrap();
        assert!(!r["executionAdmitted"].as_bool().unwrap());
        rows.push(r);
    }
    let mut slot = first.slot + 1;
    data["context"]["slot"] = json!(slot);
    mutate(&mut data, ci, |b| {
        b[..8].copy_from_slice(&slot.to_le_bytes())
    });
    let next = feed.publish(&data).unwrap();
    let reused = c.compile(&next, Some(&world)).unwrap();
    assert_eq!(reused.metrics["reused_edges"], count);
    assert_eq!(reused.metrics["decoded_edges"], 0);
    assert_ne!(world.snapshot_hash, reused.snapshot_hash);
    assert_eq!(
        world.quote(100_000_000).unwrap()["outputAtoms"],
        reused.quote(100_000_000).unwrap()["outputAtoms"]
    );
    // Fee equations depend on on-chain time even when liquidity bytes do not change.
    slot += 1;
    data["context"]["slot"] = json!(slot);
    mutate(&mut data, ci, |b| {
        b[..8].copy_from_slice(&slot.to_le_bytes());
        let t = u64::from_le_bytes(b[32..40].try_into().unwrap()) + 1;
        b[32..40].copy_from_slice(&t.to_le_bytes());
    });
    let timed = c
        .compile(&feed.publish(&data).unwrap(), Some(&reused))
        .unwrap();
    assert_eq!(timed.metrics["decoded_edges"], count - 1);
    assert_eq!(timed.metrics["reused_edges"], 1);
    let dlmm = c
        .markets
        .iter()
        .find(|m| m.venue == skew_execution_host::market::Venue::MeteoraDlmm)
        .unwrap();
    let pi = keys.iter().position(|k| k == &dlmm.pool).unwrap();
    mutate(&mut data, pi, |b| b[82] = 1);
    let halted = c
        .compile(&feed.publish(&data).unwrap(), Some(&timed))
        .unwrap();
    assert_eq!(halted.metrics["native_edges"], count - 1);
    assert_eq!(halted.metrics["reused_edges"], count - 1);
    let fallback = halted.quote(50_000_000_000).unwrap();
    assert!(fallback["legs"]
        .as_array()
        .unwrap()
        .iter()
        .all(|l| l["pool"] != dlmm.pool));
    assert_eq!(world.metrics["native_edges"], count); // old publication is immutable
    let mut invalid = c.clone();
    invalid.markets.push(invalid.markets[0].clone());
    assert!(invalid.keys().is_err());
    data["value"][pi] = Value::Null;
    assert!(feed.publish(&data).is_err());
    assert!(feed.read().is_err());
    let report = json!({"scope":"captured mainnet accounts; native allocation proposals and world invalidation, not settlement","rows":rows,"initial":world.metrics,"slot_only":reused.metrics,"clock_change":timed.metrics,"halt":halted.metrics,"halt_fallback":fallback,"missing_account_fail_closed":true,"status":"WORLD_PUBLICATION_CHECKS_PASSED"});
    fs::write(
        root.join("world-proof.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    println!("{report}");
}
