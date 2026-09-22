//! Build a deterministic, unsigned ProductPolicy v2 deployment bundle.
//!
//! This tool consumes only public keys and a previously validated StockMesh
//! lane manifest. It owns no signing key, creates no transaction and submits
//! nothing. The policy authority signs the emitted instructions separately.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use skew_execution_host::{
    lookup_registry::LookupRegistry,
    onebook_wire::{compile_stock_policy_v2, instrument_id, issuer_id, StockPolicyV2Spec},
    policy_bindings::policy_inputs,
    stockmesh_api::{validate_policy_lane_document, Manifest},
    world::WorldConfig,
};
use solana_pubkey::Pubkey;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Component, Path, PathBuf},
    str::FromStr,
};

fn key(value: &str) -> Result<Pubkey, String> {
    Pubkey::from_str(value).map_err(|_| format!("invalid public key: {value}"))
}

const FORBIDDEN_DEPLOYMENT_IDENTITIES: [&str; 13] = [
    "11111111111111111111111111111111",
    "SysvarC1ock11111111111111111111111111111111",
    "SysvarRent111111111111111111111111111111111",
    "ComputeBudget111111111111111111111111111111",
    "Vote111111111111111111111111111111111111111",
    "AddressLookupTab1e1111111111111111111111111",
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
    "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
    "BPFLoaderUpgradeab1e11111111111111111111111",
    "BPFLoader1111111111111111111111111111111111",
    "Ed25519SigVerify111111111111111111111111111",
    "Stake11111111111111111111111111111111111111",
];

fn validate_deployment_identities(identities: [Pubkey; 4]) -> Result<(), String> {
    let encoded = identities.map(|identity| identity.to_string());
    if identities
        .iter()
        .any(|identity| *identity == Pubkey::default())
        || encoded.iter().any(|identity| {
            FORBIDDEN_DEPLOYMENT_IDENTITIES
                .iter()
                .any(|forbidden| forbidden == identity)
        })
        || identities
            .iter()
            .enumerate()
            .any(|(index, identity)| identities[index + 1..].contains(identity))
    {
        return Err("deployment identities must be distinct non-system addresses".into());
    }
    Ok(())
}

fn hex_32(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("rights hash must be 32-byte hex".into());
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| "rights hash encoding".to_string())?;
    }
    if bytes == [0; 32] {
        return Err("empty rights hash".into());
    }
    Ok(bytes)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

