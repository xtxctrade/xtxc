//! Local HTTP fixtures prove recovery boundaries, never mainnet execution.
use super::*;
use crate::journal::{Entry, Journal};
use std::{io::{BufRead, BufReader, Read, Write}, net::TcpListener, path::PathBuf, thread::JoinHandle};

pub(crate) fn rpc_script(script: Vec<(&'static str, Value)>) -> (Rpc, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        for (method, result) in script {
            let deadline = Instant::now() + Duration::from_secs(8);
            let socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing expected RPC {method}");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut reader = BufReader::new(socket);
            let mut length = 0usize;
            loop {
                let mut line = String::new(); reader.read_line(&mut line).unwrap();
                if line == "\r\n" { break; }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") { length = v.trim().parse().unwrap(); }
            }
            assert!(length < 8192);
            let mut bytes = vec![0; length]; reader.read_exact(&mut bytes).unwrap();
            let request: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(request["method"], method, "recovery must not transmit, refresh a blockhash or simulate");
            let body = json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string();
            write!(reader.get_mut(), "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    (Rpc::pinned(url, MAINNET_GENESIS.into()).unwrap(), worker)
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!("skew-submit-recovery-{}-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&path).unwrap(); Self(path)
    }
    fn journal(&self) -> PathBuf { self.0.join("isolated.wal") }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        // Only the exact generated fixture files, never a broad recursive path.
        let _ = std::fs::remove_file(self.journal());
        let _ = std::fs::remove_file(self.0.join("isolated.wal.lock"));
        let _ = std::fs::remove_file(self.0.join("isolated.wal.compact"));
        let _ = std::fs::remove_dir(&self.0);
    }
}

fn fixture(sell: bool) -> (StockMesh, Entry, SubmitRequest) {
    let mut f = crate::exposure_receipt::tests::fixture();
    let (api, _, _) = super::tests::prepared_fixture(&mut f);
    let mut entry = f.entry;
    entry.id = format!("stkp_{}", "1".repeat(32));
    entry.quote_id = Some(format!("stkq_{}", "2".repeat(32)));
    entry.phase = Phase::Prepared;
    entry.attempts = 0;
    if sell {
        // Only exercise identity/lifecycle here; this is not a sell receipt.
        entry.expected_swap = Some(crate::receipt::Expected {
            owner: f.intent.owner.clone(), input_account: "input".into(), input_mint: "stock".into(),
            output_account: "output".into(), output_mint: "cash".into(), input: 1,
            minimum_output: 1, quoted_output: 1, maximum_cu: 200000, route: [7;32],native_output:None,
        });
        entry.expected_exposure = None;
    } else { entry.expected_exposure = Some(f.expected); }
    let request = SubmitRequest { quote_id: entry.quote_id.clone().unwrap(), prepared_id: entry.id.clone(),
        owner: f.intent.owner, signed_transaction_base64: STANDARD.encode(&entry.wire) };
    assert_eq!(entry.expected_exposure.as_ref().map(|v| v.wallet_owner()).or_else(|| entry.expected_swap.as_ref().map(|v| v.owner.as_str())), Some(request.owner.as_str()));
    crate::sender::authorize(&entry.wire, crate::sender::Authorization { intent_id: entry.id.clone(),
        message_hash: entry.message_hash, last_valid_height:entry.last_valid_height, resources:entry.resources.clone() }).unwrap();
    // All quote, prepared and lane state gone: a restarted read-only recovery
    // path cannot gain fresh authorization from this test's other fixtures.
    api.quotes.lock().unwrap().values.clear();
    api.sell_quotes.lock().unwrap().values.clear();
    (api, entry, request)
}

fn body(request: &SubmitRequest) -> Vec<u8> {
    serde_json::to_vec(&json!({"quoteId":request.quote_id,"preparedId":request.prepared_id,
        "owner":request.owner,"signedTransactionBase64":request.signed_transaction_base64})).unwrap()
}

#[test]
fn whole_investment_status_recovers_prefix_without_building_or_sending_next_step(){
    let scratch=Scratch::new();let (mut api,_,_)=fixture(false);let p=crate::investment::tests::publication(4);
    let owner=p.document.creator.clone();let total="200".to_string();let nonce="23".repeat(16);
    let plan=crate::investment::Plan::from_publication(&p,owner.clone(),total.clone(),nonce.clone(),20).unwrap();
    let first=crate::investment::tests::entry_fixture(&plan,0,1);let second=crate::investment::tests::entry_fixture(&plan,1,2);
    let (rpc,worker)=rpc_script(vec![("getGenesisHash",json!(MAINNET_GENESIS))]);
    let mut journal=Journal::open(&scratch.journal(),8,1024*1024).unwrap();journal.insert(first.clone()).unwrap();journal.update(&first.id,Phase::Unknown,true).unwrap();
    api.sender=Some(Mutex::new(Sender{journal,rpc,max_attempts:3}));worker.join().unwrap(); // offline provider, UNKNOWN persists
    let status=api.continue_strategy(p.clone(),owner.clone(),total.clone(),nonce.clone(),p.document.catalog_revision.clone(),20,true).unwrap();
    assert_eq!(status["schema"],"xtxc.strategy-investment/v1");assert_eq!(status["state"],"PENDING");assert_eq!(status["nextIndex"],0);assert_eq!(status["completedInputAtoms"],"0");
    {let mut s=api.sender.as_ref().unwrap().lock().unwrap();s.journal.update(&first.id,Phase::Finalized,false).unwrap();s.journal.update(&first.id,Phase::Reconciled,false).unwrap();}
    let status=api.continue_strategy(p.clone(),owner.clone(),total.clone(),nonce.clone(),p.document.catalog_revision.clone(),20,false).unwrap();
    assert_eq!(status["state"],"READY");assert_eq!(status["nextIndex"],1);assert_eq!(status["completedInputAtoms"],"90");
    {let mut s=api.sender.as_ref().unwrap().lock().unwrap();s.journal.insert(second.clone()).unwrap();s.journal.update(&second.id,Phase::Finalized,false).unwrap();s.journal.update(&second.id,Phase::Reconciled,false).unwrap();s.journal.compact().unwrap();}
    let status=api.continue_strategy(p.clone(),owner,total,nonce,p.document.catalog_revision.clone(),20,true).unwrap();
    assert_eq!(status["state"],"COMPLETE");assert_eq!(status["completedInputAtoms"],"180");assert_eq!(status["plan"]["retainedCashAtoms"],"20");
    assert_eq!(api.sender.as_ref().unwrap().lock().unwrap().journal.entries().count(),2);
}

fn restart(scratch: &Scratch, entry: &Entry) -> Journal {
    {
        let mut journal = Journal::open(&scratch.journal(), 8, 65536).unwrap();
        journal.insert(entry.clone()).unwrap();
        journal.update(&entry.id, Phase::Unknown, true).unwrap();
        journal.compact().unwrap();
    }
    Journal::open(&scratch.journal(), 8, 65536).unwrap()
}

#[test]
fn buy_and_sell_retry_survive_empty_caches_restart_and_compaction_without_send() {
    for sell in [false, true] {
        let scratch = Scratch::new();
        let (mut api, entry, request) = fixture(sell);
        let (rpc, worker) = rpc_script(vec![("getGenesisHash", json!(MAINNET_GENESIS)),
            ("getGenesisHash", json!(MAINNET_GENESIS)), ("getSignatureStatuses", json!({"value":[null]}))]);
        api.sender = Some(Mutex::new(Sender { journal: restart(&scratch, &entry), rpc, max_attempts:3 }));
        let reply = api.submit(&body(&request));
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert_eq!(reply.body["phase"], "UNKNOWN");
        assert_eq!(reply.body["attempts"], 1);
        assert_eq!(reply.body["signature"], entry.signature);
        assert_eq!(reply.body["recovered"], true);
        let mut sender = api.sender.as_ref().unwrap().lock().unwrap();
        assert_eq!(sender.journal.get(&entry.id).unwrap().wire, entry.wire);
        let mut conflicting = entry.clone(); conflicting.id = "other".into(); conflicting.signature = "other".into();
        assert!(sender.journal.insert(conflicting).is_err(), "UNKNOWN keeps economic resource reservations");
        worker.join().unwrap();
    }
}

#[test]
fn replay_binding_rejects_quote_wallet_wire_and_legacy_records_before_any_rpc() {
    let scratch = Scratch::new();
    let (mut api, entry, mut request) = fixture(false);
    let (rpc, worker) = rpc_script(vec![("getGenesisHash", json!(MAINNET_GENESIS))]);
    api.sender = Some(Mutex::new(Sender { journal: restart(&scratch, &entry), rpc, max_attempts:3 }));
    worker.join().unwrap();
    let quote = request.quote_id.clone(); request.quote_id = format!("stkq_{}", "3".repeat(32));
    assert_eq!(api.submit(&body(&request)).body["error"]["code"], "STOCKLANA_SUBMISSION_CONFLICT");
    request.quote_id = quote;
    let owner = request.owner.clone(); request.owner = "11111111111111111111111111111111".into();
    assert_eq!(api.submit(&body(&request)).status,409); request.owner = owner;
    let mut other_wire = entry.wire.clone(); other_wire[1] ^= 1;
    request.signed_transaction_base64 = STANDARD.encode(other_wire);
    assert_eq!(api.submit(&body(&request)).status,409);
    let sender = api.sender.as_ref().unwrap().lock().unwrap();
    assert_eq!(sender.journal.get(&entry.id).unwrap().attempts,1);
    drop(sender);
    let legacy_scratch = Scratch::new();
    let mut legacy = entry.clone(); legacy.quote_id = None;
    let (rpc, worker) = rpc_script(vec![("getGenesisHash", json!(MAINNET_GENESIS))]);
    api.sender = Some(Mutex::new(Sender { journal: restart(&legacy_scratch, &legacy), rpc, max_attempts:3 }));
    worker.join().unwrap(); request.signed_transaction_base64 = STANDARD.encode(&entry.wire);
    assert_eq!(api.submit(&body(&request)).status,409, "old records are not silently rebound");
}

#[test]
fn provider_failure_does_not_erase_unknown_or_admit_another_execution() {
    let scratch = Scratch::new();
    let (mut api, entry, request) = fixture(false);
    let (rpc, worker) = rpc_script(vec![("getGenesisHash", json!(MAINNET_GENESIS))]);
    api.sender = Some(Mutex::new(Sender { journal: restart(&scratch, &entry), rpc, max_attempts:3 }));
    worker.join().unwrap(); // endpoint now unavailable
    let reply = api.submit(&body(&request));
    assert_eq!(reply.status,200);
    assert_eq!(reply.body["refreshSucceeded"],false);
    assert_eq!(reply.body["phase"],"UNKNOWN");
    assert_eq!(reply.body["attempts"],1);
}

#[test]
fn finalized_is_not_filled_without_economic_receipt_and_known_failure_is_terminal() {
    let scratch = Scratch::new();
    let (mut api, entry, request) = fixture(false);
    let (rpc, worker) = rpc_script(vec![("getGenesisHash",json!(MAINNET_GENESIS)),
        ("getGenesisHash",json!(MAINNET_GENESIS)), ("getSignatureStatuses",json!({"value":[{"confirmationStatus":"finalized","err":null}]})),
        ("getGenesisHash",json!(MAINNET_GENESIS)), ("getTransaction",Value::Null)]);
    api.sender = Some(Mutex::new(Sender { journal: restart(&scratch, &entry), rpc, max_attempts:3 }));
    let reply = api.submit(&body(&request));
    assert_eq!(reply.status,503);
    assert_eq!(reply.body["error"]["code"],"STOCKLANA_RECEIPT_PENDING");
    assert_eq!(reply.body["phase"],"FINALIZED");
    assert_eq!(api.sender.as_ref().unwrap().lock().unwrap().journal.get(&entry.id).unwrap().phase,Phase::Finalized);
    worker.join().unwrap();
    let failed_scratch = Scratch::new();
    let (rpc, worker) = rpc_script(vec![("getGenesisHash",json!(MAINNET_GENESIS))]);
    let mut journal = restart(&failed_scratch,&entry); journal.update(&entry.id,Phase::Failed,false).unwrap();
    api.sender = Some(Mutex::new(Sender{journal,rpc,max_attempts:3}));
    let reply = api.submit(&body(&request));
    assert_eq!(reply.status,200); assert_eq!(reply.body["phase"],"FAILED"); assert_eq!(reply.body["attempts"],1);
    worker.join().unwrap();
}

#[test]
fn exact_finalized_receipt_reconciles_once_and_retry_retains_the_same_signature() {
    let scratch = Scratch::new();
    let (mut api, entry, request) = fixture(false);
    let receipt = crate::exposure_receipt::tests::fixture().value;
    assert_eq!(receipt["transaction"][0], STANDARD.encode(&entry.wire));
    let (rpc, worker) = rpc_script(vec![("getGenesisHash",json!(MAINNET_GENESIS)),
        ("getGenesisHash",json!(MAINNET_GENESIS)), ("getSignatureStatuses",json!({"value":[{"confirmationStatus":"finalized","err":null}]})),
        ("getGenesisHash",json!(MAINNET_GENESIS)), ("getTransaction",receipt.clone()),
        ("getGenesisHash",json!(MAINNET_GENESIS)), ("getTransaction",receipt)]);
    api.sender = Some(Mutex::new(Sender{journal:restart(&scratch,&entry),rpc,max_attempts:3}));
    for _ in 0..2 {
        let reply = api.submit(&body(&request));
        assert_eq!(reply.status,200,"{}",reply.body);
        assert_eq!(reply.body["phase"],"RECONCILED");
        assert_eq!(reply.body["verifiedExposure"]["signature"],entry.signature);
        assert_eq!(reply.body["attempts"],1);
    }
    assert_eq!(api.sender.as_ref().unwrap().lock().unwrap().journal.entries().count(),1);
    worker.join().unwrap();
}

#[test]
fn native_sale_survives_journal_restart_and_reconciles_without_another_send() {
    let scratch=Scratch::new();
    let (mut api,_,_)=fixture(false);
    let (mut entry,expected,receipt)=crate::receipt::tests::native_output_fixture("");
    entry.id=format!("stkp_{}","b".repeat(32));entry.quote_id=Some(format!("stkq_{}","a".repeat(32)));
    entry.phase=Phase::Prepared;entry.attempts=0;entry.expected_exposure=None;
    let request=SubmitRequest{quote_id:entry.quote_id.clone().unwrap(),prepared_id:entry.id.clone(),owner:expected.owner.clone(),signed_transaction_base64:STANDARD.encode(&entry.wire)};
    entry.expected_swap=Some(expected);
    let (rpc,worker)=rpc_script(vec![("getGenesisHash",json!(MAINNET_GENESIS)),
        ("getGenesisHash",json!(MAINNET_GENESIS)),("getSignatureStatuses",json!({"value":[{"confirmationStatus":"finalized","err":null}]})),
        ("getGenesisHash",json!(MAINNET_GENESIS)),("getTransaction",receipt.clone()),
        ("getGenesisHash",json!(MAINNET_GENESIS)),("getTransaction",receipt)]);
    api.sender=Some(Mutex::new(Sender{journal:restart(&scratch,&entry),rpc,max_attempts:3}));
    for attempt in 0..2 {
        let reply=api.submit(&body(&request));
        assert_eq!(reply.status,200,"{}",reply.body);
        assert_eq!(reply.body["phase"],"RECONCILED");assert_eq!(reply.body["attempts"],1);
        assert_eq!(reply.body["verifiedSwap"]["signature"],entry.signature);
        assert_eq!(reply.body["verifiedSwap"]["output"],"195");
        assert_eq!(reply.body["verifiedSwap"]["verification"],"finalized_exact_wire_and_native_payout");
        assert_eq!(reply.body["recovered"],true);
        if attempt==0 {
            if let Ok(path)=std::env::var("SKEW_NATIVE_RECEIPT_VECTOR") {
                let path=PathBuf::from(format!("{path}.api.json"));
                assert!(path.starts_with("/srv/skew/stockmesh-direct-node-20260920/evidence") && !path.components().any(|c|matches!(c,std::path::Component::ParentDir)));
                std::fs::OpenOptions::new().write(true).create_new(true).open(path).unwrap().write_all(&serde_json::to_vec_pretty(&reply.body).unwrap()).unwrap();
            }
        }
    }
    let sender=api.sender.as_ref().unwrap().lock().unwrap();
    assert_eq!(sender.journal.entries().count(),1);assert_eq!(sender.journal.get(&entry.id).unwrap().wire,entry.wire);
    assert!(sender.journal.get(&entry.id).unwrap().expected_swap.as_ref().unwrap().native_output.is_some());
    worker.join().unwrap();
}

#[test]
fn basket_retry_uses_original_owner_wire_and_nonce_reservation_after_restart(){
    let scratch=Scratch::new();let (mut api,_,_)=fixture(false);let (mut entry,_)=crate::basket_wire::recovery_fixture();entry.phase=Phase::Prepared;
    let request=SubmitRequest{quote_id:entry.quote_id.clone().unwrap(),prepared_id:entry.id.clone(),owner:entry.wallet_owner().unwrap().into(),signed_transaction_base64:STANDARD.encode(&entry.wire)};
    let (rpc,worker)=rpc_script(vec![("getGenesisHash",json!(MAINNET_GENESIS)),("getGenesisHash",json!(MAINNET_GENESIS)),("getSignatureStatuses",json!({"value":[null]}))]);
    api.sender=Some(Mutex::new(Sender{journal:restart(&scratch,&entry),rpc,max_attempts:3}));
    let reply=api.submit(&body(&request));assert_eq!(reply.status,200,"{}",reply.body);assert_eq!(reply.body["phase"],"UNKNOWN");assert_eq!(reply.body["attempts"],1);assert_eq!(reply.body["signature"],entry.signature);
    worker.join().unwrap();
    let mut wrong=request;wrong.owner="11111111111111111111111111111111".into();assert_eq!(api.submit(&body(&wrong)).status,409);
    let mut sender=api.sender.as_ref().unwrap().lock().unwrap();let mut conflicting=entry;conflicting.id="second".into();conflicting.signature="second".into();assert!(sender.journal.insert(conflicting).is_err());
}

#[test]
fn first_basket_submit_uses_cached_owner_wire_then_retry_only_observes(){
    let scratch=Scratch::new();let (mut api,_,_)=fixture(false);let (candidate,entry)=crate::basket_wire::candidate_fixture();
    api.basket_prepared.lock().unwrap().admit(entry.quote_id.clone().unwrap(),candidate,Duration::from_secs(15)).unwrap();
    let request=SubmitRequest{quote_id:entry.quote_id.clone().unwrap(),prepared_id:entry.id.clone(),owner:entry.wallet_owner().unwrap().into(),signed_transaction_base64:STANDARD.encode(&entry.wire)};
    let (rpc,worker)=rpc_script(vec![("getGenesisHash",json!(MAINNET_GENESIS)),("getGenesisHash",json!(MAINNET_GENESIS)),("getSignatureStatuses",json!({"value":[null]})),
        ("getBlockHeight",json!(110)),("sendTransaction",json!(entry.signature)),
        ("getGenesisHash",json!(MAINNET_GENESIS)),("getSignatureStatuses",json!({"value":[null]})),
        ("getGenesisHash",json!(MAINNET_GENESIS)),("getSignatureStatuses",json!({"value":[null]}))]);
    api.sender=Some(Mutex::new(Sender{journal:Journal::open(&scratch.journal(),8,65536).unwrap(),rpc,max_attempts:3}));
    for _ in 0..2{let reply=api.submit(&body(&request));assert_eq!(reply.status,200,"{}",reply.body);assert_eq!(reply.body["phase"],"UNKNOWN");assert_eq!(reply.body["attempts"],1);assert_eq!(reply.body["signature"],entry.signature);}
    worker.join().unwrap();let saved=api.sender.as_ref().unwrap().lock().unwrap().journal.get(&entry.id).unwrap().clone();assert_eq!(saved.wire,entry.wire);assert!(saved.expected_basket.is_some());
}

#[test]
fn basket_finalized_vector_receipt_and_portfolio_history_use_same_durable_order(){
    let scratch=Scratch::new();let (mut api,_,_)=fixture(false);let (mut entry,value)=crate::basket_wire::recovery_fixture();entry.phase=Phase::Prepared;
    let request=SubmitRequest{quote_id:entry.quote_id.clone().unwrap(),prepared_id:entry.id.clone(),owner:entry.wallet_owner().unwrap().into(),signed_transaction_base64:STANDARD.encode(&entry.wire)};
    let (rpc,worker)=rpc_script(vec![("getGenesisHash",json!(MAINNET_GENESIS)),("getGenesisHash",json!(MAINNET_GENESIS)),("getSignatureStatuses",json!({"value":[{"confirmationStatus":"finalized","err":null}]})),
        ("getGenesisHash",json!(MAINNET_GENESIS)),("getTransaction",value.clone()),("getGenesisHash",json!(MAINNET_GENESIS)),("getTransaction",value)]);
    api.sender=Some(Mutex::new(Sender{journal:restart(&scratch,&entry),rpc,max_attempts:3}));
    for _ in 0..2{let reply=api.submit(&body(&request));assert_eq!(reply.status,200,"{}",reply.body);assert_eq!(reply.body["phase"],"RECONCILED");assert_eq!(reply.body["verifiedBasket"]["stocks"].as_array().unwrap().len(),2);assert_eq!(reply.body["attempts"],1);}
    worker.join().unwrap();
    let page=api.orders(&serde_json::to_vec(&json!({"owner":request.owner})).unwrap());assert_eq!(page.status,200,"{}",page.body);
    assert_eq!(page.body["orders"].as_array().unwrap().len(),1);assert_eq!(page.body["orders"][0]["kind"],"BASKET");assert_eq!(page.body["orders"][0]["receiptVerified"],true);
}

#[test]
fn incomplete_basket_receipt_retains_finalized_order_and_all_reservations(){
    let scratch=Scratch::new();let (mut api,_,_)=fixture(false);let (mut entry,mut value)=crate::basket_wire::recovery_fixture();entry.phase=Phase::Prepared;
    value["meta"]["logMessages"].as_array_mut().unwrap().drain(..3);
    let request=SubmitRequest{quote_id:entry.quote_id.clone().unwrap(),prepared_id:entry.id.clone(),owner:entry.wallet_owner().unwrap().into(),signed_transaction_base64:STANDARD.encode(&entry.wire)};
    let (rpc,worker)=rpc_script(vec![("getGenesisHash",json!(MAINNET_GENESIS)),("getGenesisHash",json!(MAINNET_GENESIS)),("getSignatureStatuses",json!({"value":[{"confirmationStatus":"finalized","err":null}]})),
        ("getGenesisHash",json!(MAINNET_GENESIS)),("getTransaction",value)]);
    api.sender=Some(Mutex::new(Sender{journal:restart(&scratch,&entry),rpc,max_attempts:3}));
    let reply=api.submit(&body(&request));assert_eq!(reply.status,503);assert_eq!(reply.body["phase"],"FINALIZED");assert_eq!(reply.body["error"]["code"],"STOCKLANA_RECEIPT_PENDING");worker.join().unwrap();
    let mut sender=api.sender.as_ref().unwrap().lock().unwrap();assert_eq!(sender.journal.get(&entry.id).unwrap().phase,Phase::Finalized);
    let mut conflicting=entry;conflicting.id="replacement".into();conflicting.signature="replacement".into();assert!(sender.journal.insert(conflicting).is_err());
}

#[test]
fn preexisting_wallet_accounts_are_visible_in_history_without_a_setup_prefix() {
    let scratch = Scratch::new();
    let (mut api, entry, request) = fixture(false);
    let (rpc, worker) = rpc_script(vec![("getGenesisHash",json!(MAINNET_GENESIS)),
        ("getGenesisHash",json!(MAINNET_GENESIS)), ("getSignatureStatuses",json!({"value":[null]}))]);
    api.sender = Some(Mutex::new(Sender{journal:restart(&scratch,&entry),rpc,max_attempts:3}));
    let reply = api.orders(&serde_json::to_vec(&json!({"owner":request.owner})).unwrap());
    assert_eq!(reply.status,200); assert_eq!(reply.body["orders"].as_array().unwrap().len(),1);
    assert_eq!(reply.body["orders"][0]["preparedId"],entry.id);
    assert_eq!(reply.body["orders"][0]["receiptVerified"],false);
    worker.join().unwrap();
    let other = api.orders(br#"{"owner":"11111111111111111111111111111111"}"#);
    assert!(other.body["orders"].as_array().unwrap().is_empty());
}
