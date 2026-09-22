//! Finite opt-in provider read, real StockMesh solver, complete unsigned wire.
//! Native accounts are coherent provider observations. Wallet/nonce/ALT are
//! explicit compiler fixtures; this does not bypass production admission.
use super::*;
use crate::{
    exposure_wire::{self, DirectExposureSpec, DirectProduct, FundedExposureSpec},
    native_wire::captured_inventory_tests::{WALLET, hash, key, hex32, fixture_account, token_data, nonce_data, view},
    onebook_wire::{self, MeshProduct},
    provider::ProviderConfig,
    swap_wire::{self, SwapGraph},
    wallet_wire::WalletSetup,
};
use solana_instruction::Instruction;
use base64::{engine::general_purpose::STANDARD, Engine};
use solana_message::{AddressLookupTableAccount, VersionedMessage};
use solana_pubkey::pubkey;
use std::{fs, io::Write, path::{Component, PathBuf}};

const ROOT: &str = "/srv/skew/stockmesh-direct-node-20260920";
const SYSTEM: Pubkey = Pubkey::new_from_array([0;32]);
const ATA: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const COMPUTE: Pubkey = pubkey!("ComputeBudget111111111111111111111111111111");

fn save(path: &Path, value: &Value) -> Result<()> {
    let mut file=fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|_|"new inventory evidence required")?;
    file.write_all(&serde_json::to_vec_pretty(value).map_err(|_|"inventory serialization")?).map_err(|_|"inventory write")?;
    file.sync_all().map_err(|_|"inventory sync".into())
}
fn json_file(path: &Path, max: u64) -> Result<Value> {
    let meta=fs::symlink_metadata(path).map_err(|_|"inventory metadata")?;
    if !meta.is_file() || meta.len()>max {return Err("inventory file bounds".into());}
    serde_json::from_slice(&fs::read(path).map_err(|_|"inventory read")?).map_err(|_|"inventory JSON".into())
}
fn path(value: &Value) -> Result<PathBuf> {
    let p=PathBuf::from(value.as_str().ok_or("inventory path")?);
    if !p.starts_with(ROOT) || p.components().any(|c|matches!(c,Component::ParentDir)) {return Err("isolated inventory path".into());}
    Ok(p)
}
fn floor(amount:u64)->Result<u64> {
    u64::try_from(u128::from(amount)*9970/10000).map(|v|v.max(1)).map_err(|_|"inventory floor".into())
}

// A captured horizon may rotate, but a replacement pool/issuer path must be
// measured afresh. Never silently overwrite a new manifest with an old route.
fn capture_topology(configs:&[WorldConfig])->Result<Value> {
    let mut value=serde_json::to_value(configs).map_err(|_|"capture topology")?;
    for world in value.as_array_mut().ok_or("capture topology worlds")? {
        world.as_object_mut().ok_or("capture topology world")?.remove("execution_keys");
        for market in world["markets"].as_array_mut().ok_or("capture topology markets")? {
            market.as_object_mut().ok_or("capture topology market")?.remove("tick_arrays");
        }
    }
    Ok(value)
}

#[test]
fn capture_reuse_allows_only_horizon_rotation_not_pool_replacement() {
    let original:Vec<WorldConfig>=serde_json::from_value(json!([{"markets":[{
        "venue":"raydium_clmm","program":"program","pool":"pool-a","config":"config-a",
        "input_mint":"cash","output_mint":"stock","tick_arrays":["tick-a"],
        "array_capacity":6,"clock":"clock"
    }],"execution_keys":["vault-a"]}])).unwrap();
    let mut rotated=original.clone();rotated[0].markets[0].tick_arrays=vec!["tick-b".into()];
    assert_eq!(capture_topology(&original).unwrap(),capture_topology(&rotated).unwrap());
    for field in ["pool","program","config","output_mint"] {
        let mut changed=serde_json::to_value(&rotated).unwrap();
        changed[0]["markets"][0][field]=json!("replacement");
        let changed:Vec<WorldConfig>=serde_json::from_value(changed).unwrap();
        assert_ne!(capture_topology(&original).unwrap(),capture_topology(&changed).unwrap());
    }
}

