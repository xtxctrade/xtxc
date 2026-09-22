//! Opt-in offline measurements of native stock messages using recorded pool
//! accounts. Wallet accounts and unpublished ALT state are explicit fixtures.
//! This does NOT simulate SBF, authorize a wallet, or prove live admission.
use crate::{
    exposure_wire::{compile_direct_exposure, DirectExposureSpec, DirectProduct},
    feed::{Account, Feed, Snapshot},
    native_wire,
    onebook_wire::{compile_reverse_stock_fill, compile_unsigned_v0, instrument_id, stock_policy_v2_address, MeshProduct},
    stockmesh_api::{validate_policy_lane_document, LaneManifest, Manifest},
    swap_wire::{self, AccountView, SwapGraph, TokenAsset},
    wallet_wire::WalletSetup,
    world::{NativeSwapProposal, WorldConfig},
    Result,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use solana_instruction::Instruction;
use solana_message::{AddressLookupTableAccount, VersionedMessage};
use solana_pubkey::{pubkey, Pubkey};
use std::{collections::BTreeSet, env, fs, path::{Component, Path, PathBuf}, time::Duration};

const ROOT: &str = "/srv/skew/stockmesh-direct-node-20260920";
const SYSTEM: Pubkey = Pubkey::new_from_array([0; 32]);
pub(crate) const WALLET: Pubkey = Pubkey::new_from_array([0x51; 32]);
const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
const ATA: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const COMPUTE: Pubkey = pubkey!("ComputeBudget111111111111111111111111111111");

pub(crate) fn hash(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
pub(crate) fn key(text: &str) -> Result<Pubkey> { text.parse().map_err(|_| "inventory public key".into()) }
fn read(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path).map_err(|_| "inventory file metadata")?;
    if !meta.is_file() || meta.len() > maximum || meta.len() == 0 { return Err("inventory file bounds".into()); }
    let bytes = fs::read(path).map_err(|_| "inventory file read")?;
    if bytes.len() as u64 != meta.len() { return Err("inventory file changed".into()); }
    Ok(bytes)
}
fn relative(base: &Path, text: &str) -> Result<PathBuf> {
    let path = Path::new(text);
    if path.is_absolute() || path.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err("inventory relative path".into());
    }
    Ok(base.join(path))
}
pub(crate) fn hex32(text: &str) -> Result<[u8;32]> {
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) { return Err("inventory hash".into()); }
    let mut bytes = [0;32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i*2..i*2+2],16).map_err(|_| "inventory hash encoding")?;
    }
    Ok(bytes)
}
pub(crate) fn view<'a>(bank: &'a Snapshot, address: &Pubkey) -> Result<AccountView<'a>> {
    let row = bank.accounts.iter().find(|a| a.key == address.to_string()).ok_or_else(|| format!("inventory account absent: {address}"))?;
    Ok(AccountView {owner:key(&row.owner)?,executable:row.executable,data:&row.data})
}
pub(crate) fn fixture_account(bank: &mut Snapshot, address: Pubkey, owner: Pubkey, data: Vec<u8>, executable: bool) -> Result<()> {
    if bank.accounts.iter().any(|a| a.key == address.to_string()) { return Err("fixture would overwrite captured account".into()); }
    bank.accounts.push(Account {key:address.to_string(),owner:owner.to_string(),lamports:1_000_000_000_000,data,executable});
    Ok(())
}
pub(crate) fn token_data(asset: TokenAsset, balance: u64) -> Vec<u8> {
    // Compiler-layout fixture, not a claim about live Token-2022 ATA extensions.
    let mut data=vec![0;165];
    data[..32].copy_from_slice(asset.mint.as_ref());
    data[32..64].copy_from_slice(WALLET.as_ref());
    data[64..72].copy_from_slice(&balance.to_le_bytes());
    data[108]=1;
    data
}
pub(crate) fn nonce_data() -> Vec<u8> {
    let mut data=vec![0;64]; data[..8].copy_from_slice(b"SKEWSEQ1");
    data[8..40].copy_from_slice(WALLET.as_ref()); data
}

