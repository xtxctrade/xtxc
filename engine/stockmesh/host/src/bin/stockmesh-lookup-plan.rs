//! Deterministic, keyless ALT inventory for the real StockMesh lane manifests.
//! No network, private keys, transactions, policies or production writes.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use skew_execution_host::{
    onebook_wire::{instrument_id, stock_policy_v2_address},
    stockmesh_api::{validate_policy_lane_document, Manifest},
    policy_bindings::policy_inputs,
    world::WorldConfig,
};
use solana_pubkey::Pubkey;
use std::{collections::{BTreeMap, BTreeSet}, env, fs, path::{Component, Path}, str::FromStr};

fn key(value: &str) -> Result<Pubkey, String> { Pubkey::from_str(value).map_err(|_| "lookup plan public key".into()) }
fn hash(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
fn read(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "lookup plan metadata")?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > max { return Err("lookup plan file bound".into()); }
    let bytes = fs::read(path).map_err(|_| "lookup plan read")?;
    if bytes.len() as u64 != metadata.len() { return Err("lookup plan input changed".into()); }
    Ok(bytes)
}
fn rights(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 || !value.bytes().all(|c| c.is_ascii_hexdigit()) { return Err("lookup plan rights hash".into()); }
    let mut result = [0; 32];
    for (i, byte) in result.iter_mut().enumerate() { *byte = u8::from_str_radix(&value[i*2..i*2+2], 16).map_err(|_| "lookup plan rights encoding")?; }
    Ok(result)
}

#[derive(Default, Clone)]
struct Cohort { instruments: Vec<String>, addresses: BTreeSet<Pubkey> }

fn pack(inventory: &BTreeMap<String, BTreeSet<Pubkey>>) -> Result<Vec<Cohort>, String> {
    if inventory.is_empty() || inventory.len() > 128 { return Err("lookup plan instrument bound".into()); }
    let mut rows = inventory.iter().collect::<Vec<_>>();
    // Large first, then deterministic ticker order. Choose the cohort with the
    // most address reuse that still fits. No pool/authority/address is invented.
    rows.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
    let mut groups: Vec<Cohort> = Vec::new();
    for (instrument, addresses) in rows {
        instrument_id(instrument)?;
        if addresses.is_empty() || addresses.len() > 256 { return Err(format!("lookup plan instrument capacity: {instrument}")); }
        let best = groups.iter().enumerate().filter_map(|(index, group)| {
            let growth = addresses.difference(&group.addresses).count();
            (group.addresses.len() + growth <= 256).then_some((growth, index))
        }).min();
        let index = if let Some((_, index)) = best { index } else { groups.push(Cohort::default()); groups.len() - 1 };
        groups[index].addresses.extend(addresses);
        groups[index].instruments.push(instrument.clone());
    }
    for group in &mut groups { group.instruments.sort(); }
    groups.sort_by(|a, b| a.instruments.cmp(&b.instruments));
    Ok(groups)
}

fn table_inventory(
    stable: &BTreeMap<String, BTreeSet<Pubkey>>,
    arrays: &BTreeMap<String, BTreeSet<Pubkey>>,
    include_horizon: bool,
) -> Result<BTreeMap<String, BTreeSet<Pubkey>>, String> {
    if stable.keys().ne(arrays.keys()) { return Err("lookup plan horizon coverage".into()); }
    let mut inventory=stable.clone();
    if include_horizon {
        for (instrument, window) in arrays {
            inventory.get_mut(instrument).ok_or("lookup plan horizon instrument")?.extend(window);
        }
    }
    Ok(inventory)
}

