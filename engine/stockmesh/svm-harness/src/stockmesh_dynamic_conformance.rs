//! Invoked only by the opt-in universe proof. Counterfactual fee windows are
//! explicitly labeled; this is exact deployed-DEX conformance, not live fills.
use super::*;
use skew_execution_host::{world::{WorldConfig,NativeSwapProposal},native_wire,swap_wire::Budget};

pub(super) fn check(m:&mut Mollusk,row:&Value,observation:&Value,programs:&Value)->Result<Value,String> {
    let original=prepare(m,row,observation,programs)?;
    let p=&row["measurement"]["sbfFixture"]["prestate"];let wallet=key(&p["wallet"]);
    let worlds:Vec<WorldConfig>=serde_json::from_value(observation["worlds"].as_array().ok_or("worlds")?.iter()
        .find(|v|v["inputSymbol"]=="USDC").ok_or("cash lane")?["worlds"].clone()).map_err(|e|e.to_string())?;
    if worlds.len()!=1 || worlds[0].markets.len()!=1 {return Err("single dynamic pool only".into());}
    let market=worlds[0].markets[0].clone();let pool:Pubkey=market.pool.parse().map_err(|_|"pool")?;
    let observed_time=m.sysvars.clock.unix_timestamp as u64;
    let mut rows=Vec::new();
    for mode in [None,Some((0u8,0u64)),Some((0,59)),Some((0,60)),Some((0,599)),Some((0,600)),Some((1,0)),Some((1,60)),Some((2,0)),Some((2,600))] {
        for sell in [false,true] {
            let config=if sell{market.reversed()}else{market.clone()};let mut a=original.clone();
            if let Some((fee_on,elapsed))=mode {
                let data=&mut a.iter_mut().find(|v|v.0==pool).unwrap().1.data;
                data[390]=fee_on;data[1122..1130].copy_from_slice(&observed_time.checked_sub(elapsed).ok_or("clock")?.to_le_bytes());
            }
            let bank=host_snapshot(&a,m.sysvars.clock.slot);
            let source=native_wire::wallet_asset(wallet,config.input_mint.parse().unwrap(),&bank)?;
            let destination=native_wire::wallet_asset(wallet,config.output_mint.parse().unwrap(),&bank)?;
            a.iter_mut().find(|v|v.0==source.token).unwrap().1.data[64..72].copy_from_slice(&500_000_000_000u64.to_le_bytes());
            a.iter_mut().find(|v|v.0==destination.token).unwrap().1.data[64..72].copy_from_slice(&0u64.to_le_bytes());
            let bank=host_snapshot(&a,m.sysvars.clock.slot);
            let world=if sell{worlds[0].reversed()}else{worlds[0].clone()};
            let amounts=[1,10,100,1_000,10_000,100_000,1_000_000,10_000_000,100_000_000,500_000_000,1_000_000_000,5_000_000_000,10_000_000_000,100_000_000_000];
            for amount in amounts {
                let proposed=world.probe_declared_pair(&bank,amount).and_then(|v|v["outputAtoms"].as_u64().ok_or_else(||"quote amount".into()));
                let Ok(output)=proposed else {rows.push(json!({"mode":mode,"sell":sell,"input":amount,"admitted":false}));continue;};
                let proposal=NativeSwapProposal{market:config.clone(),stage:1,product_id:None,input_atoms:amount,expected_output_atoms:output};
                let leg=native_wire::lower_native_leg(&proposal,&bank,wallet,source,destination,Budget::Exact(amount))?;
                let mut data=[0;80];let len=leg.venue.swap_data(amount,leg.direction,&mut data).map_err(|e|format!("{e:?}"))?;
                let call=Instruction{program_id:config.program.parse().unwrap(),data:data[..len].to_vec(),accounts:leg.accounts.iter().enumerate().map(|(i,k)|AccountMeta{
                    pubkey:*k,is_signer:i==leg.venue.bindings(leg.direction).1,is_writable:leg.venue.writable(i)}).collect()};
                m.logger=Some(Default::default());let result=m.process_transaction_instructions(&[call],&a);
                let actual=if result.program_result.is_ok(){Some((500_000_000_000-u64_at(&get(&result.resulting_accounts,&source.token).data,64),u64_at(&get(&result.resulting_accounts,&destination.token).data,64)))}else{None};
                rows.push(json!({"mode":mode,"sell":sell,"input":amount,"admitted":true,"expected":output,"actual":actual,
                    "matched":actual==Some((amount,output)),"result":format!("{:?}",result.program_result),"cu":result.compute_units_consumed}));
            }
        }
    }
    Ok(json!({"scope":"CAPTURED_PEP_POOL_AND_DEPLOYED_RAYDIUM_ELF_WITH_EXPLICIT_FEE_WINDOW_COUNTERFACTUALS","rows":rows,
        "checked":rows.iter().filter(|v|v["admitted"]==true).count(),"matched":rows.iter().filter(|v|v["matched"]==true).count(),
        "failures":rows.iter().filter(|v|v["admitted"]==true&&v["matched"]!=true).count()}))
}
