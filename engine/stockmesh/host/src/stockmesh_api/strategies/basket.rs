//! The public boundary accepts a publication, budget and wallet, never routes,
//! transaction bytes, simulation results or fabricated receipt expectations.
use super::*;
use crate::direct_prepare::{BasketCandidate,BasketPrepareRequest,BasketStockQuote};

struct Stored { candidate:BasketCandidate,expires:Instant,expires_at_ms:u64 }
#[derive(Default)]
pub(in crate::stockmesh_api) struct BasketCache {entries:BTreeMap<String,Stored>}
impl BasketCache {
    pub(in crate::stockmesh_api) fn contains(&self,id:&str)->bool{self.entries.contains_key(id)}
    pub(in crate::stockmesh_api) fn admit(&mut self,id:String,candidate:BasketCandidate,ttl:Duration)->Result<Value>{
        candidate.prepared.validate_state(&candidate.feed,&candidate.snapshot)?;
        self.entries.retain(|_,v|v.expires>Instant::now());
        if self.entries.len()>=64||self.entries.contains_key(&id){return Err("basket prepared capacity/identity".into());}
        let expires_at_ms=now_ms()?.checked_add(ttl.as_millis().try_into().map_err(|_|"basket expiry")?).ok_or("basket expiry")?;
        let v=candidate.prepared.review();let expected=candidate.prepared.expected();
        let review=json!({"schema":"xtxc.strategy-prepared/v1","quoteId":id,"preparedId":candidate.prepared.prepared_id(&id)?,
            "owner":expected.wallet_owner(),"expiresAt":iso8601(expires_at_ms),"transactionBase64":v["unsignedTransactionBase64"],
            "lastValidBlockHeight":v["lastValidBlockHeight"].as_u64().ok_or("basket height")?.to_string(),
            "execution":v["execution"],"investment":expected.investment(),"submitAllowed":false});
        self.entries.insert(id,Stored{candidate,expires:Instant::now()+ttl,expires_at_ms});Ok(review)
    }
    fn authorize(&self,r:&SubmitRequest,wire:&[u8])->Result<crate::journal::Entry>{
        let row=self.entries.get(&r.quote_id).ok_or("basket prepared absent")?;
        let c=&row.candidate;
        if row.expires<=Instant::now()||row.expires_at_ms<=now_ms()?||c.prepared.expected().wallet_owner()!=r.owner
            ||c.prepared.prepared_id(&r.quote_id)?!=r.prepared_id{return Err("basket prepared owner/expiry/identity".into());}
        c.prepared.authorize(&c.feed,&c.snapshot,wire,r.quote_id.clone())
    }
}