fn measure(
    original: &Snapshot, world: &WorldConfig, lane: &LaneManifest,
    program: Pubkey, authority: Pubkey, table: &AddressLookupTableAccount,
    sell: bool, first_use: bool,
) -> Result<Value> {
    // Direct lanes only. Never stitch capture slots into a funding or
    // multi-issuer bank; those complete graphs require a coherent capture.
    if world.markets.len() != 1 { return Err("multi-pool graph requires joint inventory".into()); }
    let configured = &world.markets[0];
    let market = if sell { configured.reversed() } else { configured.clone() };
    let input_mint = key(&market.input_mint)?;
    let output_mint = key(&market.output_mint)?;
    let amount = if sell { 10u64.checked_pow(u32::from(lane.output_decimals)).ok_or("inventory unit range")? }
        else { 100_000_000 };
    let fixture_balance=amount.checked_mul(100).ok_or("fixture balance range")?;
    let deadline_slot=original.slot.checked_add(40).ok_or("inventory deadline range")?;
    let output = market.compile_exact_pair(original, &market.input_mint, &market.output_mint)?.quote(amount)?;
    if output == 0 { return Err("inventory quote empty".into()); }
    let source=native_wire::wallet_asset(WALLET,input_mint,original)?;
    let destination=native_wire::wallet_asset(WALLET,output_mint,original)?;
    let nonce=Pubkey::find_program_address(&[b"stocklana",WALLET.as_ref()],&program).0;
    let policy=stock_policy_v2_address(&program,&authority,&instrument_id(&lane.instrument)?,
        &key(&configured.input_mint)?,&key(&lane.output_mint)?,&hex32(&lane.rights_hash)?);
    let mut bank=original.clone();
    fixture_account(&mut bank,WALLET,SYSTEM,vec![],false)?;
    let mut mocked_programs=Vec::new();
    for address in [ATA,program] {
        if !bank.accounts.iter().any(|a| a.key == address.to_string()) {
            fixture_account(&mut bank,address,pubkey!("BPFLoaderUpgradeab1e11111111111111111111111"),vec![],true)?;
            mocked_programs.push(address.to_string());
        }
    }
    let wrap=(!sell && input_mint==WSOL).then_some(amount);
    let source_absent=first_use && wrap.is_some();
    fixture_account(&mut bank,source.token,if source_absent {SYSTEM}else{source.token_program},
        if source_absent {vec![]}else{token_data(source,fixture_balance)},false)?;
    fixture_account(&mut bank,destination.token,if first_use {SYSTEM}else{destination.token_program},
        if first_use {vec![]}else{token_data(destination,0)},false)?;
    fixture_account(&mut bank,nonce,if first_use {SYSTEM}else{program},if first_use {vec![]}else{nonce_data()},false)?;
    let setup=WalletSetup::plan(WALLET,&[source,destination],wrap,|address|view(&bank,address))?
        .with_nonce(program,0,|address|view(&bank,address))?;
    // Fixture post-setup bytes for compiler measurement only, not simulated
    // state. Exact live preparation still requires observed setup simulation.
    for (asset,balance) in [(source,fixture_balance),(destination,0)] {
        let row=bank.accounts.iter_mut().find(|a|a.key==asset.token.to_string()).ok_or("fixture ATA")?;
        row.owner=asset.token_program.to_string(); row.data=token_data(asset,balance);
    }
    let row=bank.accounts.iter_mut().find(|a|a.key==nonce.to_string()).ok_or("fixture nonce")?;
    row.owner=program.to_string();row.data=nonce_data();
    let proposal=NativeSwapProposal {market,stage:1,product_id:Some(lane.product_id.clone()),input_atoms:amount,expected_output_atoms:output};
    let minimum=u64::try_from(u128::from(output).checked_mul(9970).ok_or("inventory floor overflow")?/10000).map_err(|_|"inventory floor range")?;
    if minimum==0 { return Err("inventory output floor".into()); }
    let settlement=if sell {
        let graph=native_wire::lower_native_graph(SwapGraph {owner:WALLET,sequence:0,input_atoms:amount,
            minimum_output_atoms:minimum,deadline_slot,input:source,output:destination,intermediates:vec![],legs:vec![]},
            &[proposal],&bank)?;
        let graph=swap_wire::compile_swap_graph(program,&graph,|address|view(&bank,address))?;
        compile_reverse_stock_fill(program,policy,1,100,false,graph)?
    } else {
        compile_direct_exposure(program,DirectExposureSpec {buyer:WALLET,buyer_nonce:nonce,input:source,
            buyer_sequence:0,input_atoms:amount,minimum_exposure_q32:1,deadline_slot,
            maximum_policy_age:100,allow_underlying_closed:false,products:vec![DirectProduct {
                product_id:lane.product_id.clone(),minimum_output_atoms:minimum,product:MeshProduct {
                    policy,claim:None,destination:destination.token,mint:destination.mint,
                    token_program:destination.token_program,model:match lane.exposure_model.as_str() {
                        "FIXED_RATIONAL"=>0,"TOKEN_2022_SCALED_UI"=>1,_=>return Err("inventory exposure model".into())},
                    conservative_bps:lane.conservative_bps,policy_version:1,
                    numerator:lane.exposure_numerator,denominator:lane.exposure_denominator,
                }}]},&[proposal],&bank)?
    };
    let opcode=settlement.data[0];
    let mut compute_data=vec![2];compute_data.extend_from_slice(&1_400_000u32.to_le_bytes());
    let mut instructions=vec![Instruction {program_id:COMPUTE,accounts:vec![],data:compute_data}];
    instructions.extend_from_slice(setup.instructions()); instructions.push(settlement);
    let message=compile_unsigned_v0(WALLET,&instructions,std::slice::from_ref(table),[0x53;32])?;
    let decoded: VersionedMessage=bincode::deserialize(&message).map_err(|_|"inventory message")?;
    let VersionedMessage::V0(v0)=decoded else {return Err("inventory expected v0".into())};
    let signatures=usize::from(v0.header.num_required_signatures);
    if signatures!=1 || v0.account_keys[0]!=WALLET {return Err("inventory unexpected signer".into());}
    let loaded=v0.address_table_lookups.iter().map(|l|l.readonly_indexes.len()+l.writable_indexes.len()).sum::<usize>();
    Ok(json!({"opcode":opcode,"signedEnvelopeBytes":message.len()+65,"messageBytes":message.len(),
        "accountLocks":v0.account_keys.len()+loaded,"staticAccounts":v0.account_keys.len(),"lookupAccounts":loaded,
        "instructions":instructions.len(),"messageSha256":hash(&message),"walletSetupInstructions":setup.instructions().len(),
        "capturedSlot":original.slot,"capturedInputAtoms":amount.to_string(),"capturedQuotedOutputAtoms":output.to_string(),
        "fixtureProgramAccounts":mocked_programs,"withoutLookup":compile_unsigned_v0(WALLET,&instructions,&[],[0x53;32]).map(|v|json!({"signedEnvelopeBytes":v.len()+65})).unwrap_or_else(|e|json!({"rejected":e})),
        "cuMeasured":false,"simulated":false,"liveAdmission":false}))
}

