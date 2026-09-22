use skew_execution_host::{provider::{Budget, ProviderConfig}, provider_quota::{DurableQuota, QuotaConfig}};
use std::{
    fs::{self, File},
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture { dir: PathBuf, config: QuotaConfig }
impl Fixture {
    fn new(requests: u64, bytes: u64) -> Self {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let dir = std::env::temp_dir().canonicalize().unwrap().join(format!("skew-quota-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&dir).unwrap();
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let config = QuotaConfig { ledger_file: dir.join("usage.json").to_str().unwrap().into(),
            budget_id: "bounded-fixture".into(), period_start_unix: now - 60,
            period_end_unix: now + 3600, max_requests: requests, max_reserved_body_bytes: bytes };
        Self { dir, config }
    }
    fn quota(&self) -> DurableQuota { DurableQuota::new(self.config.clone(), "fixture", GENESIS).unwrap() }
    fn profile(&self) -> ProviderConfig {
        ProviderConfig { provider_id: "fixture".into(), endpoint_file: "/unused".into(),
            expected_genesis: GENESIS.into(), requests_per_second: 100, max_requests: 100,
            max_in_flight: 2, max_response_bytes: 1024, max_total_response_bytes: 1_000_000,
            durable_quota: Some(self.config.clone()) }
    }
    fn child(&self, crash: bool) -> Command {
        let mut child = Command::new(std::env::current_exe().unwrap());
        child.args(["--exact", "quota_subprocess", "--ignored"])
            .env("SKEW_QUOTA_FIXTURE", serde_json::to_string(&self.config).unwrap())
            .env("SKEW_QUOTA_CRASH", if crash { "yes" } else { "no" })
            .stdout(Stdio::null()).stderr(Stdio::null());
        child
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Only this test's unique private fixture directory is removed.
        fs::remove_dir_all(&self.dir).unwrap();
    }
}

#[test]
fn restart_does_not_reset_reservations_or_accept_changed_limits() {
    let fixture = Fixture::new(2, 100);
    assert!(fixture.quota().snapshot().is_err());
    fixture.quota().initialize().unwrap();
    assert!(fixture.quota().initialize().is_err());
    fixture.quota().reserve(40).unwrap();
    assert_eq!(fixture.quota().snapshot().unwrap().reserved_body_bytes, 40);
    fixture.quota().reserve(60).unwrap();
    assert!(fixture.quota().reserve(1).unwrap_err().contains("exhausted"));
    let mut changed = fixture.config.clone();
    changed.max_requests = 100;
    assert!(DurableQuota::new(changed, "fixture", GENESIS).unwrap().snapshot().is_err());
    assert!(DurableQuota::new(fixture.config.clone(), "other", GENESIS).unwrap().snapshot().is_err());
    assert!(DurableQuota::new(fixture.config.clone(), "fixture", "other").unwrap().snapshot().is_err());
}

#[test]
fn independent_budgets_share_usage_and_never_refund_failed_work() {
    let fixture = Fixture::new(2, 10_000);
    fixture.quota().initialize().unwrap();
    let first = Budget::new(fixture.profile()).unwrap();
    assert!(first.admit("sendTransaction", 20).is_err());
    assert_eq!(fixture.quota().snapshot().unwrap().requests, 0);
    drop(first.admit("getSlot", 20).unwrap());
    drop(first);
    let second = Budget::new(fixture.profile()).unwrap();
    let mut permit = second.admit("getSlot", 20).unwrap();
    permit.received(1);
    permit.complete();
    drop(permit);
    assert!(second.admit("getSlot", 20).is_err());
    let snapshot = second.snapshot().unwrap();
    assert_eq!(snapshot.requests, 1);
    assert_eq!(snapshot.rejected, 1);
    assert_eq!(snapshot.durable_quota.unwrap().reserved_body_bytes, 2 * (1024 + 1 + 20));
}

#[test]
fn corrupt_or_missing_ledger_fails_closed_without_losing_provider_status() {
    let fixture = Fixture::new(3, 10_000);
    fixture.quota().initialize().unwrap();
    let budget = Budget::new(fixture.profile()).unwrap();
    let original = fs::read(&fixture.config.ledger_file).unwrap();
    fs::write(&fixture.config.ledger_file, &original[..original.len()/2]).unwrap();
    assert!(budget.admit("getSlot", 1).is_err());
    let metrics = budget.snapshot().unwrap();
    assert_eq!(metrics.requests, 0);
    assert!(metrics.durable_quota_error.is_some());
    assert!(fixture.quota().initialize().is_err());
    fs::remove_file(&fixture.config.ledger_file).unwrap();
    assert!(Budget::new(fixture.profile()).is_err());
    assert!(!std::path::Path::new(&fixture.config.ledger_file).exists());
}

#[test]
fn corrupt_checksum_is_rejected_and_reserved_bytes_cannot_overflow() {
    let fixture = Fixture::new(3, 100);
    fixture.quota().initialize().unwrap();
    fixture.quota().reserve(1).unwrap();
    assert!(fixture.quota().reserve(u64::MAX).is_err());
    let mut document: serde_json::Value = serde_json::from_slice(&fs::read(&fixture.config.ledger_file).unwrap()).unwrap();
    document["usage"]["requests"] = serde_json::json!(0);
    fs::write(&fixture.config.ledger_file, serde_json::to_vec(&document).unwrap()).unwrap();
    assert!(fixture.quota().snapshot().is_err());
}

#[test]
fn occupied_lock_is_bounded_and_recovery_preserves_usage() {
    let fixture = Fixture::new(3, 100);
    fixture.quota().initialize().unwrap();
    fixture.quota().reserve(17).unwrap();
    let locked = File::open(&fixture.config.ledger_file).unwrap();
    fs2::FileExt::lock_exclusive(&locked).unwrap();
    assert_eq!(fixture.quota().reserve(1).unwrap_err(), "provider quota busy");
    fs2::FileExt::unlock(&locked).unwrap();
    drop(locked);
    fixture.quota().reserve(19).unwrap();
    assert_eq!(fixture.quota().snapshot().unwrap().reserved_body_bytes, 36);
}

#[test]
fn expiration_has_no_automatic_rollover_or_renewal() {
    let mut fixture = Fixture::new(3, 100);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    fixture.config.period_start_unix = now - 100;
    fixture.config.period_end_unix = now;
    assert!(fixture.quota().initialize().is_err());
    assert!(!std::path::Path::new(&fixture.config.ledger_file).exists());
    fixture.config.period_start_unix = now + 60;
    fixture.config.period_end_unix = now + 120;
    assert!(fixture.quota().initialize().is_err());
}

#[test]
#[cfg(unix)]
fn symlinks_hardlinks_and_public_permissions_are_rejected() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let fixture = Fixture::new(3, 100);
    fixture.quota().initialize().unwrap();
    fs::set_permissions(&fixture.config.ledger_file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(fixture.quota().reserve(1).is_err());
    fs::set_permissions(&fixture.config.ledger_file, fs::Permissions::from_mode(0o600)).unwrap();
    let other = fixture.dir.join("other");
    fs::hard_link(&fixture.config.ledger_file, &other).unwrap();
    assert!(fixture.quota().reserve(1).is_err());
    fs::remove_file(&other).unwrap();
    fs::rename(&fixture.config.ledger_file, &other).unwrap();
    symlink(&other, &fixture.config.ledger_file).unwrap();
    assert!(fixture.quota().reserve(1).is_err());
    fs::remove_file(&fixture.config.ledger_file).unwrap();
    fs::rename(&other, &fixture.config.ledger_file).unwrap();
    fs::set_permissions(&fixture.dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(fixture.quota().reserve(1).is_err());
    fs::set_permissions(&fixture.dir, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.quota().reserve(1).unwrap();
}

#[test]
fn crash_after_reservation_is_conservatively_charged() {
    let fixture = Fixture::new(2, 100);
    fixture.quota().initialize().unwrap();
    assert_eq!(fixture.child(true).status().unwrap().code(), Some(23));
    assert_eq!(fixture.quota().snapshot().unwrap().requests, 1);
    fixture.quota().reserve(1).unwrap();
    assert!(fixture.quota().reserve(1).is_err());
}

#[test]
fn parallel_processes_cannot_exceed_shared_cap() {
    let fixture = Fixture::new(17, 1000);
    fixture.quota().initialize().unwrap();
    let mut children: Vec<_> = (0..4).map(|_| fixture.child(false).spawn().unwrap()).collect();
    for child in &mut children { assert!(child.wait().unwrap().success()); }
    let usage = fixture.quota().snapshot().unwrap();
    assert_eq!(usage.requests, 17);
    assert_eq!(usage.reserved_body_bytes, 17);
}

#[test]
#[ignore = "helper invoked by the cross-process tests"]
fn quota_subprocess() {
    let config = serde_json::from_str(&std::env::var("SKEW_QUOTA_FIXTURE").unwrap()).unwrap();
    let quota = DurableQuota::new(config, "fixture", GENESIS).unwrap();
    for _ in 0..100 {
        match quota.reserve(1) {
            Ok(()) => if std::env::var("SKEW_QUOTA_CRASH").unwrap() == "yes" { std::process::exit(23); },
            Err(error) if error.contains("exhausted") => break,
            Err(error) if error.contains("busy") => std::thread::sleep(Duration::from_millis(2)),
            Err(error) => panic!("unexpected quota error: {error}"),
        }
    }
}
