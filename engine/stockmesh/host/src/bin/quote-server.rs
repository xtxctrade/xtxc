//! Loopback-only bounded HTTP quote lane. No signing/submission endpoint.
//! --fixture is a throughput experiment; --live polls coherent mainnet state.
use serde_json::json;
use skew_execution_host::{
    feed::Feed,
    rpc::Rpc,
    world::{LaneConfig, World},
};
use std::{
    env,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        mpsc::{sync_channel, TrySendError},
        Arc, Mutex, RwLock,
    },
    time::{Duration, Instant},
};
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
fn response(s: &mut TcpStream, status: u16, body: &str) {
    let _ = s.set_write_timeout(Some(Duration::from_millis(20)));
    let _=write!(s,"HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",if status==200{"OK"}else{"Unavailable"},body.len());
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 5 {
        return Err("quote-server --fixture SNAPSHOT CONFIG PORT | --live RPC CONFIG PORT".into());
    }
    let config: LaneConfig =
        serde_json::from_slice(&std::fs::read(&args[3]).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let config = config.world();
    let port = args[4].parse::<u16>().map_err(|e| e.to_string())?;
    if !(19000..=19999).contains(&port) {
        return Err("isolated port range".into());
    }
    let live = args[1] == "--live";
    if !live && args[1] != "--fixture" {
        return Err("mode".into());
    }
    let feed = Arc::new(Feed::new(
        config.keys()?,
        if live {
            Duration::from_millis(1500)
        } else {
            Duration::from_secs(3600)
        },
        4 * 1024 * 1024,
    )?);
    let market: Arc<RwLock<Option<Arc<World>>>> = Arc::new(RwLock::new(None));
    if live {
        let rpc = Rpc::pinned(
            args[2].clone(),
            "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d".into(),
        )?;
        let f = feed.clone();
        let state = market.clone();
        std::thread::spawn(move || loop {
            let began = Instant::now();
            let prior = state.read().unwrap().clone();
            let result = f
                .refresh(&rpc)
                .and_then(|s| config.compile(&s, prior.as_deref()));
            match result {
                Ok(m) => {
                    println!(
                        "{}",
                        json!({"event":"published","slot":m.slot,"generation":m.generation,"world":m.metrics,"refresh_and_compile_ns":began.elapsed().as_nanos() as u64})
                    );
                    *state.write().unwrap() = Some(Arc::new(m));
                }
                Err(e) => {
                    *state.write().unwrap() = None;
                    eprintln!("feed: {e}");
                }
            }
            if let Some(delay) = Duration::from_millis(500).checked_sub(began.elapsed()) {
                std::thread::sleep(delay);
            }
        });
    } else {
        let data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&args[2]).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        let s = feed.publish(&data)?;
        *market.write().unwrap() = Some(Arc::new(config.compile(&s, None)?));
    }
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    let default_workers = std::thread::available_parallelism()
        .map_or(2, usize::from)
        .clamp(1, 16);
    let workers = bounded_env("SKEW_QUOTE_WORKERS", default_workers, 1, 64)?;
    let queue_capacity = bounded_env("SKEW_QUOTE_QUEUE", workers * 32, workers, 4096)?;
    let header_deadline_ms = bounded_env("SKEW_QUOTE_HEADER_DEADLINE_MS", 100, 10, 500)?;
    let (tx, rx) = sync_channel::<(TcpStream, Instant)>(queue_capacity);
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..workers {
        let rx = rx.clone();
        let state = market.clone();
        let f = feed.clone();
        std::thread::spawn(move || {
            let mut memo = skew_native::memo::QuoteMemo::default();
            loop {
                let stream = rx.lock().unwrap().recv();
                let Ok((mut s, admitted)) = stream else {
                    break;
                };
                // An absolute ingress deadline prevents slow byte trickles from
                // renewing a per-read timeout forever. Queue residence counts too.
                let deadline = admitted + Duration::from_millis(header_deadline_ms as u64);
                let mut b = [0u8; 1024];
                let mut n = 0;
                while n < b.len() {
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        break;
                    };
                    if remaining.is_zero() {
                        break;
                    }
                    let _ = s.set_read_timeout(Some(remaining));
                    match s.read(&mut b[n..]) {
                        Ok(0) | Err(_) => break,
                        Ok(k) => {
                            n += k;
                            if b[..n].windows(4).any(|x| x == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let request = if b[..n].windows(4).any(|x| x == b"\r\n\r\n") {
                    std::str::from_utf8(&b[..n]).unwrap_or("")
                } else {
                    ""
                };
                let input = request
                    .lines()
                    .next()
                    .and_then(|line| line.strip_prefix("GET /quote?amount="))
                    .and_then(|s| s.strip_suffix(" HTTP/1.1"))
                    .and_then(|s| s.parse::<u64>().ok());
                let result = (|| {
                    let q = input.ok_or("invalid quote request".to_string())?;
                    let snapshot = f.read()?;
                    let m = state
                        .read()
                        .map_err(|_| "market poisoned")?
                        .clone()
                        .ok_or("market unavailable")?;
                    if m.generation != snapshot.generation || m.slot != snapshot.slot {
                        return Err("publication in progress".into());
                    }
                    let mut out = m.quote_with_memo(q, &mut memo)?;
                    f.validate_fence(&snapshot)?;
                    out["stateRevision"] = json!(snapshot.revision);
                    out["fixture"] = json!(!live);
                    Ok::<_, String>(out.to_string())
                })();
                match result {
                    Ok(body) => response(&mut s, 200, &body),
                    Err(e) => response(&mut s, 503, &json!({"error":e}).to_string()),
                }
            }
        });
    }
    println!(
        "{}",
        json!({"listen":format!("127.0.0.1:{port}"),"workers":workers,"queue_capacity":queue_capacity,"header_deadline_ms":header_deadline_ms,"mode":args[1],"signing":false})
    );
    for stream in listener.incoming() {
        let Ok(mut s) = stream else {
            continue;
        };
        let _ = s.set_nodelay(true);
        match tx.try_send((s, Instant::now())) {
            Ok(()) => {}
            Err(TrySendError::Full((stream, _))) => {
                s = stream;
                response(&mut s, 503, "{\"error\":\"overloaded\"}");
            }
            Err(TrySendError::Disconnected(_)) => return Err("workers unavailable".into()),
        }
    }
    Ok(())
}