#[test]
#[ignore = "requires Cherry recorded native-world inventory, never network"]
fn recorded_stock_complete_message_inventory() {
    assert!(cfg!(target_os="linux"),"Cherry only");
    let config_path=PathBuf::from(env::var("SKEW_WIRE_INVENTORY_CONFIG").expect("inventory config"));
    assert!(config_path.starts_with(ROOT));
    let config:Value=serde_json::from_slice(&read(&config_path,16*1024).unwrap()).unwrap();
    let bounded_path=|name:&str| {let p=PathBuf::from(config[name].as_str().unwrap());
        assert!(p.starts_with(ROOT)&&!p.components().any(|c|matches!(c,Component::ParentDir)));p};
    let manifest_path=bounded_path("manifest");let deployment_path=bounded_path("deployment");
    let lookup_path=bounded_path("lookupPlan");let output=bounded_path("output");
    assert!(!output.exists(),"do not overwrite previous evidence");
    let manifest_bytes=read(&manifest_path,4*1024*1024).unwrap();
    let manifest:Manifest=serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(manifest.network,"mainnet-beta");assert!(manifest.policy_integrity_required);
    assert!((1..=128).contains(&manifest.banks.len()));
    let deployment_bytes=read(&deployment_path,256*1024).unwrap();
    let deployment:Value=serde_json::from_slice(&deployment_bytes).unwrap();
    let program=key(deployment["settlementProgram"].as_str().unwrap()).unwrap();
    let authority=key(deployment["policyAuthority"].as_str().unwrap()).unwrap();
    let lookup_bytes=read(&lookup_path,4*1024*1024).unwrap();
    let lookup:Value=serde_json::from_slice(&lookup_bytes).unwrap();
    assert_eq!(lookup["sourceManifestSha256"],hash(&manifest_bytes));
    assert_eq!(lookup["sourceDeploymentSha256"],hash(&deployment_bytes));
    let capture_dirs=config["captureDirectories"].as_array().unwrap().iter().map(|v| {
        let p=PathBuf::from(v.as_str().unwrap());assert!(p.starts_with(ROOT)&&!p.components().any(|c|matches!(c,Component::ParentDir)));p
    }).collect::<Vec<_>>();
    assert!((1..=4).contains(&capture_dirs.len()));
    let mut rows=Vec::new();
    for bank in &manifest.banks {
        assert!(bank.name.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'.'||b==b'-'));
        let captured=capture_dirs.iter().map(|d|d.join(format!("{}-complete.json",bank.name))).find(|p|p.exists());
        let Some(captured)=captured else {rows.push(json!({"instrument":bank.name,"status":"CAPTURE_MISSING"}));continue};
        let captured_bytes=read(&captured,8*1024*1024).unwrap();
        let value:Value=serde_json::from_slice(&captured_bytes).unwrap();
        let keys:Vec<String>=serde_json::from_value(value["keys"].clone()).unwrap();
        let feed=Feed::new(keys,Duration::from_secs(60),4*1024*1024).unwrap();
        let original=match feed.publish(&value["response"]) {Ok(bank)=>bank,Err(error)=> {
            rows.push(json!({"instrument":bank.name,"status":"CAPTURE_REJECTED","error":error}));continue}};
        let cohort_id=lookup["instrumentCohorts"][&bank.name].as_str().unwrap();
        let cohort=lookup["cohorts"].as_array().unwrap().iter().find(|c|c["id"]==cohort_id).unwrap();
        let addresses=cohort["addresses"].as_array().unwrap().iter().map(|v|key(v.as_str().unwrap()).unwrap()).collect::<Vec<_>>();
        assert!(addresses.len()<=256 && addresses.iter().collect::<BTreeSet<_>>().len()==addresses.len());
        let table=AddressLookupTableAccount {key:Pubkey::new_from_array(hex32(cohort_id).unwrap()),addresses};
        for lane in &bank.lanes {
            assert!(validate_policy_lane_document(lane));
            if lane.worlds.len()!=1 {continue;}
            let world_bytes=read(&relative(manifest_path.parent().unwrap(),&lane.worlds[0]).unwrap(),64*1024).unwrap();
            assert_eq!(lookup["worldHashes"][&lane.worlds[0]],hash(&world_bytes));
            let world:WorldConfig=serde_json::from_slice(&world_bytes).unwrap();
            for sell in [false,true] {for first_use in [false,true] {
                let result=measure(&original,&world,lane,program,authority,&table,sell,first_use);
                rows.push(json!({"instrument":bank.name,"productId":lane.product_id,"inputSymbol":lane.input_symbol,
                    "side":if sell {"SELL"}else{"BUY"},"firstUse":first_use,"captureSha256":hash(&captured_bytes),
                    "worldSha256":hash(&world_bytes),"status":if result.is_ok(){"WIRE_MEASURED"}else{"COMPILER_REJECTED"},
                    "measurement":result.as_ref().ok(),"error":result.err()}));
            }}
        }
    }
    let measured=rows.iter().filter(|r|r["status"]=="WIRE_MEASURED").count();
    let report=json!({"schema":"skew.stockmesh.captured-wire-inventory/v1",
        "sourceManifestSha256":hash(&manifest_bytes),"lookupPlanSha256":hash(&lookup_bytes),
        "scope":"RECORDED_POOLS_FIXTURE_WALLET_UNPUBLISHED_ALT_COMPILER_ONLY",
        "directSinglePoolOnly":true,"measured":measured,"rows":rows,
        "rpcRequests":0,"signedTransactions":0,"submittedTransactions":0,"liveGateCompleted":false});
    fs::write(&output,serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    println!("inventory measured={measured} report={}",output.display());
    assert!(measured>0,"no complete native message measured; inspect exact failures");
}