fn run() -> Result<(), String> {
    let args = env::args().collect::<Vec<_>>();
    let include_horizon = args.len()==4 && args[3]=="--include-configured-horizon";
    if args.len()!=3 && !include_horizon { return Err("usage: stockmesh-lookup-plan MANIFEST PUBLIC_DEPLOYMENT [--include-configured-horizon]".into()); }
    let manifest_path = Path::new(&args[1]);
    let manifest_bytes = read(manifest_path, 4 * 1024 * 1024)?;
    let deployment_bytes = read(Path::new(&args[2]), 256 * 1024)?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes).map_err(|_| "lookup plan manifest")?;
    let deployment: Value = serde_json::from_slice(&deployment_bytes).map_err(|_| "lookup plan deployment")?;
    if manifest.network != "mainnet-beta" || !manifest.policy_integrity_required
        || manifest.banks.is_empty() || manifest.banks.len() > 128
        || deployment["network"] != "mainnet-beta" || deployment["schema"] != "skew.stockmesh.prepare-deployment/v2" {
        return Err("lookup plan network or manifest bound".into());
    }
    let program = key(deployment["settlementProgram"].as_str().ok_or("lookup plan settlement")?)?;
    let authority = key(deployment["policyAuthority"].as_str().ok_or("lookup plan policy authority")?)?;
    if program == Pubkey::default() || authority == Pubkey::default() || program == authority { return Err("lookup plan authority identity".into()); }
    let base = manifest_path.parent().ok_or("lookup plan source root")?.canonicalize().map_err(|_| "lookup plan source root")?;
    let mut stable = BTreeMap::<String, BTreeSet<Pubkey>>::new();
    let mut dynamic = BTreeMap::<String, BTreeSet<Pubkey>>::new();
    let mut policies = BTreeMap::<String, Value>::new();
    let mut source_hashes = BTreeMap::new();
    let mut lanes = 0;
    for bank in &manifest.banks {
        let products=bank.lanes.iter().map(|lane|lane.output_mint.clone()).collect::<BTreeSet<_>>();
        for lane in &bank.lanes {
            lanes += 1;
            if lanes > 256 || lane.instrument != bank.name || !validate_policy_lane_document(lane)
                || lane.worlds.is_empty() || lane.worlds.len() > 3 { return Err("lookup plan lane identity/bounds".into()); }
            let addresses = stable.entry(lane.instrument.clone()).or_default();
            let arrays = dynamic.entry(lane.instrument.clone()).or_default();
            let mut final_cash = None;
            let mut worlds=Vec::new();
            for (index, name) in lane.worlds.iter().enumerate() {
                let relative = Path::new(name);
                if relative.is_absolute() || relative.components().any(|c| !matches!(c, Component::Normal(_))) { return Err("lookup plan world path".into()); }
                let path = base.join(relative).canonicalize().map_err(|_| "lookup plan world path")?;
                if !path.starts_with(&base) { return Err("lookup plan world outside source root".into()); }
                let bytes = read(&path, 64 * 1024)?;
                source_hashes.insert(name.clone(), hash(&bytes));
                let world: WorldConfig = serde_json::from_slice(&bytes).map_err(|_| "lookup plan world")?;
                for address in world.keys()? { addresses.insert(key(&address)?); }
                for market in &world.markets {
                    arrays.extend(market.tick_arrays.iter().map(|a| key(a)).collect::<Result<Vec<_>,_>>()?);
                }
                if index + 1 == lane.worlds.len() {
                    let first = world.markets.first().ok_or("lookup plan final market")?;
                    if world.markets.iter().any(|m| m.input_mint != first.input_mint || m.output_mint != lane.output_mint) {
                        return Err("lookup plan final product cash".into());
                    }
                    final_cash = Some(key(&first.input_mint)?);
                }
                worlds.push(world);
            }
            let root=match lane.input_symbol.as_str() {
                "USDC"=>"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "SOL"=>"So11111111111111111111111111111111111111112",
                _=>return Err("lookup plan input symbol".into()),
            };
            let inputs=policy_inputs(root,&lane.output_mint,&worlds,&products)?;
            if Some(key(&inputs.sell_cash)?)!=final_cash {return Err("lookup plan reverse cash mismatch".into());}
            for input in BTreeSet::from([key(&inputs.buy_cash)?,key(&inputs.sell_cash)?]) {
            let policy = stock_policy_v2_address(&program, &authority, &instrument_id(&lane.instrument)?, &input, &key(&lane.output_mint)?, &rights(&lane.rights_hash)?);
            addresses.extend([policy, program, key(&lane.output_mint)?, key(&lane.token_program)?]);
            let binding = format!("{}:{input}", lane.product_id);
            let entry = json!({"productId":lane.product_id,"instrument":lane.instrument,"inputMint":input.to_string(),"outputMint":lane.output_mint,"policy":policy.to_string()});
            if policies.insert(binding, entry.clone()).is_some_and(|prior| prior != entry) { return Err("lookup plan policy identity conflict".into()); }
            }
        }
    }
    for (instrument, arrays) in &dynamic {
        // Mutable tick/bin windows are inline-account candidates. A frozen
        // table cannot promise every future window or be silently extended.
        stable.get_mut(instrument).ok_or("lookup plan instrument")?.retain(|a| !arrays.contains(a));
    }
    // Frozen tables can contain observed array addresses without treating their
    // mutable contents as stable. A future window not in this table stays inline
    // and must independently pass exact message/simulation admission. This is
    // a candidate publication plan, never permission to mutate an existing ALT.
    let table_keys=table_inventory(&stable,&dynamic,include_horizon)?;
    let groups = pack(&table_keys)?;
    let union = stable.values().flat_map(|keys| keys.iter().copied()).collect::<BTreeSet<_>>();
    let lookup_union=table_keys.values().flat_map(|keys|keys.iter().copied()).collect::<BTreeSet<_>>();
    let mut configured = BTreeMap::new();
    for row in deployment["products"].as_array().ok_or("lookup plan configured policies")? {
        let product = row["productId"].as_str().ok_or("lookup plan configured product")?;
        let input = key(row["inputMint"].as_str().ok_or("lookup plan configured policy cash")?)?;
        let policy = key(row["policy"].as_str().ok_or("lookup plan configured policy address")?)?;
        if configured.insert(format!("{product}:{input}"), policy.to_string()).is_some() { return Err("lookup plan duplicate configured policy".into()); }
    }
    let mut new_bindings = Vec::new();
    for (binding, policy) in &policies {
        match configured.get(binding) {
            Some(existing) if policy["policy"].as_str() != Some(existing.as_str()) => return Err("lookup plan configured policy address mismatch".into()),
            None => new_bindings.push(binding.clone()),
            _ => {}
        }
    }
    let mut assignments = BTreeMap::new();
    let cohorts = groups.iter().map(|group| {
        let addresses = group.addresses.iter().map(ToString::to_string).collect::<Vec<_>>();
        let id = hash(&serde_json::to_vec(&addresses).expect("public key strings"));
        for instrument in &group.instruments { assignments.insert(instrument.clone(), id.clone()); }
        json!({"id":id,"instruments":group.instruments,"addressCount":addresses.len(),"addresses":addresses,
            "existingTableAddress":Value::Null,"publicationRequired":true})
    }).collect::<Vec<_>>();
    let instruments = stable.iter().map(|(name, keys)| json!({"instrument":name,"stableAddresses":keys.len(),
        "dynamicArrayAddresses":dynamic[name].iter().map(ToString::to_string).collect::<Vec<_>>(),
        "actualMessageAndCuVerified":false})).collect::<Vec<_>>();
    println!("{}", serde_json::to_string_pretty(&json!({
        "schema":"skew.stockmesh.lookup-plan/v1","status":"UNPUBLISHED_CONFIGURATION_PLAN",
        "sourceManifestSha256":hash(&manifest_bytes),"sourceDeploymentSha256":hash(&deployment_bytes),
        "worldHashes":source_hashes,"instrumentCohorts":assignments,"cohorts":cohorts,"instruments":instruments,
        "productPolicies":policies,"laneCount":lanes,"maxTableAddresses":256,
        "uniqueStableAddresses":union.len(),"newPolicyBindings":new_bindings,
        "addressMode":if include_horizon{"STABLE_AND_CONFIGURED_ARRAY_HORIZON"}else{"STABLE_ONLY"},
        "uniqueLookupAddresses":lookup_union.len(),
        "walletSpecificAccounts":"DERIVE_AT_PREPARE_NOT_GLOBAL_ALT",
        "dynamicArrays":if include_horizon{"CONFIGURED_ADDRESSES_ONLY_NEW_WINDOWS_INLINE_EXACT_ADMISSION_REQUIRED"}else{"FINAL_COHERENT_BANK_AND_MESSAGE_BOUNDS_REQUIRED"},
        "signedTransactions":0,"submittedTransactions":0
    })).map_err(|_| "lookup plan JSON")?);
    Ok(())
}
fn main() { if let Err(error) = run() { eprintln!("{error}"); std::process::exit(1); } }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cohorts_bound_universe_without_dropping_shared_or_distinct_accounts() {
        let common = Pubkey::new_unique();
        let mut inventory = BTreeMap::new();
        for i in 0..47 {
            let mut keys = BTreeSet::from([common]);
            keys.extend((0..20).map(|_| Pubkey::new_unique()));
            inventory.insert(format!("S{i:02}"), keys);
        }
        let groups = pack(&inventory).unwrap();
        assert_eq!(groups.len(), 4);
        let mut accounted = BTreeSet::new();
        for group in groups {
            assert!(group.addresses.len() <= 256);
            for instrument in &group.instruments {
                assert!(accounted.insert(instrument.clone()));
                assert!(inventory[instrument].is_subset(&group.addresses));
            }
        }
        assert_eq!(accounted.len(), 47);
    }
    #[test]
    fn unrepresentable_instrument_is_not_silently_split_or_truncated() {
        let too_big = (0..257).map(|_| Pubkey::new_unique()).collect();
        assert!(pack(&BTreeMap::from([("NVDA".into(), too_big)])).is_err());
        assert!(pack(&BTreeMap::new()).is_err());
        let keys = BTreeSet::from([Pubkey::new_unique()]);
        let groups = pack(&BTreeMap::from([("BRK.B".into(), keys.clone()), ("BRK.A".into(), keys.clone())])).unwrap();
        assert_eq!(groups.len(), 1); assert_eq!(groups[0].addresses, keys);
        assert_eq!(groups[0].instruments, ["BRK.A", "BRK.B"]);
    }

    #[test]
    fn configured_horizon_is_explicit_and_preserves_every_stable_address() {
        let common=Pubkey::new_unique();let a=Pubkey::new_unique();let b=Pubkey::new_unique();
        let stable=BTreeMap::from([("AAPL".into(),BTreeSet::from([common,a])),("GOOGL".into(),BTreeSet::from([common,b]))]);
        let window=Pubkey::new_unique();
        let arrays=BTreeMap::from([("AAPL".into(),BTreeSet::from([window])),("GOOGL".into(),BTreeSet::from([window]))]);
        assert_eq!(table_inventory(&stable,&arrays,false).unwrap(),stable);
        let combined=table_inventory(&stable,&arrays,true).unwrap();
        for (name,original) in &stable {
            assert!(original.is_subset(&combined[name]));
            assert!(combined[name].contains(&window));
        }
        let groups=pack(&combined).unwrap();
        assert_eq!(groups.len(),1);assert_eq!(groups[0].addresses.len(),4);
    }

    #[test]
    fn configured_horizon_does_not_silently_truncate_or_add_unknown_instruments() {
        let stable=BTreeMap::from([("SPY".into(),BTreeSet::from([Pubkey::new_unique()]))]);
        let arrays=BTreeMap::from([("SPY".into(),(0..256).map(|_|Pubkey::new_unique()).collect())]);
        assert!(pack(&table_inventory(&stable,&arrays,true).unwrap()).is_err());
        assert!(pack(&table_inventory(&stable,&arrays,false).unwrap()).is_ok());
        let unknown=BTreeMap::from([("QQQ".into(),BTreeSet::from([Pubkey::new_unique()]))]);
        assert!(table_inventory(&stable,&unknown,true).is_err());
    }
}
