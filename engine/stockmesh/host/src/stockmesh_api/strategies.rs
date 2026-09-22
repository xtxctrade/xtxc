use super::*;
use crate::strategy::{Document,Published,StrategyStore};
mod basket;
pub(super) use basket::BasketCache;
#[derive(Deserialize)]
#[serde(tag="operation",rename_all="SCREAMING_SNAKE_CASE",deny_unknown_fields)]
enum Request {
 List{#[serde(default)] offset:usize,revision:Option<String>},
 Get{#[serde(rename="strategyId")]strategy_id:String,#[serde(rename="versionHash")]version_hash:Option<String>},
 Challenge{document:Document},
 Publish{publication:Published},
 Preview{#[serde(rename="strategyId")]strategy_id:String,#[serde(rename="versionHash")]version_hash:String,#[serde(rename="inputAtoms")]input_atoms:String,#[serde(rename="maxSlippageBps")]max_slippage_bps:u16},
 Prepare{#[serde(rename="strategyId")]strategy_id:String,#[serde(rename="versionHash")]version_hash:String,#[serde(rename="inputAtoms")]input_atoms:String,owner:String,#[serde(rename="catalogRevision")]catalog_revision:String,#[serde(rename="maxSlippageBps")]max_slippage_bps:u16},
 Continue{#[serde(rename="strategyId")]strategy_id:String,#[serde(rename="versionHash")]version_hash:String,#[serde(rename="inputAtoms")]input_atoms:String,owner:String,nonce:String,#[serde(rename="catalogRevision")]catalog_revision:String,#[serde(rename="maxSlippageBps")]max_slippage_bps:u16,#[serde(rename="prepareNext")]prepare_next:bool},
}
impl StockMesh {
 pub fn open_strategy_store(&self,path:&Path)->Result<()> {
  let mut store=self.strategies.lock().map_err(|_|"strategy store lock")?;
  if store.is_some(){return Err("strategy store already configured".into())}*store=Some(StrategyStore::open(path)?);Ok(())
 }
 pub fn strategies(&self,body:&[u8])->ApiReply {
  match self.strategy_operation(body){Ok(v)=>ApiReply::ok(v),Err(e)=>e}
 }
 fn strategy_operation(&self,body:&[u8])->std::result::Result<Value,ApiReply>{
  let request:Request=serde_json::from_slice(body).map_err(|_|ApiReply::error(400,"STOCKLANA_STRATEGY_REQUEST","Check the composition request."))?;
  let catalog=self.catalog.read().map_err(|_|ApiReply::error(503,"STOCKLANA_CATALOG_UNAVAILABLE","Refresh the stock list."))?.clone();
  // The issuer list can leave a well-known stock UNCLASSIFIED. Use the
  // validated native policy admission, not a second UI ticker classification.
  let composition_instruments=catalog.products.iter().filter(|p|p.issuer_tradable!=Some(false)&&(
   matches!(p.kind,AssetKind::Equity|AssetKind::ListedEtf)||
   (p.source_kind==SourceKind::LocalAdmission&&!matches!(p.kind,AssetKind::PreIpo))
  )).map(|p|p.instrument.clone()).collect::<BTreeSet<_>>();
  let mut guard=self.strategies.lock().map_err(|_|ApiReply::error(503,"STOCKLANA_STRATEGY_UNAVAILABLE","Try again shortly."))?;
  let ready=guard.is_some();
  if let Request::List{offset,revision}=&request {
   let total=guard.as_ref().map_or(0,StrategyStore::count);let current=guard.as_ref().map_or("empty",StrategyStore::revision);
   if *offset>total||(*offset>0&&revision.as_deref()!=Some(current)){return Err(ApiReply::error(409,"STOCKLANA_STRATEGY_CHANGED","Refresh the ETF list."))}
   let rows=guard.as_ref().map_or_else(Vec::new,|s|s.list(*offset));let next=offset+rows.len();
   return Ok(json!({"schema":"xtxc.strategies/v1","catalogRevision":catalog.revision,"compositionInstruments":composition_instruments,"revision":current,"publicationEnabled":ready,"total":total,"nextOffset":if next<total{Some(next)}else{None},"strategies":rows}));
  }
  let store=guard.as_mut().ok_or_else(||ApiReply::error(503,"STOCKLANA_STRATEGY_UNAVAILABLE","Publishing is temporarily unavailable."))?;
  let invalid=|_|ApiReply::error(400,"STOCKLANA_STRATEGY_DOCUMENT","Check the name, stocks, weights and signature.");
  let validate_catalog=|d:&Document|->std::result::Result<(),ApiReply>{
   d.validate().map_err(invalid)?;
   if d.catalog_revision!=catalog.revision{return Err(ApiReply::error(409,"STOCKLANA_CATALOG_CHANGED","Refresh the stock list before publishing."))}
   if d.legs.iter().any(|l|!composition_instruments.contains(&l.instrument)){return Err(ApiReply::error(400,"STOCKLANA_STRATEGY_STOCK","Choose stocks or listed ETFs from the catalog."))}Ok(())
  };
  match request {
   Request::List{..}=>unreachable!(),
   Request::Get{strategy_id,version_hash}=>{
    let p=store.get(&strategy_id,version_hash.as_deref()).ok_or_else(||ApiReply::error(404,"STOCKLANA_STRATEGY_NOT_FOUND","ETF not found."))?;
    Ok(json!({"schema":"xtxc.strategy/v1","publication":p,"latestVersionHash":store.get(&strategy_id,None).map(|p|&p.version_hash)}))
   },
   Request::Challenge{document}=>{
    validate_catalog(&document)?;let now=now_ms().map_err(invalid)?/1000;
    if document.issued_at>now.saturating_add(30)||document.expires_at<now{return Err(ApiReply::error(410,"STOCKLANA_STRATEGY_EXPIRED","Review the composition again."))}
    Ok(json!({"schema":"xtxc.strategy-challenge/v1","strategyId":document.strategy_id().map_err(invalid)?,"versionHash":document.version_hash().map_err(invalid)?,"message":document.message().map_err(invalid)?,"document":document}))
   },
   Request::Publish{publication}=>{
    // Retry a durable publication even if its signing window/catalog changed.
    if store.get(&publication.strategy_id,Some(&publication.version_hash)).is_none(){validate_catalog(&publication.document)?;}
    let p=store.publish(publication,now_ms().map_err(invalid)?/1000).map_err(|_|ApiReply::error(409,"STOCKLANA_STRATEGY_PUBLICATION","The version or signature changed. Review before publishing."))?;
    Ok(json!({"schema":"xtxc.strategy/v1","latestVersionHash":store.get(&p.strategy_id,None).map(|p|&p.version_hash),"publication":p}))
   },
   Request::Prepare{strategy_id,version_hash,input_atoms,owner,catalog_revision,max_slippage_bps}=>{
    let p=store.get(&strategy_id,Some(&version_hash)).ok_or_else(||ApiReply::error(404,"STOCKLANA_STRATEGY_NOT_FOUND","ETF version not found."))?.clone();
    if catalog_revision!=catalog.revision{return Err(ApiReply::error(409,"STOCKLANA_CATALOG_CHANGED","Refresh the stock list."))}
    drop(guard);
    self.prepare_strategy(p,owner,input_atoms,catalog_revision,max_slippage_bps)
   },
   Request::Continue{strategy_id,version_hash,input_atoms,owner,nonce,catalog_revision,max_slippage_bps,prepare_next}=>{
    let p=store.get(&strategy_id,Some(&version_hash)).ok_or_else(||ApiReply::error(404,"STOCKLANA_STRATEGY_NOT_FOUND","ETF version not found."))?.clone();
    if catalog_revision!=catalog.revision{return Err(ApiReply::error(409,"STOCKLANA_CATALOG_CHANGED","Refresh the stock list."))}drop(guard);
    self.continue_strategy(p,owner,input_atoms,nonce,catalog_revision,max_slippage_bps,prepare_next)
   },
   Request::Preview{strategy_id,version_hash,input_atoms,max_slippage_bps}=>{
    let p=store.get(&strategy_id,Some(&version_hash)).ok_or_else(||ApiReply::error(404,"STOCKLANA_STRATEGY_NOT_FOUND","ETF version not found."))?.clone();
    let (allocations,cash)=p.document.allocations(&input_atoms).map_err(invalid)?;
    if !(1..=100).contains(&max_slippage_bps){return Err(ApiReply::error(400,"STOCKLANA_STRATEGY_BOUNDS","Check price protection."))}
    // Do not hold the durable metadata lock during price work.
    drop(guard);
    let mut legs=Vec::new();let mut pools=BTreeSet::new();let mut shared=BTreeSet::new();
    for (leg,amount) in p.document.legs.iter().zip(allocations){
     let quote=self.quote(&leg_quote_body(&leg.instrument,amount,max_slippage_bps));
     let mut used=BTreeSet::new();collect_route_pools(&quote.body,&mut used);for pool in used{if !pools.insert(pool.clone()){shared.insert(pool);}}
     legs.push(json!({"instrument":leg.instrument,"weightBps":leg.weight_bps,"inputAtoms":amount.to_string(),"status":quote.status,"quote":quote.body}));
    }
    Ok(json!({"schema":"xtxc.strategy-preview/v1","strategyId":p.strategy_id,"versionHash":p.version_hash,"currentCatalogRevision":catalog.revision,"inputAsset":"USDC","inputAtoms":input_atoms,"cashAtoms":cash.to_string(),"legs":legs,"sharedPools":shared,"atomic":false,"submitAllowed":false}))
   }
  }
 }
}
fn leg_quote_body(instrument:&str,amount:u64,slippage:u16)->Vec<u8>{
 serde_json::to_vec(&json!({"instrument":instrument,"side":"BUY","notional":format!("{}.{:06}",amount/1_000_000,amount%1_000_000),"notionalAtoms":amount.to_string(),"notionalAsset":"USDC","maxSlippageBps":slippage})).expect("fixed stock quote schema")
}
#[cfg(test)]mod tests{
 use super::*;
 #[test]fn basket_uses_the_existing_stockmesh_quote_schema(){
  let bytes=leg_quote_body("NVDA",45000001,20);let _:QuoteRequest=serde_json::from_slice(&bytes).unwrap();
  let value:Value=serde_json::from_slice(&bytes).unwrap();assert_eq!(value["notionalAtoms"],"45000001");assert_eq!(value["notional"],"45.000001");assert_eq!(value["notionalAsset"],"USDC");
 }
}