type Binding = (String, String);

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ExistingDeployment {
    schema: String,
    network: String,
    settlement_program: String,
    policy_authority: String,
    lookup_table: String,
    #[serde(default)]
    instrument_lookup_tables: BTreeMap<String, String>,
    executor: String,
    deadline_slots: u64,
    maximum_policy_age_slots: u64,
    compute_unit_limit: u32,
    maximum_heap_frame_bytes: u32,
    compute_unit_price_micro_lamports: u64,
    allow_underlying_closed: bool,
    products: Vec<ExistingProduct>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ExistingProduct {
    product_id: String,
    input_mint: String,
    policy: String,
    policy_version: u64,
}

fn read_public(path: &Path, maximum: u64) -> Result<Vec<u8>, String> {
    let meta = fs::symlink_metadata(path).map_err(|_| "public input metadata")?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > maximum {
        return Err("public input must be a bounded regular file".into());
    }
    let bytes = fs::read(path).map_err(|_| "public input read")?;
    if bytes.len() as u64 != meta.len() {
        return Err("public input changed while reading".into());
    }
    Ok(bytes)
}

impl ExistingDeployment {
    fn validate(&self, program: Pubkey, authority: Pubkey, executor: Pubkey) -> Result<(), String> {
        if !matches!(self.schema.as_str(), "skew.stockmesh.prepare-deployment/v1" | "skew.stockmesh.prepare-deployment/v2")
            || self.network != "mainnet-beta"
            || self.settlement_program != program.to_string()
            || self.policy_authority != authority.to_string()
            || self.executor != executor.to_string()
        {
            return Err("retained deployment network, schema or authority mismatch".into());
        }
        validate_deployment_identities([program, authority, key(&self.lookup_table)?, executor])?;
        LookupRegistry::parse(&self.lookup_table, &self.instrument_lookup_tables, &[program, authority, executor])?;
        if !(1..=150).contains(&self.deadline_slots)
            || !(1..=150).contains(&self.maximum_policy_age_slots)
            || !(1..=1_400_000).contains(&self.compute_unit_limit)
            || !(32_768..=262_144).contains(&self.maximum_heap_frame_bytes)
            || !self.maximum_heap_frame_bytes.is_multiple_of(1024)
            || self.compute_unit_price_micro_lamports > 10_000_000
            || self.products.is_empty()
            || self.products.len() > 256
        {
            return Err("retained deployment configuration bounds".into());
        }
        let mut bindings = BTreeMap::new();
        let mut addresses = BTreeMap::new();
        for product in &self.products {
            let input = key(&product.input_mint)?;
            let policy = key(&product.policy)?;
            if !product.product_id.starts_with("stkprd_")
                || product.product_id.len() != 39
                || !product.product_id[7..].bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                || input == Pubkey::default()
                || product.policy_version == 0
                || [program, authority, executor, input, Pubkey::default()].contains(&policy)
                || FORBIDDEN_DEPLOYMENT_IDENTITIES.contains(&product.policy.as_str())
                || bindings.insert((product.product_id.clone(), input), ()).is_some()
                || addresses.insert(policy, ()).is_some()
            {
                return Err("retained deployment duplicate or invalid policy binding".into());
            }
        }
        Ok(())
    }

    fn preserve_configuration(&self, output: &mut Value) {
        output["deadlineSlots"] = json!(self.deadline_slots);
        output["maximumPolicyAgeSlots"] = json!(self.maximum_policy_age_slots);
        output["computeUnitLimit"] = json!(self.compute_unit_limit);
        output["maximumHeapFrameBytes"] = json!(self.maximum_heap_frame_bytes);
        output["computeUnitPriceMicroLamports"] = json!(self.compute_unit_price_micro_lamports);
        output["allowUnderlyingClosed"] = json!(self.allow_underlying_closed);
    }
}

// Retention is deliberately not an update operation. The manifest-derived PDA
// must still match, and every old binding must survive. Freshness and rights
// remain the responsibility of the coherent on-chain preflight, not this file.
fn retain_existing(
    existing: &ExistingDeployment,
    products: &mut BTreeMap<Binding, Value>,
    publications: &mut BTreeMap<Binding, Value>,
) -> Result<Vec<Value>, String> {
    let mut retained = Vec::new();
    for product in &existing.products {
        let id = (product.product_id.clone(), product.input_mint.clone());
        let compiled = products.get_mut(&id).ok_or("existing binding would be removed")?;
        if compiled["policy"].as_str() != Some(product.policy.as_str()) {
            return Err("existing policy differs from manifest-derived PDA".into());
        }
        compiled["policyVersion"] = json!(product.policy_version);
        if publications.remove(&id).is_none() {
            return Err("retained binding has no unique compiled publication".into());
        }
        retained.push(compiled.clone());
    }
    Ok(retained)
}

fn bounded_relative(base: &Path, value: &str) -> Result<PathBuf, String> {
    let relative = Path::new(value);
    if relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("world path must be a simple relative path".into());
    }
    Ok(base.join(relative))
}

fn lane_policy_inputs(base: &Path, paths: &[String], root: &str, output: &str, products: &BTreeSet<String>) -> Result<BTreeSet<String>, String> {
    if !(1..=3).contains(&paths.len()) { return Err("policy world count".into()); }
    let worlds=paths.iter().map(|path| {
        let bytes=read_public(&bounded_relative(base,path)?,64*1024)?;
        let world:WorldConfig=serde_json::from_slice(&bytes).map_err(|e|e.to_string())?;
        if world.markets.is_empty() || world.markets.len()>8 || world.markets.iter().any(|m|key(&m.input_mint).is_err() || key(&m.output_mint).is_err()) {
            return Err("policy world markets".into());
        }
        Ok(world)
    }).collect::<Result<Vec<_>,String>>()?;
    let inputs=policy_inputs(root,output,&worlds,products)?;
    Ok(BTreeSet::from([inputs.buy_cash,inputs.sell_cash]))
}

