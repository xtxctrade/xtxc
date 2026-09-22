//! Loopback StockMesh transport. It never signs. Submission exists only when
//! the operator explicitly binds a deployment-ready compiler and durable sender;
//! the sender relays the exact wallet-signed wire without changing it.
use serde_json::json;
use skew_execution_host::{
    direct_state::{DirectConfig, DirectMode},
    provider::ProviderConfig,
    stockmesh_api::{ApiReply, StockMesh},
};
use std::{
    env,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        mpsc::{sync_channel, TrySendError},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const MAX_HEADER: usize = 4 * 1024;
const MAX_BODY: usize = 32 * 1024;
const SUBMISSION_SENTINEL: &str = "SUBMIT_EXACT_USER_SIGNED_WIRE";

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    authorization: String,
    product: String,
    content_type: String,
    body: Vec<u8>,
    keep_alive: bool,
}

fn bounded_env(
    name: &str,
    default: usize,
    minimum: usize,
    maximum: usize,
) -> Result<usize, String> {
    match env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .map_err(|_| format!("{name} integer"))
            .and_then(|value| {
                if (minimum..=maximum).contains(&value) {
                    Ok(value)
                } else {
                    Err(format!("{name} bounds"))
                }
            }),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(_) => Err(format!("{name} encoding")),
    }
}

fn direct_config() -> Result<Option<DirectConfig>, String> {
    let socket = match env::var_os("SKEW_STOCKMESH_DIRECT_STATE_SOCKET") {
        Some(socket) => PathBuf::from(socket),
        None => {
            if env::var_os("SKEW_STOCKMESH_DIRECT_STATE_MODE").is_some() {
                return Err("SKEW_STOCKMESH_DIRECT_STATE_MODE requires DIRECT_STATE_SOCKET".into());
            }
            return Ok(None);
        }
    };
    let mode = DirectMode::parse(
        &env::var("SKEW_STOCKMESH_DIRECT_STATE_MODE").unwrap_or_else(|_| "shadow".into()),
    )?;
    let max_age_ms = bounded_env("SKEW_STOCKMESH_DIRECT_MAX_AGE_MS", 1_200, 250, 1_500)?;
    let promotion_frames = bounded_env("SKEW_STOCKMESH_DIRECT_PROMOTION_FRAMES", 8, 2, 32)?;
    let config = DirectConfig {
        socket,
        mode,
        max_age: Duration::from_millis(max_age_ms as u64),
        promotion_frames: promotion_frames as u8,
    };
    config.validate()?;
    Ok(Some(config))
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        _ => "Service Unavailable",
    }
}

fn response(stream: &mut TcpStream, reply: ApiReply, keep_alive: bool) -> bool {
    let body = reply.body.to_string();
    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
    let connection = if keep_alive {
        "Connection: keep-alive\r\nKeep-Alive: timeout=1\r\n"
    } else {
        "Connection: close\r\n"
    };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Length: {}\r\n{}\r\n{}",
        reply.status,
        reason(reply.status),
        body.len(),
        connection,
        body
    )
    .and_then(|()| stream.flush())
    .is_ok()
}

fn error(status: u16, code: &str, message: &str) -> ApiReply {
    ApiReply {
        status,
        body: json!({"error":{"code":code,"message":message}}),
    }
}

