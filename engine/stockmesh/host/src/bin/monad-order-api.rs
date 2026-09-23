//! Isolated loopback control plane for PR02. No RPC sender, signer or private
//! key. A reported transaction hash is not proof of inclusion or delivery.
use serde_json::{json, Value};
use skew_execution_host::{monad::{feed::BoundedRpc, journal::MonadJournal,
    monday_public::{MarketRequest, MarketSide}, preflight_market::{preflight_market_call,
        preflight_market_budget, MarketBudgetRequest, MarketPreflight}}, monad_contract::Catalog};
use std::{env, io::{Read, Write}, net::{TcpListener, TcpStream}, path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH}};

const HEADER_BOUND: usize = 4096;
const BODY_BOUND: usize = 8192;

fn answer(stream: &mut TcpStream, status: u16, value: Value) {
    let bytes = value.to_string();
    let reason = match status { 200 => "OK", 201 => "Created", 400 => "Bad Request", 401 => "Unauthorized",
        403 => "Forbidden", 404 => "Not Found", 405 => "Method Not Allowed", 409 => "Conflict",
        413 => "Content Too Large", 415 => "Unsupported Media Type", 422 => "Unprocessable Entity",
        _ => "Service Unavailable" };
    let _ = write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}", bytes.len(), bytes);
    let _ = stream.flush();
}
fn error(status: u16, code: &str) -> (u16, Value) { (status, json!({"error":{"code":code}})) }

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MarketPreflightInput {
    owner: String,
    asset_id: String,
    side: MarketSide,
    wallet_debit_atoms: String,
    order_amount_atoms: String,
    deadline_secs: u64,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MarketBudgetInput {
    owner: String,
    asset_id: String,
    side: MarketSide,
    wallet_debit_atoms: String,
    deadline_secs: u64,
}

fn decimal_atoms(value: &str) -> Option<u128> {
    if value.is_empty() || value.len() > 39 || value.starts_with('0')
        || !value.bytes().all(|byte| byte.is_ascii_digit()) { return None; }
    value.parse().ok()
}

fn market_preflight_response(result: MarketPreflight, order_amount_atoms: u128) -> (u16, Value) {
    (200, json!({"schema":"xtxc.monad.market-preflight/v1",
        "call":{"chainId":result.call.chain_id,"from":result.call.from,
            "to":result.call.to,"data":result.call.data,"valueAtoms":result.call.value_atoms,
            "inputToken":result.call.input_token,"stockToken":result.call.stock_token,
            "allowanceSpender":result.call.allowance_spender,
            "allowanceAtoms":result.call.allowance_atoms.to_string(),
            "deadlineSecs":result.call.deadline_secs,
            "executionClass":result.call.execution_class,
            "guaranteesStockMinimum":result.call.guarantees_stock_minimum},
        "orderAmountAtoms":order_amount_atoms.to_string(),
        "blockNumber":result.block_number,"blockHash":result.block_hash,
        "routerImplementationSha256":result.router_implementation_sha256,
        "stockImplementationSha256":result.stock_implementation_sha256,
        "inputBalanceAtoms":result.input_balance_atoms.to_string(),
        "inputAllowanceAtoms":result.input_allowance_atoms.to_string(),
        "approvalRequired":result.approval_required,"simulated":result.simulated,
        "settlementPendingAfterSubmission":result.settlement_pending_after_submission}))
}

fn market_preflight(body: &[u8], owner: &str, catalog: &Catalog) -> (u16, Value) {
    let input: MarketPreflightInput = match serde_json::from_slice(body) {
        Ok(value) => value, Err(_) => return error(422, "MARKET_REQUEST_INVALID"),
    };
    if !input.owner.eq_ignore_ascii_case(owner) { return error(403, "OWNER_MISMATCH"); }
    let (Some(wallet_debit_atoms), Some(order_amount_atoms)) =
        (decimal_atoms(&input.wallet_debit_atoms), decimal_atoms(&input.order_amount_atoms))
    else { return error(422, "MARKET_AMOUNT_INVALID"); };
    let request = MarketRequest { owner: input.owner, asset_id: input.asset_id,
        side: input.side, wallet_debit_atoms, order_amount_atoms,
        deadline_secs: input.deadline_secs };
    let endpoint = match env::var("MONAD_RPC_URL_FILE") {
        Ok(path) => match std::fs::read_to_string(path) {
            Ok(value) => value.trim().to_owned(), Err(_) => return error(503, "MONAD_PROVIDER_UNAVAILABLE"),
        },
        Err(_) => "https://rpc.monad.xyz".into(),
    };
    let Ok(mut rpc) = BoundedRpc::new(endpoint, 32) else {
        return error(503, "MONAD_PROVIDER_UNAVAILABLE");
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return error(503, "CLOCK_UNAVAILABLE");
    };
    let result = match preflight_market_call(&mut rpc, catalog, &request, now.as_secs()) {
        Ok(value) => value,
        Err(message) if message.contains("wallet input balance insufficient") =>
            return error(422, "WALLET_INPUT_INSUFFICIENT"),
        Err(_) => return error(503, "MARKET_PREFLIGHT_FAILED"),
    };
    // Never serialize u128 atoms as JSON numbers: browser Number loses precision.
    market_preflight_response(result, request.order_amount_atoms)
}

fn market_budget_preflight(body: &[u8], owner: &str, catalog: &Catalog) -> (u16, Value) {
    let input: MarketBudgetInput = match serde_json::from_slice(body) {
        Ok(value) => value, Err(_) => return error(422, "MARKET_BUDGET_INVALID"),
    };
    if !input.owner.eq_ignore_ascii_case(owner) { return error(403, "OWNER_MISMATCH"); }
    let Some(wallet_debit_atoms) = decimal_atoms(&input.wallet_debit_atoms) else {
        return error(422, "MARKET_AMOUNT_INVALID");
    };
    let budget = MarketBudgetRequest { owner: input.owner, asset_id: input.asset_id,
        side: input.side, wallet_debit_atoms, deadline_secs: input.deadline_secs };
    let endpoint = match env::var("MONAD_RPC_URL_FILE") {
        Ok(path) => match std::fs::read_to_string(path) {
            Ok(value) => value.trim().to_owned(), Err(_) => return error(503, "MONAD_PROVIDER_UNAVAILABLE"),
        },
        Err(_) => "https://rpc.monad.xyz".into(),
    };
    let Ok(mut rpc) = BoundedRpc::new(endpoint, 32) else {
        return error(503, "MONAD_PROVIDER_UNAVAILABLE");
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return error(503, "CLOCK_UNAVAILABLE");
    };
    match preflight_market_budget(&mut rpc, catalog, &budget, now.as_secs()) {
        Ok((request, result)) => market_preflight_response(result, request.order_amount_atoms),
        Err(message) if message.contains("wallet input balance insufficient") =>
            error(422, "WALLET_INPUT_INSUFFICIENT"),
        Err(message) if message.contains("budget below minimum") =>
            error(422, "MARKET_BUDGET_TOO_SMALL"),
        Err(_) => error(503, "MARKET_PREFLIGHT_FAILED"),
    }
}

struct Request { method: String, path: String, owner: String, body: Vec<u8> }
fn read_request(stream: &mut TcpStream, secret: &str) -> Result<Request, (u16, Value)> {
    stream.set_read_timeout(Some(Duration::from_secs(2))).map_err(|_| error(400, "TRANSPORT"))?;
    stream.set_write_timeout(Some(Duration::from_secs(2))).map_err(|_| error(400, "TRANSPORT"))?;
    let mut header = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() == HEADER_BOUND { return Err(error(413, "HEADER_BOUND")); }
        stream.read_exact(&mut byte).map_err(|_| error(400, "HEADER_INCOMPLETE"))?;
        header.push(byte[0]);
    }
    let text = std::str::from_utf8(&header).map_err(|_| error(400, "HEADER_ENCODING"))?;
    let mut lines = text.trim_end_matches("\r\n").split("\r\n");
    let request_line = lines.next().ok_or_else(|| error(400, "REQUEST_LINE"))?;
    let mut words = request_line.split(' ');
    let method = words.next().unwrap_or("");
    let path = words.next().unwrap_or("");
    if words.next() != Some("HTTP/1.1") || words.next().is_some() || !matches!(method, "GET" | "POST")
        || !path.starts_with("/v1/") || path.len() > 200 || !path.is_ascii() {
        return Err(error(400, "REQUEST_LINE"));
    }
    let (mut authorization, mut owner, mut content_type, mut content_length, mut host) = (None, None, None, None, None);
    for line in lines {
        if line.is_empty() { continue; }
        let (name, value) = line.split_once(':').ok_or_else(|| error(400, "HEADER"))?;
        let value = value.trim();
        if value.contains('\r') || value.contains('\n') { return Err(error(400, "HEADER")); }
        let slot = if name.eq_ignore_ascii_case("authorization") { &mut authorization }
            else if name.eq_ignore_ascii_case("x-owner") { &mut owner }
            else if name.eq_ignore_ascii_case("content-type") { &mut content_type }
            else if name.eq_ignore_ascii_case("content-length") { &mut content_length }
            else if name.eq_ignore_ascii_case("host") { &mut host }
            else if name.eq_ignore_ascii_case("transfer-encoding") { return Err(error(400, "TRANSFER_ENCODING")); }
            else { continue };
        if slot.replace(value).is_some() { return Err(error(400, "DUPLICATE_HEADER")); }
    }
    if host.is_none() { return Err(error(400, "HOST_REQUIRED")); }
    if authorization != Some(format!("Bearer {secret}").as_str()) { return Err(error(401, "GATEWAY_AUTH")); }
    let length = match content_length { Some(value) => value.parse::<usize>().map_err(|_| error(400, "CONTENT_LENGTH"))?, None => 0 };
    if length > BODY_BOUND { return Err(error(413, "BODY_BOUND")); }
    if method == "POST" && content_type != Some("application/json") { return Err(error(415, "JSON_REQUIRED")); }
    if method == "GET" && length != 0 { return Err(error(400, "GET_BODY")); }
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).map_err(|_| error(400, "BODY_INCOMPLETE"))?;
    Ok(Request { method: method.into(), path: path.into(), owner: owner.unwrap_or("").into(), body })
}