impl StockMesh {
    pub(in crate::stockmesh_api) fn continue_strategy(&self,p:Published,owner:String,total:String,nonce:String,catalog:String,bps:u16,prepare_next:bool)->std::result::Result<Value,ApiReply>{
        use crate::investment::{Plan,Progress};
        let plan=Plan::from_publication(&p,owner.clone(),total.clone(),nonce,bps)
            .map_err(|_|ApiReply::error(422,"STOCKLANA_INVESTMENT_BOUNDS","Check the investment amount and composition."))?;
        let unavailable=||ApiReply::error(503,"STOCKLANA_INVESTMENT_HISTORY","Check this investment again shortly.");
        let source=self.sender.as_ref().ok_or_else(unavailable)?;
        let mut sender=source.lock().map_err(|_|unavailable())?;
        let ids=sender.journal.entries().filter(|e|e.expected_basket.as_ref().and_then(crate::basket_wire::ExpectedBasket::investment).is_some_and(|t|t.plan.id==plan.id))
            .map(|e|e.id.clone()).collect::<Vec<_>>();
        let active=ids.iter().filter(|id|sender.journal.get(id).is_some_and(|e|!matches!(e.phase,Phase::Failed|Phase::Reconciled))).cloned().collect::<Vec<_>>();
        // A step never sends. Lost acknowledgements/reload look up the SAME
        // durable entries. Observation failure retains the existing phase.
        if !active.is_empty(){let _=sender.observe_batch(&active.iter().map(String::as_str).collect::<Vec<_>>());}
        for id in &active{if sender.journal.get(id).is_some_and(|e|e.phase==Phase::Finalized){let _=sender.reconcile_basket(id);}}
        let (progress,start)=plan.cursor(sender.journal.entries()).map_err(|_|unavailable())?;
        let orders=ids.iter().filter_map(|id|sender.journal.get(id)).map(|e|{let t=e.expected_basket.as_ref().unwrap().investment().unwrap();
            json!({"preparedId":e.id,"quoteId":e.quote_id,"phase":phase_name(&e.phase),"index":t.index,"stockRange":t.range().expect("validated journal range")})}).collect::<Vec<_>>();
        let index=match progress{Progress::Ready(i)|Progress::Pending(i)=>i,Progress::Complete=>orders.iter().filter(|o|o["phase"]=="RECONCILED").count()};
        let completed=plan.allocations[..start].iter().map(|a|a.input_atoms.parse::<u64>().expect("validated investment amount")).sum::<u64>();
        let status=json!({"schema":"xtxc.strategy-investment/v1","plan":plan,"state":match progress{Progress::Ready(_)=>"READY",Progress::Pending(_)=>"PENDING",Progress::Complete=>"COMPLETE"},
            "nextIndex":index,"nextStockIndex":start,"completedInputAtoms":completed.to_string(),"orders":orders});
        drop(sender);
        if prepare_next&&matches!(progress,Progress::Ready(_)){
            let left=plan.allocations.len()-start;
            let tranche=plan.adaptive_tranche(index,start,if left==4{2}else{left.min(crate::basket_wire::MAX_ATOMIC_STOCKS)}).map_err(|_|unavailable())?;
            self.prepare_strategy_tranche(p,owner,total,catalog,bps,Some(tranche))
        }else{Ok(status)}
    }
    pub(super) fn prepare_strategy(&self,p:Published,owner:String,total:String,catalog_revision:String,bps:u16)->std::result::Result<Value,ApiReply>{
        self.prepare_strategy_tranche(p,owner,total,catalog_revision,bps,None)
    }
    pub(super) fn prepare_strategy_tranche(&self,p:Published,owner:String,total:String,catalog_revision:String,bps:u16,tranche:Option<crate::investment::Tranche>)->std::result::Result<Value,ApiReply>{
        let request_error=||ApiReply::error(422,"STOCKLANA_BASKET_BOUNDS","Check the investment amount and composition.");
        let range=tranche.as_ref().map(|t|t.range()).transpose().map_err(|_|request_error())?.unwrap_or(0..p.document.legs.len());
        let stocks=&p.document.legs[range.clone()];
        if !(1..=100).contains(&bps)||decode_key(&owner).is_err()||!((if tranche.is_some(){1}else{2})..=crate::basket_wire::MAX_ATOMIC_STOCKS).contains(&stocks.len()) {
            return Err(request_error());}
        let (amounts,_)=p.document.allocations(&total).map_err(|_|request_error())?;
        let runtime=self.prepare_runtime.as_ref().ok_or_else(||ApiReply::error(503,"STOCKLANA_PREPARE_UNAVAILABLE","Try this investment again shortly."))?;
        if !self.deployment_ready&&!self.captured_fixture{return Err(ApiReply::error(503,"STOCKLANA_PREPARE_UNAVAILABLE","Try this investment again shortly."))}
        let prepare=(||->Result<Value>{
            let mut lanes=Vec::new();
            for stock in stocks{
                let key=IntentKey{instrument:stock.instrument.clone(),input_symbol:"USDC".into()};
                let index=*self.intent_bank.get(&key).ok_or("basket stock lane absent")?;
                self.bank_activity.mark(index);
                lanes.extend(self.banks[index].layout()?.lanes.iter().filter(|l|l.key.instrument==key.instrument&&l.key.input_symbol=="USDC").cloned());
            }
            let keys=lanes.iter().flat_map(|l|&l.configs).map(WorldConfig::keys).collect::<Result<Vec<_>>>()?
                .into_iter().flatten().collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
            let (feed,snapshot)=runtime.basket_discovery(keys.clone())?;
            // No stale per-stock quote stitching, no mutation of the publisher.
            let mut worlds=BTreeMap::new();
            for lane in &lanes{let mut compiled=Vec::new();for config in &lane.configs{
                for market in &config.markets{if native_wire::execution_dependencies(market,&snapshot)?.iter().any(|k|!keys.contains(&k.to_string())){return Err("basket dependency rotation required".into());}}
                let first=config.markets.first().ok_or("basket empty world")?;
                compiled.push(Arc::new(config.compile_admitted_pair(&snapshot,None,&first.input_mint,&first.output_mint)?));
            }worlds.insert(lane.key.clone(),compiled);}
            let joint=Publication{layout:Arc::new(BankLayout{feed,lanes}),snapshot,worlds,published_at_ms:now_ms()?};
            let mut quotes=Vec::new();
            for (stock,amount) in stocks.iter().zip(amounts[range].iter().copied()){
                let plan=solve_exposure(&joint,&IntentKey{instrument:stock.instrument.clone(),input_symbol:"USDC".into()},amount)?;
                let proposals=if plan.global_reflow{plan.native_candidates}else{plan.native_allocation};
                let mut products=Vec::new();
                for id in proposals.iter().filter_map(|v|v.product_id.as_ref()).collect::<BTreeSet<_>>(){
                    let l=joint.layout.lanes.iter().find(|l|l.key.product_id==*id&&l.key.instrument==stock.instrument).ok_or("basket selected product")?;
                    let quoted=plan.products.iter().find(|p|p.product_id==*id).map(|p|p.raw_output_atoms).unwrap_or(0);
                    products.push(PrepareProduct{product_id:id.clone(),identity:ProductIdentity{instrument:l.key.instrument.clone(),issuer:l.issuer.clone(),mint:l.output_mint.clone(),token_program:l.token_program.clone(),rights_hash:decode_hex_32(&l.rights_hash)?,raw_decimals:l.output_decimals},
                        model:match l.exposure_model.as_str(){"FIXED_RATIONAL"=>0,"TOKEN_2022_SCALED_UI"=>1,_=>return Err("basket exposure model".into())},
                        numerator:l.exposure_numerator,denominator:l.exposure_denominator,conservative_bps:l.conservative_bps,minimum_output_atoms:floor(quoted,bps)?});
                }
                let minimum_cash_atoms=crate::basket_wire::funding_cash(&proposals)?.map(|(_,q)|floor(q,bps)).transpose()?;
                quotes.push(BasketStockQuote{instrument:stock.instrument.clone(),minimum_exposure_q32:floor(plan.exposure_q32,bps)?,products,proposals,economic_reflow:plan.global_reflow,minimum_cash_atoms});
            }
            // Shared pools execute sequentially in ONE exact simulation. Each
            // target must meet its own floor; no other stock can cover a miss.
            let candidate=runtime.prepare_basket(&keys,&joint.snapshot,BasketPrepareRequest{publication:p.clone(),owner:Pubkey::new_from_array(decode_key(&owner)?),total_input_atoms:total.clone(),catalog_revision:catalog_revision.clone(),quotes,tranche:tranche.clone()})?;
            if self.catalog.read().map_err(|_|"basket catalog lock")?.revision!=catalog_revision{return Err("basket catalog changed during prepare".into());}
            let id=self.quote_id(&IntentKey{instrument:p.strategy_id.clone(),input_symbol:"USDC".into()},parse_u64_string(&total,false)?,0,candidate.snapshot.hash,&p.version_hash);
            let mut review=self.basket_prepared.lock().map_err(|_|"basket cache lock")?.admit(id,candidate,self.quote_ttl.min(Duration::from_secs(15)))?;
            review["strategyId"]=json!(p.strategy_id);review["versionHash"]=json!(p.version_hash);review["inputAtoms"]=json!(total);
            review["submitAllowed"]=json!(self.sender.is_some()&&self.deployment_ready&&!self.captured_fixture);
            Ok(review)
        })();
        match prepare{
            Err(ref e) if crate::basket_wire::capacity_error(e)=>{
                if let Some(t)=tranche{if let Some(smaller)=t.smaller_after(e).map_err(|_|request_error())?{
                    return self.prepare_strategy_tranche(p,owner,total,catalog_revision,bps,Some(smaller));
                }}
                Err(ApiReply::error(409,"STOCKLANA_BASKET_SPLIT_REQUIRED","Continue with a smaller purchase."))
            },
            result=>result.map_err(|_|ApiReply::error(409,"STOCKLANA_BASKET_REFRESH","Refresh this investment quote.")),
        }
    }
    pub(in crate::stockmesh_api) fn submit_basket(&self,r:&SubmitRequest,wire:&[u8])->ApiReply{
        let entry=match self.basket_prepared.lock().map_err(|_|"basket cache".into()).and_then(|c|c.authorize(r,wire)){
            Ok(e)=>e,Err(_)=>return ApiReply::error(409,"STOCKLANA_PREPARED_REVOKED","Review the investment again.")};
        let Some(sender)=&self.sender else{return ApiReply::error(503,"STOCKLANA_SUBMISSION_DISABLED","Trading is temporarily unavailable.")};
        let mut sender=match sender.lock(){Ok(s)=>s,Err(_)=>return ApiReply::error(503,"STOCKLANA_SENDER_UNAVAILABLE","Check your order again shortly.")};
        if let Some(reply)=recover_submission(&mut sender,r,wire){return reply;}
        if sender.journal.insert(entry.clone()).is_err(){return ApiReply::error(409,"STOCKLANA_SUBMISSION_CONFLICT","Check the existing order before trying again.")}
        // Durable reservation precedes send. All retries use observe-only
        // recovery, including transport failure after a node received the wire.
        let _=sender.step(&entry.id);
        recover_submission(&mut sender,r,wire).unwrap_or_else(||ApiReply::error(503,"STOCKLANA_SUBMISSION_UNCERTAIN","Check your order again shortly."))
    }
}
fn floor(value:u64,bps:u16)->Result<u64>{u64::try_from(u128::from(value)*u128::from(10000-bps)/10000).map(|v|v.max(1)).map_err(|_|"basket floor overflow".into())}