struct WireFixture {
    bank: Snapshot,
    setup: WalletSetup,
    nonce: Pubkey,
    // Declarative prestate only. Runtime preparation must initialize real ATAs;
    // the 165-byte compiler stubs above are never exported as SBF accounts.
    runtime: Value,
}
fn fixture(original:&Snapshot,program:Pubkey,proposals:&[crate::world::NativeSwapProposal],input:Pubkey,amount:u64,wrap:bool,first:bool,payout:Option<&crate::native_payout::NativePayout>)->Result<WireFixture> {
    let mints=proposals.iter().flat_map(|p|[&p.market.input_mint,&p.market.output_mint])
        .map(|v|key(v)).collect::<Result<BTreeSet<_>>>()?;
    let assets=mints.iter().map(|mint|if let Some(p)=payout.filter(|_|mint.to_string()==WSOL_MINT){p.asset()}else{native_wire::wallet_asset(WALLET,*mint,original)}).collect::<Result<Vec<_>>>()?;
    let nonce=Pubkey::find_program_address(&[b"stocklana",WALLET.as_ref()],&program).0;
    let mut bank=original.clone();
    fixture_account(&mut bank,WALLET,SYSTEM,vec![],false)?;
    for address in [ATA,program] {
        if !bank.accounts.iter().any(|a|a.key==address.to_string()) {
            fixture_account(&mut bank,address,pubkey!("BPFLoaderUpgradeab1e11111111111111111111111"),vec![],true)?;
        }
    }
    for asset in &assets {
        let absent=(first && (asset.mint!=input || wrap)) || payout.is_some_and(|p|p.asset().is_ok_and(|a|a.token==asset.token));
        fixture_account(&mut bank,asset.token,if absent{SYSTEM}else{asset.token_program},
            if absent{vec![]}else{token_data(*asset,if asset.mint==input{amount.checked_mul(100).ok_or("fixture amount")?}else{0})},false)?;
    }
    fixture_account(&mut bank,nonce,if first{SYSTEM}else{program},if first{vec![]}else{nonce_data()},false)?;
    let setup_assets=assets.iter().filter(|a|payout.is_none()||a.mint.to_string()!=WSOL_MINT).copied().collect::<Vec<_>>();
    let mut setup=WalletSetup::plan(WALLET,&setup_assets,wrap.then_some(amount),|a|view(&bank,a))?.with_nonce(program,0,|a|view(&bank,a))?;
    if let Some(p)=payout {setup=setup.with_native_payout(p.clone(),|a|view(&bank,a))?;}
    for asset in &assets {
        let row=bank.accounts.iter_mut().find(|a|a.key==asset.token.to_string()).ok_or("fixture asset")?;
        row.owner=asset.token_program.to_string();
        row.data=token_data(*asset,if asset.mint==input{amount.checked_mul(100).ok_or("fixture amount")?}else{0});
    }
    let row=bank.accounts.iter_mut().find(|a|a.key==nonce.to_string()).ok_or("fixture nonce")?;
    row.owner=program.to_string();row.data=nonce_data();
    let runtime=json!({"wallet":WALLET.to_string(),"program":program.to_string(),"nonce":nonce.to_string(),
        "firstUse":first,"inputMint":input.to_string(),"inputAtoms":amount.to_string(),"wrapLamports":if wrap{amount}else{0},
        "assets":assets.iter().map(|a|json!({"mint":a.mint.to_string(),"token":a.token.to_string(),"tokenProgram":a.token_program.to_string(),
            "absentAtStart":(first && (a.mint!=input || wrap)) || payout.is_some_and(|p|p.asset().is_ok_and(|v|v.token==a.token)),
            "startingAtoms":if a.mint==input && !(first && wrap){amount*100}else{0}})).collect::<Vec<_>>(),
        "nativePayout":payout});
    Ok(WireFixture{bank,setup,nonce,runtime})
}
fn encoded_instruction(ix:&Instruction)->Value {
    json!({"programId":ix.program_id.to_string(),"data":STANDARD.encode(&ix.data),"accounts":ix.accounts.iter().map(|a|
        json!({"pubkey":a.pubkey.to_string(),"isSigner":a.is_signer,"isWritable":a.is_writable})).collect::<Vec<_>>()})
}
fn fixture_policy(lane:&Lane,input:Pubkey,program:Pubkey,authority:Pubkey,slot:u64)->Result<Value> {
    Ok(encoded_instruction(&onebook_wire::compile_stock_policy_v2(program,onebook_wire::StockPolicyV2Spec{
        authority,instrument:onebook_wire::instrument_id(&lane.key.instrument)?,issuer:onebook_wire::issuer_id(&lane.issuer)?,
        input_mint:input,output_mint:key(&lane.output_mint)?,rights_hash:hex32(&lane.rights_hash)?,
        version:1,expires_slot:slot.checked_add(200).ok_or("fixture policy expiry")?,flags:17})?))
}
fn envelope(fixture:&WireFixture,table:&AddressLookupTableAccount,settlement:Instruction,reflow:bool)->Result<Value> {
    let opcode=settlement.data[0];
    let compute=|tag:u8,n:u32| {let mut data=vec![tag];data.extend_from_slice(&n.to_le_bytes());Instruction{program_id:COMPUTE,accounts:vec![],data}};
    let mut instructions=vec![compute(2,1_400_000)];
    if reflow {instructions.push(compute(1,262_144));}
    instructions.extend_from_slice(fixture.setup.instructions()); instructions.push(settlement);
    if let Some(payout)=fixture.setup.native_payout() {
        // Also exercise production's setup-message validation, not only SBF.
        let prefix=fixture.setup.compile_setup_only(&[],[0x53;32],1_400_000)?.ok_or("native payout setup missing")?;
        fixture.setup.validate_setup_only_message(&fixture.bank,&prefix)?;
        instructions.push(payout.close()?);
    }
    if instructions.len()>crate::wallet_wire::MAX_STOCK_TRANSACTION_INSTRUCTIONS {return Err("inventory instruction count".into());}
    let message=onebook_wire::compile_unsigned_v0(WALLET,&instructions,std::slice::from_ref(table),[0x53;32])?;
    let VersionedMessage::V0(v0)=bincode::deserialize(&message).map_err(|_|"inventory v0")? else {return Err("inventory v0 version".into())};
    if v0.header.num_required_signatures!=1 || v0.account_keys[0]!=WALLET {return Err("inventory signer layout".into());}
    let loaded=v0.address_table_lookups.iter().map(|l|l.readonly_indexes.len()+l.writable_indexes.len()).sum::<usize>();
    Ok(json!({"opcode":opcode,"signedEnvelopeBytes":message.len()+65,"accountLocks":v0.account_keys.len()+loaded,
        "setupInstructions":fixture.setup.instructions().len(),"messageSha256":hash(&message),
        "sbfFixture":{"prestate":fixture.runtime,"instructions":instructions.iter().map(encoded_instruction).collect::<Vec<_>>(),
            "unsignedMessage":STANDARD.encode(&message),"lookupTable":{"key":table.key.to_string(),"addresses":table.addresses.iter().map(ToString::to_string).collect::<Vec<_>>()},
            "policies":[]},
        "withoutLookup":onebook_wire::compile_unsigned_v0(WALLET,&instructions,&[],[0x53;32]).map(|v|json!({"signedEnvelopeBytes":v.len()+65})).unwrap_or_else(|e|json!({"rejected":e})),
        "nativeOutput":fixture.setup.native_payout().map(|payout|crate::native_payout::NativeOutput{
            payout:payout.clone(),settlement_program:fixture.runtime["program"].as_str().unwrap().into(),
            created_accounts:fixture.setup.created_assets().map(|a|a.token.to_string()).chain(fixture.setup.created_nonce().map(|n|n.to_string())).collect(),
        }),
        "simulated":false,"cuMeasured":false,"liveAdmission":false}))
}
fn product(lane:&Lane,input:Pubkey,program:Pubkey,authority:Pubkey,bank:&Snapshot,minimum:u64)->Result<DirectProduct> {
    let mint=key(&lane.output_mint)?;
    let asset=native_wire::wallet_asset(WALLET,mint,bank)?;
    let policy=onebook_wire::stock_policy_v2_address(&program,&authority,&onebook_wire::instrument_id(&lane.key.instrument)?,&input,&mint,&hex32(&lane.rights_hash)?);
    Ok(DirectProduct{product_id:lane.key.product_id.clone(),minimum_output_atoms:minimum,product:MeshProduct{
        policy,claim:None,destination:asset.token,mint,token_program:asset.token_program,model:match lane.exposure_model.as_str(){"FIXED_RATIONAL"=>0,"TOKEN_2022_SCALED_UI"=>1,_=>return Err("inventory product model".into())},
        conservative_bps:lane.conservative_bps,policy_version:1,numerator:lane.exposure_numerator,denominator:lane.exposure_denominator}})
}
fn buy_wire(publication:&Publication,bank:&Snapshot,intent:&IntentKey,plan:&ExposurePlan,amount:u64,program:Pubkey,authority:Pubkey,table:&AddressLookupTableAccount,first:bool)->Result<Value> {
    let proposals=if plan.global_reflow{&plan.native_candidates}else{&plan.native_allocation};
    let variants=if plan.global_reflow{resource_admission_variants(proposals)?}else{vec![proposals.clone()]};
    let mut attempts=0;
    let mut reasons=Vec::new();
    let mut lower=|proposals:&[crate::world::NativeSwapProposal]| {
    attempts+=1;
    let candidate:Result<Value>=(|| {
    let input=key(if intent.input_symbol=="USDC"{USDC_MINT}else{WSOL_MINT})?;
    let fixture=fixture(bank,program,proposals,input,amount,intent.input_symbol=="SOL",first,None)?;
    let source=native_wire::wallet_asset(WALLET,input,&fixture.bank)?;
    let mut products=Vec::new();
    let mut policies=Vec::new();
    for id in proposals.iter().filter_map(|p|p.product_id.as_ref()).collect::<BTreeSet<_>>() {
        let lane=publication.layout.lanes.iter().find(|l|l.key.product_id==*id && l.key.input_symbol==intent.input_symbol).ok_or("inventory product lane")?;
        let group=publication.layout.lanes.iter().filter(|l|l.key.input_symbol==intent.input_symbol).collect::<Vec<_>>();
        let (_,cash)=intent_execution_shape(&group)?;
        let policy_input=key(&cash)?;
        let minimum=plan.products.iter().find(|p|p.product_id==*id).map_or(1,|p|floor(p.raw_output_atoms).unwrap_or(1));
        products.push(product(lane,policy_input,program,authority,&fixture.bank,minimum)?);
        policies.push(fixture_policy(lane,policy_input,program,authority,bank.slot)?);
    }
    let spec=DirectExposureSpec{buyer:WALLET,buyer_nonce:fixture.nonce,input:source,buyer_sequence:0,input_atoms:amount,minimum_exposure_q32:floor(plan.exposure_q32)?,
        deadline_slot:bank.slot.checked_add(40).ok_or("inventory deadline")?,maximum_policy_age:100,allow_underlying_closed:false,products};
    // World count includes issuer conversions. Funding is identified by the
    // compiler's proposal stages, not the number of conceptual route worlds.
    let settlement=if proposals.iter().map(|p|p.stage).max()==Some(2) {
        let funding=proposals.iter().filter(|p|p.stage==1).collect::<Vec<_>>();
        let cash=key(&funding.first().ok_or("inventory funding")?.market.output_mint)?;
        let minimum_cash=funding.iter().try_fold(0u64,|sum,p|sum.checked_add(p.expected_output_atoms).ok_or("inventory cash overflow"))?;
        exposure_wire::compile_funded_exposure_reflow(program,FundedExposureSpec{buyer:WALLET,buyer_nonce:fixture.nonce,input:source,
            cash:native_wire::wallet_asset(WALLET,cash,&fixture.bank)?,buyer_sequence:0,input_atoms:amount,minimum_cash_atoms:floor(minimum_cash)?,
            minimum_exposure_q32:spec.minimum_exposure_q32,reflow_oracle_calls:16,deadline_slot:spec.deadline_slot,maximum_policy_age:100,allow_underlying_closed:false,products:spec.products},proposals,&fixture.bank)?
    } else if plan.global_reflow {exposure_wire::compile_direct_exposure_reflow(program,spec,proposals,&fixture.bank)?}
    else {exposure_wire::compile_direct_exposure(program,spec,proposals,&fixture.bank)?};
    let mut measured=envelope(&fixture,table,settlement,plan.global_reflow)?;
    measured["sbfFixture"]["policies"]=json!(policies);
    measured["resourceAttempts"]=json!(attempts);
    measured["admittedCandidates"]=json!(proposals.len());
    Ok(measured)
    })();
    if let Err(error)=&candidate {reasons.push(error.clone());}
    candidate
    };
    let result=exact_resource_admission(variants.clone(),&mut lower);
    // Wire size alone is not runtime admission. Preserve the same bounded
    // alternatives that production's exact simulation evaluates, including
    // the unchanged input and aggregate exposure floor for each candidate.
    let result=match result {
        Ok(mut first)=> {
            let attempted=first["resourceAttempts"].as_u64().ok_or("inventory attempts")? as usize;
            let alternatives=variants.into_iter().skip(attempted)
                .filter_map(|v|exact_resource_admission(vec![v],&mut lower).ok()).collect::<Vec<_>>();
            first["sbfAlternatives"]=json!(alternatives);Ok(first)
        },
        Err(error)=>Err(error),
    };
    result.map_err(|error|format!("{error}; lowering: {}",reasons.join(" | ")))
}
fn sell_wire(bank:&Snapshot,lane:&Lane,plan:&SellPlan,amount:u64,program:Pubkey,authority:Pubkey,table:&AddressLookupTableAccount,first:bool)->Result<Value> {
    let proposals=&plan.native_allocation;
    let input=key(&lane.output_mint)?;
    let output=key(&proposals.last().ok_or("inventory sell end")?.market.output_mint)?;
    // Exercise production's account planner as well as low-level lowering.
    // A passing SBF wire must not bypass a stale two-hop prepare front door.
    let policy=onebook_wire::stock_policy_v2_address(&program,&authority,&onebook_wire::instrument_id(&lane.key.instrument)?,
        &key(&lane_policy_input_mint(lane)?)?,&input,&hex32(&lane.rights_hash)?);
    let payout=if output.to_string()==WSOL_MINT {Some(crate::native_payout::NativePayout::new(WALLET,&format!("stkq_{}","7".repeat(32)),2_039_280)?)}else{None};
    let planned=crate::direct_prepare::plan_sell_execution_bank(program,table.key,WALLET,policy,input,output,amount,proposals,bank,payout.as_ref())?;
    let fixture=fixture(bank,program,proposals,input,amount,false,first,payout.as_ref())?;
    let input_asset=native_wire::wallet_asset(WALLET,input,&fixture.bank)?;
    let output_asset=if let Some(p)=&payout{p.asset()?}else{native_wire::wallet_asset(WALLET,output,&fixture.bank)?};
    // Match prepare_sell: preserve first production order, never public-key
    // sort (which can reverse the two intermediate nodes in a three-hop sale).
    let mut intermediate_mints=Vec::new();
    for p in proposals {
        let mint=key(&p.market.output_mint)?;
        if mint!=output && !intermediate_mints.contains(&mint) {intermediate_mints.push(mint);}
    }
    let intermediate=intermediate_mints.into_iter().map(|mint|native_wire::wallet_asset(WALLET,mint,&fixture.bank)).collect::<Result<Vec<_>>>()?;
    let graph=native_wire::lower_native_graph(SwapGraph{owner:WALLET,sequence:0,input_atoms:amount,minimum_output_atoms:floor(plan.output_atoms)?,
        deadline_slot:bank.slot.checked_add(40).ok_or("inventory deadline")?,input:input_asset,output:output_asset,intermediates:intermediate,legs:vec![]},proposals,&fixture.bank)?;
    let graph=swap_wire::compile_swap_graph(program,&graph,|a|view(&fixture.bank,a))?;
    let product=product(lane,key(&lane_policy_input_mint(lane)?)?,program,authority,&fixture.bank,1)?;
    let settlement=onebook_wire::compile_reverse_stock_fill(program,product.product.policy,1,100,false,graph)?;
    let mut measured=envelope(&fixture,table,settlement,false)?;
    measured["productionPrepareBankAssets"]=json!(planned.assets.iter().map(|a|a.mint.to_string()).collect::<Vec<_>>());
    measured["productionPrepareBankAccounts"]=json!(planned.keys.len());
    measured["sbfFixture"]["policies"]=json!([fixture_policy(lane,key(&lane_policy_input_mint(lane)?)?,program,authority,bank.slot)?]);
    Ok(measured)
}