fn run() -> Result<(), String> {
    let args = env::args().collect::<Vec<_>>();
    if args.len() < 9 {
        return Err("usage: product-policy-bundle MANIFEST SETTLEMENT_PROGRAM AUTHORITY LOOKUP_TABLE EXECUTOR VERSION EXPIRES_SLOT OUTPUT_DIR [INSTRUMENT_LOOKUP_TABLES_JSON] [--retain-deployment EXISTING_DEPLOYMENT_JSON]".into());
    }
    let manifest_path = PathBuf::from(&args[1]);
    let settlement_program = key(&args[2])?;
    let authority = key(&args[3])?;
    let lookup_table = key(&args[4])?;
    let executor = key(&args[5])?;
    let version = args[6]
        .parse::<u64>()
        .map_err(|_| "policy version".to_string())?;
    let expires_slot = args[7]
        .parse::<u64>()
        .map_err(|_| "policy expiry slot".to_string())?;
    let output = PathBuf::from(&args[8]);
    validate_deployment_identities([settlement_program, authority, lookup_table, executor])?;
    let mut mapping_path = None;
    let mut retain_path = None;
    let mut options = args[9..].iter();
    while let Some(option) = options.next() {
        if option == "--retain-deployment" && retain_path.is_none() {
            retain_path = Some(options.next().ok_or("missing retained deployment path")?);
        } else if !option.starts_with('-') && mapping_path.is_none() {
            mapping_path = Some(option);
        } else {
            return Err("unknown or duplicate bundle option".into());
        }
    }
    let existing_bytes = retain_path.map(|path| read_public(Path::new(path), 256 * 1024)).transpose()?;
    let existing: Option<ExistingDeployment> = existing_bytes.as_ref().map(|bytes|
        serde_json::from_slice(bytes).map_err(|_| "retained deployment JSON".to_string())
    ).transpose()?;
    if let Some(existing) = &existing {
        existing.validate(settlement_program, authority, executor)?;
    }
    let instrument_lookup_tables: BTreeMap<String, String> = if let Some(path) = mapping_path {
        let bytes = read_public(Path::new(path), 32 * 1024)?;
        serde_json::from_slice(&bytes).map_err(|_| "instrument lookup mapping JSON")?
    } else { existing.as_ref().map(|value| value.instrument_lookup_tables.clone()).unwrap_or_default() };
    let lookup_registry = LookupRegistry::parse(&lookup_table.to_string(), &instrument_lookup_tables,
        &[settlement_program, authority, executor])?;
    if version == 0 || expires_slot == 0 || output.exists() {
        return Err("deployment identity, version, expiry or output path".into());
    }
    let manifest_bytes = read_public(&manifest_path, 4 * 1024 * 1024)?;
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).map_err(|error| error.to_string())?;
    if manifest.network != "mainnet-beta"
        || !manifest.policy_integrity_required
        || manifest.banks.is_empty()
        || manifest.banks.len() > 128
    {
        return Err("manifest network or bank count".into());
    }
    let base = manifest_path.parent().ok_or("manifest parent")?;
    lookup_registry.validate_coverage(manifest.banks.iter().flat_map(|bank| bank.lanes.iter().map(|lane| lane.instrument.as_str())))?;
    let mut policies = BTreeMap::<(String, String), Value>::new();
    let mut deployment_products = BTreeMap::<(String, String), Value>::new();
    for bank in &manifest.banks {
        let products=bank.lanes.iter().map(|lane|lane.output_mint.clone()).collect::<BTreeSet<_>>();
        for lane in &bank.lanes {
            if !validate_policy_lane_document(lane) {
                return Err(format!(
                    "canonical ProductPolicy document rejected for {}",
                    lane.product_id
                ));
            }
            let root=match lane.input_symbol.as_str() {
                "USDC"=>"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "SOL"=>"So11111111111111111111111111111111111111112",
                _=>return Err("policy lane cash symbol".into()),
            };
            // Economic BUY and native held-product SELL can need different
            // policies. Preserve both; never retarget a published binding.
            for input_mint in lane_policy_inputs(base,&lane.worlds,root,&lane.output_mint,&products)? {
            let input = key(&input_mint)?;
            let output_mint = key(&lane.output_mint)?;
            let rights_hash = hex_32(&lane.rights_hash)?;
            let instruction = compile_stock_policy_v2(
                settlement_program,
                StockPolicyV2Spec {
                    authority,
                    instrument: instrument_id(&lane.instrument)?,
                    issuer: issuer_id(&lane.issuer)?,
                    input_mint: input,
                    output_mint,
                    rights_hash,
                    version,
                    expires_slot,
                    // Secondary trading admitted, not halted, underlying open.
                    flags: 1 | 16,
                },
            )?;
            let policy = instruction.accounts[1].pubkey.to_string();
            let publication = json!({
                "productId": lane.product_id,
                "instrument": lane.instrument,
                "issuer": lane.issuer,
                "inputMint": input_mint,
                "outputMint": lane.output_mint,
                "rightsHash": lane.rights_hash,
                "policy": policy,
                "programId": instruction.program_id.to_string(),
                "accounts": instruction.accounts.iter().map(|account| json!({
                    "pubkey":account.pubkey.to_string(),
                    "isSigner":account.is_signer,
                    "isWritable":account.is_writable,
                })).collect::<Vec<_>>(),
                "dataBase64": STANDARD.encode(&instruction.data),
                "dataSha256": sha256(&instruction.data),
            });
            let id = (lane.product_id.clone(), input.to_string());
            if let Some(existing) = policies.insert(id.clone(), publication.clone()) {
                if existing != publication {
                    return Err("duplicate product/cash binding differs".into());
                }
            }
            let deployment = json!({
                "productId": lane.product_id,
                "inputMint": input.to_string(),
                "policy": policy,
                "policyVersion": version,
            });
            if let Some(existing) = deployment_products.insert(id, deployment.clone()) {
                if existing != deployment {
                    return Err("duplicate deployment binding differs".into());
                }
            }
            }
        }
    }
    // This is a publication inventory, not a single transaction. Per-order
    // product, account and CU limits are unchanged by catalog expansion.
    if policies.is_empty() || policies.len() > 256 {
        return Err("policy count".into());
    }
    let mut unique_addresses = BTreeMap::new();
    for (binding, value) in &deployment_products {
        let address = value["policy"].as_str().ok_or("compiled policy address")?;
        if unique_addresses.insert(address, binding).is_some() {
            return Err("different product bindings alias one policy address".into());
        }
    }
    let retained = match &existing {
        Some(value) => retain_existing(value, &mut deployment_products, &mut policies)?,
        None => Vec::new(),
    };
    let new_count = policies.len();
    let retained_count = retained.len();
    let existing_sha256 = existing_bytes.as_ref().map(|bytes| sha256(bytes));
    fs::create_dir(&output).map_err(|error| error.to_string())?;
    let publications = serde_json::to_vec_pretty(&json!({
        "schema":"skew.stockmesh.unsigned-product-policy-bundle/v1",
        "network":"mainnet-beta",
        "settlementProgram":settlement_program.to_string(),
        "authority":authority.to_string(),
        "version":version,
        "expiresSlot":expires_slot,
        "flags":17,
        "retainedDeploymentSha256":existing_sha256,
        "retainedPolicies":retained,
        "newPublicationCount":new_count,
        "retainedPolicyCount":retained_count,
        "retainedOnchainStateVerified":false,
        "status":"UNSIGNED_REQUIRES_OWNER_REVIEW",
        "identityState":"OPERATOR_SUPPLIED_UNVERIFIED",
        "signedTransactions":0,
        "submittedTransactions":0,
        "instructions":policies.into_values().collect::<Vec<_>>(),
    }))
    .map_err(|error| error.to_string())?;
    let mut deployment_value = json!({
        "schema":"skew.stockmesh.prepare-deployment/v2",
        "network":"mainnet-beta",
        "settlementProgram":settlement_program.to_string(),
        "policyAuthority":authority.to_string(),
        "lookupTable":lookup_table.to_string(),
        "instrumentLookupTables":instrument_lookup_tables,
        "executor":executor.to_string(),
        "deadlineSlots":40,
        "maximumPolicyAgeSlots":100,
        "computeUnitLimit":1_400_000,
        "maximumHeapFrameBytes":262_144,
        "computeUnitPriceMicroLamports":0,
        "allowUnderlyingClosed":false,
        "products":deployment_products.into_values().collect::<Vec<_>>(),
    });
    if let Some(existing) = &existing {
        existing.preserve_configuration(&mut deployment_value);
    }
    let deployment = serde_json::to_vec_pretty(&deployment_value).map_err(|error| error.to_string())?;
    fs::write(
        output.join("unsigned-policy-publications.json"),
        &publications,
    )
    .map_err(|error| error.to_string())?;
    fs::write(output.join("prepare-deployment.json"), &deployment)
        .map_err(|error| error.to_string())?;
    let evidence = serde_json::to_vec_pretty(&json!({
        "schema":"skew.stockmesh.product-policy-bundle-evidence/v1",
        "sourceManifest":manifest_path.to_string_lossy(),
        "sourceManifestSha256":sha256(&manifest_bytes),
        "retainedDeploymentSha256":existing_sha256,
        "newPublicationCount":new_count,
        "retainedPolicyCount":retained_count,
        "retainedOnchainStateVerified":false,
        "unsignedPolicyPublicationsSha256":sha256(&publications),
        "prepareDeploymentSha256":sha256(&deployment),
        "status":"UNSIGNED_REQUIRES_OWNER_REVIEW",
        "identityState":"OPERATOR_SUPPLIED_UNVERIFIED",
        "onchainIdentityVerificationComplete":false,
        "authoritySecretPresent":false,
        "signedTransactions":0,
        "submittedTransactions":0,
    }))
    .map_err(|error| error.to_string())?;
    fs::write(output.join("evidence.json"), &evidence).map_err(|error| error.to_string())?;
    println!(
        "{}",
        json!({"output":output,"policies":deployment_products_len(&deployment)?,"newPublications":new_count,"retainedPolicies":retained_count,"signedTransactions":0,"submittedTransactions":0})
    );
    Ok(())
}

