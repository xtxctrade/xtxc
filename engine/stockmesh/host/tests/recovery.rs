use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use sha2::{Digest, Sha256};
use skew_execution_host::{
    feed::Feed,
    journal::{Entry, Journal, Phase},
    sender::{authorize, Authorization},
};
#[cfg(unix)]
use std::os::unix::fs::{symlink, PermissionsExt};
use std::{io::Write, path::PathBuf, time::Duration};
fn path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("skew-host-test-{}-{name}", std::process::id()))
}
fn entry(id: &str, r: u8) -> Entry {
    Entry {
        id: id.into(),
        quote_id: None,
        signature: format!("fixture-{id}"),
        wire: vec![1],
        message_hash: [0; 32],
        last_valid_height: 100,
        resources: vec![[r; 32]],
        expected_exposure: None,
        expected_swap: None,
        expected_basket: None,
        phase: Phase::Prepared,
        attempts: 0,
    }
}
#[cfg(unix)]
#[test]
fn journal_rejects_symlinks_hardlinks_and_group_readable_files() {
    let target = path("private-target");
    let link = path("private-link");
    let hardlink = path("private-hardlink");
    let permissive = path("private-permissive");
    for candidate in [&target, &link, &hardlink, &permissive] {
        let _ = std::fs::remove_file(candidate);
        let mut lock = candidate.as_os_str().to_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(PathBuf::from(lock));
    }

    std::fs::write(&target, b"").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&target, &link).unwrap();
    assert!(Journal::open(&link, 4, 65536).is_err());

    std::fs::hard_link(&target, &hardlink).unwrap();
    assert!(Journal::open(&hardlink, 4, 65536).is_err());

    std::fs::write(&permissive, b"").unwrap();
    std::fs::set_permissions(&permissive, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert!(Journal::open(&permissive, 4, 65536).is_err());

    for candidate in [&target, &link, &hardlink, &permissive] {
        let _ = std::fs::remove_file(candidate);
        let mut lock = candidate.as_os_str().to_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(PathBuf::from(lock));
    }
}
#[test]
fn durable_uncertainty_and_resource_reservations_survive_restart() {
    let p = path("restart");
    let _ = std::fs::remove_file(&p);
    {
        let mut j = Journal::open(&p, 4, 65536).unwrap();
        j.insert(entry("a", 1)).unwrap();
        j.update("a", Phase::Unknown, true).unwrap();
        assert!(Journal::open(&p, 4, 65536).is_err());
    }
    {
        let mut j = Journal::open(&p, 4, 65536).unwrap();
        assert_eq!(j.get("a").unwrap().attempts, 1);
        assert!(j.insert(entry("b", 1)).is_err());
        assert!(j.insert(entry("a", 2)).is_err());
        j.update("a", Phase::Finalized, false).unwrap();
        assert!(j.insert(entry("b", 1)).is_err());
        j.update("a", Phase::Reconciled, false).unwrap();
        let mut duplicate_wire = entry("c", 3);
        duplicate_wire.signature = j.get("a").unwrap().signature.clone();
        assert!(
            j.insert(duplicate_wire).is_err(),
            "same transaction cannot acknowledge a second intent"
        );
        j.insert(entry("b", 1)).unwrap();
        assert!(j.update("a", Phase::Submitted, true).is_err());
    }
    std::fs::remove_file(p).unwrap();
}
#[test]
fn incomplete_tail_recovers_but_checksum_corruption_fails_closed() {
    let p = path("torn");
    let _ = std::fs::remove_file(&p);
    {
        let mut j = Journal::open(&p, 4, 65536).unwrap();
        j.insert(entry("a", 1)).unwrap();
    }
    let good = std::fs::read(&p).unwrap();
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(&[10, 0, 0, 0, 0, 0, 0, 0, 1]).unwrap();
    }
    {
        let j = Journal::open(&p, 4, 65536).unwrap();
        assert!(j.get("a").is_some());
    }
    assert_eq!(std::fs::read(&p).unwrap(), good);
    let mut corrupt = good;
    corrupt[12] ^= 1;
    std::fs::write(&p, corrupt).unwrap();
    assert!(Journal::open(&p, 4, 65536).is_err());
    std::fs::remove_file(p).unwrap();
}
#[test]
fn bounded_admission_does_not_evict_approved_intents() {
    let p = path("bounds");
    let _ = std::fs::remove_file(&p);
    let mut j = Journal::open(&p, 2, 65536).unwrap();
    j.insert(entry("a", 1)).unwrap();
    j.insert(entry("b", 2)).unwrap();
    assert!(j.insert(entry("c", 3)).is_err());
    assert_eq!(j.entries().count(), 2);
    drop(j);
    std::fs::remove_file(p).unwrap();
}
#[test]
fn compacted_log_keeps_exclusive_ownership_uncertainty_and_deduplication() {
    let p = path("compact");
    let _ = std::fs::remove_file(&p);
    {
        let mut j = Journal::open(&p, 4, 65536).unwrap();
        j.insert(entry("a", 1)).unwrap();
        j.update("a", Phase::Unknown, true).unwrap();
        j.insert(entry("b", 2)).unwrap();
        j.update("b", Phase::Finalized, false).unwrap();
        j.update("b", Phase::Reconciled, false).unwrap();
        assert!(j.compact().unwrap() > 0);
        assert!(Journal::open(&p, 4, 65536).is_err());
        assert!(j.insert(entry("a", 3)).is_err());
        assert!(j.insert(entry("c", 1)).is_err());
        j.insert(entry("c", 2)).unwrap();
    }
    {
        let j = Journal::open(&p, 4, 65536).unwrap();
        assert_eq!(j.get("a").unwrap().phase, Phase::Unknown);
        assert_eq!(j.get("a").unwrap().attempts, 1);
        assert_eq!(j.get("b").unwrap().phase, Phase::Reconciled);
        assert_eq!(j.entries().count(), 3);
    }
    std::fs::remove_file(&p).unwrap();
    let mut lock = p.as_os_str().to_os_string();
    lock.push(".lock");
    std::fs::remove_file(PathBuf::from(lock)).unwrap();
}
#[test]
fn group_commit_rejects_conflicts_before_any_ack_and_recovers_each_intent() {
    let p = path("batch");
    let _ = std::fs::remove_file(&p);
    {
        let mut j = Journal::open(&p, 4, 65536).unwrap();
        assert!(j.insert_batch(vec![entry("a", 1), entry("b", 1)]).is_err());
        assert_eq!(j.entries().count(), 0);
        j.insert_batch(vec![entry("a", 1), entry("b", 2)]).unwrap();
        assert!(j
            .update_batch(&[("a", Phase::Unknown, true), ("b", Phase::Reconciled, false)])
            .is_err());
        assert_eq!(j.get("a").unwrap().phase, Phase::Prepared);
        j.update_batch(&[("a", Phase::Unknown, true), ("b", Phase::Unknown, true)])
            .unwrap();
    }
    {
        let j = Journal::open(&p, 4, 65536).unwrap();
        assert!(j
            .entries()
            .all(|e| e.phase == Phase::Unknown && e.attempts == 1));
    }
    std::fs::remove_file(p).unwrap();
}
#[test]
fn account_publication_rejects_missing_out_of_order_and_stale_views() {
    let key = bs58::encode([1u8; 32]).into_string();
    let owner = bs58::encode([2u8; 32]).into_string();
    let f = Feed::new(vec![key], Duration::from_millis(10), 1024).unwrap();
    assert!(f.read().is_err());
    let make = |slot, data| json!({"context":{"slot":slot},"value":[{"owner":owner,"lamports":1,"executable":false,"data":[data,"base64"]}]});
    let first = f.publish(&make(10, "AA==")).unwrap();
    let next = f.publish(&make(11, "AA==")).unwrap();
    assert_eq!(first.generation, next.generation);
    assert!(f.publish(&make(9, "AA==")).is_err());
    assert!(f.read().is_err());
    let changed = f.publish(&make(12, "AQ==")).unwrap();
    assert_eq!(changed.generation, first.generation + 1);
    assert_eq!(first.accounts[0].data, vec![0]);
    assert!(f
        .publish(&json!({"context":{"slot":13},"value":[null]}))
        .is_err());
    assert!(f.read().is_err());
    f.publish(&make(14, "AQ==")).unwrap();
    std::thread::sleep(Duration::from_millis(15));
    assert!(f.read().is_err());
    f.publish(&make(14, "AQ==")).unwrap();
    assert!(
        f.read().is_err(),
        "fresh response from a frozen bank is still stale"
    );
    f.publish(&make(15, "AQ==")).unwrap();
    assert!(f.read().is_ok());
}
#[test]
fn signed_wire_is_bound_to_approved_message_and_all_signatures() {
    let key = SigningKey::from_bytes(&[7; 32]);
    // Canonical legacy message with one writable signer, a blockhash and no instructions.
    let mut message = vec![1, 0, 0, 1];
    message.extend_from_slice(key.verifying_key().as_bytes());
    message.extend_from_slice(&[9; 32]);
    message.push(0);
    let mut wire = vec![1];
    wire.extend_from_slice(&key.sign(&message).to_bytes());
    wire.extend_from_slice(&message);
    let auth = || Authorization {
        intent_id: "a".into(),
        message_hash: Sha256::digest(&message).into(),
        last_valid_height: 100,
        resources: vec![[1; 32]],
    };
    assert!(authorize(&wire, auth()).is_ok());
    let mut bad = wire.clone();
    bad[1] ^= 1;
    assert!(authorize(&bad, auth()).is_err());
    let mut bad = wire;
    *bad.last_mut().unwrap() = 1;
    assert!(authorize(&bad, auth()).is_err());
}