#[test]
#[ignore = "explicit finite Cherry common-bank basket capture and deployed-program proof"]
fn bounded_three_stock_basket_wire() {
    use crate::basket_wire::{self,Intent,Target,Leg};
    let config=json_file(&path(&json!(std::env::var("SKEW_BASKET_CONFIG").unwrap())).unwrap(),16*1024).unwrap();
    let out=path(&config["output"]).unwrap();assert!(!out.exists());fs::create_dir(&out).unwrap();
    let deployment=json_file(&path(&config["deployment"]).unwrap(),256*1024).unwrap();
    let program=key(deployment["settlementProgram"].as_str().unwrap()).unwrap();
    let authority=key(deployment["policyAuthority"].as_str().unwrap()).unwrap();
    let (mesh,_)=StockMesh::load(&path(&config["manifest"]).unwrap(),"basket-no-server-no-signing-authority".into(),false).unwrap();
    let names=config["instruments"].as_array().unwrap().iter().map(|v|v.as_str().unwrap().to_owned()).collect::<Vec<_>>();
    assert!((1..=3).contains(&names.len()));
    let mut layouts=Vec::new();
    let capture=if config["reuseCapture"].is_string(){
        let saved=json_file(&path(&config["reuseCapture"]).unwrap(),16*1024*1024).unwrap();
        for name in &names {
            let bank=mesh.banks.iter().find(|b|&b.name==name).unwrap();
            let mut lanes=bank.layout().unwrap().lanes.iter().filter(|l|l.key.input_symbol=="USDC").cloned().collect::<Vec<_>>();
            for lane in &mut lanes {let row=saved["worlds"].as_array().unwrap().iter().find(|v|v["instrument"]==*name&&v["productId"]==lane.key.product_id).unwrap();
                let configs:Vec<WorldConfig>=serde_json::from_value(row["worlds"].clone()).unwrap();assert_eq!(capture_topology(&configs).unwrap(),capture_topology(&lane.configs).unwrap());lane.configs=configs;}
            layouts.push(lanes);
        }saved
    }else{
        let profile=ProviderConfig::load(&path(&config["providerProfile"]).unwrap()).unwrap();assert!(profile.max_requests<=80);let rpc=profile.connect().unwrap();rpc.check_genesis().unwrap();
        for name in &names {
            let index=mesh.banks.iter().position(|b|&b.name==name).unwrap();
            std::thread::sleep(Duration::from_millis(1100));
            refresh_superbank_group_once(&mesh.banks,&rpc,&[index],true).unwrap();
            layouts.push(mesh.banks[index].layout().unwrap().lanes.iter().filter(|l|l.key.input_symbol=="USDC").cloned().collect::<Vec<_>>());
        }
        let keys=layouts.iter().flatten().flat_map(|l|&l.configs).map(WorldConfig::keys).collect::<Result<Vec<_>>>().unwrap().into_iter().flatten().collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
        println!("joint market accounts {}",keys.len());assert!(keys.len()<=100,"basket must not stitch separate RPC responses");
        std::thread::sleep(Duration::from_millis(1100));
        let response=rpc.call("getMultipleAccounts",json!([keys,{"encoding":"base64","commitment":"confirmed"}])).unwrap();
        let worlds=layouts.iter().flatten().map(|l|json!({"instrument":l.key.instrument,"productId":l.key.product_id,"worlds":l.configs})).collect::<Vec<_>>();
        json!({"keys":keys,"response":response,"worlds":worlds})
    };
    save(&out.join("basket-complete.json"),&capture).unwrap();
    let keys:Vec<String>=serde_json::from_value(capture["keys"].clone()).unwrap();
    let full=Feed::new(keys.clone(),Duration::from_secs(60),4*1024*1024).unwrap().publish(&capture["response"]).unwrap();
    let mut publications=Vec::new();let mut plans=Vec::new();
    for (name,lanes) in names.iter().zip(layouts){
        let lane_keys=lanes.iter().flat_map(|l|&l.configs).map(WorldConfig::keys).collect::<Result<Vec<_>>>().unwrap().into_iter().flatten().collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
        let projected=full.project(&lane_keys).unwrap();
        let response=json!({"context":{"slot":full.slot},"value":projected.accounts.iter().map(|a|json!({"owner":a.owner,"executable":a.executable,"lamports":a.lamports,"data":[STANDARD.encode(&a.data),"base64"]})).collect::<Vec<_>>()});
        let bank=mesh.banks.iter().find(|b|&b.name==name).unwrap();
        let layout=Arc::new(BankLayout{feed:Arc::new(Feed::new(lane_keys,Duration::from_secs(60),4*1024*1024).unwrap()),lanes});
        bank.publish_layout(layout,&response,true).unwrap();let publication=bank.current().unwrap();
        let amount=30_000_000;let plan=solve_exposure(&publication,&IntentKey{instrument:name.clone(),input_symbol:"USDC".into()},amount).unwrap();
        publications.push(publication);plans.push(plan);
    }
    let all=plans.iter().flat_map(|p|if p.global_reflow{p.native_candidates.clone()}else{p.native_allocation.clone()}).collect::<Vec<_>>();
    let spend=names.len() as u64*30_000_000;let total=spend+10_000_000;let mut rows=Vec::new();
    for first in [false,true]{
        let mut fixture=fixture(&full,program,&all,key(USDC_MINT).unwrap(),total,false,first,None).unwrap();
        // Exercise real funded opcodes with existing investor inventory, not
        // only zero-valued transit accounts. The SBF runner constructs a real
        // native token account from this explicitly declared fixture prestate.
        if !first{
            if let Some(asset)=fixture.runtime["assets"].as_array_mut().unwrap().iter_mut().find(|a|a["mint"]==WSOL_MINT){
                asset["startingAtoms"]=json!(7_777_777u64);
                let row=fixture.bank.accounts.iter_mut().find(|a|a.key==asset["token"].as_str().unwrap()).unwrap();
                row.data[64..72].copy_from_slice(&7_777_777u64.to_le_bytes());
                let assets=fixture.runtime["assets"].as_array().unwrap().iter().map(|a|crate::swap_wire::TokenAsset{
                    token:key(a["token"].as_str().unwrap()).unwrap(),mint:key(a["mint"].as_str().unwrap()).unwrap(),token_program:key(a["tokenProgram"].as_str().unwrap()).unwrap()}).collect::<Vec<_>>();
                fixture.setup=WalletSetup::plan(WALLET,&assets,None,|a|view(&fixture.bank,a)).unwrap().with_nonce(program,0,|a|view(&fixture.bank,a)).unwrap();
            }
        }
        let mut legs=Vec::new();let mut policies=Vec::new();
        for (publication,plan) in publications.iter().zip(&plans){
            let proposals=if plan.global_reflow{plan.native_candidates.clone()}else{plan.native_allocation.clone()};let mut products=Vec::new();
            let funding=basket_wire::funding_cash(&proposals).unwrap();
            let cash=funding.map_or(key(USDC_MINT).unwrap(),|(mint,_)|mint);
            let minimum_cash_atoms=funding.map(|(_,n)|floor(n).unwrap());
            for id in proposals.iter().filter_map(|p|p.product_id.as_ref()).collect::<BTreeSet<_>>(){
                let lane=publication.layout.lanes.iter().find(|l|l.key.product_id==*id).unwrap();
                let p=product(lane,cash,program,authority,&fixture.bank,1).unwrap();
                let ix=fixture_policy(lane,cash,program,authority,full.slot).unwrap();
                let data=STANDARD.decode(ix["data"].as_str().unwrap()).unwrap();let mut state=b"SKEWSTK2".to_vec();state.extend_from_slice(authority.as_ref());state.extend_from_slice(&data[1..169]);state.extend_from_slice(&full.slot.to_le_bytes());state.extend_from_slice(&data[169..]);
                fixture_account(&mut fixture.bank,p.product.policy,program,state,false).unwrap();policies.push(ix);products.push(p);
            }
            legs.push(Leg{products,proposals,economic_reflow:plan.global_reflow,minimum_cash_atoms});
        }
        let intent=Intent{owner:WALLET.to_string(),strategy_version:[0x61;32],catalog_revision:[0x62;32],total_input_atoms:total,retained_cash_atoms:10_000_000,owner_sequence:0,
            deadline_slot:full.slot+40,maximum_cu:1_400_000,targets:names.iter().zip(&plans).map(|(instrument,p)|Target{instrument:instrument.clone(),input_atoms:30_000_000,minimum_exposure_q32:floor(p.exposure_q32).unwrap()}).collect()};
        // Fixture ALT is the complete captured account union, never published.
        let addresses=fixture.bank.accounts.iter().map(|a|key(&a.key).unwrap()).filter(|k|*k!=WALLET).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
        let table=AddressLookupTableAccount{key:Pubkey::new_from_array([0x63;32]),addresses};
        let result=basket_wire::compile(program,intent.clone(),legs,&fixture.bank,&fixture.setup,std::slice::from_ref(&table),[0x53;32]);
        match result{
            Ok(compiled)=>{
                if config["proof"].is_string(){
                    let proof=json_file(&path(&config["proof"]).unwrap(),16*1024*1024).unwrap();let row=proof["rows"].as_array().unwrap().iter().find(|v|v["firstUse"]==first).unwrap();
                    let make=|field:&str|->Snapshot{let mut s=(*full).clone();s.accounts=serde_json::from_value::<Vec<Value>>(row[field].clone()).unwrap().iter().map(|a|crate::feed::Account{key:a["key"].as_str().unwrap().into(),owner:a["owner"].as_str().unwrap().into(),lamports:a["lamports"].as_u64().unwrap(),executable:a["executable"].as_bool().unwrap(),data:STANDARD.decode(a["data"].as_str().unwrap()).unwrap()}).collect();s};
                    let before=make("before");let after=make("after");let cu=row["computeUnits"].as_u64().unwrap();
                    let verified=compiled.verify_simulated(&compiled.message,&before,&after,cu).unwrap();
                    let prepared=basket_wire::archived_simulation(&compiled,&before,&after,&table,&row["logs"],cu).unwrap();
                    save(&out.join(format!("prepared-{first}.json")),&prepared).unwrap();
                    assert!(compiled.verify_simulated(&[0],&before,&after,cu).is_err());
                    let mut wrong_slot=before.clone();wrong_slot.slot+=1;
                    assert!(compiled.verify_simulated(&compiled.message,&wrong_slot,&after,cu).is_err());
                    let mut wrong_clock=before.clone();wrong_clock.accounts.iter_mut().find(|a|a.key=="SysvarC1ock11111111111111111111111111111111").unwrap().data[32]^=1;
                    assert!(compiled.verify_simulated(&compiled.message,&wrong_clock,&after,cu).is_err());
                    let mut bad_cash=after.clone();let cash=native_wire::wallet_asset(WALLET,key(USDC_MINT).unwrap(),&after).unwrap();
                    bad_cash.accounts.iter_mut().find(|a|a.key==cash.token.to_string()).unwrap().data[64]^=1;
                    assert!(compiled.verify_simulated(&compiled.message,&before,&bad_cash,cu).is_err());
                    let mut bad_nonce=after.clone();bad_nonce.accounts.iter_mut().find(|a|a.key==fixture.nonce.to_string()).unwrap().data[40]^=1;
                    assert!(compiled.verify_simulated(&compiled.message,&before,&bad_nonce,cu).is_err());
                    save(&out.join(format!("receipt-{first}.json")),&json!(verified)).unwrap();
                    // A surplus in one stock cannot erase a different missing
                    // stock, nor can one final returnData stand in for all legs.
                    for asset in fixture.runtime["assets"].as_array().unwrap().iter().filter(|a|a["mint"]!=USDC_MINT&&a["mint"]!=WSOL_MINT){
                        let mut bad=after.clone();let address=asset["token"].as_str().unwrap();let before_row=before.accounts.iter().find(|a|a.key==address).unwrap();
                        let before_amount=if before_row.data.len()>=72{u64::from_le_bytes(before_row.data[64..72].try_into().unwrap())}else{0};
                        bad.accounts.iter_mut().find(|a|a.key==address).unwrap().data[64..72].copy_from_slice(&before_amount.to_le_bytes());
                        assert!(compiled.verify_simulated(&compiled.message,&before,&bad,cu).is_err());
                    }
                    if let Some(asset)=fixture.runtime["assets"].as_array().unwrap().iter().find(|a|a["mint"]==WSOL_MINT){
                        let mut bad=after.clone();bad.accounts.iter_mut().find(|a|a.key==asset["token"].as_str().unwrap()).unwrap().data[64]^=1;
                        assert!(compiled.verify_simulated(&compiled.message,&before,&bad,cu).is_err());
                    }
                }
                rows.push(json!({"firstUse":first,"intent":intent,"packetBytes":compiled.message.len()+65,"messageSha256":hash(&compiled.message),"sharedPools":compiled.shared_pools,
                    "settlementIndices":compiled.settlement_indices,"prestate":fixture.runtime,"policies":policies,
                    "instructions":compiled.instructions.iter().map(encoded_instruction).collect::<Vec<_>>(),"unsignedMessage":STANDARD.encode(&compiled.message),
                    "lookupTable":{"key":table.key.to_string(),"addresses":table.addresses.iter().map(ToString::to_string).collect::<Vec<_>>()}}));
            },Err(error)=>rows.push(json!({"firstUse":first,"error":error,"intent":intent})),
        }
    }
    save(&out.join("report.json"),&json!({"schema":"stockmesh.basket-wire/v1","slot":full.slot,"oneCoherentBank":true,"marketAccounts":full.accounts.len(),"rows":rows,"networkSubmitted":0})).unwrap();
    assert!(rows.iter().all(|r|r["error"].is_null()),"inspect basket compiler resource failures");
}

