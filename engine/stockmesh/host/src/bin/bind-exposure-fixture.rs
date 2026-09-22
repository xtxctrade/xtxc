//! AWS isolated-validator helper. It derives a synthetic admission from a
//! captured opcode-18 v2 fixture and emits the production ExpectedExposure
//! object. This is test tooling; issuer admission in the service remains an
//! operator-pinned input and must never be inferred from an executor message.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use skew_execution_host::{
    exposure_receipt::{Admission, ExpectedExposure, ProductAdmission},
    feed::Feed,
    mesh::{FrozenIntent, ProductIdentity},
    pipeline,
};
use std::{io::Read, sync::Arc, time::Duration};
use stocklana_adapters::{graph::Graph, u64_at};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    slot: u64,
    accounts: Vec<Account>,
    #[serde(rename = "messageBase64")]
    message_base64: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Account {
    key: String,
    owner: String,
    executable: bool,
    lamports: u64,
    data: String,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut body = Vec::new();
    std::io::stdin()
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut body)
        .map_err(|error| error.to_string())?;
    if body.len() > 4 * 1024 * 1024 {
        return Err("fixture request budget".into());
    }
    let request: Request = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
    if request.accounts.is_empty() || request.accounts.len() > 100 {
        return Err("fixture account count".into());
    }
    let keys = request
        .accounts
        .iter()
        .map(|account| account.key.clone())
        .collect::<Vec<_>>();
    let feed = Arc::new(Feed::new(keys, Duration::from_secs(60), 4 * 1024 * 1024)?);
    let response = json!({
        "context":{"slot":request.slot},
        "value":request.accounts.iter().map(|account| json!({
            "owner":account.owner,
            "executable":account.executable,
            "lamports":account.lamports,
            "data":[account.data,"base64"]
        })).collect::<Vec<_>>()
    });
    let snapshot = feed.publish(&response)?;
    let message = STANDARD
        .decode(request.message_base64)
        .map_err(|error| error.to_string())?;
    let decoded = pipeline::decode(&message)?;
    let resolved = pipeline::resolved(&decoded, &snapshot)?;
    let settlement = decoded
        .instructions()
        .last()
        .ok_or("settlement instruction")?;
    let program = resolved
        .get(usize::from(settlement.program_id_index))
        .ok_or("settlement program index")?;
    let local = settlement
        .accounts
        .iter()
        .map(|index| {
            resolved
                .get(usize::from(*index))
                .cloned()
                .ok_or_else(|| "settlement account index".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let at = |index: u8| {
        local
            .get(usize::from(index))
            .map(String::as_str)
            .ok_or_else(|| "local account index".to_string())
    };
    let wire = settlement.data.as_slice();
    if wire.len() < 8 || wire[0] != 18 || wire[1] != 2 {
        return Err("opcode-18 v2 fixture required".into());
    }
    let funding_end = 8usize
        .checked_add(usize::from(u16::from_le_bytes([wire[4], wire[5]])))
        .ok_or("funding length")?;
    let funding = Graph::decode(
        wire.get(8..funding_end).ok_or("funding graph bytes")?,
        local.len(),
    )
    .map_err(|_| "funding graph")?;
    let cell = wire.get(funding_end..).ok_or("mesh cell")?;
    if cell.len() < 56 || cell[0] != 14 || !(1..=4).contains(&cell[1]) {
        return Err("mesh cell shape".into());
    }
    let account = |address: &str| {
        snapshot
            .accounts
            .iter()
            .find(|account| account.key == address)
            .ok_or_else(|| "fixture account missing".to_string())
    };
    let mut products = Vec::with_capacity(usize::from(cell[1]));
    for index in 0..usize::from(cell[1]) {
        let start = 56 + index * 32;
        let row = cell.get(start..start + 32).ok_or("product row")?;
        let policy = account(at(row[0])?)?;
        let mint = account(at(row[2])?)?;
        if policy.data.len() != 225 || &policy.data[..8] != b"SKEWSTK2" {
            return Err("ProductPolicy v2 fixture required".into());
        }
        let claim = (row[5] != 0)
            .then(|| at(row[5]).map(str::to_owned))
            .transpose()?;
        let identity = ProductIdentity {
            instrument: "NVDA".into(),
            issuer: format!("SBF-{index}"),
            mint: mint.key.clone(),
            token_program: mint.owner.clone(),
            rights_hash: policy.data[168..200].try_into().unwrap(),
            raw_decimals: *mint.data.get(44).ok_or("mint decimals")?,
        };
        products.push(ProductAdmission {
            identity,
            policy: policy.key.clone(),
            policy_data_hash: Sha256::digest(&policy.data).into(),
            claim_data_hash: claim
                .as_ref()
                .map(|address| account(address).map(|value| Sha256::digest(&value.data).into()))
                .transpose()?,
            claim,
            policy_version: u64_at(row, 8).map_err(|_| "policy version")?,
            model: row[4],
            numerator: u64_at(row, 16).map_err(|_| "conversion numerator")?,
            denominator: u64_at(row, 24).map_err(|_| "conversion denominator")?,
            conservative_bps: u16::from_le_bytes([row[6], row[7]]),
        });
    }
    let product_policy_hash = [91; 32];
    let admission = Admission {
        settlement_program: program.clone(),
        product_policy_hash,
        maximum_cu: 1_400_000,
        maximum_heap_frame_bytes: 256 * 1024,
        maximum_compute_price: 0,
        allow_underlying_closed: false,
        products,
    };
    let intent = FrozenIntent {
        intent_id: [92; 32],
        owner: at(cell[48])?.into(),
        owner_nonce: funding.sequence,
        instrument: "NVDA".into(),
        input_mint: at(funding.assets[0].mint)?.into(),
        input_atoms: funding.input,
        minimum_exposure_q32: u128::from(u64_at(cell, 40).map_err(|_| "exposure floor")?),
        admitted_product_ids: admission
            .products
            .iter()
            .map(|product| product.identity.id())
            .collect::<Result<Vec<_>, _>>()?,
        product_policy_hash,
        world_generation_hash: snapshot.hash,
        deadline_slot: u64_at(cell, 8).map_err(|_| "deadline")?,
    };
    let expected = ExpectedExposure::bind(&snapshot, &intent, &admission, &message)?;
    let value: Value = serde_json::to_value(expected).map_err(|error| error.to_string())?;
    println!(
        "{}",
        serde_json::to_string(&value).map_err(|error| error.to_string())?
    );
    Ok(())
}