fn dispatch(req: Request, catalog: &Catalog, journal: &mut MonadJournal) -> (u16, Value) {
    if req.method == "GET" && req.path == "/v1/catalog" { return (200, serde_json::to_value(catalog).unwrap()); }
    if req.owner.len() != 42 || !req.owner.starts_with("0x")
        || !req.owner[2..].bytes().all(|b| b.is_ascii_hexdigit()) { return error(403, "OWNER_REQUIRED"); }
    if req.method == "GET" && req.path == "/v1/orders" {
        return (200, json!({"orders":journal.list(&req.owner)}));
    }
    if req.method == "GET" && req.path.starts_with("/v1/orders/") {
        let id = &req.path[11..];
        return match journal.get(id, &req.owner) { Some(order) => (200, json!(order)), None => error(404, "ORDER_NOT_FOUND") };
    }
    if req.method == "POST" && req.path == "/v1/market-preflight" {
        return market_preflight(&req.body, &req.owner, catalog);
    }
    if req.method == "POST" && req.path == "/v1/market-budget-preflight" {
        return market_budget_preflight(&req.body, &req.owner, catalog);
    }
    if req.method == "POST" && req.path == "/v1/prepare" {
        // A catalog entry does not attest a quote. PR03 must bind an actual
        // provider quote to owner, input, deadline and an admitted adapter.
        // Do not let an arbitrary browser digest become a prepared trade.
        return error(503, "QUOTE_ADMISSION_NOT_CONNECTED");
    }
    if req.method == "POST" && req.path.starts_with("/v1/orders/") && req.path.ends_with("/report-submission") {
        let id = &req.path[11..req.path.len()-18];
        let body: Value = match serde_json::from_slice(&req.body) { Ok(value) => value, Err(_) => return error(422, "REPORT_SCHEMA") };
        if body.as_object().map(|o| o.len()) != Some(2) { return error(422, "REPORT_SCHEMA"); }
        let (Some(hash), Some(nonce)) = (body.get("txHash").and_then(Value::as_str), body.get("txNonce").and_then(Value::as_u64)) else { return error(422, "REPORT_SCHEMA"); };
        return match journal.report_submission(id, &req.owner, hash, nonce) {
            Ok(order) => (200, json!(order)), Err(_) => error(409, "ORDER_BINDING_CONFLICT") };
    }
    if req.method == "POST" && req.path.starts_with("/v1/orders/") && req.path.ends_with("/unknown") {
        let id = &req.path[11..req.path.len()-8];
        return match journal.mark_unknown(id, &req.owner) { Ok(order) => (200, json!(order)), Err(_) => error(409, "ORDER_STATE_CONFLICT") };
    }
    if req.method == "POST" && req.path.starts_with("/v1/orders/") && req.path.ends_with("/cancel") {
        let id = &req.path[11..req.path.len()-7];
        return match journal.cancel_unsigned(id, &req.owner) { Ok(order) => (200, json!(order)), Err(_) => error(409, "ORDER_STATE_CONFLICT") };
    }
    if matches!(req.path.as_str(), "/v1/quote" | "/v1/portfolio" | "/v1/etfs") {
        return error(503, "DEPENDENCY_NOT_CONNECTED");
    }
    error(404, "NOT_FOUND")
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 4 { return Err("monad-order-api CATALOG JOURNAL PORT".into()); }
    let port = args[3].parse::<u16>().map_err(|_| "port")?;
    if !(19000..=19999).contains(&port) { return Err("isolated port range".into()); }
    let secret = env::var("XTXC_MONAD_GATEWAY_TOKEN").map_err(|_| "gateway token required")?;
    if secret.len() < 32 || secret.len() > 256 { return Err("gateway token bounds".into()); }
    let catalog: Catalog = serde_json::from_slice(&std::fs::read(&args[1]).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    catalog.validate()?;
    let mut journal = MonadJournal::open(Path::new(&args[2]))?;
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    println!("{}", json!({"listen":format!("127.0.0.1:{port}"),"signing":false,"submission":false,"products":catalog.products.len()}));
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue; };
        let reply = match read_request(&mut stream, &secret) {
            Ok(req) => dispatch(req, &catalog, &mut journal), Err(reply) => reply,
        };
        answer(&mut stream, reply.0, reply.1);
    }
    Ok(())
}
fn main() { if let Err(error) = run() { eprintln!("monad-order-api: {error}"); std::process::exit(1); } }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_and_unadmitted_requests_fail_closed() {
        let catalog: Catalog = serde_json::from_str(include_str!("../../../monad/catalog/registry.v1.json")).unwrap();
        let path = std::env::temp_dir().join(format!("xtxc-monad-api-{}", std::process::id()));
        let mut journal = MonadJournal::open(&path).unwrap();
        let owner = "0x1111111111111111111111111111111111111111";
        let req = Request { method: "POST".into(), path: "/v1/prepare".into(), owner: owner.into(), body: b"{}".to_vec() };
        assert_eq!(dispatch(req, &catalog, &mut journal).0, 503);
        assert!(journal.list(owner).is_empty());
        drop(journal);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(format!("{}.lock", path.display())).unwrap();
    }
    #[test]
    fn market_request_rejects_foreign_owner_and_precision_loss_before_rpc() {
        let catalog: Catalog = serde_json::from_str(include_str!("../../../monad/catalog/registry.v1.json")).unwrap();
        assert_eq!(decimal_atoms("1000000000000000000"), Some(1_000_000_000_000_000_000));
        assert_eq!(decimal_atoms("01"), None);
        let input = serde_json::json!({"owner":"0x2222222222222222222222222222222222222222",
            "assetId":"unused","side":"BUY","walletDebitAtoms":"1000000",
            "orderAmountAtoms":"990000000000000000","deadlineSecs":1});
        assert_eq!(market_preflight(input.to_string().as_bytes(),
            "0x1111111111111111111111111111111111111111", &catalog).0, 403);
        let budget = serde_json::json!({"owner":"0x2222222222222222222222222222222222222222",
            "assetId":"unused","side":"BUY","walletDebitAtoms":"1000000","deadlineSecs":1});
        assert_eq!(market_budget_preflight(budget.to_string().as_bytes(),
            "0x1111111111111111111111111111111111111111", &catalog).0, 403);
    }
}
