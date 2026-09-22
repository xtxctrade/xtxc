//! Execute the host's exact unsigned stock transactions against archived live
//! programs and coherent per-stock observations. All wallet balances, policy
//! publications and lookup tables are local fixtures; this cannot submit.
use base64::{engine::general_purpose::STANDARD, Engine};
use mollusk_svm::Mollusk;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::AddressLookupTableAccount;
use solana_pubkey::{pubkey, Pubkey};
use std::{collections::BTreeSet, fs, io::Write, path::{Component, Path, PathBuf}, str::FromStr};
#[path="../stockmesh_dynamic_conformance.rs"]
mod dynamic_conformance;
#[path="../stockmesh_basket_proof.rs"]
mod basket_proof;

const ROOT: &str = "/srv/skew/stockmesh-direct-node-20260920";
const ATA: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const CLOCK: Pubkey = pubkey!("SysvarC1ock11111111111111111111111111111111");
const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
type Accounts = Vec<(Pubkey, Account)>;
fn key(v: &Value) -> Pubkey { Pubkey::from_str(v.as_str().expect("pubkey string")).expect("pubkey") }
fn hash(v: &[u8]) -> String { format!("{:x}", Sha256::digest(v)) }
fn isolated(s: &str) -> PathBuf {
    let p=PathBuf::from(s);
    assert!(p.starts_with(ROOT) && !p.components().any(|c| matches!(c, Component::ParentDir)));
    p
}
fn read(p: &Path) -> Value { serde_json::from_slice(&fs::read(p).unwrap()).unwrap() }
fn save(p: &Path, v: &Value) {
    let mut f=fs::OpenOptions::new().write(true).create_new(true).open(p).unwrap();
    f.write_all(&serde_json::to_vec_pretty(v).unwrap()).unwrap(); f.sync_all().unwrap();
}
fn u64_at(d: &[u8], o: usize) -> u64 { u64::from_le_bytes(d[o..o+8].try_into().unwrap()) }
fn get<'a>(a: &'a Accounts, k: &Pubkey) -> &'a Account { &a.iter().find(|v|v.0==*k).unwrap_or_else(||panic!("missing {k}")).1 }
fn put(a: &mut Accounts,k:Pubkey,v:Account) {
    if let Some(row)=a.iter_mut().find(|v|v.0==k) {row.1=v;} else {a.push((k,v));}
}
fn ix(v: &Value) -> Instruction {
    Instruction {program_id:key(&v["programId"]),data:STANDARD.decode(v["data"].as_str().unwrap()).unwrap(),
        accounts:v["accounts"].as_array().unwrap().iter().map(|a|AccountMeta{pubkey:key(&a["pubkey"]),
            is_signer:a["isSigner"].as_bool().unwrap(),is_writable:a["isWritable"].as_bool().unwrap()}).collect()}
}
fn captured(v: &Value) -> Accounts {
    let keys=v["keys"].as_array().unwrap();let values=v["response"]["value"].as_array().unwrap();
    assert_eq!(keys.len(),values.len());
    keys.iter().zip(values).map(|(k,v)| {
        let account=if v.is_null() {Account::default()} else {
            assert_eq!(v["data"][1],"base64");
            Account{lamports:v["lamports"].as_u64().unwrap(),owner:key(&v["owner"]),executable:v["executable"].as_bool().unwrap(),
                rent_epoch:v["rentEpoch"].as_u64().unwrap_or(0),data:STANDARD.decode(v["data"][0].as_str().unwrap()).unwrap()}
        };(key(k),account)
    }).collect()
}
fn native_program() -> Account {
    Account{lamports:1,owner:pubkey!("NativeLoader1111111111111111111111111111111"),executable:true,..Account::default()}
}
fn ata_ix(wallet:Pubkey,a:&Value)->Instruction {
    Instruction{program_id:ATA,data:vec![1],accounts:vec![AccountMeta::new(wallet,true),AccountMeta::new(key(&a["token"]),false),
        AccountMeta::new_readonly(wallet,false),AccountMeta::new_readonly(key(&a["mint"]),false),
        AccountMeta::new_readonly(Pubkey::default(),false),AccountMeta::new_readonly(key(&a["tokenProgram"]),false)]}
}