fn parse(
    stream: &mut BufReader<TcpStream>,
    admitted: Instant,
    deadline_ms: u64,
) -> Result<Request, ApiReply> {
    let deadline = admitted + Duration::from_millis(deadline_ms);
    let mut bytes = Vec::with_capacity(MAX_HEADER + MAX_BODY);
    let header_end = loop {
        if bytes.len() >= MAX_HEADER {
            return Err(error(
                413,
                "STOCKLANA_HEADER_BOUND",
                "Request headers exceed 4 KiB.",
            ));
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                error(
                    408,
                    "STOCKLANA_HEADER_TIMEOUT",
                    "Request admission expired.",
                )
            })?;
        stream
            .get_mut()
            .set_read_timeout(Some(remaining))
            .map_err(|_| error(400, "STOCKLANA_TRANSPORT", "Request transport failed."))?;
        let header_budget = MAX_HEADER.saturating_add(1).saturating_sub(bytes.len());
        let mut bounded = Read::take(&mut *stream, header_budget as u64);
        let read = bounded
            .read_until(b'\n', &mut bytes)
            .map_err(|_| error(400, "STOCKLANA_TRANSPORT", "Request transport failed."))?;
        if read == 0 {
            return Err(error(
                400,
                "STOCKLANA_REQUEST",
                "Request ended before headers completed.",
            ));
        }
        if bytes.ends_with(b"\r\n\r\n") {
            break bytes.len();
        }
    };
    if header_end > MAX_HEADER {
        return Err(error(
            413,
            "STOCKLANA_HEADER_BOUND",
            "Request headers exceed 4 KiB.",
        ));
    }
    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| error(400, "STOCKLANA_REQUEST", "Request headers must be UTF-8."))?;
    let mut lines = headers[..headers.len() - 4].split("\r\n");
    let first = lines
        .next()
        .ok_or_else(|| error(400, "STOCKLANA_REQUEST", "Request line is missing."))?;
    let mut first = first.split(' ');
    let method = first.next().unwrap_or("");
    let path = first.next().unwrap_or("");
    let version = first.next().unwrap_or("");
    if first.next().is_some()
        || !matches!(method, "GET" | "POST")
        || !path.starts_with('/')
        || path.contains('?')
        || version != "HTTP/1.1"
    {
        return Err(error(400, "STOCKLANA_REQUEST", "Request line is invalid."));
    }
    let method = method.to_string();
    let path = path.to_string();
    let mut authorization = None;
    let mut product = None;
    let mut content_type = None;
    let mut length = None;
    let mut connection = None;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| error(400, "STOCKLANA_REQUEST", "Request header is malformed."))?;
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "authorization" if authorization.replace(value.to_string()).is_some() => {
                return Err(error(
                    400,
                    "STOCKLANA_REQUEST",
                    "Authorization header is duplicated.",
                ));
            }
            "x-skew-product" if product.replace(value.to_string()).is_some() => {
                return Err(error(
                    400,
                    "STOCKLANA_REQUEST",
                    "Product header is duplicated.",
                ));
            }
            "content-type" if content_type.replace(value.to_string()).is_some() => {
                return Err(error(
                    400,
                    "STOCKLANA_REQUEST",
                    "Content-Type header is duplicated.",
                ));
            }
            "content-length" => {
                if length.is_some() {
                    return Err(error(
                        400,
                        "STOCKLANA_REQUEST",
                        "Content-Length header is duplicated.",
                    ));
                }
                length =
                    Some(value.parse::<usize>().map_err(|_| {
                        error(400, "STOCKLANA_REQUEST", "Content-Length is invalid.")
                    })?);
            }
            "transfer-encoding" => {
                return Err(error(
                    400,
                    "STOCKLANA_REQUEST",
                    "Transfer-Encoding is not admitted.",
                ));
            }
            "connection" => {
                if connection.is_some() {
                    return Err(error(
                        400,
                        "STOCKLANA_REQUEST",
                        "Connection header is duplicated.",
                    ));
                }
                let value = value.to_ascii_lowercase();
                if !matches!(value.as_str(), "close" | "keep-alive") {
                    return Err(error(
                        400,
                        "STOCKLANA_REQUEST",
                        "Connection mode is not admitted.",
                    ));
                }
                connection = Some(value);
            }
            _ => {}
        }
    }
    let length = length.unwrap_or(0);
    if length > MAX_BODY {
        return Err(error(
            413,
            "STOCKLANA_BODY_BOUND",
            "Request body exceeds 32 KiB.",
        ));
    }
    if method == "POST" && length == 0 {
        return Err(error(
            400,
            "STOCKLANA_REQUEST",
            "POST requires a JSON body.",
        ));
    }
    let mut body = vec![0u8; length];
    let mut body_read = 0;
    while body_read < length {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| error(408, "STOCKLANA_BODY_TIMEOUT", "Request admission expired."))?;
        stream
            .get_mut()
            .set_read_timeout(Some(remaining))
            .map_err(|_| error(400, "STOCKLANA_TRANSPORT", "Request transport failed."))?;
        let read = stream
            .read(&mut body[body_read..])
            .map_err(|_| error(400, "STOCKLANA_TRANSPORT", "Request transport failed."))?;
        if read == 0 {
            return Err(error(
                400,
                "STOCKLANA_REQUEST",
                "Request body is incomplete.",
            ));
        }
        body_read += read;
    }
    Ok(Request {
        method,
        path,
        authorization: authorization.unwrap_or_default(),
        product: product.unwrap_or_default(),
        content_type: content_type.unwrap_or_default(),
        body,
        keep_alive: connection.as_deref() != Some("close"),
    })
}

