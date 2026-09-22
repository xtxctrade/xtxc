//! Multi-product preparation: frozen intent -> exact wire simulation -> the
//! same externally signed wire -> durable economic receipt expectation.
use crate::{
    exposure_receipt::{self, Admission, ExpectedExposure},
    feed::{Account, Feed, Snapshot},
    journal::Entry,
    mesh::{ExecutorBid, FrozenIntent},
    rpc::Rpc,
    sender::{self, Authorization},
    wallet_wire::WalletSetup,
    Result,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

fn unsigned(message: &[u8]) -> Result<Vec<u8>> {
    let msg = crate::pipeline::decode(message)?;
    let signatures = msg.header().num_required_signatures;
    let mut wire = vec![signatures];
    wire.resize(1 + usize::from(signatures) * 64, 0);
    wire.extend_from_slice(message);
    Ok(wire)
}

pub(crate) fn simulation_fee(result: &Value) -> Result<u64> {
    result["value"]["fee"].as_u64().filter(|fee|*fee<=10_000_000)
        .ok_or_else(||"simulation reported fee missing or outside bound".into())
}

pub(crate) fn returned_accounts(result: &Value, addresses: &[String], scope: &str) -> Result<Vec<Account>> {
    let accounts = result["value"]["accounts"]
        .as_array()
        .filter(|rows| rows.len() == addresses.len())
        .ok_or_else(|| format!("{scope} accounts incomplete"))?;
    accounts
        .iter()
        .zip(addresses)
        .map(|(row, address)| {
            if row["data"][1].as_str() != Some("base64") {
                return Err(format!("{scope} account encoding"));
            }
            Ok(Account {
                key: address.clone(),
                owner: row["owner"]
                    .as_str()
                    .ok_or_else(|| format!("{scope} account owner"))?
                    .into(),
                executable: row["executable"]
                    .as_bool()
                    .ok_or_else(|| format!("{scope} executable flag"))?,
                lamports: row["lamports"]
                    .as_u64()
                    .ok_or_else(|| format!("{scope} lamports"))?,
                data: STANDARD
                    .decode(
                        row["data"][0]
                            .as_str()
                            .ok_or_else(|| format!("{scope} account data"))?,
                    )
                    .map_err(|_| format!("{scope} account base64"))?,
            })
        })
        .collect::<Result<Vec<_>>>()
}

/// Execute only the trusted wallet setup prefix against the exact execution
/// bank. The returned snapshot is a lowering projection; callers must still run
/// `PreparedExposure::simulate_market` for the complete settlement message.
pub fn simulate_wallet_setup(
    feed: &Feed,
    snapshot: &Snapshot,
    rpc: &Rpc,
    setup: &WalletSetup,
    setup_message: &[u8],
) -> Result<Snapshot> {
    feed.validate_fence(snapshot)?;
    setup.validate_setup_only_message(snapshot, setup_message)?;
    let addresses = setup
        .setup_only_addresses()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if addresses.is_empty() || addresses.len() > 7 {
        return Err("wallet setup simulation observation bound".into());
    }
    rpc.check_genesis()?;
    let result = rpc.call(
        "simulateTransaction",
        json!([STANDARD.encode(unsigned(setup_message)?),{
        "encoding":"base64","sigVerify":false,"replaceRecentBlockhash":false,
        "commitment":"confirmed","minContextSlot":snapshot.slot,
        "accounts":{"encoding":"base64","addresses":addresses}}]),
    )?;
    if result["context"]["slot"].as_u64() != Some(snapshot.slot)
        || !result["value"]
            .get("err")
            .ok_or("wallet setup simulation status missing")?
            .is_null()
    {
        return Err("wallet setup simulation failed or bank changed; rebuild".into());
    }
    let returned = returned_accounts(&result, &addresses, "wallet setup simulation")?;
    let projected = setup.lowering_projection(snapshot, &returned, simulation_fee(&result)?)?;
    feed.validate_fence(snapshot)?;
    Ok(projected)
}

#[derive(Clone)]
pub struct PreparedExposure {
    expected: ExpectedExposure,
    revision: u64,
    state_hash: [u8; 32],
    market_hash: [u8; 32],
    simulated_exposure: u64,
    simulated_cu: u64,
    unsigned_wire: Vec<u8>,
    genesis: String,
}

impl PreparedExposure {
    pub fn simulate(
        feed: &Feed,
        snapshot: &Snapshot,
        rpc: &Rpc,
        intent: &FrozenIntent,
        admission: &Admission,
        message: &[u8],
    ) -> Result<Self> {
        Self::simulate_market(
            feed,
            snapshot,
            &snapshot
                .accounts
                .iter()
                .map(|a| a.key.clone())
                .collect::<Vec<_>>(),
            rpc,
            intent,
            admission,
            message,
        )
    }

    #[allow(clippy::too_many_arguments)] // Full bank and signed market are distinct commitments.
    pub fn simulate_market(
        feed: &Feed,
        snapshot: &Snapshot,
        market_keys: &[String],
        rpc: &Rpc,
        intent: &FrozenIntent,
        admission: &Admission,
        message: &[u8],
    ) -> Result<Self> {
        feed.validate_fence(snapshot)?;
        let expected =
            ExpectedExposure::bind_market(snapshot, market_keys, intent, admission, message)?;
        let full_unsigned = unsigned(message)?;
        let bindings = expected.observation_bindings();
        let before = bindings
            .iter()
            .map(|binding| expected.before_amount(snapshot, binding))
            .collect::<Result<Vec<_>>>()?;
        let mut addresses = bindings
            .iter()
            .map(|binding| binding.account.clone())
            .collect::<Vec<_>>();
        addresses.extend(expected.setup_observation_addresses());
        if addresses
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != addresses.len()
        {
            return Err("economic simulation duplicate observation account".into());
        }
        rpc.check_genesis()?;
        if let Some(setup_message) = expected.setup_message(message)? {
            let setup_addresses = expected.setup_only_addresses();
            if setup_addresses.is_empty() {
                return Err("wallet setup-only observations missing".into());
            }
            let setup_result = rpc.call(
                "simulateTransaction",
                json!([STANDARD.encode(unsigned(&setup_message)?),{
                "encoding":"base64","sigVerify":false,"replaceRecentBlockhash":false,
                "commitment":"confirmed","minContextSlot":snapshot.slot,
                "accounts":{"encoding":"base64","addresses":setup_addresses}}]),
            )?;
            if setup_result["context"]["slot"].as_u64() != Some(snapshot.slot)
                || !setup_result["value"]
                    .get("err")
                    .ok_or("wallet setup-only simulation status missing")?
                    .is_null()
            {
                return Err("wallet setup-only simulation failed or bank changed; rebuild".into());
            }
            let setup_returned = returned_accounts(
                &setup_result,
                &setup_addresses,
                "wallet setup-only simulation",
            )?;
            expected.verify_setup_only(snapshot, &setup_returned)?;
        }
        let result = rpc.call(
            "simulateTransaction",
            json!([STANDARD.encode(&full_unsigned),{
            "encoding":"base64","sigVerify":false,"replaceRecentBlockhash":false,
            "commitment":"confirmed","minContextSlot":snapshot.slot,
            "accounts":{"encoding":"base64","addresses":addresses}}]),
        )?;
        if result["context"]["slot"].as_u64() != Some(snapshot.slot) {
            return Err("economic simulation bank changed; rebuild".into());
        }
        let simulation_error = result["value"]
            .get("err")
            .ok_or("economic simulation status missing")?;
        if !simulation_error.is_null() {
            return Err(classify_economic_simulation_failure(&result["value"]).into());
        }
        let returned = returned_accounts(&result, &addresses, "economic simulation")?;
        let after = returned[..bindings.len()]
            .iter()
            .zip(&bindings)
            .map(|(account, binding)| exposure_receipt::raw_amount(account, binding))
            .collect::<Result<Vec<_>>>()?;
        expected.verify_simulated_setup(snapshot, &after, &returned)?;
        let simulated_cu = result["value"]["unitsConsumed"]
            .as_u64()
            .ok_or("economic simulation CU missing")?;
        let (simulated_exposure, _) = expected.outcome(
            &result["value"]["returnData"],
            &before,
            &after,
            simulated_cu,
        )?;
        feed.validate_fence(snapshot)?;
        Ok(Self {
            expected,
            revision: snapshot.revision,
            state_hash: snapshot.hash,
            market_hash: intent.world_generation_hash,
            simulated_exposure,
            simulated_cu,
            unsigned_wire: full_unsigned,
            genesis: rpc.genesis().into(),
        })
    }

    pub fn expected(&self) -> &ExpectedExposure {
        &self.expected
    }

    /// Frontend transaction envelopes contain the same zero-signature wire
    /// that was simulated, never a reconstructed message.
    pub(crate) fn unsigned_wire(&self) -> &[u8] {
        &self.unsigned_wire
    }
    pub fn genesis(&self) -> &str {
        &self.genesis
    }

    pub fn message_hash(&self) -> [u8; 32] {
        self.expected.message_hash
    }

    pub fn simulated_exposure(&self) -> u64 {
        self.simulated_exposure
    }

    pub fn simulated_cu(&self) -> u64 {
        self.simulated_cu
    }

    pub fn validate_state(
        &self,
        feed: &Feed,
        snapshot: &Snapshot,
        intent: &FrozenIntent,
    ) -> Result<()> {
        feed.validate_fence(snapshot)?;
        if snapshot.revision != self.revision
            || snapshot.hash != self.state_hash
            || snapshot.slot != self.expected.prepared_slot
            || intent.commitment(snapshot.slot)? != self.expected.intent_commitment
        {
            return Err("prepared economic intent revoked".into());
        }
        Ok(())
    }

    pub fn summary(&self) -> Value {
        json!({"schema":"skew.stockmesh.prepared-exposure/v1",
            "approvedMessageHash":self.expected.message_hash,"intentCommitment":self.expected.intent_commitment,
            "simulationExposureQ32":self.simulated_exposure.to_string(),"simulationCU":self.simulated_cu,
            "heapFrameBytes":self.expected.heap_frame_bytes(),
            "executionBankHash":self.state_hash,"marketHash":self.market_hash,
            "stateSlot":self.expected.prepared_slot,"requiresOwnerSignatures":true,"submitted":false})
    }

    /// A winning bid must describe these exact simulated bytes and cannot claim
    /// exposure or CU that the selected prepared candidate did not achieve.
    pub fn validate_bid(&self, bid: &ExecutorBid, intent: &FrozenIntent, slot: u64) -> Result<()> {
        bid.verify(intent, slot)?;
        if bid.transaction_message_hash != self.expected.message_hash
            || bid.intent_commitment != self.expected.intent_commitment
            || bid.world_generation_hash != self.market_hash
            || bid.guaranteed_exposure_q32 > u128::from(self.simulated_exposure)
            || u64::from(bid.predicted_compute_units) < self.simulated_cu
            || u64::from(bid.predicted_compute_units) > self.expected.maximum_cu
            || bid.executor_fee_input_atoms != 0
        {
            return Err("executor bid differs from simulated economic outcome".into());
        }
        Ok(())
    }

    pub fn authorize(
        &self,
        feed: &Feed,
        snapshot: &Snapshot,
        intent: &FrozenIntent,
        wire: &[u8],
        id: String,
        last_valid_height: u64,
    ) -> Result<Entry> {
        self.validate_state(feed, snapshot, intent)?;
        let mut entry = sender::authorize(
            wire,
            Authorization {
                intent_id: id,
                message_hash: self.expected.message_hash,
                last_valid_height,
                resources: self.expected.resources.clone(),
            },
        )?;
        entry.expected_exposure = Some(self.expected.clone());
        Ok(entry)
    }
}

fn classify_economic_simulation_failure(value: &Value) -> &'static str {
    let resource_failure = value
        .get("logs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|line| {
            let line = line.to_ascii_lowercase();
            line.contains("out of memory")
                || line.contains("computationalbudgetexceeded")
                || line.contains("program failed to complete")
        });
    if resource_failure {
        "economic simulation resource rejected"
    } else {
        "economic simulation candidate rejected"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
    };

    #[test]
    fn economic_simulation_failures_separate_resources_from_state_changes() {
        assert_eq!(
            classify_economic_simulation_failure(&json!({
                "err":{"InstructionError":[1,"ProgramFailedToComplete"]},
                "logs":["Program log: memory allocation failed, out of memory"]
            })),
            "economic simulation resource rejected"
        );
        assert_eq!(
            classify_economic_simulation_failure(&json!({
                "err":{"InstructionError":[1,"Custom"]},
                "logs":["Program log: slippage floor"]
            })),
            "economic simulation candidate rejected"
        );
    }

    #[test]
    fn exact_multi_owner_simulation_authorizes_only_the_same_frozen_message() {
        let mut f = crate::exposure_receipt::tests::fixture();
        let (api, quote_id, market_keys) = crate::stockmesh_api::tests::prepared_fixture(&mut f);
        let returned=f.expected.balance_bindings().iter().enumerate().map(|(i,b)| {
            let original=f.snapshot.accounts.iter().find(|a|a.key==b.account).unwrap();
            let mut bytes=original.data.clone();
            let amount=f.value["meta"]["postTokenBalances"][i]["uiTokenAmount"]["amount"].as_str().unwrap().parse::<u64>().unwrap();
            bytes[64..72].copy_from_slice(&amount.to_le_bytes());
            json!({"owner":original.owner,"executable":false,"lamports":original.lamports,"data":[STANDARD.encode(bytes),"base64"]})
        }).collect::<Vec<_>>();
        let simulated = json!({"context":{"slot":100},"value":{"err":null,"unitsConsumed":70000,"returnData":f.value["meta"]["returnData"],"accounts":returned}});
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            // Rpc::pinned, simulate genesis fence, then exact simulation.
            for _ in 0..3 {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap();
                        }
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                let result = if request["method"] == "getGenesisHash" {
                    json!("fixture-genesis")
                } else {
                    assert_eq!(request["method"], "simulateTransaction");
                    assert_eq!(request["params"][1]["replaceRecentBlockhash"], false);
                    let wire = STANDARD
                        .decode(request["params"][0].as_str().unwrap())
                        .unwrap();
                    assert_eq!(wire[0], 2);
                    assert!(wire[1..129].iter().all(|b| *b == 0));
                    simulated.clone()
                };
                let body =
                    serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"result":result})).unwrap();
                write!(reader.get_mut(),"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
                reader.get_mut().write_all(&body).unwrap();
            }
        });
        let rpc = Rpc::pinned(format!("http://{address}"), "fixture-genesis".into()).unwrap();
        let prepared = PreparedExposure::simulate_market(
            &f.feed,
            &f.snapshot,
            &market_keys,
            &rpc,
            &f.intent,
            &f.admission,
            &f.message,
        )
        .unwrap();
        assert_eq!(
            prepared.summary()["simulationExposureQ32"],
            (60u64 << 32).to_string()
        );
        let authorized = prepared
            .authorize(
                &f.feed,
                &f.snapshot,
                &f.intent,
                &f.entry.wire,
                "fixture".into(),
                999,
            )
            .unwrap();
        assert_eq!(
            authorized.expected_exposure.as_ref().unwrap().message_hash,
            prepared.message_hash()
        );
        let mut tampered = f.entry.wire.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(prepared
            .authorize(
                &f.feed,
                &f.snapshot,
                &f.intent,
                &tampered,
                "fixture".into(),
                999
            )
            .is_err());
        use ed25519_dalek::{Signer, SigningKey};
        let executor = SigningKey::from_bytes(&[203; 32]);
        let mut bid = ExecutorBid {
            executor: bs58::encode(executor.verifying_key().to_bytes()).into_string(),
            sequence: 1,
            intent_commitment: f.intent.commitment(f.snapshot.slot).unwrap(),
            world_generation_hash: f.intent.world_generation_hash,
            transaction_message_hash: prepared.expected.message_hash,
            guaranteed_exposure_q32: 60u128 << 32,
            executor_fee_input_atoms: 0,
            predicted_compute_units: 70000,
            expires_slot: 200,
            signature: String::new(),
        };
        bid.signature =
            bs58::encode(executor.sign(&bid.signing_bytes().unwrap()).to_bytes()).into_string();
        for attack in ["overpromise", "fee", "understate_cu", "message"] {
            let mut bad = bid.clone();
            match attack {
                "overpromise" => bad.guaranteed_exposure_q32 += 1,
                "fee" => bad.executor_fee_input_atoms = 1,
                "understate_cu" => bad.predicted_compute_units -= 1,
                "message" => bad.transaction_message_hash = [9; 32],
                _ => unreachable!(),
            }
            bad.signature =
                bs58::encode(executor.sign(&bad.signing_bytes().unwrap()).to_bytes()).into_string();
            assert!(
                prepared
                    .validate_bid(&bad, &f.intent, f.snapshot.slot)
                    .is_err(),
                "{attack}"
            );
        }
        let candidate = crate::prepared_quotes::Candidate {
            intent: f.intent.clone(),
            feed: f.feed.clone(),
            snapshot: f.snapshot.clone(),
            prepared: prepared.clone(),
            bid,
            last_valid_block_height: 999,
        };
        let mut submission_cache = crate::prepared_quotes::PreparedQuotes::default();
        submission_cache
            .admit(
                &quote_id,
                candidate.clone(),
                std::time::Instant::now() + std::time::Duration::from_secs(30),
                "2026-09-14T00:01:00.000Z".into(),
            )
            .unwrap();
        let prepared_id = submission_cache
            .review(
                &quote_id,
                &f.intent.owner,
                f.intent.world_generation_hash,
                f.snapshot.slot,
            )
            .unwrap()
            .unwrap()["preparedId"]
            .as_str()
            .unwrap()
            .to_string();
        // A later confirmed slot with byte-identical market accounts does not
        // revoke a prepared transaction. The content hash and deadline remain
        // the economic fence, while the slot advance is checked for expiry.
        assert!(submission_cache
            .review(
                &quote_id,
                &f.intent.owner,
                f.intent.world_generation_hash,
                f.snapshot.slot + 1,
            )
            .unwrap()
            .is_some());
        let submitted = submission_cache
            .authorize_submission(
                &quote_id,
                &f.intent.owner,
                &prepared_id,
                f.intent.world_generation_hash,
                f.snapshot.slot,
                &f.entry.wire,
            )
            .unwrap();
        assert_eq!(submitted.wire, f.entry.wire);
        assert_eq!(
            submitted.expected_exposure.as_ref().unwrap().message_hash,
            prepared.message_hash()
        );
        let journal_path = std::env::temp_dir().join(format!(
            "stockmesh-exposure-submission-{}.journal",
            std::process::id()
        ));
        let mut lock_path = journal_path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock_path = std::path::PathBuf::from(lock_path);
        let _ = std::fs::remove_file(&journal_path);
        let _ = std::fs::remove_file(&lock_path);
        {
            let mut journal = crate::journal::Journal::open(&journal_path, 4, 64 * 1024).unwrap();
            journal.insert(submitted.clone()).unwrap();
        }
        {
            let journal = crate::journal::Journal::open(&journal_path, 4, 64 * 1024).unwrap();
            assert_eq!(
                journal
                    .get(&prepared_id)
                    .unwrap()
                    .expected_exposure
                    .as_ref()
                    .unwrap()
                    .message_hash,
                prepared.message_hash()
            );
        }
        std::fs::remove_file(&journal_path).unwrap();
        std::fs::remove_file(&lock_path).unwrap();
        assert!(submission_cache
            .authorize_submission(
                &quote_id,
                &f.intent.owner,
                &format!("stkp_{}", "0".repeat(32)),
                f.intent.world_generation_hash,
                f.snapshot.slot,
                &f.entry.wire,
            )
            .is_err());
        let mut wrong_quote = candidate.clone();
        wrong_quote.intent.input_atoms += 1;
        assert!(api.admit_prepared(&quote_id, wrong_quote).is_err());
        let mut wrong_id = candidate.clone();
        wrong_id.intent.intent_id = [7; 32];
        assert!(api.admit_prepared(&quote_id, wrong_id).is_err());
        api.admit_prepared(&quote_id, candidate.clone()).unwrap();
        assert!(api.admit_prepared(&quote_id, candidate).is_err()); // exact replay
        let review_request =
            serde_json::to_vec(&json!({"quoteId":quote_id,"owner":f.intent.owner})).unwrap();
        let review = api.prepare(&review_request);
        assert_eq!(review.status, 200, "{}", review.body);
        assert_eq!(review.body["schema"], "skew.stocklana.prepared/v1");
        assert_eq!(review.body["submitAllowed"], false);
        assert_eq!(review.body["estimatedCu"], 70000);
        let submit = api.submit(
            &serde_json::to_vec(&json!({
                "quoteId":quote_id,
                "preparedId":review.body["preparedId"],
                "owner":&f.intent.owner,
                "signedTransactionBase64":STANDARD.encode(&f.entry.wire)
            }))
            .unwrap(),
        );
        assert_eq!(submit.status, 503);
        assert_eq!(
            submit.body["error"]["code"],
            "STOCKLANA_SUBMISSION_DISABLED"
        );
        if let Ok(path) = std::env::var("SKEW_PREPARED_API_VECTOR") {
            assert!(path.starts_with("/srv/skew/"));
            std::fs::write(path, serde_json::to_vec_pretty(&review.body).unwrap()).unwrap();
        }
        assert_eq!(
            STANDARD
                .decode(review.body["transactionBase64"].as_str().unwrap())
                .unwrap(),
            prepared.unsigned_wire()
        );
        assert_eq!(
            api.prepare(
                &serde_json::to_vec(
                    &json!({"quoteId":quote_id,"owner":"11111111111111111111111111111111"})
                )
                .unwrap()
            )
            .status,
            503
        );
        f.feed.invalidate().unwrap();
        assert_eq!(api.prepare(&review_request).status, 409);
        assert!(prepared
            .authorize(
                &f.feed,
                &f.snapshot,
                &f.intent,
                &f.entry.wire,
                "fixture".into(),
                999
            )
            .is_err());
        server.join().unwrap();
    }
}
