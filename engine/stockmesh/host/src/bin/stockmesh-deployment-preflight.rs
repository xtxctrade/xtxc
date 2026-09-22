//! Read-only mainnet activation preflight for StockMesh prepared settlement.
//!
//! The tool owns no signing key and never creates or submits a transaction. It
//! verifies one coherent RPC bank containing the immutable settlement program,
//! frozen ALT and every ProductPolicy v2 account pinned by the deployment file.
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use skew_execution_host::{
    feed::{Account, Feed, Snapshot},
    lookup_registry::LookupRegistry,
    onebook_wire::{
        instrument_id, issuer_id, stock_policy_v2_address, STOCK_POLICY_V2_LEN, STOCK_POLICY_V2_TAG,
    },
    rpc::Rpc,
    provider::ProviderConfig,
    stockmesh_api::{validate_policy_lane_document, LaneManifest, Manifest},
    wallet_wire::frozen_lookup_table,
};
use solana_pubkey::Pubkey;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Component, Path, PathBuf},
    str::FromStr,
    time::Duration,
};

const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
const UPGRADEABLE_LOADER: &str = "BPFLoaderUpgradeab1e11111111111111111111111";

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Deployment {
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
    products: Vec<DeploymentProduct>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DeploymentProduct {
    product_id: String,
    input_mint: String,
    policy: String,
    policy_version: u64,
}

#[derive(Clone, PartialEq, Eq)]
struct ExpectedProduct {
    instrument: String,
    issuer: String,
    output_mint: Pubkey,
    rights_hash: [u8; 32],
}

fn key(value: &str) -> Result<Pubkey, String> {
    Pubkey::from_str(value).map_err(|_| format!("invalid public key: {value}"))
}

fn hex_32(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("expected 32-byte lowercase hex".into());
    }
    let mut output = [0u8; 32];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| "hex encoding".to_string())?;
    }
    if output == [0; 32] {
        return Err("zero hash".into());
    }
    Ok(output)
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn bounded_read(path: &Path, maximum: usize) -> Result<Vec<u8>, String> {
    let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if bytes.is_empty() || bytes.len() > maximum {
        return Err(format!("{} size bound", path.display()));
    }
    Ok(bytes)
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

fn final_cash_mint(base: &Path, lane: &LaneManifest) -> Result<Pubkey, String> {
    let world = lane.worlds.last().ok_or("lane has no final world")?;
    let path = bounded_relative(base, world)?;
    let bytes = bounded_read(&path, 64 * 1024)?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    let markets = value["markets"].as_array().ok_or("world markets")?;
    if markets.is_empty() || markets.len() > 8 {
        return Err("world market count".into());
    }
    let input = markets[0]["input_mint"]
        .as_str()
        .ok_or("world input mint")?;
    if markets.iter().any(|market| {
        market["input_mint"].as_str() != Some(input)
            || market["output_mint"].as_str() != Some(lane.output_mint.as_str())
    }) {
        return Err("final world mint pair differs across markets".into());
    }
    key(input)
}

fn account<'a>(snapshot: &'a Snapshot, address: &Pubkey) -> Result<&'a Account, String> {
    let encoded = address.to_string();
    let mut rows = snapshot
        .accounts
        .iter()
        .filter(|account| account.key == encoded);
    let row = rows.next().ok_or("preflight account missing")?;
    if rows.next().is_some() {
        return Err("preflight account ambiguous".into());
    }
    Ok(row)
}

fn integer(bytes: &[u8], offset: usize) -> Result<u64, String> {
    bytes
        .get(offset..offset + 8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| "ProductPolicy integer layout".into())
}

fn program_data_address(program: &Account) -> Result<Pubkey, String> {
    if program.owner != UPGRADEABLE_LOADER
        || !program.executable
        || program.data.len() != 36
        || program.data[..4] != 2u32.to_le_bytes()
    {
        return Err("settlement program must use the upgradeable loader Program layout".into());
    }
    Ok(Pubkey::new_from_array(
        program.data[4..36]
            .try_into()
            .map_err(|_| "settlement ProgramData address layout")?,
    ))
}