fn deployment_products_len(bytes: &[u8]) -> Result<usize, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    value["products"]
        .as_array()
        .map(Vec::len)
        .ok_or_else(|| "deployment product encoding".into())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn retained_fixture() -> (ExistingDeployment, [Pubkey; 4]) {
        let ids = [Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique()];
        let existing = serde_json::from_value(json!({
            "schema":"skew.stockmesh.prepare-deployment/v2", "network":"mainnet-beta",
            "settlementProgram":ids[0].to_string(), "policyAuthority":ids[1].to_string(),
            "lookupTable":ids[2].to_string(), "executor":ids[3].to_string(),
            "deadlineSlots":55,"maximumPolicyAgeSlots":120,"computeUnitLimit":1_200_000,
            "maximumHeapFrameBytes":131_072,"computeUnitPriceMicroLamports":123,
            "allowUnderlyingClosed":true,
            "products":[{
                "productId":format!("stkprd_{}", "a".repeat(32)),
                "inputMint":Pubkey::new_unique().to_string(),
                "policy":Pubkey::new_unique().to_string(),"policyVersion":31
            }]
        })).unwrap();
        (existing, ids)
    }

    fn compiled_fixture(existing: &ExistingDeployment) -> BTreeMap<Binding, Value> {
        existing.products.iter().map(|product| (
            (product.product_id.clone(), product.input_mint.clone()),
            json!({"productId":product.product_id,"inputMint":product.input_mint,
                "policy":product.policy,"policyVersion":1})
        )).collect()
    }

    #[test]
    fn delta_preserves_existing_version_and_only_publishes_new_bindings() {
        let (existing, ids) = retained_fixture();
        existing.validate(ids[0], ids[1], ids[3]).unwrap();
        let mut products = compiled_fixture(&existing);
        let old_id = products.keys().next().unwrap().clone();
        let new_id = (format!("stkprd_{}", "b".repeat(32)), Pubkey::new_unique().to_string());
        let new_product = json!({"productId":new_id.0,"inputMint":new_id.1,
            "policy":Pubkey::new_unique().to_string(),"policyVersion":1});
        products.insert(new_id.clone(), new_product.clone());
        let mut publications = products.clone();
        let retained = retain_existing(&existing, &mut products, &mut publications).unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(products[&old_id]["policyVersion"], 31);
        assert_eq!(publications.len(), 1);
        assert_eq!(publications[&new_id], new_product);
        assert_eq!(products[&new_id]["policyVersion"], 1);
    }

    #[test]
    fn fully_retained_inventory_produces_no_publications() {
        let (existing, _) = retained_fixture();
        let mut products = compiled_fixture(&existing);
        let mut publications = products.clone();
        assert_eq!(retain_existing(&existing, &mut products, &mut publications).unwrap().len(), 1);
        assert!(publications.is_empty());
        assert_eq!(products.values().next().unwrap()["policyVersion"], 31);
    }

    #[test]
    fn delta_rejects_dropped_or_rebound_existing_product() {
        let (existing, _) = retained_fixture();
        let mut products = compiled_fixture(&existing);
        let mut publications = products.clone();
        products.values_mut().next().unwrap()["policy"] = json!(Pubkey::new_unique().to_string());
        assert!(retain_existing(&existing, &mut products, &mut publications).unwrap_err().contains("PDA"));
        assert!(retain_existing(&existing, &mut BTreeMap::new(), &mut publications).unwrap_err().contains("removed"));
    }

    #[test]
    fn retained_identity_and_runtime_configuration_cannot_drift_silently() {
        let (mut existing, ids) = retained_fixture();
        assert!(existing.validate(Pubkey::new_unique(), ids[1], ids[3]).is_err());
        assert!(existing.validate(ids[0], Pubkey::new_unique(), ids[3]).is_err());
        assert!(existing.validate(ids[0], ids[1], Pubkey::new_unique()).is_err());
        let mut output = json!({});
        existing.preserve_configuration(&mut output);
        assert_eq!(output, json!({"deadlineSlots":55,"maximumPolicyAgeSlots":120,
            "computeUnitLimit":1_200_000,"maximumHeapFrameBytes":131_072,
            "computeUnitPriceMicroLamports":123,"allowUnderlyingClosed":true}));
        existing.maximum_policy_age_slots = 151;
        assert!(existing.validate(ids[0], ids[1], ids[3]).is_err());
    }

    #[test]
    fn retained_duplicate_and_aliased_policy_bindings_are_rejected() {
        let (mut existing, ids) = retained_fixture();
        existing.products.push(ExistingProduct {
            product_id:existing.products[0].product_id.clone(),
            input_mint:existing.products[0].input_mint.clone(),
            policy:existing.products[0].policy.clone(), policy_version:32,
        });
        assert!(existing.validate(ids[0], ids[1], ids[3]).is_err());
        existing.products[1].product_id = format!("stkprd_{}", "b".repeat(32));
        assert!(existing.validate(ids[0], ids[1], ids[3]).is_err());
        existing.products[1].policy = Pubkey::new_unique().to_string();
        existing.validate(ids[0], ids[1], ids[3]).unwrap();
        existing.products[1].policy_version = 0;
        assert!(existing.validate(ids[0], ids[1], ids[3]).is_err());
    }

    #[test]
    fn deployment_identities_reject_system_aliases_and_duplicates() {
        let identities = [
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        assert!(validate_deployment_identities(identities).is_ok());
        assert!(validate_deployment_identities([
            identities[0],
            identities[0],
            identities[2],
            identities[3],
        ])
        .is_err());
        assert!(validate_deployment_identities([
            Pubkey::from_str("Vote111111111111111111111111111111111111111").unwrap(),
            identities[1],
            identities[2],
            identities[3],
        ])
        .is_err());
    }

    #[test]
    fn policy_publication_keeps_native_sell_and_economic_buy_cash() {
        let root = std::env::temp_dir().join(format!(
            "stockmesh-policy-cash-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let usdc = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        let wsol = "So11111111111111111111111111111111111111112";
        let stock = Pubkey::new_unique().to_string();
        let alternate = Pubkey::new_unique().to_string();
        let products=BTreeSet::from([stock.clone(),alternate.clone()]);
        let world=|input:&str,output:&str|json!({"markets":[{
            "venue":"orca_whirlpool","program":"program","pool":"pool","config":"oracle",
            "input_mint":input,"output_mint":output,"tick_arrays":["array"],"array_capacity":6,"clock":"clock"
        }],"execution_keys":[]});
        fs::write(
            root.join("funding.json"),
            serde_json::to_vec(&world(usdc,wsol))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("stock.json"),
            serde_json::to_vec(&world(wsol,&stock))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            lane_policy_inputs(&root, &["funding.json".into(), "stock.json".into()],usdc,&stock,&products).unwrap(),
            BTreeSet::from([wsol.to_string()])
        );
        fs::write(root.join("issuer.json"),serde_json::to_vec(&world(&stock,&alternate)).unwrap()).unwrap();
        assert_eq!(lane_policy_inputs(&root,&["funding.json".into(),"stock.json".into(),"issuer.json".into()],usdc,&alternate,&products).unwrap(),
            BTreeSet::from([wsol.to_string(),stock.clone()]));
        let mut mixed=world(wsol,&stock);
        let mut other=mixed["markets"][0].clone(); other["input_mint"]=json!(usdc);
        mixed["markets"].as_array_mut().unwrap().push(other);
        fs::write(
            root.join("mixed.json"),
            serde_json::to_vec(&mixed)
            .unwrap(),
        )
        .unwrap();
        assert!(lane_policy_inputs(&root, &["mixed.json".into()],wsol,&stock,&products).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