fn prepare(m:&mut Mollusk,row:&Value,observation:&Value,programs:&Value)->Result<Accounts,String> {
    let f=&row["measurement"]["sbfFixture"];let p=&f["prestate"];let wallet=key(&p["wallet"]);
    let mut a=captured(observation);let clock=get(&a,&CLOCK).data.clone();
    m.sysvars.clock.slot=u64_at(&clock,0);m.sysvars.clock.epoch_start_timestamp=u64_at(&clock,8) as i64;
    m.sysvars.clock.epoch=u64_at(&clock,16);m.sysvars.clock.leader_schedule_epoch=u64_at(&clock,24);
    m.sysvars.clock.unix_timestamp=u64_at(&clock,32) as i64;
    if m.sysvars.clock.slot!=row["slot"].as_u64().unwrap() {return Err("clock/capture slot mismatch".into());}
    for program in programs["programs"].as_array().unwrap() {
        if program["lastDeploySlot"].as_u64().unwrap_or(0)>m.sysvars.clock.slot {return Err("ELF deployed after capture".into());}
        let id=key(&program["id"]);
        if !a.iter().any(|v|v.0==id) {
            let mut data=Vec::new();
            if program["programData"].is_string() {data.extend_from_slice(&2u32.to_le_bytes());data.extend_from_slice(key(&program["programData"]).as_ref());}
            put(&mut a,id,Account{lamports:1,owner:key(&program["loader"]),executable:true,data,..Account::default()});
        }
    }
    put(&mut a,Pubkey::default(),native_program());
    put(&mut a,pubkey!("ComputeBudget111111111111111111111111111111"),native_program());
    put(&mut a,wallet,Account{lamports:1_000_000_000_000,..Account::default()});
    // The real ATA program chooses the account length/extensions from each
    // captured mint. Only the starting balance is fixture-injected afterward.
    for asset in p["assets"].as_array().unwrap() {
        let address=key(&asset["token"]);put(&mut a,address,Account::default());
        if asset["absentAtStart"]==true {continue;}
        let r=m.process_transaction_instructions(&[ata_ix(wallet,asset)],&a);
        if r.program_result.is_err() {return Err(format!("fixture ATA: {:?}",r.program_result));}
        a=r.resulting_accounts;
        let mut token=get(&a,&address).clone();let balance=asset["startingAtoms"].as_u64().unwrap();
        if token.owner!=key(&asset["tokenProgram"]) || token.data.len()<165 {return Err("ATA binding/layout".into());}
        token.data[64..72].copy_from_slice(&balance.to_le_bytes());
        if key(&asset["mint"])==WSOL {
            if token.data[109..113]!=1u32.to_le_bytes() {return Err("native ATA option".into());}
            token.lamports=u64_at(&token.data,113).checked_add(balance).ok_or("native balance overflow")?;
        }
        put(&mut a,address,token);
    }
    if p["nativePayout"].is_object() {
        // A preexisting nonzero user wSOL ATA must not be touched by the sale.
        let old=Pubkey::find_program_address(&[wallet.as_ref(),skew_execution_host::native_payout::TOKEN.as_ref(),WSOL.as_ref()],&ATA).0;
        let asset=json!({"token":old.to_string(),"mint":WSOL.to_string(),"tokenProgram":skew_execution_host::native_payout::TOKEN.to_string()});
        put(&mut a,old,Account::default());
        let r=m.process_transaction_instructions(&[ata_ix(wallet,&asset)],&a);
        if r.program_result.is_err(){return Err("old native ATA fixture".into());}a=r.resulting_accounts;
        let mut old_account=get(&a,&old).clone();old_account.data[64..72].copy_from_slice(&987_654_321u64.to_le_bytes());
        old_account.lamports=u64_at(&old_account.data,113)+987_654_321;put(&mut a,old,old_account);
    }
    let nonce=key(&p["nonce"]);put(&mut a,nonce,Account::default());
    if p["firstUse"]!=true {
        let init=Instruction{program_id:key(&p["program"]),data:vec![0],accounts:vec![AccountMeta::new(wallet,true),AccountMeta::new(nonce,false),AccountMeta::new_readonly(Pubkey::default(),false)]};
        let r=m.process_transaction_instructions(&[init],&a);
        if r.program_result.is_err(){return Err(format!("fixture nonce: {:?}",r.program_result));}a=r.resulting_accounts;
    }
    for policy in f["policies"].as_array().unwrap() {
        let publish=ix(policy);let authority=publish.accounts[0].pubkey;let address=publish.accounts[1].pubkey;
        put(&mut a,authority,Account{lamports:1_000_000_000,..Account::default()});put(&mut a,address,Account::default());
        let r=m.process_transaction_instructions(&[publish],&a);
        if r.program_result.is_err(){return Err(format!("fixture policy: {:?}",r.program_result));}a=r.resulting_accounts;
    }
    Ok(a)
}

