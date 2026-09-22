use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use skew_execution_host::{journal::{Journal, Phase}, rpc::Rpc, sender::{authorize, Authorization, Sender}};
use std::{io::{BufRead, BufReader, Read, Write}, net::TcpListener, time::Duration};
const GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";

#[test]
fn observation_survives_restart_and_never_sends_or_treats_absence_as_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for result in [json!(GENESIS), json!(GENESIS), json!({"value":[null]}), json!(GENESIS), json!(GENESIS), json!({"value":[{"confirmationStatus":"finalized","err":null}]})] {
            let (socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut reader = BufReader::new(socket);
            let mut length = 0;
            loop {
                let mut line = String::new(); reader.read_line(&mut line).unwrap();
                if line == "\r\n" { break; }
                if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") { length = n.trim().parse().unwrap(); }
            }
            let mut data = vec![0; length]; reader.read_exact(&mut data).unwrap();
            let request: Value = serde_json::from_slice(&data).unwrap();
            assert!(matches!(request["method"].as_str(), Some("getGenesisHash" | "getSignatureStatuses")));
            let body = json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string();
            write!(reader.get_mut(), "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    let path = std::env::temp_dir().join(format!("skew-order-observe-{}", std::process::id()));
    let key = SigningKey::from_bytes(&[18;32]);
    let mut message = vec![1,0,0,1]; message.extend_from_slice(key.verifying_key().as_bytes()); message.extend_from_slice(&[9;32]); message.push(0);
    let mut wire = vec![1]; wire.extend_from_slice(&key.sign(&message).to_bytes()); wire.extend_from_slice(&message);
    let entry = authorize(&wire, Authorization { intent_id:"order".into(), message_hash:Sha256::digest(&message).into(), last_valid_height:1, resources:vec![[8;32]] }).unwrap();
    {
        let mut journal = Journal::open(&path, 4, 65536).unwrap(); journal.insert(entry).unwrap();
        journal.update("order", Phase::Unknown, true).unwrap();
        let mut sender = Sender { journal, rpc:Rpc::pinned(url.clone(), GENESIS.into()).unwrap(), max_attempts:3 };
        assert_eq!(sender.observe_batch(&["order"]).unwrap(), vec![Phase::Unknown]);
        assert_eq!(sender.journal.get("order").unwrap().attempts, 1);
    }
    {
        let mut sender = Sender { journal:Journal::open(&path,4,65536).unwrap(), rpc:Rpc::pinned(url,GENESIS.into()).unwrap(), max_attempts:3 };
        assert_eq!(sender.observe_batch(&["order"]).unwrap(), vec![Phase::Finalized]);
        assert_eq!(sender.journal.get("order").unwrap().attempts,1);
        assert!(sender.reconcile_swap("order").is_err());
        assert_eq!(sender.journal.get("order").unwrap().phase,Phase::Finalized);
    }
    server.join().unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(format!("{}.lock",path.display())).unwrap();
}