fn publication_operation(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body).ok()
        .and_then(|v| v.get("operation").and_then(|v| v.as_str()).map(str::to_owned))
        .is_some_and(|op| matches!(op.as_str(), "LIST" | "GET" | "CHALLENGE" | "PUBLISH"))
}

fn dispatch(mesh: &StockMesh, request: Request, publication_only: bool) -> ApiReply {
    if !mesh.authorized(&request.authorization) {
        return error(
            401,
            "STOCKLANA_UNAUTHORIZED",
            "A valid Stocklana engine token is required.",
        );
    }
    if request.product != "stocklana" {
        return error(
            400,
            "STOCKLANA_PRODUCT",
            "X-Skew-Product must be stocklana.",
        );
    }
    if publication_only && (request.path != "/v1/strategies" || !publication_operation(&request.body)) {
        return error(503, "STOCKLANA_PUBLICATION_ONLY", "This connection publishes portfolios only. Investment is not enabled on this connection.");
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/v1/strategies") if request.content_type.eq_ignore_ascii_case("application/json") => mesh.strategies(&request.body),
        ("GET", "/v1/status") if request.body.is_empty() => mesh.status(),
        ("POST", "/v1/catalog/search")
            if request
                .content_type
                .eq_ignore_ascii_case("application/json") =>
        {
            mesh.catalog_search(&request.body)
        }
        ("POST", "/v1/etf/preview")
            if request
                .content_type
                .eq_ignore_ascii_case("application/json") =>
        {
            mesh.etf_preview(&request.body)
        }
        ("POST", "/v1/quote")
            if request
                .content_type
                .eq_ignore_ascii_case("application/json") =>
        {
            mesh.quote(&request.body)
        }
        ("POST", "/v1/portfolio")
            if request
                .content_type
                .eq_ignore_ascii_case("application/json") =>
        {
            mesh.portfolio(&request.body)
        }
        ("POST", "/v1/orders") if request.content_type.eq_ignore_ascii_case("application/json") => {
            mesh.orders(&request.body)
        }
        ("POST", "/v1/prepare")
            if request
                .content_type
                .eq_ignore_ascii_case("application/json") =>
        {
            mesh.prepare(&request.body)
        }
        ("POST", "/v1/submit")
            if request
                .content_type
                .eq_ignore_ascii_case("application/json") =>
        {
            mesh.submit(&request.body)
        }
        ("POST", "/v1/clear/preview")
            if request
                .content_type
                .eq_ignore_ascii_case("application/json") =>
        {
            mesh.clearing_preview(&request.body)
        }
        ("POST", "/v1/executor/freeze")
            if request
                .content_type
                .eq_ignore_ascii_case("application/json") =>
        {
            mesh.freeze(&request.body)
        }
        (
            "GET",
            "/v1/quote"
            | "/v1/catalog/search"
            | "/v1/etf/preview"
            | "/v1/strategies"
            | "/v1/portfolio"
            | "/v1/orders"
            | "/v1/prepare"
            | "/v1/submit"
            | "/v1/clear/preview"
            | "/v1/executor/freeze",
        )
        | ("POST", "/v1/status") => error(
            405,
            "STOCKLANA_METHOD",
            "HTTP method is not admitted for this endpoint.",
        ),
        (
            _,
            "/v1/quote"
            | "/v1/catalog/search"
            | "/v1/etf/preview"
            | "/v1/strategies"
            | "/v1/portfolio"
            | "/v1/orders"
            | "/v1/prepare"
            | "/v1/submit"
            | "/v1/clear/preview"
            | "/v1/executor/freeze",
        ) => error(415, "STOCKLANA_CONTENT_TYPE", "Use application/json."),
        _ => error(404, "STOCKLANA_NOT_FOUND", "Endpoint does not exist."),
    }
}