fn host_account(key:Pubkey,a:&Account)->skew_execution_host::feed::Account {
    skew_execution_host::feed::Account{key:key.to_string(),owner:a.owner.to_string(),executable:a.executable,lamports:a.lamports,data:a.data.clone()}
}
fn host_snapshot(a:&Accounts,slot:u64)->skew_execution_host::feed::Snapshot {
    let now=std::time::Instant::now();
    skew_execution_host::feed::Snapshot{slot,generation:1,revision:1,hash:[0;32],observed:now,slot_advanced:now,
        accounts:a.iter().map(|(k,a)|host_account(*k,a)).collect()}
}
fn check_setup_projection(m:&mut Mollusk,a:&Accounts,p:&Value)->Result<(),String> {
    use skew_execution_host::{swap_wire::{AccountView,TokenAsset},wallet_wire::WalletSetup};
    let wallet=key(&p["wallet"]);
    let payout:Option<skew_execution_host::native_payout::NativePayout>=serde_json::from_value(p["nativePayout"].clone()).map_err(|e|e.to_string())?;
    let assets=p["assets"].as_array().unwrap().iter().filter(|v|payout.is_none()||key(&v["mint"])!=WSOL)
        .map(|v|TokenAsset{token:key(&v["token"]),mint:key(&v["mint"]),token_program:key(&v["tokenProgram"])}).collect::<Vec<_>>();
    let read=|k:&Pubkey|{let v=get(a,k);Ok(AccountView{owner:v.owner,executable:v.executable,data:&v.data})};
    let wrap=p["wrapLamports"].as_u64().unwrap();
    let mut setup=WalletSetup::plan(wallet,&assets,(wrap>0).then_some(wrap),read)?.with_nonce(key(&p["program"]),0,read)?;
    if let Some(payout)=payout {setup=setup.with_native_payout(payout,read)?;}
    let Some(bytes)=setup.compile_setup_only(&[],[0x53;32],1_400_000)? else{return Ok(());};
    let original=host_snapshot(a,m.sysvars.clock.slot);setup.validate_setup_only_message(&original,&bytes)?;
    let mut data=vec![2];data.extend(1_400_000u32.to_le_bytes());
    let mut instructions=vec![Instruction{program_id:pubkey!("ComputeBudget111111111111111111111111111111"),accounts:vec![],data}];
    instructions.extend_from_slice(setup.instructions());
    let r=m.process_transaction_instructions(&instructions,a);
    if r.program_result.is_err(){return Err(format!("setup prefix SBF: {:?}",r.program_result));}
    let mut returned=setup.setup_only_addresses().iter().map(|k|host_account(*k,get(&r.resulting_accounts,k))).collect::<Vec<_>>();
    // Mollusk does not debit Bank transaction fees. Model that explicit outer
    // Bank step after real setup execution, and require exact fee accounting.
    returned.iter_mut().find(|v|v.key==wallet.to_string()).unwrap().lamports-=5_000;
    setup.lowering_projection(&original,&returned,5_000)?;
    if setup.lowering_projection(&original,&returned,0).is_ok(){return Err("setup ignored payer fee".into());}
    Ok(())
}

