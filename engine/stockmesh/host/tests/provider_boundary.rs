use serde_json::json;
use skew_execution_host::provider::{Budget, ProviderConfig};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

const GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
static NEXT: AtomicU64 = AtomicU64::new(0);

fn config() -> ProviderConfig {
    ProviderConfig {
        provider_id: "fixture-provider".into(),
        endpoint_file: "/unused".into(),
        expected_genesis: GENESIS.into(),
        requests_per_second: 100,
        max_requests: 100,
        max_in_flight: 2,
        max_response_bytes: 1024,
        max_total_response_bytes: 1024 * 1024,
        durable_quota: None,
    }
}

fn endpoint(
    replies: Vec<(u16, String)>,
) -> (
    ProviderConfig,
    std::path::PathBuf,
    std::thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://{}/credential-path?api-key=NEVER-LOG-THIS",
        listener.local_addr().unwrap()
    );
    let path = std::env::temp_dir().join(format!(
        "skew-provider-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(&path)
        .unwrap()
        .write_all(url.as_bytes())
        .unwrap();
    let mut config = config();
    config.endpoint_file = path.to_str().unwrap().into();
    let server = std::thread::spawn(move || {
        for (status, body) in replies {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut request = vec![0; length];
            reader.read_exact(&mut request).unwrap();
            let stream = reader.get_mut();
            write!(stream, "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\nRetry-After: 1\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    (config, path, server)
}

fn genesis() -> (u16, String) {
    (
        200,
        json!({"jsonrpc":"2.0","id":1,"result":GENESIS}).to_string(),
    )
}

#[test]
fn shared_inflight_rate_and_run_budget_recover_without_resetting() {
    let mut c = config();
    c.max_in_flight = 1;
    c.requests_per_second = 2;
    c.max_requests = 2;
    let budget = Budget::new(c).unwrap();
    let permit = budget.admit("getSlot", 50).unwrap();
    assert!(budget.clone().admit("getSlot", 50).is_err());
    drop(permit);
    let mut permit = budget.admit("getSlot", 50).unwrap();
    permit.received(24);
    permit.complete();
    drop(permit);
    assert!(budget
        .admit("getSlot", 50)
        .unwrap_err_string()
        .contains("run request"));
    let metrics = budget.snapshot().unwrap();
    assert_eq!(
        (
            metrics.requests,
            metrics.completed,
            metrics.failed,
            metrics.in_flight,
            metrics.response_bytes
        ),
        (2, 1, 1, 0, 24)
    );
}

// Avoid Debug on handles that may later contain transport configuration.
trait ErrorString {
    fn unwrap_err_string(self) -> String;
}
impl<T> ErrorString for Result<T, String> {
    fn unwrap_err_string(self) -> String {
        match self {
            Ok(_) => panic!("expected rejection"),
            Err(e) => e,
        }
    }
}

#[test]
fn byte_reservations_prevent_concurrent_overspend() {
    let mut c = config();
    c.max_total_response_bytes = 1050;
    let budget = Budget::new(c).unwrap();
    let mut permit = budget.admit("getSlot", 1).unwrap();
    assert!(budget.admit("getSlot", 1).is_err());
    permit.received(26);
    drop(permit);
    assert!(budget.admit("getSlot", 1).is_err());
}

#[test]
fn rate_window_and_cooldown_expire_but_do_not_clear_counters() {
    let mut c = config();
    c.requests_per_second = 1;
    let budget = Budget::new(c).unwrap();
    let permit = budget.admit("getSlot", 1).unwrap();
    permit.backoff(1);
    drop(permit);
    assert!(budget.admit("getSlot", 1).is_err());
    std::thread::sleep(Duration::from_millis(1050));
    drop(budget.admit("getSlot", 1).unwrap());
    assert!(budget
        .admit("getSlot", 1)
        .unwrap_err_string()
        .contains("rate limit"));
    assert_eq!(budget.snapshot().unwrap().requests, 2);
}

#[test]
fn rpc_clones_share_budget_and_never_send_write_methods() {
    let (mut c, path, server) = endpoint(vec![
        genesis(),
        (200, json!({"jsonrpc":"2.0","id":1,"result":42}).to_string()),
    ]);
    c.max_requests = 2;
    let rpc = c.connect().unwrap();
    assert!(rpc
        .call("sendTransaction", json!([]))
        .unwrap_err()
        .contains("read-only"));
    assert!(rpc.call("requestAirdrop", json!([])).is_err());
    assert_eq!(
        rpc.relaxed_read_clone()
            .unwrap()
            .call("getSlot", json!([]))
            .unwrap(),
        42
    );
    assert!(rpc
        .clone()
        .call("getSlot", json!([]))
        .unwrap_err()
        .contains("run request"));
    assert_eq!(rpc.provider_metrics().unwrap().requests, 2);
    server.join().unwrap();
    std::fs::remove_file(path).unwrap();
}

#[test]
fn wrong_genesis_is_rejected_without_fallback() {
    let (c, path, server) = endpoint(vec![(
        200,
        json!({"jsonrpc":"2.0","id":1,"result":"wrong"}).to_string(),
    )]);
    assert_eq!(c.connect().unwrap_err_string(), "wrong genesis");
    server.join().unwrap();
    std::fs::remove_file(path).unwrap();
}

#[test]
fn transport_failures_are_redacted_and_request_body_is_bounded() {
    let (c, path, server) = endpoint(vec![genesis()]);
    let rpc = c.connect().unwrap();
    server.join().unwrap();
    assert_eq!(rpc.call("getSlot", json!(["x".repeat(1024 * 1024)])).unwrap_err(), "provider request byte budget exceeded");
    let error = rpc.call("getSlot", json!([])).unwrap_err();
    assert!(error == "RPC transport failed" || error == "RPC request timed out");
    assert!(!error.contains("NEVER-LOG-THIS"));
    assert_eq!(rpc.provider_metrics().unwrap().requests, 2);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn provider_errors_cannot_leak_endpoint_or_body_and_cooldown_recovers() {
    let (c, path, server) = endpoint(vec![genesis(), (429, "NEVER-LOG-THIS".into()),
        (200, json!({"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"NEVER-LOG-THIS","data":"secret"}}).to_string())]);
    let rpc = c.connect().unwrap();
    assert_eq!(
        rpc.call("getSlot", json!([])).unwrap_err(),
        "RPC HTTP status 429"
    );
    assert_eq!(
        rpc.clone().call("getSlot", json!([])).unwrap_err(),
        "provider cooling down"
    );
    std::thread::sleep(Duration::from_millis(1050));
    assert_eq!(
        rpc.call("getSlot", json!([])).unwrap_err(),
        "RPC rejected (code -32000)"
    );
    server.join().unwrap();
    std::fs::remove_file(path).unwrap();
}

#[test]
fn response_identity_and_size_are_bounded() {
    let (c, path, server) = endpoint(vec![
        genesis(),
        (200, json!({"jsonrpc":"2.0","id":9,"result":42}).to_string()),
        (200, "x".repeat(2048)),
    ]);
    let rpc = c.connect().unwrap();
    assert_eq!(
        rpc.call("getSlot", json!([])).unwrap_err(),
        "RPC response identity mismatch"
    );
    assert_eq!(
        rpc.call("getSlot", json!([])).unwrap_err(),
        "RPC response budget exceeded"
    );
    assert_eq!(rpc.provider_metrics().unwrap().failed, 2);
    server.join().unwrap();
    std::fs::remove_file(path).unwrap();
}

#[test]
fn secret_files_must_be_private_and_profiles_bounded() {
    let mut c = config();
    c.max_requests = 0;
    assert!(c.validate().is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path =
            std::env::temp_dir().join(format!("skew-provider-public-{}", std::process::id()));
        std::fs::write(&path, b"never read this").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut c = config();
        c.endpoint_file = path.to_str().unwrap().into();
        assert!(c.connect().unwrap_err_string().contains("permissions"));
        std::fs::remove_file(path).unwrap();
    }
}