fn verify_immutable_program_data(account: &Account, expected: Pubkey) -> Result<String, String> {
    if account.key != expected.to_string()
        || account.owner != UPGRADEABLE_LOADER
        || account.executable
        || account.data.len() < 13
        || account.data[..4] != 3u32.to_le_bytes()
        || account.data[12] != 0
    {
        return Err("settlement ProgramData must be immutable".into());
    }
    Ok(digest(&account.data))
}

#[allow(clippy::too_many_arguments)]
fn verify_policy(
    account: &Account,
    address: Pubkey,
    settlement_program: Pubkey,
    authority: Pubkey,
    input_mint: Pubkey,
    expected: &ExpectedProduct,
    version: u64,
    slot: u64,
    maximum_age: u64,
    deadline_slots: u64,
    allow_underlying_closed: bool,
) -> Result<String, String> {
    let expected_address = stock_policy_v2_address(
        &settlement_program,
        &authority,
        &instrument_id(&expected.instrument)?,
        &input_mint,
        &expected.output_mint,
        &expected.rights_hash,
    );
    if address != expected_address
        || account.key != address.to_string()
        || account.owner != settlement_program.to_string()
        || account.executable
        || account.data.len() != STOCK_POLICY_V2_LEN
        || &account.data[..8] != STOCK_POLICY_V2_TAG
        || account.data[8..40] != authority.to_bytes()
        || account.data[40..72] != instrument_id(&expected.instrument)?
        || account.data[72..104] != issuer_id(&expected.issuer)?
        || account.data[104..136] != input_mint.to_bytes()
        || account.data[136..168] != expected.output_mint.to_bytes()
        || account.data[168..200] != expected.rights_hash
        || integer(&account.data, 200)? != version
    {
        return Err("ProductPolicy identity or immutable rights binding".into());
    }
    let updated = integer(&account.data, 208)?;
    let expires = integer(&account.data, 216)?;
    let flags = account.data[224];
    if updated > slot
        || slot.saturating_sub(updated) > maximum_age
        || expires
            < slot
                .checked_add(deadline_slots)
                .ok_or("deadline overflow")?
        || flags & !31 != 0
        || flags & 1 == 0
        || flags & 8 != 0
        || (!allow_underlying_closed && flags & 16 == 0)
    {
        return Err("ProductPolicy stale, expired, halted or ineligible".into());
    }
    Ok(digest(&account.data))
}