fn execute(m:&mut Mollusk,row:&Value,observation:&Value,programs:&Value)->Result<Value,String> {
    let f=&row["measurement"]["sbfFixture"];let p=&f["prestate"];
    let instructions=f["instructions"].as_array().unwrap().iter().map(ix).collect::<Vec<_>>();
    // This minimal SVM has no Bank's budget preprocessor. Apply only the exact
    // validated limits requested by the signed wire, then retain its builtin
    // instructions at the same indices (including instructions-sysvar users).
    m.compute_budget.heap_size=32_768;
    m.compute_budget.compute_unit_limit=1_400_000;
    let mut seen_budget=BTreeSet::new();
    for instruction in instructions.iter().filter(|v|v.program_id==pubkey!("ComputeBudget111111111111111111111111111111")) {
        if instruction.data.len()!=5 || !seen_budget.insert(instruction.data[0]) {return Err("invalid/duplicate budget request".into());}
        let n=u32::from_le_bytes(instruction.data[1..].try_into().unwrap());
        match instruction.data[0] {
            1 if (32_768..=262_144).contains(&n) && n%1024==0=>m.compute_budget.heap_size=n,
            2 if n>0 && n<=1_400_000=>m.compute_budget.compute_unit_limit=u64::from(n),
            _=>return Err("unsupported budget request".into()),
        }
    }
    let table=AddressLookupTableAccount{key:key(&f["lookupTable"]["key"]),addresses:f["lookupTable"]["addresses"].as_array().unwrap().iter().map(key).collect()};
    let message=skew_execution_host::onebook_wire::compile_unsigned_v0(key(&p["wallet"]),&instructions,std::slice::from_ref(&table),[0x53;32])?;
    if hash(&message)!=row["measurement"]["messageSha256"] || STANDARD.encode(&message)!=f["unsignedMessage"] {return Err("wire mismatch".into());}
    m.logger=Some(Default::default());
    let a=prepare(m,row,observation,programs).map_err(|e|format!("{e}; logs={:?}",m.logger.as_ref().unwrap().borrow().get_recorded_content()))?;
    check_setup_projection(m,&a,p)?;
    m.logger=Some(Default::default());
    // Compute-budget instructions are retained in the exact instruction list.
    // Mollusk's configured transaction limit matches the signed request.
    let r=m.process_transaction_instructions(&instructions,&a);
    if r.program_result.is_err() {
        return Ok(json!({"passed":false,"stage":"transaction","result":format!("{:?}",r.program_result),
            "rawResult":format!("{:?}",r.raw_result),"logs":m.logger.as_ref().unwrap().borrow().get_recorded_content(),
            "computeUnits":r.compute_units_consumed,"rollback":r.resulting_accounts==a}));
    }
    let nonce=get(&r.resulting_accounts,&key(&p["nonce"]));
    if nonce.data.len()!=64 || &nonce.data[..8]!=b"SKEWSEQ1" || u64_at(&nonce.data,40)!=1 {return Err("nonce did not advance exactly once".into());}
    let input=p["inputAtoms"].as_str().unwrap().parse::<u64>().unwrap();
    let source=p["assets"].as_array().unwrap().iter().find(|v|v["mint"]==p["inputMint"]).unwrap();
    let before=source["startingAtoms"].as_u64().unwrap()+p["wrapLamports"].as_u64().unwrap();
    let after=u64_at(&get(&r.resulting_accounts,&key(&source["token"])).data,64);
    if before.checked_sub(after)!=Some(input) {return Err("source debit mismatch".into());}
    let native_output:Option<skew_execution_host::native_payout::NativeOutput>=serde_json::from_value(row["measurement"]["nativeOutput"].clone()).map_err(|e|e.to_string())?;
    let settlement_index=instructions.len()-if native_output.is_some(){2}else{1};
    let opcode=instructions[settlement_index].data[0];
    let expected=match opcode {13=>"SKEWEXP1",14=>"SKEWMSR1",18 if instructions[settlement_index].data[1]==1=>"SKEWMSF1",18=>"SKEWMSF2",20=>"SKEWSTK2",_=>return Err("unexpected settlement opcode".into())};
    let tag=if native_output.is_some(){"NATIVE_SOL_NONCE_AND_LAMPORTS".to_owned()}else{
        let tag=String::from_utf8_lossy(r.return_data.get(..8).ok_or("missing execution receipt")?).to_string();
        if tag!=expected{return Err(format!("receipt tag {tag} != {expected}"));}tag
    };
    let native_received=if let Some(native)=&native_output {
        let decoded=skew_execution_host::pipeline::decode(&message)?;
        let mut keys=decoded.static_account_keys().iter().map(ToString::to_string).collect::<Vec<_>>();
        if let solana_message::VersionedMessage::V0(v)=&decoded {
            for l in &v.address_table_lookups {keys.extend(l.writable_indexes.iter().map(|i|table.addresses[usize::from(*i)].to_string()));}
            for l in &v.address_table_lookups {keys.extend(l.readonly_indexes.iter().map(|i|table.addresses[usize::from(*i)].to_string()));}
        }
        let amounts=|accounts:&Accounts|keys.iter().map(|k|get(accounts,&k.parse().unwrap()).lamports).collect::<Vec<_>>();
        // Fixture metadata is derived solely from real program results, not a
        // hand-authored successful transfer. No network/finality claim.
        let meta=json!({"preBalances":amounts(&a),"postBalances":amounts(&r.resulting_accounts),"fee":0});
        let received=native.verify_receipt(&decoded,&keys,&meta)?;
        if received!=u64_at(&nonce.data,56) {return Err("native payout/settlement receipt mismatch".into());}
        let mut returned=r.resulting_accounts.iter().map(|(k,a)|host_account(*k,a)).collect::<Vec<_>>();
        returned.iter_mut().find(|v|v.key==p["wallet"].as_str().unwrap()).unwrap().lamports-=5_000;
        if native.verify_simulation(&host_snapshot(&a,m.sysvars.clock.slot),&returned,5_000)?!=received {
            return Err("native simulation fee accounting".into());
        }
        let wallet=key(&p["wallet"]);let old=Pubkey::find_program_address(&[wallet.as_ref(),skew_execution_host::native_payout::TOKEN.as_ref(),WSOL.as_ref()],&ATA).0;
        if get(&a,&old)!=get(&r.resulting_accounts,&old){return Err("existing wrapped SOL modified".into());}
        Some(received)
    }else{None};
    let received=p["assets"].as_array().unwrap().iter().filter(|v|v["mint"]!=p["inputMint"]).map(|asset| {
        let amount=if let Some(amount)=native_received.filter(|_|asset["mint"]==WSOL.to_string()){amount}else{u64_at(&get(&r.resulting_accounts,&key(&asset["token"])).data,64)};
        json!({"mint":asset["mint"],"atoms":amount.to_string()})
    }).collect::<Vec<_>>();
    if !received.iter().any(|v|v["atoms"].as_str().unwrap()!="0") {return Err("no received asset".into());}
    // Reusing the settlement after success must not debit twice. First-use
    // setup is intentionally omitted here to exercise the nonce guard itself.
    let replay=m.process_transaction_instructions(&instructions[settlement_index..=settlement_index],&r.resulting_accounts);
    if !matches!(&replay.program_result,mollusk_svm::result::types::TransactionProgramResult::Failure(0,e) if u64::from(e.clone())==2)
        || replay.resulting_accounts!=r.resulting_accounts {return Err(format!("replay protection failure: {:?}",replay.program_result));}
    let mut floor=instructions.clone();let data=&mut floor[settlement_index].data;
    let offset=match opcode {13=>12,14=>40,18=>8+usize::from(u16::from_le_bytes([data[4],data[5]]))+40,20=>40,_=>unreachable!()};
    let above_observed=u64_at(&nonce.data,56).checked_add(1).ok_or("floor overflow")?;
    data[offset..offset+8].copy_from_slice(&above_observed.to_le_bytes());
    let failed=m.process_transaction_instructions(&floor,&a);
    if !matches!(&failed.program_result,mollusk_svm::result::types::TransactionProgramResult::Failure(index,e) if *index==settlement_index && u64::from(e.clone())==5)
        || failed.resulting_accounts!=a {return Err(format!("minimum output rollback failure: {:?}",failed.program_result));}
    Ok(json!({"passed":true,"computeUnits":r.compute_units_consumed,"sourceDebit":input.to_string(),"received":received,
        "receiptTag":tag,"receipt":STANDARD.encode(&r.return_data),"nonceAfter":1,"duplicateDebitRejected":true,
        "nativeSolReceived":native_received.map(|v|v.to_string()),"existingWrappedSolPreserved":native_received.map(|_|true),
        "setupProjectionWithExplicitBankFee":true,"realizedSettlementValue":u64_at(&nonce.data,56).to_string(),
        "minimumOutputRollback":true,"replayCode":format!("{:?}",replay.program_result),"failureCode":format!("{:?}",failed.program_result),"messageSha256":hash(&message)}))
}

