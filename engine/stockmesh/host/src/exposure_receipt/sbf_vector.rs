//! Cross-layer test consumes the real captured-state SBF outcome, not an
//! invented finalized RPC response. Run explicitly after the AWS SBF proof.
use super::*;
use crate::feed::{Account, Feed};
use std::{path::PathBuf, time::Duration};

pub(super) fn accounts(value: &Value) -> Vec<Account> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|row| Account {
            key: row["key"].as_str().unwrap().into(),
            owner: row["owner"].as_str().unwrap().into(),
            executable: row["executable"].as_bool().unwrap(),
            lamports: row["lamports"].as_u64().unwrap(),
            data: STANDARD.decode(row["data"][0].as_str().unwrap()).unwrap(),
        })
        .collect()
}

#[test]
#[ignore = "requires AWS stockmesh-exposure-proof captured SBF artifact"]
fn actual_sbf_multi_issuer_output_satisfies_the_host_receipt_contract() {
    let path =
        PathBuf::from(std::env::var("SKEW_EXPOSURE_VECTOR").expect("explicit AWS vector path"));
    let bytes = std::fs::read(&path).unwrap();
    let proof: Value =
        serde_json::from_slice(&std::fs::read(path.with_file_name("proof.json")).unwrap()).unwrap();
    assert_eq!(
        hex(&Sha256::digest(&bytes)),
        proof["host_vector_sha256"].as_str().unwrap()
    );
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let before = accounts(&v["before"]);
    let after = accounts(&v["after"]);
    let feed = Feed::new(
        before.iter().map(|a| a.key.clone()).collect(),
        Duration::from_secs(60),
        4 * 1024 * 1024,
    )
    .unwrap();
    let snapshot = feed
        .publish(&json!({"context":{"slot":v["slot"]},"value":v["before"]}))
        .unwrap();
    let products = v["productMints"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, mint)| {
            let mint = mint.as_str().unwrap();
            let account = before.iter().find(|a| a.key == mint).unwrap();
            let policy = v["policies"][i].as_str().unwrap();
            let state = before.iter().find(|a| a.key == policy).unwrap();
            ProductAdmission {
                identity: ProductIdentity {
                    instrument: "NVDA".into(),
                    issuer: format!("SBF-{i}"),
                    mint: mint.into(),
                    token_program: account.owner.clone(),
                    rights_hash: state.data[168..200].try_into().unwrap(),
                    raw_decimals: account.data[44],
                },
                policy: policy.into(),
                policy_data_hash: Sha256::digest(&state.data).into(),
                claim: None,
                claim_data_hash: None,
                policy_version: 1,
                model: 0,
                numerator: 1,
                denominator: 1,
                conservative_bps: 10000,
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
        owner: v["owner"].as_str().unwrap().into(),
        owner_nonce: 0,
        instrument: "NVDA".into(),
        input_mint: v["inputMint"].as_str().unwrap().into(),
        input_atoms: v["inputAtoms"].as_u64().unwrap(),
        minimum_exposure_q32: u128::from(v["floor"].as_u64().unwrap()),
        admitted_product_ids: admission
            .products
            .iter()
            .map(|p| p.identity.id().unwrap())
            .collect(),
        product_policy_hash: admission.product_policy_hash,
        world_generation_hash: snapshot.hash,
        deadline_slot: v["deadline"].as_u64().unwrap(),
    };
    let message = STANDARD.decode(v["message"].as_str().unwrap()).unwrap();
    let expected = ExpectedExposure::bind(&snapshot, &intent, &admission, &message).unwrap();
    assert_eq!(expected.opcode, 13);
    assert!(expected.sellers.is_empty());
    let bindings = expected.balance_bindings();
    let pre = bindings
        .iter()
        .map(|b| snapshot_amount(&snapshot, b).unwrap())
        .collect::<Vec<_>>();
    let post = bindings
        .iter()
        .map(|b| raw_amount(after.iter().find(|a| a.key == b.account).unwrap(), b).unwrap())
        .collect::<Vec<_>>();
    let cu = v["computeUnits"].as_u64().unwrap();
    let (exposure, products) = expected.outcome(&v["returnData"], &pre, &post, cu).unwrap();
    assert_eq!(exposure, proof["actual_exposure_q32"].as_u64().unwrap());
    assert_eq!(products.len(), 2);
    assert_eq!(
        message.len() + 65,
        proof["signed_packet_bytes"].as_u64().unwrap() as usize
    );
    for index in 0..post.len() {
        let mut bad = post.clone();
        bad[index] += 1;
        assert!(expected.outcome(&v["returnData"], &pre, &bad, cu).is_err());
    }
    for attack in ["product", "policy", "conversion", "floor", "budget"] {
        let mut intent = intent.clone();
        let mut admission = admission.clone();
        match attack {
            "product" => intent.admitted_product_ids.pop().map(|_| ()).unwrap(),
            "policy" => admission.products[0].policy_data_hash = [1; 32],
            "conversion" => admission.products[0].numerator = 2,
            "floor" => intent.minimum_exposure_q32 += 1,
            "budget" => admission.maximum_cu -= 1,
            _ => unreachable!(),
        }
        assert!(
            ExpectedExposure::bind(&snapshot, &intent, &admission, &message).is_err(),
            "{attack}"
        );
    }
    println!(
        "{}",
        json!({"scope":"captured SBF to host economic receipt contract","products":products.len(),"cu":cu,"packetBytes":message.len()+65,"exposureQ32":exposure,"mainnet":false})
    );
}
