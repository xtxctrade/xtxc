//! Refresh the already-published ProductPolicy v2 accounts on mainnet.
//!
//! This binary deliberately has one narrow authority: it advances the version
//! and freshness window of policy accounts compiled by `product-policy-bundle`.
//! It cannot prepare, alter, sign, or submit customer transactions.

use base64::{engine::general_purpose::STANDARD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use reqwest::blocking::Client;
use serde_json::{json, Value};
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::legacy::Message;
use solana_pubkey::Pubkey;
use std::{env, fs, str::FromStr, time::Duration};

const POLICY_LEN: usize = 225;
const POLICY_TAG: &[u8; 8] = b"SKEWSTK2";
const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
const MAX_EXPIRY_SLOTS: u64 = 3_600;
const REFRESH_WINDOW_SLOTS: u64 = 3_000;

fn key(value: &str) -> Result<Pubkey, String> {
    Pubkey::from_str(value).map_err(|_| "invalid public key".into())
}

fn rpc(client: &Client, url: &str, method: &str, params: Value) -> Result<Value, String> {
    let response = client
        .post(url)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .send()
        .map_err(|_| "policy RPC unavailable")?;
    if !response.status().is_success() {
        return Err("policy RPC HTTP failure".into());
    }
    let body: Value = response.json().map_err(|_| "policy RPC encoding")?;
    if body.get("error").is_some() {
        return Err("policy RPC rejected request".into());
    }
    body.get("result").cloned().ok_or_else(|| "policy RPC result missing".into())
}

fn get_slot(client: &Client, url: &str) -> Result<u64, String> {
    rpc(client, url, "getSlot", json!([{"commitment":"confirmed"}]))?
        .as_u64()
        .ok_or_else(|| "policy slot encoding".into())
}

fn get_blockhash(client: &Client, url: &str) -> Result<Hash, String> {
    let value = rpc(client, url, "getLatestBlockhash", json!([{"commitment":"confirmed"}]))?;
    let hash = value["value"]["blockhash"]
        .as_str()
        .ok_or("policy blockhash missing")?;
    Hash::from_str(hash).map_err(|_| "policy blockhash encoding".into())
}

fn policy_state(client: &Client, url: &str, policy: Pubkey, program: Pubkey) -> Result<u64, String> {
    let value = rpc(
        client,
        url,
        "getAccountInfo",
        json!([policy.to_string(), {"encoding":"base64","commitment":"confirmed"}]),
    )?;
    let account = value["value"].as_object().ok_or("policy account missing")?;
    if account.get("owner").and_then(Value::as_str) != Some(&program.to_string()) {
        return Err("policy account owner differs".into());
    }
    let encoded = account["data"]
        .as_array()
        .and_then(|data| data.first())
        .and_then(Value::as_str)
        .ok_or("policy account data missing")?;
    let data = STANDARD.decode(encoded).map_err(|_| "policy account data encoding")?;
    if data.len() != POLICY_LEN || &data[..8] != POLICY_TAG {
        return Err("policy account layout differs".into());
    }
    let version: [u8; 8] = data[200..208].try_into().map_err(|_| "policy version")?;
    let version = u64::from_le_bytes(version);
    if version == 0 {
        return Err("policy version is zero".into());
    }
    Ok(version)
}

fn authority(path: &str, expected: Pubkey) -> Result<SigningKey, String> {
    let metadata = fs::metadata(path).map_err(|_| "policy authority key unavailable")?;
    if metadata.len() > 1024 {
        return Err("policy authority key bound".into());
    }
    let value: Vec<u8> = serde_json::from_slice(&fs::read(path).map_err(|_| "policy authority key unavailable")?)
        .map_err(|_| "policy authority key encoding")?;
    if value.len() != 64 {
        return Err("policy authority key length".into());
    }
    let seed: [u8; 32] = value[..32].try_into().map_err(|_| "policy authority seed")?;
    let key = SigningKey::from_bytes(&seed);
    if key.verifying_key().to_bytes() != expected.to_bytes() || value[32..] != expected.to_bytes() {
        return Err("policy authority identity differs".into());
    }
    Ok(key)
}

fn signed_wire(instruction: Instruction, payer: Pubkey, signer: &SigningKey, blockhash: Hash) -> Vec<u8> {
    let message = Message::new_with_blockhash(&[instruction], Some(&payer), &blockhash).serialize();
    let signature = signer.sign(&message).to_bytes();
    let mut wire = Vec::with_capacity(1 + signature.len() + message.len());
    wire.push(1);
    wire.extend_from_slice(&signature);
    wire.extend_from_slice(&message);
    wire
}

fn run() -> Result<(), String> {
    let args = env::args().collect::<Vec<_>>();
    if args.len() != 4 {
        return Err("usage: policy-heartbeat RPC_URL AUTHORITY_KEYPAIR BUNDLE".into());
    }
    let bundle: Value = serde_json::from_slice(&fs::read(&args[3]).map_err(|_| "policy bundle unavailable")?)
        .map_err(|_| "policy bundle encoding")?;
    if bundle["schema"] != "skew.stockmesh.unsigned-product-policy-bundle/v1"
        || bundle["network"] != "mainnet-beta"
        || bundle["version"].as_u64() != Some(1)
        || bundle["instructions"].as_array().is_none_or(|rows| rows.len() != 6)
    {
        return Err("policy bundle bounds".into());
    }
    let program = key(bundle["settlementProgram"].as_str().ok_or("policy program")?)?;
    let payer = key(bundle["authority"].as_str().ok_or("policy authority")?)?;
    let signer = authority(&args[2], payer)?;
    let client = Client::builder().timeout(Duration::from_secs(8)).build().map_err(|_| "policy HTTP client")?;
    let genesis = rpc(&client, &args[1], "getGenesisHash", json!([]))?;
    if genesis.as_str() != Some(MAINNET_GENESIS) {
        return Err("policy RPC is not mainnet".into());
    }
    let slot = get_slot(&client, &args[1])?;
    let expires = slot.checked_add(REFRESH_WINDOW_SLOTS).ok_or("policy expiry overflow")?;
    if expires > slot + MAX_EXPIRY_SLOTS {
        return Err("policy expiry bound".into());
    }
    let mut refreshed = Vec::new();
    for row in bundle["instructions"].as_array().ok_or("policy instructions")? {
        let policy = key(row["policy"].as_str().ok_or("policy address")?)?;
        let current = policy_state(&client, &args[1], policy, program)?;
        let next = current.checked_add(1).ok_or("policy version overflow")?;
        let mut data = STANDARD.decode(row["dataBase64"].as_str().ok_or("policy instruction data")?)
            .map_err(|_| "policy instruction encoding")?;
        if data.len() != 178 || data[0] != 19 {
            return Err("policy instruction layout".into());
        }
        data[161..169].copy_from_slice(&next.to_le_bytes());
        data[169..177].copy_from_slice(&expires.to_le_bytes());
        let accounts = row["accounts"].as_array().ok_or("policy instruction accounts")?
            .iter()
            .map(|account| Ok(AccountMeta {
                pubkey: key(account["pubkey"].as_str().ok_or("policy account" )?)?,
                is_signer: account["isSigner"].as_bool().ok_or("policy signer")?,
                is_writable: account["isWritable"].as_bool().ok_or("policy writable")?,
            }))
            .collect::<Result<Vec<_>, String>>()?;
        if accounts.len() != 3 || accounts[0].pubkey != payer || accounts[1].pubkey != policy || accounts[2].pubkey != Pubkey::default() {
            return Err("policy instruction identity".into());
        }
        let wire = signed_wire(Instruction { program_id: program, accounts, data }, payer, &signer, get_blockhash(&client, &args[1])?);
        if wire.len() > 1232 {
            return Err("policy transaction bound".into());
        }
        let signature = rpc(&client, &args[1], "sendTransaction", json!([
            STANDARD.encode(wire), {"encoding":"base64","skipPreflight":false,"preflightCommitment":"confirmed","maxRetries":2}
        ]))?.as_str().ok_or("policy submission signature")?.to_owned();
        refreshed.push(json!({"instrument":row["instrument"],"policy":policy.to_string(),"fromVersion":current,"toVersion":next,"signature":signature}));
    }
    println!("{}", json!({"schema":"skew.stockmesh.policy-heartbeat/v1","slot":slot,"expiresSlot":expires,"refreshed":refreshed}));
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
