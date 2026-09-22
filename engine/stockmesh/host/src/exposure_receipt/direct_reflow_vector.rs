//! Captured direct Economic Reflow SBF result through the prepared-wallet
//! transaction, executor-bid and durable receipt boundary.
use super::*;
use crate::{
    exposure_pipeline::PreparedExposure,
    feed::Feed,
    mesh::ExecutorBid,
    prepared_quotes::{Candidate, PreparedQuotes},
    rpc::Rpc,
};
use ed25519_dalek::{Signer, SigningKey};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

#[test]
#[ignore = "requires AWS coherent SPY direct-Reflow SBF vector"]
fn actual_spy_direct_reflow_binds_the_prepared_wallet_and_executor_contract() {
    let path = PathBuf::from(
        std::env::var("SKEW_DIRECT_REFLOW_VECTOR").expect("explicit AWS vector path"),
    );
    let bytes = std::fs::read(&path).unwrap();
    let vector: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        vector["schema"],
        "skew.stockmesh.direct-reflow-sbf-vector/v1"
    );
    let proof: Value =
        serde_json::from_slice(&std::fs::read(path.with_file_name("proof.json")).unwrap()).unwrap();
    assert_eq!(
        hex(&Sha256::digest(&bytes)),
        proof["directPathReflow"]["preparedHostVectorSha256"]
    );

    let before = super::sbf_vector::accounts(&vector["before"]);
    let after = super::sbf_vector::accounts(&vector["after"]);
    let feed = Arc::new(
        Feed::new(
            before.iter().map(|account| account.key.clone()).collect(),
            Duration::from_secs(60),
            4 * 1024 * 1024,
        )
        .unwrap(),
    );
    let snapshot = feed
        .publish(&json!({"context":{"slot":vector["slot"]},"value":vector["before"]}))
        .unwrap();
    let instrument = vector["instrument"].as_str().unwrap();
    let products = vector["products"]
        .as_array()
        .unwrap()
        .iter()
        .map(|product| {
            let policy = product["policy"].as_str().unwrap();
            let policy_state = before.iter().find(|account| account.key == policy).unwrap();
            ProductAdmission {
                identity: ProductIdentity {
                    instrument: instrument.into(),
                    issuer: product["issuer"].as_str().unwrap().into(),
                    mint: product["mint"].as_str().unwrap().into(),
                    token_program: product["tokenProgram"].as_str().unwrap().into(),
                    rights_hash: policy_state.data[168..200].try_into().unwrap(),
                    raw_decimals: product["rawDecimals"].as_u64().unwrap() as u8,
                },
                policy: policy.into(),
                policy_data_hash: Sha256::digest(&policy_state.data).into(),
                claim: None,
                claim_data_hash: None,
                policy_version: 1,
                model: product["model"].as_u64().unwrap() as u8,
                numerator: product["numerator"].as_u64().unwrap(),
                denominator: product["denominator"].as_u64().unwrap(),
                conservative_bps: product["conservativeBps"].as_u64().unwrap() as u16,
            }
        })
        .collect::<Vec<_>>();
    let product_policy_hash: [u8; 32] = vector["productPolicyHash"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_u64().unwrap() as u8)
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let admission = Admission {
        settlement_program: vector["program"].as_str().unwrap().into(),
        product_policy_hash,
        maximum_cu: 1_400_000,
        maximum_heap_frame_bytes: 256 * 1024,
        maximum_compute_price: 0,
        allow_underlying_closed: false,
        products,
    };
    let intent = FrozenIntent {
        intent_id: [92; 32],
        owner: vector["owner"].as_str().unwrap().into(),
        owner_nonce: vector["ownerSequence"].as_u64().unwrap(),
        instrument: instrument.into(),
        input_mint: vector["inputMint"].as_str().unwrap().into(),
        input_atoms: vector["inputAtoms"].as_u64().unwrap(),
        minimum_exposure_q32: u128::from(vector["minimumExposureQ32"].as_u64().unwrap()),
        admitted_product_ids: admission
            .products
            .iter()
            .map(|product| product.identity.id().unwrap())
            .collect(),
        product_policy_hash,
        world_generation_hash: snapshot.hash,
        deadline_slot: vector["deadlineSlot"].as_u64().unwrap(),
    };
    let message = STANDARD
        .decode(vector["messageBase64"].as_str().unwrap())
        .unwrap();
    let expected = ExpectedExposure::bind(&snapshot, &intent, &admission, &message).unwrap();
    assert_eq!(expected.opcode, 14);
    assert_eq!(
        expected.heap_frame_bytes(),
        vector["heapFrameBytes"].as_u64().unwrap() as u32
    );
    assert!(expected.sellers.is_empty());
    assert_eq!(expected.products.len(), 2);
    let bindings = expected.balance_bindings();
    let pre = bindings
        .iter()
        .map(|binding| snapshot_amount(&snapshot, binding).unwrap())
        .collect::<Vec<_>>();
    let post = bindings
        .iter()
        .map(|binding| {
            raw_amount(
                after
                    .iter()
                    .find(|account| account.key == binding.account)
                    .unwrap(),
                binding,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    let return_data = json!({
        "programId":vector["program"],
        "data":[vector["returnData"],"base64"]
    });
    let cu = vector["cu"].as_u64().unwrap();
    let (exposure, outcomes) = expected.outcome(&return_data, &pre, &post, cu).unwrap();
    assert_eq!(exposure, vector["actualExposureQ32"].as_u64().unwrap());
    assert_eq!(outcomes.len(), 2);
    assert_eq!(
        outcomes
            .iter()
            .filter(|product| {
                product["rawAtoms"]
                    .as_str()
                    .is_some_and(|atoms| atoms != "0")
            })
            .count(),
        1
    );
    assert_eq!(
        message.len() + 65,
        vector["eventualSignedPacketBytes"].as_u64().unwrap() as usize
    );

    // The exact account deltas produced by SBF are replayed through the RPC
    // interface used by production preparation. No signature or submission is
    // fabricated by this fixture.
    let addresses = bindings
        .iter()
        .map(|binding| binding.account.clone())
        .collect::<Vec<_>>();
    let returned = addresses
        .iter()
        .map(|address| {
            let account = after.iter().find(|account| &account.key == address).unwrap();
            json!({"owner":account.owner,"executable":account.executable,"lamports":account.lamports,
                "data":[STANDARD.encode(&account.data),"base64"]})
        })
        .collect::<Vec<_>>();
    let simulated = json!({"context":{"slot":snapshot.slot},"value":{
        "err":null,"unitsConsumed":cu,"returnData":return_data,"accounts":returned
    }});
    let mut unsigned = vec![pipeline::decode(&message)
        .unwrap()
        .header()
        .num_required_signatures];
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
                json!("direct-reflow-sbf-fixture")
            } else {
                assert_eq!(request["method"], "simulateTransaction");
                assert_eq!(request["params"][1]["minContextSlot"], slot);
                assert_eq!(request["params"][1]["accounts"]["addresses"], json!(addresses));
                assert_eq!(
                    STANDARD
                        .decode(request["params"][0].as_str().unwrap())
                        .unwrap(),
                    exact_wire
                );
                simulated.clone()
            };
            let body = serde_json::to_vec(
                &json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
            )
            .unwrap();
            write!(
                reader.get_mut(),
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            reader.get_mut().write_all(&body).unwrap();
        }
    });
    let rpc = Rpc::pinned(
        format!("http://{address}"),
        "direct-reflow-sbf-fixture".into(),
    )
    .unwrap();
    let prepared =
        PreparedExposure::simulate(&feed, &snapshot, &rpc, &intent, &admission, &message).unwrap();
    server.join().unwrap();
    assert_eq!(prepared.simulated_cu(), cu);
    assert_eq!(prepared.simulated_exposure(), exposure);
    assert_eq!(
        prepared.summary()["heapFrameBytes"],
        vector["heapFrameBytes"]
    );
    assert_eq!(prepared.unsigned_wire(), unsigned);
    assert!(prepared
        .authorize(
            &feed,
            &snapshot,
            &intent,
            &unsigned,
            "unsigned-owner-proof".into(),
            999,
        )
        .is_err());

    let executor = SigningKey::from_bytes(&[204; 32]);
    let mut bid = ExecutorBid {
        executor: bs58::encode(executor.verifying_key().to_bytes()).into_string(),
        sequence: 1,
        intent_commitment: intent.commitment(slot).unwrap(),
        world_generation_hash: snapshot.hash,
        transaction_message_hash: prepared.message_hash(),
        guaranteed_exposure_q32: u128::from(exposure),
        executor_fee_input_atoms: 0,
        predicted_compute_units: u32::try_from(cu).unwrap(),
        expires_slot: intent.deadline_slot,
        signature: String::new(),
    };
    bid.signature = bs58::encode(executor.sign(&bid.signing_bytes().unwrap()).to_bytes()).into_string();
    prepared.validate_bid(&bid, &intent, slot).unwrap();
    let candidate = Candidate {
        intent: intent.clone(),
        feed: feed.clone(),
        snapshot: snapshot.clone(),
        prepared,
        bid,
        last_valid_block_height: 999,
    };
    let quote_id = "stkq_0123456789abcdef0123456789abcdef";
    let mut store = PreparedQuotes::default();
    store
        .admit(
            quote_id,
            candidate,
            Instant::now() + Duration::from_secs(30),
            "2026-09-15T00:00:00.000Z".into(),
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
    if let Ok(output) = std::env::var("SKEW_DIRECT_REFLOW_PREPARED_OUTPUT") {
        let output = PathBuf::from(output);
        assert!(output.starts_with("/srv/skew/"));
        std::fs::write(output, serde_json::to_vec_pretty(&review).unwrap()).unwrap();
    }
    feed.invalidate().unwrap();
    assert!(store
        .review(quote_id, &intent.owner, snapshot.hash, slot)
        .is_err());
    println!(
        "{}",
        json!({
            "scope":"captured coherent SPY direct Reflow through prepared wallet/executor contract",
            "products":outcomes.len(),"selectedProducts":1,"cu":cu,
            "exposureQ32":exposure,"ownerSigned":false,"submitted":false,"mainnetFilled":false
        })
    );
}
