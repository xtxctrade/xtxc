//! One captured Bank, one combined unsigned message, archived deployed ELFs.
//! Wallet/policies/ALT/rent remain explicit fixtures. No network transaction.
use super::*;
fn encoded(a:&Accounts)->Value{json!(a.iter().map(|(k,a)|json!({"key":k.to_string(),"owner":a.owner.to_string(),"lamports":a.lamports,"executable":a.executable,"data":STANDARD.encode(&a.data)})).collect::<Vec<_>>())}
pub(super) fn run(m:&mut Mollusk,wire:&Path,output:&Path,programs:&Value)->Result<(),String>{
    let inventory=read(&wire.join("report.json"));let observation=read(&wire.join("basket-complete.json"));let mut rows=Vec::new();
    for row in inventory["rows"].as_array().ok_or("basket rows")?{
        let fixture=json!({"slot":inventory["slot"],"measurement":{"sbfFixture":row}});
        m.compute_budget.heap_size=262_144;m.compute_budget.compute_unit_limit=1_400_000;
        let before=prepare(m,&fixture,&observation,programs)?;
        let instructions=row["instructions"].as_array().ok_or("basket instructions")?.iter().map(ix).collect::<Vec<_>>();
        let table=AddressLookupTableAccount{key:key(&row["lookupTable"]["key"]),addresses:row["lookupTable"]["addresses"].as_array().unwrap().iter().map(key).collect()};
        let message=skew_execution_host::onebook_wire::compile_unsigned_v0(key(&row["prestate"]["wallet"]),&instructions,&[table],[0x53;32])?;
        if hash(&message)!=row["messageSha256"]||STANDARD.encode(&message)!=row["unsignedMessage"]{return Err("basket exact message mismatch".into());}
        m.logger=Some(Default::default());let r=m.process_transaction_instructions(&instructions,&before);
        if r.program_result.is_err(){save(&output.join(format!("failure-{}.json",row["firstUse"])),&json!({"result":format!("{:?}",r.program_result),"cu":r.compute_units_consumed,"logs":m.logger.as_ref().unwrap().borrow().get_recorded_content()}));return Err(format!("basket execution {:?}",r.program_result));}
        let success_logs=m.logger.as_ref().unwrap().borrow().get_recorded_content().to_vec();
        // Deliberately constrained execution profile, not a mainnet capacity
        // claim: prove the actual VM identifies CU exhaustion and rolls back.
        m.compute_budget.compute_unit_limit=1_000_000;
        // Each invocation has its own bounded collector, just like an RPC
        // simulation response. Prior success logs must not consume this one's
        // byte budget or provide a stale exhaustion line.
        m.logger=Some(Default::default());
        let limited=m.process_transaction_instructions(&instructions,&before);
        let limited_logs=m.logger.as_ref().unwrap().borrow().get_recorded_content().to_vec();
        let limited_error=match &limited.program_result{
            mollusk_svm::result::types::TransactionProgramResult::UnknownError(i,e)=>json!({"InstructionError":[i,e]}),
            _=>Value::Null,
        };
        let recognized=skew_execution_host::basket_wire::computational_exhaustion(&limited_error,limited.compute_units_consumed,1000000,instructions.len(),&json!(limited_logs));
        let constrained=json!({"limit":1000000,"result":format!("{:?}",limited.program_result),"computeUnits":limited.compute_units_consumed,
            "logs":limited_logs,"recognizedExhaustion":recognized,
            "rolledBack":limited.program_result.is_err()&&limited.resulting_accounts==before});
        if limited.program_result.is_err()&&limited.resulting_accounts!=before{return Err("basket capacity failure changed balances".into());}
        if limited.program_result.is_err()&&!recognized{return Err("basket capacity failure not classified".into());}
        m.compute_budget.compute_unit_limit=1_400_000;
        let nonce=key(&row["prestate"]["nonce"]);let n=row["intent"]["targets"].as_array().unwrap().len();
        if u64_at(&get(&r.resulting_accounts,&nonce).data,40)!=n as u64{return Err("basket nonce count".into());}
        let indices=row["settlementIndices"].as_array().unwrap().iter().map(|v|v.as_u64().unwrap()as usize).collect::<Vec<_>>();
        let replay=m.process_transaction_instructions(&instructions[indices[0]..],&r.resulting_accounts);
        if replay.program_result.is_ok()||replay.resulting_accounts!=r.resulting_accounts{return Err("basket duplicate debit".into());}
        let mut failures=Vec::new();
        for index in &indices{
            let mut bad=instructions.clone();let data=&mut bad[*index].data;
            let offset=match data[0]{13=>12,14=>40,18=>8+usize::from(u16::from_le_bytes([data[4],data[5]]))+40,_=>return Err("basket opcode".into())};
            data[offset..offset+8].copy_from_slice(&u64::MAX.to_le_bytes());
            let failed=m.process_transaction_instructions(&bad,&before);
            if failed.program_result.is_ok()||failed.resulting_accounts!=before{return Err("basket all-or-nothing rollback".into());}
            failures.push(json!({"instruction":index,"result":format!("{:?}",failed.program_result),"fullRollback":true}));
        }
        let source=row["prestate"]["assets"].as_array().unwrap().iter().find(|a|a["mint"]==row["prestate"]["inputMint"]).unwrap();let source=key(&source["token"]);
        let spent=row["intent"]["targets"].as_array().unwrap().iter().map(|t|t["inputAtoms"].as_u64().unwrap()).sum::<u64>();
        if u64_at(&get(&before,&source).data,64).checked_sub(u64_at(&get(&r.resulting_accounts,&source).data,64))!=Some(spent){return Err("basket exact cash debit".into());}
        let mut preserved=Vec::new();
        for asset in row["prestate"]["assets"].as_array().unwrap().iter().filter(|a|a["mint"]=="So11111111111111111111111111111111111111112"){
            let address=key(&asset["token"]);let a=get(&before,&address);let b=get(&r.resulting_accounts,&address);
            let starting=if a.data.is_empty(){0}else{u64_at(&a.data,64)};let ending=u64_at(&b.data,64);
            if starting!=ending{return Err("basket existing wSOL changed".into());}
            preserved.push(json!({"mint":asset["mint"],"before":starting.to_string(),"after":ending.to_string()}));
        }
        let mut short=before.clone();let a=short.iter_mut().find(|a|a.0==source).unwrap();a.1.data[64..72].copy_from_slice(&(spent-1).to_le_bytes());
        let insufficient=m.process_transaction_instructions(&instructions,&short);
        if insufficient.program_result.is_ok()||insufficient.resulting_accounts!=short{return Err("basket insufficient cash rollback".into());}
        rows.push(json!({"firstUse":row["firstUse"],"packetBytes":message.len()+65,"messageSha256":hash(&message),"computeUnits":r.compute_units_consumed,
            "nonceAfter":n,"spentAtoms":spent.to_string(),"preservedTransitInventory":preserved,"constrainedProfile":constrained,"duplicateRejected":true,"insufficientCashRollback":true,"floorFailures":failures,
            "before":encoded(&before),"after":encoded(&r.resulting_accounts),"logs":success_logs}));
    }
    save(&output.join("report.json"),&json!({"schema":"stockmesh.basket-sbf/v1","scope":"SINGLE_CAPTURED_BANK_ARCHIVED_ELF_FIXTURE_WALLET_POLICY_ALT_NO_MAINNET_TRANSACTION",
        "wireSha256":hash(&fs::read(wire.join("report.json")).unwrap()),"programs":programs["programs"],"rows":rows,"passed":rows.len(),"submittedTransactions":0,"liveGateCompleted":false}));
    Ok(())
}