fn run() -> Result<(), String> {
    let args = env::args().collect::<Vec<_>>();
    if args.len() != 4 {
        return Err(
            "usage: stockmesh-deployment-preflight STOCKMESH_MANIFEST PREPARE_DEPLOYMENT OUTPUT"
                .into(),
        );
    }
    let manifest_path = PathBuf::from(&args[1]);
    let deployment_path = PathBuf::from(&args[2]);
    let output = PathBuf::from(&args[3]);
    if output.exists() {
        return Err("preflight output already exists".into());
    }
    let manifest_bytes = bounded_read(&manifest_path, 4 * 1024 * 1024)?;
    let deployment_bytes = bounded_read(&deployment_path, 256 * 1024)?;
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).map_err(|error| error.to_string())?;
    let deployment: Deployment =
        serde_json::from_slice(&deployment_bytes).map_err(|error| error.to_string())?;
    if manifest.network != "mainnet-beta"
        || !manifest.policy_integrity_required
        || deployment.schema != "skew.stockmesh.prepare-deployment/v2"
        || deployment.network != "mainnet-beta"
        || !(1..=150).contains(&deployment.deadline_slots)
        || !(1..=150).contains(&deployment.maximum_policy_age_slots)
        || deployment.compute_unit_limit == 0
        || deployment.compute_unit_limit > 1_400_000
        || !(32 * 1024..=256 * 1024).contains(&deployment.maximum_heap_frame_bytes)
        || !deployment.maximum_heap_frame_bytes.is_multiple_of(1024)
        || deployment.compute_unit_price_micro_lamports > 10_000_000
        || deployment.products.is_empty()
        // Three common accounts plus policies fit one coherent 100-account
        // read. Larger publication inventories are admitted by cohort.
        || deployment.products.len() > 97
    {
        return Err("preflight manifest bounds".into());
    }
    let settlement_program = key(&deployment.settlement_program)?;
    let authority = key(&deployment.policy_authority)?;
    let lookup_table = key(&deployment.lookup_table)?;
    let executor = key(&deployment.executor)?;
    let lookup_tables = LookupRegistry::parse(&deployment.lookup_table, &deployment.instrument_lookup_tables,
        &[settlement_program, authority, executor])?;
    let identities = [settlement_program, authority, lookup_table, executor];
    if identities
        .iter()
        .any(|identity| *identity == Pubkey::default())
        || identities
            .iter()
            .enumerate()
            .any(|(index, identity)| identities[index + 1..].contains(identity))
    {
        return Err("preflight deployment identities".into());
    }
    let base = manifest_path.parent().ok_or("manifest parent")?;
    let mut expected = BTreeMap::<(String, Pubkey), ExpectedProduct>::new();
    for bank in &manifest.banks {
        for lane in &bank.lanes {
            if !validate_policy_lane_document(lane) {
                return Err("preflight canonical product document".into());
            }
            let input_mint = final_cash_mint(base, lane)?;
            let product = ExpectedProduct {
                instrument: lane.instrument.clone(),
                issuer: lane.issuer.clone(),
                output_mint: key(&lane.output_mint)?,
                rights_hash: hex_32(&lane.rights_hash)?,
            };
            if let Some(previous) =
                expected.insert((lane.product_id.clone(), input_mint), product.clone())
            {
                if previous != product {
                    return Err("preflight duplicate product metadata differs".into());
                }
            }
        }
    }
    let mut deployed = BTreeMap::<(String, Pubkey), (Pubkey, u64)>::new();
    let mut policy_keys = BTreeSet::new();
    for product in &deployment.products {
        let input_mint = key(&product.input_mint)?;
        let policy = key(&product.policy)?;
        if product.policy_version == 0
            || !policy_keys.insert(policy)
            || deployed
                .insert(
                    (product.product_id.clone(), input_mint),
                    (policy, product.policy_version),
                )
                .is_some()
        {
            return Err("preflight duplicate deployment product".into());
        }
    }
    if expected.len() != deployed.len()
        || expected.keys().collect::<BTreeSet<_>>() != deployed.keys().collect::<BTreeSet<_>>()
    {
        return Err("preflight product coverage differs from StockMesh manifest".into());
    }
    lookup_tables.validate_coverage(expected.values().map(|product| product.instrument.as_str()))?;
    let mut table_keys = BTreeSet::new();
    for product in expected.values() { table_keys.insert(lookup_tables.for_instrument(&product.instrument)?); }
    // One coherent read still covers the complete activation cohort. Splitting
    // this into unrelated reads would weaken the existing activation contract.
    if 2 + table_keys.len() + policy_keys.len() > 100 {
        return Err("activation cohort exceeds coherent account bound; partition the deployment".into());
    }
    let rpc = match env::var_os("SKEW_PROVIDER_PROFILE") {
        Some(path) => {
            if env::var_os("SKEW_RPC_URL").is_some() { return Err("ambiguous provider configuration".into()); }
            ProviderConfig::load(Path::new(&path))?.connect()?
        }
        None => Rpc::pinned(env::var("SKEW_RPC_URL").map_err(|_| "provider profile missing")?, MAINNET_GENESIS.into())?,
    };
    let probe = rpc.call(
        "getMultipleAccounts",
        json!([[settlement_program.to_string()], {"encoding":"base64","commitment":"confirmed"}]),
    )?;
    let probe_feed = Feed::new(
        vec![settlement_program.to_string()],
        Duration::from_secs(5),
        4 * 1024 * 1024,
    )?;
    let probe_snapshot = probe_feed.publish(&probe)?;
    let program_data = program_data_address(account(&probe_snapshot, &settlement_program)?)?;
    let mut keys = BTreeSet::from([settlement_program, program_data]);
    keys.extend(&table_keys);
    keys.extend(policy_keys);
    let encoded_keys = keys.iter().map(ToString::to_string).collect::<Vec<_>>();
    let response = rpc.call(
        "getMultipleAccounts",
        json!([encoded_keys,{"encoding":"base64","commitment":"confirmed","minContextSlot":probe_snapshot.slot}]),
    )?;
    let feed = Feed::new(encoded_keys, Duration::from_secs(5), 4 * 1024 * 1024)?;
    let snapshot = feed.publish(&response)?;
    let observed_program_data = program_data_address(account(&snapshot, &settlement_program)?)?;
    if observed_program_data != program_data {
        return Err("settlement ProgramData changed during preflight".into());
    }
    let program_data_hash =
        verify_immutable_program_data(account(&snapshot, &program_data)?, program_data)?;
    let mut table_inventory = BTreeMap::new();
    for table_key in &table_keys {
        let table = frozen_lookup_table(&snapshot, *table_key)?;
        table_inventory.insert(table_key.to_string(), json!({
            "addresses": table.addresses.len(), "frozen": true,
            "accountDataSha256": digest(&account(&snapshot, table_key)?.data),
        }));
    }
    let mut policy_hashes = BTreeMap::new();
    for (binding, expected_product) in &expected {
        let (policy, version) = deployed
            .get(binding)
            .ok_or("preflight deployed product missing")?;
        let hash = verify_policy(
            account(&snapshot, policy)?,
            *policy,
            settlement_program,
            authority,
            binding.1,
            expected_product,
            *version,
            snapshot.slot,
            deployment.maximum_policy_age_slots,
            deployment.deadline_slots,
            deployment.allow_underlying_closed,
        )?;
        policy_hashes.insert(format!("{}:{}", binding.0, binding.1), hash);
    }
    let evidence = serde_json::to_vec_pretty(&json!({
        "schema":if deployment.instrument_lookup_tables.is_empty() {"skew.stockmesh.deployment-preflight/v1"} else {"skew.stockmesh.deployment-preflight/v2"},
        "status":"ONCHAIN_IDENTITIES_VERIFIED_READ_ONLY",
        "network":"mainnet-beta",
        "slot":snapshot.slot,
        "executionBankHash":digest(&snapshot.hash),
        "sourceManifestSha256":digest(&manifest_bytes),
        "prepareDeploymentSha256":digest(&deployment_bytes),
        "settlementProgram":settlement_program.to_string(),
        "settlementProgramData":program_data.to_string(),
        "settlementProgramDataSha256":program_data_hash,
        "settlementProgramImmutable":true,
        "policyAuthority":authority.to_string(),
        "lookupTable":lookup_table.to_string(),
        "lookupTableFrozen":table_inventory.get(&lookup_table.to_string()).map(|value| &value["frozen"]),
        "lookupTableAddresses":table_inventory.get(&lookup_table.to_string()).map(|value| &value["addresses"]),
        "instrumentLookupTables":deployment.instrument_lookup_tables,
        "lookupTableInventory":table_inventory,
        "executor":executor.to_string(),
        "productPolicies":policy_hashes,
        "maximumHeapFrameBytes":deployment.maximum_heap_frame_bytes,
        "computeUnitLimit":deployment.compute_unit_limit,
        "signedTransactions":0,
        "submittedTransactions":0,
    }))
    .map_err(|error| error.to_string())?;
    fs::write(&output, &evidence).map_err(|error| error.to_string())?;
    println!(
        "{}",
        json!({"status":"ONCHAIN_IDENTITIES_VERIFIED_READ_ONLY","slot":snapshot.slot,"products":expected.len(),"output":output,"signedTransactions":0,"submittedTransactions":0})
    );
    Ok(())
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

    #[test]
    fn program_data_must_be_immutable() {
        let address = Pubkey::new_unique();
        let mut bytes = vec![0u8; 13];
        bytes[..4].copy_from_slice(&3u32.to_le_bytes());
        let immutable = Account {
            key: address.to_string(),
            owner: UPGRADEABLE_LOADER.into(),
            executable: false,
            lamports: 1,
            data: bytes.clone(),
        };
        assert!(verify_immutable_program_data(&immutable, address).is_ok());
        bytes[12] = 1;
        let mutable = Account {
            data: bytes,
            ..immutable
        };
        assert!(verify_immutable_program_data(&mutable, address).is_err());
    }
}