#[cfg(test)]mod tests{
 use super::*;
 #[test]fn investment_cache_binds_owner_wire_identity_expiry_and_bank(){
  let (candidate,entry)=crate::basket_wire::candidate_fixture();let feed=candidate.feed.clone();
  let id=entry.quote_id.clone().unwrap();let mut cache=BasketCache::default();
  let v=cache.admit(id.clone(),candidate,Duration::from_secs(15)).unwrap();assert_eq!(v["preparedId"],entry.id);assert_eq!(v["submitAllowed"],false);
  if let Ok(path)=std::env::var("SKEW_BASKET_UI_VECTOR"){
   assert!(path.starts_with("/srv/skew/stockmesh-direct-node-20260920/evidence/stock-invest-")&&path.ends_with("/basket-ui-vector.json"));
   let (_,transaction)=crate::basket_wire::recovery_fixture();let receipt=entry.expected_basket.as_ref().unwrap().verify(&entry,&transaction).unwrap();
   use std::io::Write;let mut file=std::fs::OpenOptions::new().create_new(true).write(true).open(path).unwrap();
   file.write_all(&serde_json::to_vec_pretty(&json!({"scope":"SYNTHETIC_CACHE_RECEIPT_CONTRACT_NOT_MAINNET_EXECUTION","prepared":v,"receipt":receipt})).unwrap()).unwrap();
  }
  let mut r=SubmitRequest{quote_id:id.clone(),prepared_id:entry.id.clone(),owner:entry.wallet_owner().unwrap().into(),signed_transaction_base64:STANDARD.encode(&entry.wire)};
  assert_eq!(cache.authorize(&r,&entry.wire).unwrap().signature,entry.signature);
  let owner=r.owner.clone();r.owner=Pubkey::default().to_string();assert!(cache.authorize(&r,&entry.wire).is_err());r.owner=owner;
  r.prepared_id=format!("stkp_{}","f".repeat(32));assert!(cache.authorize(&r,&entry.wire).is_err());r.prepared_id=entry.id.clone();
  let mut wire=entry.wire.clone();wire[70]^=1;assert!(cache.authorize(&r,&wire).is_err());
  cache.entries.get_mut(&id).unwrap().expires=Instant::now();assert!(cache.authorize(&r,&entry.wire).is_err());
  cache.entries.get_mut(&id).unwrap().expires=Instant::now()+Duration::from_secs(10);feed.invalidate().unwrap();assert!(cache.authorize(&r,&entry.wire).is_err());
 }
}