fn run() -> Result<(), String> {
    let args: Vec<_> = env::args().collect();
    let (mode, rpc, manifest, port) = match args.as_slice() {
        [_, mode, manifest, port] if mode == "--fixture" || mode == "--publication-only" => {
            (mode.as_str(), None, manifest.as_str(), port.as_str())
        }
        [_, mode, rpc, manifest, port] if mode == "--live" || mode == "--provider-readonly" => (
            mode.as_str(),
            Some(rpc.clone()),
            manifest.as_str(),
            port.as_str(),
        ),
        _ => return Err("stockmesh-api --fixture MANIFEST PORT | --live RPC MANIFEST PORT | --provider-readonly PROFILE MANIFEST PORT".into()),
    };
    let port = port.parse::<u16>().map_err(|error| error.to_string())?;
    if !(19000..=19999).contains(&port) {
        return Err("isolated port range".into());
    }
    let token = env::var("SKEW_STOCKMESH_TOKEN").map_err(|_| "SKEW_STOCKMESH_TOKEN required")?;
    let publication_only = mode == "--publication-only";
    let mesh = if publication_only {
        for variable in ["SKEW_STOCKMESH_PREPARE_MANIFEST", "SKEW_STOCKMESH_EXECUTOR_SEED_FILE", "SKEW_STOCKMESH_SENDER_JOURNAL", "SKEW_STOCKMESH_SUBMISSION_ARMED", "SKEW_STOCKMESH_DIRECT_STATE_SOCKET", "SKEW_STOCKMESH_DIRECT_STATE_MODE"] {
            if env::var_os(variable).is_some() { return Err(format!("publication-only mode forbids {variable}")); }
        }
        if env::var_os("SKEW_STOCKMESH_STRATEGY_STORE").is_none() { return Err("publication-only requires durable strategy store".into()); }
        StockMesh::load_publication(Path::new(manifest), token)?
    } else if mode == "--provider-readonly" {
        for variable in [
            "SKEW_STOCKMESH_PREPARE_MANIFEST",
            "SKEW_STOCKMESH_EXECUTOR_SEED_FILE",
            "SKEW_STOCKMESH_SENDER_JOURNAL",
            "SKEW_STOCKMESH_SUBMISSION_ARMED",
            "SKEW_STOCKMESH_DIRECT_STATE_SOCKET",
            "SKEW_STOCKMESH_DIRECT_STATE_MODE",
        ] {
            if env::var_os(variable).is_some() {
                return Err(format!("provider read-only mode forbids {variable}"));
            }
        }
        let profile = ProviderConfig::load(Path::new(
            rpc.as_deref().ok_or("provider profile required")?,
        ))?;
        StockMesh::load_provider_readonly(Path::new(manifest), token, profile.connect()?)?
    } else if let Some(rpc) = rpc {
        let deployment = env::var_os("SKEW_STOCKMESH_PREPARE_MANIFEST").map(PathBuf::from);
        let executor_seed = env::var_os("SKEW_STOCKMESH_EXECUTOR_SEED_FILE").map(PathBuf::from);
        if deployment.is_some() != executor_seed.is_some() {
            return Err(
                "SKEW_STOCKMESH_PREPARE_MANIFEST and SKEW_STOCKMESH_EXECUTOR_SEED_FILE must be set together"
                    .into(),
            );
        }
        let sender_journal = env::var_os("SKEW_STOCKMESH_SENDER_JOURNAL").map(PathBuf::from);
        let submission_arm = env::var("SKEW_STOCKMESH_SUBMISSION_ARMED").ok();
        match (sender_journal.as_ref(), submission_arm.as_deref()) {
            (None, None) => {}
            (Some(_), Some(SUBMISSION_SENTINEL)) => {}
            _ => {
                return Err(
                    "sender journal and exact SKEW_STOCKMESH_SUBMISSION_ARMED sentinel are required together"
                        .into(),
                )
            }
        }
        StockMesh::load_live_with_runtime_and_direct(
            Path::new(manifest),
            token,
            rpc,
            deployment.as_deref(),
            executor_seed.as_deref(),
            sender_journal.as_deref(),
            direct_config()?,
        )?
    } else {
        StockMesh::load_fixture(Path::new(manifest), token)?
    };
    if let Some(path) = env::var_os("SKEW_STOCKMESH_DISCOVERY_CATALOG") {
        mesh.load_discovery_catalog(Path::new(&path))?;
    }
    if let Some(path)=env::var_os("SKEW_STOCKMESH_STRATEGY_STORE") {mesh.open_strategy_store(Path::new(&path))?;}
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|error| error.to_string())?;
    let default_workers = std::thread::available_parallelism()
        .map_or(2, usize::from)
        .clamp(1, 16);
    let workers = bounded_env("SKEW_STOCKMESH_WORKERS", default_workers, 1, 64)?;
    let queue = bounded_env("SKEW_STOCKMESH_QUEUE", workers * 256, workers, 4096)?;
    let deadline_ms = bounded_env("SKEW_STOCKMESH_REQUEST_DEADLINE_MS", 250, 25, 2_000)? as u64;
    let max_requests_per_connection =
        bounded_env("SKEW_STOCKMESH_MAX_REQUESTS_PER_CONNECTION", 64, 1, 1_024)?;
    let (sender, receiver) = sync_channel::<(TcpStream, Instant)>(queue);
    let receiver = Arc::new(Mutex::new(receiver));
    for _ in 0..workers {
        let receiver = receiver.clone();
        let mesh = mesh.clone();
        std::thread::spawn(move || loop {
            let received = receiver
                .lock()
                .ok()
                .and_then(|receiver| receiver.recv().ok());
            let Some((stream, admitted)) = received else {
                break;
            };
            let mut stream = BufReader::with_capacity(MAX_HEADER + MAX_BODY, stream);
            let mut admitted = admitted;
            for served in 0..max_requests_per_connection {
                match parse(&mut stream, admitted, deadline_ms) {
                    Ok(request) => {
                        let keep_alive = request.keep_alive
                            && served.saturating_add(1) < max_requests_per_connection;
                        let reply = dispatch(&mesh, request, publication_only);
                        if !response(stream.get_mut(), reply, keep_alive) || !keep_alive {
                            break;
                        }
                        admitted = Instant::now();
                    }
                    Err(reply) => {
                        response(stream.get_mut(), reply, false);
                        break;
                    }
                }
            }
        });
    }
    println!(
        "{}",
        json!({
            "listen":format!("127.0.0.1:{port}"),
            "mode":mode,
            "workers":workers,
            "queue":queue,
            "requestDeadlineMs":deadline_ms,
            "maxRequestsPerConnection":max_requests_per_connection,
            "transactionSigning":false,
            "executorBidSigning":mesh.prepare_enabled(),
            "automaticPrepare":mesh.prepare_enabled(),
            "submission":mesh.submission_enabled()
        })
    );
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        match sender.try_send((stream, Instant::now())) {
            Ok(()) => {}
            Err(TrySendError::Full((mut stream, _))) => {
                response(
                    &mut stream,
                    error(
                        429,
                        "STOCKLANA_OVERLOADED",
                        "Stocklana admission is saturated. Retry with jitter.",
                    ),
                    false,
                );
            }
            Err(TrySendError::Disconnected(_)) => return Err("workers unavailable".into()),
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::Shutdown;

    #[test]
    fn publication_transport_cannot_prepare_quote_submit_or_continue() {
        for operation in ["LIST", "GET", "CHALLENGE", "PUBLISH"] {
            assert!(publication_operation(format!("{{\"operation\":\"{operation}\"}}").as_bytes()));
        }
        for operation in ["PREPARE", "CONTINUE", "PREVIEW", "SUBMIT", "publish"] {
            assert!(!publication_operation(format!("{{\"operation\":\"{operation}\"}}").as_bytes()));
        }
        assert!(!publication_operation(b"not json"));
        assert!(!publication_operation(b"{\"operation\":42}"));
    }

    fn parse_bytes(bytes: &[u8]) -> Result<Request, ApiReply> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let bytes = bytes.to_vec();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(&bytes).unwrap();
            stream.shutdown(Shutdown::Write).unwrap();
        });
        let (stream, _) = listener.accept().unwrap();
        let result = parse(&mut BufReader::new(stream), Instant::now(), 100);
        client.join().unwrap();
        result
    }

    #[test]
    fn transport_accepts_exact_bounded_request() {
        let request = parse_bytes(
            b"POST /v1/quote HTTP/1.1\r\nAuthorization: Bearer abc\r\nX-Skew-Product: stocklana\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        )
        .unwrap();
        assert_eq!(request.path, "/v1/quote");
        assert_eq!(request.body, b"{}");
        assert!(request.keep_alive);
    }

    #[test]
    fn transport_rejects_ambiguous_framing() {
        assert_eq!(
            parse_bytes(
                b"POST /v1/quote HTTP/1.1\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n{}"
            )
            .unwrap_err()
            .status,
            400
        );
        assert_eq!(
            parse_bytes(b"POST /v1/quote HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n")
                .unwrap_err()
                .status,
            400
        );
    }

    #[test]
    fn transport_retains_bounded_pipelined_requests() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .write_all(
                    b"GET /v1/status HTTP/1.1\r\nContent-Length: 0\r\n\r\nGET /v1/status HTTP/1.1\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                )
                .unwrap();
            stream.shutdown(Shutdown::Write).unwrap();
        });
        let (stream, _) = listener.accept().unwrap();
        let mut stream = BufReader::new(stream);
        let first = parse(&mut stream, Instant::now(), 100).unwrap();
        let second = parse(&mut stream, Instant::now(), 100).unwrap();
        assert!(first.keep_alive);
        assert!(!second.keep_alive);
        client.join().unwrap();
    }
}