fn main() {
    assert!(cfg!(target_os="linux"),"Cherry execution only");
    let config_path=isolated(&std::env::args().nth(1).expect("isolated config"));let config=read(&config_path);
    let wire=isolated(config["wire"].as_str().unwrap());let archive=isolated(config["programs"].as_str().unwrap());
    let output=isolated(config["output"].as_str().unwrap());assert!(!output.exists());fs::create_dir(&output).unwrap();
    let programs=read(&archive.join("report.json"));let inventory=read(&wire.join("report.json"));
    let mut m=Mollusk::default();m.compute_budget.compute_unit_limit=1_400_000;
    m.program_cache.add_builtin(mollusk_svm::program::Builtin{
        program_id:pubkey!("ComputeBudget111111111111111111111111111111"),name:"compute_budget_program",
        entrypoint:solana_compute_budget_program::Entrypoint::vm});
    for program in programs["programs"].as_array().unwrap() {
        let bytes=fs::read(archive.join("programs").join(format!("{}.so",program["name"].as_str().unwrap()))).unwrap();
        assert_eq!(hash(&bytes),program["elfSha256"]);m.add_program_with_loader_and_elf(&key(&program["id"]),&key(&program["loader"]),&bytes);
    }
    if config["basketWire"]==true {basket_proof::run(&mut m,&wire,&output,&programs).unwrap();return;}
    let filter=config["instruments"].as_array().map(|v|v.iter().map(|x|x.as_str().unwrap().to_owned()).collect::<BTreeSet<_>>());
    let mut rows=Vec::new();
    if config["dynamicConformance"]==true {
        let row=inventory["rows"].as_array().unwrap().iter().find(|r|r["instrument"]=="PEP"&&r["side"]=="BUY"&&r["inputSymbol"]=="USDC"&&r["firstUse"]==false).unwrap();
        let observation=read(&wire.join("PEP-complete.json"));
        let result=dynamic_conformance::check(&mut m,row,&observation,&programs).unwrap();
        save(&output.join("dynamic-conformance.json"),&result);
        assert!(result["checked"].as_u64().unwrap()>100);assert_eq!(result["failures"],0);
    }
    for (index,row) in inventory["rows"].as_array().unwrap().iter().enumerate() {
        let instrument=row["instrument"].as_str().unwrap();
        if filter.as_ref().is_some_and(|v|!v.contains(instrument)) || !row["measurement"]["sbfFixture"].is_object() {continue;}
        let observation=read(&wire.join(format!("{instrument}-complete.json")));
        let mut result=execute(&mut m,row,&observation,&programs).unwrap_or_else(|error|json!({"passed":false,"stage":"fixture_or_invariant","error":error}));
        let mut attempts=vec![result.clone()];
        if result["passed"]!=true {
            for alternative in row["measurement"]["sbfAlternatives"].as_array().into_iter().flatten() {
                let mut candidate=row.clone();candidate["measurement"]=alternative.clone();
                result=execute(&mut m,&candidate,&observation,&programs).unwrap_or_else(|error|json!({"passed":false,"stage":"fixture_or_invariant","error":error}));
                attempts.push(result.clone());
                if result["passed"]==true {break;}
            }
        }
        result["candidateAttempts"]=json!(attempts.len());
        if attempts.len()>1 {result["priorRejected"]=json!(&attempts[..attempts.len()-1]);}
        println!("{index} {instrument} {} {} first={} passed={} {}",row["side"],row["inputSymbol"].as_str().or(row["outputSymbol"].as_str()).unwrap_or(""),row["firstUse"],result["passed"],result["result"].as_str().or(result["error"].as_str()).unwrap_or(""));
        let item=json!({"index":index,"instrument":instrument,"side":row["side"],"inputSymbol":row["inputSymbol"],"outputSymbol":row["outputSymbol"],"productId":row["productId"],"firstUse":row["firstUse"],"slot":row["slot"],"result":result});
        save(&output.join(format!("{index:04}.json")),&item);rows.push(item);
    }
    let passed=rows.iter().filter(|v|v["result"]["passed"]==true).count();
    save(&output.join("report.json"),&json!({"schema":"stockmesh.actual-program-universe-sbf/v1","scope":"CAPTURED_MARKET_STATE_AND_DEPLOYED_ELFS_FIXTURE_WALLET_POLICY_ALT_NO_NETWORK_TRANSACTION",
        "wireReportSha256":hash(&fs::read(wire.join("report.json")).unwrap()),"programReportSha256":hash(&fs::read(archive.join("report.json")).unwrap()),
        "configSha256":hash(&fs::read(&config_path).unwrap()),"runtime":"mollusk-svm 0.13.4 default features; not a mainnet Bank replay",
        "passed":passed,"cases":rows.len(),"rows":rows,"signedTransactions":0,"submittedTransactions":0,"liveGateCompleted":false}));
    println!("SBF passed {passed}/{}",rows.len());
}
