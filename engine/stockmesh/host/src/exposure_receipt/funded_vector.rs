//! Real SBF return data and account deltas through the host's exact-wire parser.
//! Admission below is explicitly synthetic, not a claim of issuer eligibility.
use super::*;
use crate::feed::{Account, Feed};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[test]
#[ignore = "requires AWS stockmesh-cell-proof funded SBF artifact"]
fn actual_funded_sbf_preserves_cash_and_binds_original_economic_input() {
    let path = PathBuf::from(std::env::var("SKEW_FUNDED_VECTOR").expect("explicit AWS vector"));
    verify(&path);
    verify(&path.parent().unwrap().join("no-match/host-vector.json"));
    verify(
        &path
            .parent()
            .unwrap()
            .join("global-reflow/host-vector.json"),
    );
}

fn verify(path: &Path) {
    let bytes = std::fs::read(path).unwrap();
    let proof: Value =
        serde_json::from_slice(&std::fs::read(path.with_file_name("proof.json")).unwrap()).unwrap();
    assert_eq!(hex(&Sha256::digest(&bytes)), proof["hostVectorSha256"]);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["schema"], "skew.funded-mesh-sbf-vector/v1");
    let mut before = super::sbf_vector::accounts(&v["before"]);
    let after = super::sbf_vector::accounts(&v["after"]);
    // This frozen ALT is constructed from the exact table used to compile the
    // proven v0 message. No RPC, wallet key or invented transaction signature.
    let mut table = vec![0; 56];
    table[..4].copy_from_slice(&1u32.to_le_bytes());
    table[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
    for address in v["lookup"]["addresses"].as_array().unwrap() {
        table.extend_from_slice(key(address.as_str().unwrap()).unwrap().as_ref());
    }
    before.push(Account {
        key: v["lookup"]["key"].as_str().unwrap().into(),
        owner: "AddressLookupTab1e1111111111111111111111111".into(),
        executable: false,
        lamports: 10_000_000,
        data: table,
    });
    let feed = Arc::new(
        Feed::new(
            before.iter().map(|a| a.key.clone()).collect(),
            Duration::from_secs(60),
            4 * 1024 * 1024,
        )
        .unwrap(),
    );
    let snapshot = feed.publish(&json!({"context":{"slot":v["slot"]},"value":before.iter().map(|a|json!({"owner":a.owner,"executable":a.executable,"lamports":a.lamports,"data":[STANDARD.encode(&a.data),"base64"]})).collect::<Vec<_>>()})).unwrap();
    let message = STANDARD
        .decode(v["messageBase64"].as_str().unwrap())
        .unwrap();
    let msg = pipeline::decode(&message).unwrap();
    let keys = pipeline::resolved(&msg, &snapshot).unwrap();
    let ix = msg.instructions().last().unwrap();
    assert_eq!(keys[usize::from(ix.program_id_index)], v["program"]);
    let at = |i: u8| keys[usize::from(ix.accounts[usize::from(i)])].as_str();
    let wire = &ix.data;
    assert_eq!(wire[0], 18);
    let funding_end = 8 + usize::from(u16::from_le_bytes([wire[4], wire[5]]));
    let funding = Graph::decode(&wire[8..funding_end], ix.accounts.len()).unwrap();
    let cell = &wire[funding_end..];
    let products = (0..usize::from(cell[1]))
        .map(|i| {
            let row = &cell[56 + i * 32..56 + (i + 1) * 32];
            let state = account(&snapshot, at(row[0])).unwrap();
            let mint = account(&snapshot, at(row[2])).unwrap();
            let claim = (row[5] != 0).then(|| at(row[5]).to_string());
            ProductAdmission {
                identity: ProductIdentity {
                    instrument: "NVDA".into(),
                    issuer: format!("SBF-{i}"),
                    mint: mint.key.clone(),
                    token_program: mint.owner.clone(),
                    rights_hash: state.data[168..200].try_into().unwrap(),
                    raw_decimals: mint.data[44],
                },
                policy: state.key.clone(),
                policy_data_hash: Sha256::digest(&state.data).into(),
                claim_data_hash: claim
                    .as_ref()
                    .map(|key| Sha256::digest(&account(&snapshot, key).unwrap().data).into()),
                claim,
                policy_version: integer(row, 8).unwrap(),
                model: row[4],
                numerator: integer(row, 16).unwrap(),
                denominator: integer(row, 24).unwrap(),
                conservative_bps: u16::from_le_bytes([row[6], row[7]]),
            }
        })
        .collect::<Vec<_>>();
    let admission = Admission {
        settlement_program: v["program"].as_str().unwrap().into(),
        product_policy_hash: [91; 32],
        maximum_cu: 1_400_000,
        maximum_heap_frame_bytes: 256 * 1024,
        maximum_compute_price: 0,
        allow_underlying_closed: false,
        products,
    };
    let intent = FrozenIntent {
        intent_id: [92; 32],
        owner: at(cell[48]).into(),
        owner_nonce: funding.sequence,
        instrument: "NVDA".into(),
        input_mint: at(funding.assets[0].mint).into(),
        input_atoms: funding.input,
        minimum_exposure_q32: u128::from(integer(cell, 40).unwrap()),
        admitted_product_ids: admission
            .products
            .iter()
            .map(|p| p.identity.id().unwrap())
            .collect(),
        product_policy_hash: admission.product_policy_hash,
        world_generation_hash: snapshot.hash,
        deadline_slot: integer(cell, 8).unwrap(),
    };
    let expected = ExpectedExposure::bind(&snapshot, &intent, &admission, &message).unwrap();
    assert_eq!(expected.opcode, 18);
    assert_eq!(expected.input, 3_000_000_000);
    assert_eq!(
        expected.input_token.mint,
        "So11111111111111111111111111111111111111112"
    );
    assert_eq!(expected.products.len(), 2);
    assert_eq!(
        expected.sellers.len(),
        proof["sellerCount"].as_u64().unwrap() as usize
    );
    let preserved = &expected.funding.as_ref().unwrap().preserved;
    assert_eq!(preserved.len(), 1);
    assert_eq!(preserved[0].mint, crate::market::USDC_MINT);
    let bindings = expected.balance_bindings();
    let pre = bindings
        .iter()
        .map(|b| snapshot_amount(&snapshot, b).unwrap())
        .collect::<Vec<_>>();
    let post = bindings
        .iter()
        .map(|b| raw_amount(after.iter().find(|a| a.key == b.account).unwrap(), b).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        *pre.last().unwrap(),
        proof["preexistingCashAtoms"].as_u64().unwrap()
    );
    assert_eq!(pre.last(), post.last());
    let data = json!({"programId":v["program"],"data":[v["returnData"],"base64"]});
    let cu = v["cu"].as_u64().unwrap();
    let (exposure, products) = expected.outcome(&data, &pre, &post, cu).unwrap();
    assert_eq!(exposure, proof["actualExposureQ32"]);
    assert_eq!(
        message.len() + 1 + usize::from(msg.header().num_required_signatures) * 64,
        proof["signedPacketBytes"].as_u64().unwrap() as usize
    );
    // Durable control-plane recovery must retain preservation constraints.
    let restored: ExpectedExposure =
        serde_json::from_slice(&serde_json::to_vec(&expected).unwrap()).unwrap();
    assert_eq!(
        restored.outcome(&data, &pre, &post, cu).unwrap().0,
        exposure
    );
    for index in 0..post.len() {
        for delta in [-1i64, 1] {
            let Some(changed) = post[index].checked_add_signed(delta) else {
                continue;
            };
            let mut bad = post.clone();
            bad[index] = changed;
            assert!(
                restored.outcome(&data, &pre, &bad, cu).is_err(),
                "balance {index} delta {delta}"
            );
        }
    }
    let actual = STANDARD.decode(v["returnData"].as_str().unwrap()).unwrap();
    for (offset, value) in [
        (16, funding.input + 1),
        (48, funding.min_out - 1),
        (48, stocklana_adapters::MAX_INPUT + 1),
    ] {
        let mut bad = actual.clone();
        bad[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        assert!(expected
            .outcome(
                &json!({"programId":v["program"],"data":[STANDARD.encode(bad),"base64"]}),
                &pre,
                &post,
                cu
            )
            .is_err());
    }
    for attack in [
        "issuer",
        "policy",
        "conversion",
        "floor",
        "input",
        "nonce",
        "cash-as-input",
        "ceiling",
    ] {
        let mut intent = intent.clone();
        let mut admission = admission.clone();
        match attack {
            "issuer" => {
                intent.admitted_product_ids.pop();
            }
            "policy" => admission.products[0].policy_data_hash = [1; 32],
            "conversion" => admission.products[0].numerator += 1,
            "floor" => intent.minimum_exposure_q32 += 1,
            "input" => intent.input_atoms += 1,
            "nonce" => intent.owner_nonce += 1,
            "cash-as-input" => intent.input_mint = crate::market::USDC_MINT.into(),
            "ceiling" => admission.maximum_cu -= 1,
            _ => unreachable!(),
        }
        assert!(
            ExpectedExposure::bind(&snapshot, &intent, &admission, &message).is_err(),
            "{attack}"
        );
    }
    // Public executor wires may be malformed. Every truncated envelope must
    // reject without panicking, including zero/short funding/cell descriptors.
    for end in 0..wire.len() {
        let mut altered = msg.clone();
        match &mut altered {
            solana_message::VersionedMessage::V0(m) => {
                m.instructions.last_mut().unwrap().data.truncate(end)
            }
            _ => unreachable!(),
        }
        assert!(
            ExpectedExposure::bind(&snapshot, &intent, &admission, &altered.serialize()).is_err(),
            "truncated {end}"
        );
    }
    // Replay those exact SBF results through the RPC transport and preparation
    // cache. The transport is a local fixture; execution/CU came from SBF above.
    use crate::{
        exposure_pipeline::PreparedExposure,
        mesh::ExecutorBid,
        prepared_quotes::{Candidate, PreparedQuotes},
    };
    use ed25519_dalek::{Signer, SigningKey};
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        time::Instant,
    };
    let addresses = bindings
        .iter()
        .map(|b| b.account.clone())
        .collect::<Vec<_>>();
    let returned=addresses.iter().map(|address| {
        let a=after.iter().find(|a|&a.key==address).unwrap();
        json!({"owner":a.owner,"executable":a.executable,"lamports":a.lamports,"data":[STANDARD.encode(&a.data),"base64"]})
    }).collect::<Vec<_>>();
    let simulated = json!({"context":{"slot":snapshot.slot},"value":{"err":null,"unitsConsumed":cu,"returnData":data,"accounts":returned}});
    let mut unsigned = vec![msg.header().num_required_signatures];
    unsigned.resize(1 + usize::from(unsigned[0]) * 64, 0);
    unsigned.extend_from_slice(&message);
    let exact_wire = unsigned.clone();
    let slot = snapshot.slot;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..3 {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
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
                json!("funded-sbf-fixture")
            } else {
                assert_eq!(request["method"], "simulateTransaction");
                assert_eq!(request["params"][1]["replaceRecentBlockhash"], false);
                assert_eq!(request["params"][1]["sigVerify"], false);
                assert_eq!(request["params"][1]["minContextSlot"], slot);
                assert_eq!(
                    request["params"][1]["accounts"]["addresses"],
                    json!(addresses)
                );
                assert_eq!(
                    STANDARD
                        .decode(request["params"][0].as_str().unwrap())
                        .unwrap(),
                    exact_wire
                );
                simulated.clone()
            };
            let body =
                serde_json::to_vec(&json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
                    .unwrap();
            write!(reader.get_mut(),"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
            reader.get_mut().write_all(&body).unwrap();
        }
    });
    let rpc = Rpc::pinned(format!("http://{address}"), "funded-sbf-fixture".into()).unwrap();
    let prepared =
        PreparedExposure::simulate(&feed, &snapshot, &rpc, &intent, &admission, &message).unwrap();
    server.join().unwrap();
    assert_eq!(prepared.summary()["simulationCU"], cu);
    assert_eq!(
        prepared.summary()["simulationExposureQ32"],
        exposure.to_string()
    );
    assert_eq!(prepared.unsigned_wire(), unsigned);
    // No owner signature is invented or bypassed for this captured wallet.
    assert!(prepared
        .authorize(
            &feed,
            &snapshot,
            &intent,
            &unsigned,
            "funded-fixture".into(),
            999
        )
        .is_err());
    let executor = SigningKey::from_bytes(&[203; 32]);
    let mut bid = ExecutorBid {
        executor: bs58::encode(executor.verifying_key().to_bytes()).into_string(),
        sequence: 1,
        intent_commitment: intent.commitment(slot).unwrap(),
        world_generation_hash: snapshot.hash,
        transaction_message_hash: expected.message_hash,
        guaranteed_exposure_q32: u128::from(exposure),
        executor_fee_input_atoms: 0,
        predicted_compute_units: cu as u32,
        expires_slot: intent.deadline_slot,
        signature: String::new(),
    };
    bid.signature =
        bs58::encode(executor.sign(&bid.signing_bytes().unwrap()).to_bytes()).into_string();
    let candidate = Candidate {
        intent: intent.clone(),
        feed: feed.clone(),
        snapshot: snapshot.clone(),
        prepared: prepared.clone(),
        bid,
        last_valid_block_height: 999,
    };
    let mut store = PreparedQuotes::default();
    let quote_id = "stkq_0123456789abcdef0123456789abcdef";
    store
        .admit(
            quote_id,
            candidate,
            Instant::now() + Duration::from_secs(30),
            "2026-09-14T00:01:00.000Z".into(),
        )
        .unwrap();
    let review = store
        .review(quote_id, &intent.owner, snapshot.hash, slot)
        .unwrap()
        .unwrap();
    assert_eq!(review["estimatedCu"], cu);
    assert_eq!(review["submitAllowed"], false);
    assert_eq!(
        STANDARD
            .decode(review["transactionBase64"].as_str().unwrap())
            .unwrap(),
        unsigned
    );
    if let Ok(path) = std::env::var("SKEW_FUNDED_PREPARED_OUTPUT") {
        assert!(path.starts_with("/srv/skew/"));
        let path = PathBuf::from(path);
        let path = if expected.sellers.is_empty() {
            path.parent()
                .unwrap()
                .join("no-match")
                .join(path.file_name().unwrap())
        } else {
            path
        };
        std::fs::write(path, serde_json::to_vec_pretty(&review).unwrap()).unwrap();
    }
    feed.invalidate().unwrap();
    assert!(store
        .review(quote_id, &intent.owner, snapshot.hash, slot)
        .is_err());
    println!(
        "{}",
        json!({"scope":"captured funded SBF to host receipt contract; synthetic policies","cu":cu,"exposureQ32":exposure,"products":products.len(),"originalInputAtoms":expected.input,"preservedCashAtoms":post.last(),"persistedExpectation":true,"mainnet":false})
    );
}