#[test]
#[ignore="finite Cherry provider inventory; explicit config and initialized durable read quota required"]
fn bounded_native_universe_complete_wire_inventory() {
    assert!(cfg!(target_os="linux"));
    let config_path=path(&json!(std::env::var("SKEW_UNIVERSE_INVENTORY_CONFIG").expect("inventory config"))).unwrap();
    let config=json_file(&config_path,16*1024).unwrap();
    // A typo in a reuse path must not silently turn recorded verification into
    // another full paid capture pass.
    if let Some(reuse)=config.get("reuseCaptures") {
        assert!(path(reuse).unwrap().is_dir(),"capture reuse directory missing");
    }
    let manifest=path(&config["manifest"]).unwrap();let out=path(&config["output"]).unwrap();
    assert!(!out.exists(),"new evidence directory required");fs::create_dir(&out).unwrap();
    let deployment=json_file(&path(&config["deployment"]).unwrap(),256*1024).unwrap();
    let lookup=json_file(&path(&config["lookupPlan"]).unwrap(),4*1024*1024).unwrap();
    assert_eq!(lookup["sourceManifestSha256"],hash(&fs::read(&manifest).unwrap()));
    let program=key(deployment["settlementProgram"].as_str().unwrap()).unwrap();
    let authority=key(deployment["policyAuthority"].as_str().unwrap()).unwrap();
    let profile=ProviderConfig::load(&path(&config["providerProfile"]).unwrap()).unwrap();
    assert!(profile.max_requests<=300 && profile.durable_quota.is_some());
    let recorded_only=config["recordedOnly"]==true;
    let rpc=if recorded_only{None}else{Some(profile.connect().unwrap())};
    let (mesh,_)=StockMesh::load(&manifest,"finite-inventory-no-server-no-authorization".into(),false).unwrap();
    let started=Instant::now();let mut rows=Vec::new();let mut banks=Vec::new();
    for (index,bank) in mesh.banks.iter().enumerate() {
        if config["instruments"].as_array().is_some_and(|v|!v.iter().any(|name|name==&bank.name)) {continue;}
        if started.elapsed()>Duration::from_secs(900) {banks.push(json!({"instrument":bank.name,"error":"FINITE_RUN_EXPIRED"}));break;}
        let result=(|| -> Result<(Arc<Publication>,Arc<Snapshot>)> {
            if let Some(reuse)=config.get("reuseCaptures") {
                let file=path(reuse)?.join(format!("{}-complete.json",bank.name));
                if file.exists() {
                    let value=json_file(&file,8*1024*1024)?;
                    let mut lanes=bank.layout()?.lanes.clone();
                    let mut compatible=true;
                    for lane in &mut lanes {
                        let Some(saved)=value["worlds"].as_array().ok_or("captured worlds")?.iter().find(|v|v["productId"]==lane.key.product_id && v["inputSymbol"]==lane.key.input_symbol) else {compatible=false;break};
                        let configs:Vec<WorldConfig>=serde_json::from_value(saved["worlds"].clone()).map_err(|_|"captured world format")?;
                        if capture_topology(&lane.configs)?!=capture_topology(&configs)? {compatible=false;break;}
                        lane.configs=configs;
                    }
                    if compatible {
                    let keys:Vec<String>=serde_json::from_value(value["keys"].clone()).map_err(|_|"captured keys")?;
                    let layout=Arc::new(BankLayout{feed:Arc::new(Feed::new(keys.clone(),Duration::from_secs(60),4*1024*1024)?),lanes});
                    bank.publish_layout(layout,&value["response"],true)?;
                    let full=Feed::new(keys,Duration::from_secs(60),4*1024*1024)?.publish(&value["response"])?;
                    save(&out.join(format!("{}-complete.json",bank.name)),&value)?;
                    return Ok((bank.current()?,full));
                    }
                }
            }
            // Include failure paths in pacing: an early continue must not turn
            // one rate-limit rejection into a burst over every remaining bank.
            let rpc=rpc.as_ref().ok_or("RECORDED_BANK_UNAVAILABLE")?;
            std::thread::sleep(Duration::from_millis(1100));
            refresh_superbank_group_once(&mesh.banks,rpc,&[index],true)?;
            let layout=bank.layout()?;
            let keys=layout.lanes.iter().flat_map(|lane|lane.configs.iter()).map(WorldConfig::keys).collect::<Result<Vec<_>>>()?
                .into_iter().flatten().collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
            if keys.len()>100 {return Err("inventory complete coherent bank bound".into());}
            let response=rpc.call("getMultipleAccounts",json!([keys,{"encoding":"base64","commitment":"confirmed","minContextSlot":bank.last_slot()}]))?;
            let worlds=layout.lanes.iter().map(|lane|json!({"productId":lane.key.product_id,"inputSymbol":lane.key.input_symbol,"worlds":lane.configs})).collect::<Vec<_>>();
            save(&out.join(format!("{}-complete.json",bank.name)),&json!({"keys":keys,"response":response,"worlds":worlds}))?;
            publish_bank(bank,&key_index(&keys),&response)?;
            let full=Feed::new(keys,Duration::from_secs(60),4*1024*1024)?.publish(&response)?;
            Ok((bank.current()?,full))
        })();
        let (publication,full)=match result {Ok(v)=>v,Err(error)=>{banks.push(json!({"instrument":bank.name,"error":error}));continue}};
        banks.push(json!({"instrument":bank.name,"slot":full.slot,"accounts":full.accounts.len(),"rotations":bank.rotations.load(Ordering::Acquire)}));
        let cohort_id=lookup["instrumentCohorts"][&bank.name].as_str().unwrap();
        let cohort=lookup["cohorts"].as_array().unwrap().iter().find(|v|v["id"]==cohort_id).unwrap();
        let table=AddressLookupTableAccount{key:Pubkey::new_from_array(hex32(cohort_id).unwrap()),addresses:cohort["addresses"].as_array().unwrap().iter().map(|v|key(v.as_str().unwrap()).unwrap()).collect()};
        for symbol in ["USDC","SOL"] {
            let intent=IntentKey{instrument:bank.name.clone(),input_symbol:symbol.into()};
            let amount=100_000_000;
            match solve_exposure(&publication,&intent,amount) {
                Ok(mut plan)=>for first in [false,true] {
                    if config["diagnosticForceDirectReflow"]==true && symbol=="USDC" {plan.global_reflow=true;}
                    let wire=buy_wire(&publication,&full,&intent,&plan,amount,program,authority,&table,first);
                    rows.push(json!({"instrument":bank.name,"side":"BUY","inputSymbol":symbol,"firstUse":first,"slot":full.slot,
                        "nativeLegs":plan.native_allocation.len(),"nativeCandidates":plan.native_candidates.len(),"products":plan.products.len(),"stageCount":plan.stage_count,
                        "quotedExposureQ32":plan.exposure_q32.to_string(),"measurement":wire.as_ref().ok(),"error":wire.err()}));
                },
                Err(error)=>rows.push(json!({"instrument":bank.name,"side":"BUY","inputSymbol":symbol,"error":error})),
            }
        }
        for lane in &publication.layout.lanes {
            let decimal_shift=config["sellDecimalShift"].as_u64().unwrap_or(0);
            assert!(decimal_shift<=u64::from(lane.output_decimals));
            let amount=10u64.checked_pow(u32::from(lane.output_decimals)-decimal_shift as u32).unwrap();
            if config["diagnosticSellStages"]==true {
                let mut stage_input=amount;
                let mut diagnostics=Vec::new();
                for (stage,c) in lane.configs.iter().rev().enumerate() {
                    let c=c.reversed();let pair=&c.markets[0];
                    let world=c.compile_admitted_pair(&publication.snapshot,None,&pair.input_mint,&pair.output_mint).unwrap();
                    let edges=(0..world.edge_count()).map(|i|json!({"pool":world.edge_identity(i).unwrap().1,"result":world.quote_edge(i,stage_input)})).collect::<Vec<_>>();
                    let allocation=world.quote(stage_input);
                    let next=allocation.as_ref().ok().and_then(|v|v["outputAtoms"].as_u64());
                    diagnostics.push(json!({"stage":stage+1,"inputMint":pair.input_mint,"outputMint":pair.output_mint,
                        "inputAtoms":stage_input,"edges":edges,"allocation":allocation}));
                    if let Some(next)=next {stage_input=next;} else {break;}
                }
                save(&out.join(format!("{}-{}-{}-sell-stages.json",bank.name,lane.key.product_id,lane.key.input_symbol)),&json!(diagnostics)).unwrap();
            }
            match solve_sell(&publication,lane,amount) {
                Ok(plan)=>for first in [false,true] {
                    let wire=sell_wire(&full,lane,&plan,amount,program,authority,&table,first);
                    rows.push(json!({"instrument":bank.name,"side":"SELL","productId":lane.key.product_id,"outputSymbol":lane.key.input_symbol,"firstUse":first,
                        "slot":full.slot,"nativeLegs":plan.native_allocation.len(),"quotedOutputAtoms":plan.output_atoms.to_string(),"measurement":wire.as_ref().ok(),"error":wire.err()}));
                },
                Err(error)=>rows.push(json!({"instrument":bank.name,"side":"SELL","productId":lane.key.product_id,"outputSymbol":lane.key.input_symbol,"error":error})),
            }
        }
        println!("{} slot={} rows={}",bank.name,full.slot,rows.len());
        std::thread::sleep(Duration::from_millis(250));
    }
    let measured=rows.iter().filter(|r|r["measurement"].is_object()).count();
    save(&out.join("report.json"),&json!({"schema":"skew.stockmesh.complete-universe-wire/v1","manifestSha256":hash(&fs::read(&manifest).unwrap()),
        "scope":"PROVIDER_NATIVE_POOLS_REAL_SOLVER_FIXTURE_WALLET_UNPUBLISHED_ALT_WIRE_ONLY","banks":banks,"rows":rows,"measured":measured,
        "configSha256":hash(&fs::read(&config_path).unwrap()),
        "recordedOnly":recorded_only,"providerMetrics":rpc.as_ref().and_then(Rpc::provider_metrics),"signedTransactions":0,"submittedTransactions":0,"liveGateCompleted":false})).unwrap();
    assert!(measured>0,"inspect exact failure report");
}
