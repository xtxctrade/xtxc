//! Whole-composition budget, split into independently owner-signed tranches.
//! The existing execution journal is the only recovery authority. An expired
//! quote, missing acknowledgement or FINALIZED-only row cannot advance a plan.
use crate::{basket_wire::ExpectedBasket,journal::{Entry,Phase},strategy::Published,Result};
use serde::{Deserialize,Serialize};
use sha2::{Digest,Sha256};

#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Allocation{pub instrument:String,pub input_atoms:String}
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Plan{
    pub id:String,pub nonce:String,pub owner:String,pub strategy_id:String,pub version_hash:String,
    pub total_input_atoms:String,pub retained_cash_atoms:String,pub max_slippage_bps:u16,
    pub allocations:Vec<Allocation>,
}
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Tranche{pub plan:Plan,pub index:usize,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub stock_range:Option<StockRange>,
}
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct StockRange{pub start:usize,pub end:usize}
#[derive(Debug,PartialEq,Eq)]
pub enum Progress{Ready(usize),Pending(usize),Complete}
fn hex(s:&str,n:usize)->bool{s.len()==n&&s.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b))}
fn atoms(s:&str)->Result<u64>{let n=s.parse::<u64>().map_err(|_|"investment atoms")?;if n.to_string()!=s{return Err("investment atoms encoding".into());}Ok(n)}
impl Plan{
    pub fn from_publication(p:&Published,owner:String,total:String,nonce:String,bps:u16)->Result<Self>{
        p.verify()?;let (amounts,cash)=p.document.allocations(&total)?;
        let mut plan=Self{id:String::new(),nonce,owner,strategy_id:p.strategy_id.clone(),version_hash:p.version_hash.clone(),
            total_input_atoms:total,retained_cash_atoms:cash.to_string(),max_slippage_bps:bps,
            allocations:p.document.legs.iter().zip(amounts).map(|(l,n)|Allocation{instrument:l.instrument.clone(),input_atoms:n.to_string()}).collect()};
        plan.id=plan.digest()?;plan.validate()?;Ok(plan)
    }
    fn digest(&self)->Result<String>{
        let mut value=self.clone();value.id.clear();let mut h=Sha256::new();h.update(b"XTXC_STOCK_INVESTMENT_V1\0");
        h.update(serde_json::to_vec(&value).map_err(|e|e.to_string())?);
        Ok(format!("stki_{}",h.finalize().iter().map(|b|format!("{b:02x}")).collect::<String>()))
    }
    pub fn validate(&self)->Result<()>{
        let owner=self.owner.parse::<solana_pubkey::Pubkey>().map_err(|_|"investment owner")?;
        if owner==solana_pubkey::Pubkey::default()||owner.to_string()!=self.owner||!hex(&self.nonce,32)||!hex(&self.version_hash,64)
            ||!self.strategy_id.starts_with("stks_")||!hex(&self.strategy_id[5..],64)
            ||!(2..=16).contains(&self.allocations.len())||!(1..=100).contains(&self.max_slippage_bps)||self.id!=self.digest()?{
            return Err("investment identity/bounds".into());}
        let total=atoms(&self.total_input_atoms)?;let mut sum=atoms(&self.retained_cash_atoms)?;let mut names=std::collections::BTreeSet::new();
        if total==0||total>stocklana_adapters::MAX_INPUT{return Err("investment input bound".into());}
        for a in &self.allocations{let n=atoms(&a.input_atoms)?;if !crate::catalog::valid_instrument(&a.instrument)||n==0||!names.insert(&a.instrument){return Err("investment stock allocation".into());}
            sum=sum.checked_add(n).ok_or("investment sum overflow")?;}
        if sum!=total{return Err("investment cash conservation".into());}Ok(())
    }
    /// Stable partition with no singleton, omitted stock or silent rebalance.
    /// Fresh state/fees/signatures are required for each group, not pre-signed.
    pub fn ranges(&self)->Vec<std::ops::Range<usize>>{
        let mut out=Vec::new();let mut start=0;
        while start<self.allocations.len(){let left=self.allocations.len()-start;let n=if left==4{2}else{left.min(3)};out.push(start..start+n);start+=n;}out
    }
    pub fn tranche(&self,index:usize)->Result<Tranche>{self.validate()?;if index>=self.ranges().len(){return Err("investment step".into());}Ok(Tranche{plan:self.clone(),index,stock_range:None})}
    pub fn adaptive_tranche(&self,index:usize,start:usize,count:usize)->Result<Tranche>{
        let t=Tranche{plan:self.clone(),index,stock_range:Some(StockRange{start,end:start.checked_add(count).ok_or("investment range overflow")?})};
        t.range()?;Ok(t)
    }
    pub fn progress<'a>(&self,entries:impl Iterator<Item=&'a Entry>)->Result<Progress>{
        Ok(self.cursor(entries)?.0)
    }
    /// The durable, reconciled prefix defines both step and stock cursor.
    /// Legacy records derive their original fixed ranges; new records carry
    /// exact ranges chosen before signing from measured transaction capacity.
    pub fn cursor<'a>(&self,entries:impl Iterator<Item=&'a Entry>)->Result<(Progress,usize)>{
        self.validate()?;let mut states=std::collections::BTreeMap::new();
        for e in entries{let Some(t)=e.expected_basket.as_ref().and_then(ExpectedBasket::investment)else{continue};
            if t.plan.id!=self.id{continue;}if t.plan!=*self{return Err("investment identity collision".into());}
            t.validate(e.expected_basket.as_ref().unwrap())?;
            if e.phase==Phase::Failed{continue;}
            if states.insert(t.index,(t.range()?,&e.phase)).is_some(){return Err("investment duplicate tranche".into());}
        }
        let mut start=0;let mut index=0;let mut pending=None;
        for (i,(range,phase)) in states{
            if pending.is_some()||i!=index||range.start!=start{return Err("investment predecessor not reconciled or stock overlap".into());}
            if *phase==Phase::Reconciled{start=range.end;index+=1;}else{pending=Some(i);}
        }
        Ok((if let Some(i)=pending{Progress::Pending(i)}else if start==self.allocations.len(){Progress::Complete}else{Progress::Ready(index)},start))
    }
}
impl Tranche{
    pub fn smaller_after(&self,error:&str)->Result<Option<Self>>{
        let range=self.range()?;
        if !crate::basket_wire::capacity_error(error)||range.len()==1{return Ok(None);}
        Ok(Some(self.plan.adaptive_tranche(self.index,range.start,range.len()-1)?))
    }
    pub fn range(&self)->Result<std::ops::Range<usize>>{
        self.plan.validate()?;
        if let Some(r)=&self.stock_range{
            if r.start>=r.end||r.end>self.plan.allocations.len()||r.end-r.start>crate::basket_wire::MAX_ATOMIC_STOCKS
                ||self.index>r.start||self.index.checked_mul(crate::basket_wire::MAX_ATOMIC_STOCKS).is_none_or(|n|n<r.start){return Err("investment adaptive range".into());}
            Ok(r.start..r.end)
        }else{self.plan.ranges().get(self.index).cloned().ok_or("investment step".into())}
    }
    pub fn input(&self)->Result<u64>{self.range()?.map(|i|atoms(&self.plan.allocations[i].input_atoms)).try_fold(0u64,|n,v|n.checked_add(v?).ok_or("tranche sum overflow".into()))}
    pub fn validate(&self,expected:&ExpectedBasket)->Result<()>{
        let range=self.range()?;let intent=expected.intent();
        let version=intent.strategy_version.iter().map(|b|format!("{b:02x}")).collect::<String>();
        if intent.owner!=self.plan.owner||version!=self.plan.version_hash||intent.retained_cash_atoms!=0
            ||intent.total_input_atoms!=self.input()?||intent.targets.len()!=range.len(){return Err("investment wire binding".into());}
        for (t,a) in intent.targets.iter().zip(&self.plan.allocations[range]){if t.instrument!=a.instrument||t.input_atoms!=atoms(&a.input_atoms)?{return Err("investment target binding".into());}}
        Ok(())
    }
}
/// Called within the journal's exclusive admission lock, before fsync/send.
pub(crate) fn admission<'a>(entry:&Entry,prior:impl Iterator<Item=&'a Entry>)->Result<()>{
    if let Some(t)=entry.expected_basket.as_ref().and_then(ExpectedBasket::investment){
        t.validate(entry.expected_basket.as_ref().unwrap())?;
        let (progress,start)=t.plan.cursor(prior)?;
        if progress!=Progress::Ready(t.index)||t.range()?.start!=start{return Err("investment already recorded or predecessor unresolved".into());}
    }Ok(())
}

#[cfg(test)]pub(crate) mod tests{
 use super::*;use crate::strategy::{Document,Leg};use base64::{engine::general_purpose::STANDARD,Engine};use ed25519_dalek::{Signer,SigningKey};
 pub(crate) fn publication(count:usize)->Published{
  let key=SigningKey::from_bytes(&[7;32]);let symbols=["NVDA","MSFT","AAPL","TSLA","AMZN","GOOGL","META","COIN","MSTR","SPY","QQQ","DIA","SLV","GLD","AMD","AVGO"];
  let mut legs=symbols[..count].iter().map(|s|Leg{instrument:s.to_string(),weight_bps:9000/count as u16}).collect::<Vec<_>>();
  legs[0].weight_bps+=9000-legs.iter().map(|l|l.weight_bps).sum::<u16>();
  let document=Document{creator:bs58::encode(key.verifying_key().as_bytes()).into_string(),nonce:"12".repeat(16),version:1,previous_version_hash:None,catalog_revision:"ab".repeat(32),name:"Whole composition".into(),description:String::new(),legs,cash_weight_bps:1000,creator_fee_bps:0,issued_at:1000,expires_at:1600};
  Published{strategy_id:document.strategy_id().unwrap(),version_hash:document.version_hash().unwrap(),signature:STANDARD.encode(key.sign(document.message().unwrap().as_bytes()).to_bytes()),document}
 }
 // Explicit metadata fixture, NOT evidence of execution/valid onchain bytes.
 pub(crate) fn entry_fixture(plan:&Plan,index:usize,suffix:u8)->Entry{
  let (mut entry,_)=crate::basket_wire::recovery_fixture();let mut e=serde_json::to_value(entry.expected_basket.as_ref().unwrap()).unwrap();let range=plan.ranges()[index].clone();
  e["intent"]["owner"]=serde_json::json!(plan.owner);e["intent"]["totalInputAtoms"]=serde_json::json!(plan.tranche(index).unwrap().input().unwrap());e["intent"]["retainedCashAtoms"]=serde_json::json!(0);
  let hash=(0..32).map(|i|u8::from_str_radix(&plan.version_hash[i*2..i*2+2],16).unwrap()).collect::<Vec<_>>();e["intent"]["strategyVersion"]=serde_json::json!(hash);
  e["intent"]["targets"]=serde_json::json!(plan.allocations[range].iter().map(|a|serde_json::json!({"instrument":a.instrument,"inputAtoms":a.input_atoms.parse::<u64>().unwrap(),"minimumExposureQ32":1})).collect::<Vec<_>>());
  e["investment"]=serde_json::to_value(plan.tranche(index).unwrap()).unwrap();entry.expected_basket=Some(serde_json::from_value(e).unwrap());
  entry.id=format!("stkp_{suffix:032x}");entry.signature=format!("fixture-signature-{suffix}");entry.phase=Phase::Prepared;entry.attempts=0;entry
 }
 fn plan(n:usize,total:&str)->Plan{let p=publication(n);Plan::from_publication(&p,p.document.creator.clone(),total.into(),"23".repeat(16),20).unwrap()}
 #[test]fn all_two_through_sixteen_stocks_keep_exact_budget_and_stable_groups(){
  for n in 2..=16{let p=plan(n,"100000001");let ranges=p.ranges();assert_eq!(ranges.iter().flat_map(|r|r.clone()).collect::<Vec<_>>(),(0..n).collect::<Vec<_>>());assert!(ranges.iter().all(|r|(2..=3).contains(&r.len())));
   let sum=(0..ranges.len()).map(|i|p.tranche(i).unwrap().input().unwrap()).sum::<u64>();assert_eq!(sum+atoms(&p.retained_cash_atoms).unwrap(),100000001);
   let mut changed=p.clone();changed.allocations[0].input_atoms="1".into();assert!(changed.validate().is_err());
  }
 }
 #[test]fn unknown_finalized_or_missing_step_never_allows_the_next_purchase(){
  let p=plan(4,"200");let mut first=entry_fixture(&p,0,1);let second=entry_fixture(&p,1,2);
  assert_eq!(p.progress(std::iter::empty()).unwrap(),Progress::Ready(0));assert!(admission(&second,std::iter::empty()).is_err());
  for phase in [Phase::Prepared,Phase::Submitted,Phase::Unknown,Phase::Finalized]{first.phase=phase;assert_eq!(p.progress([&first].into_iter()).unwrap(),Progress::Pending(0));assert!(admission(&second,[&first].into_iter()).is_err());}
  first.phase=Phase::Reconciled;assert_eq!(p.progress([&first].into_iter()).unwrap(),Progress::Ready(1));admission(&second,[&first].into_iter()).unwrap();
  assert!(admission(&entry_fixture(&p,0,3),[&first].into_iter()).is_err());
  let mut second=second;second.phase=Phase::Reconciled;assert_eq!(p.progress([&first,&second].into_iter()).unwrap(),Progress::Complete);
 }
 #[test]fn failed_attempt_can_retry_only_the_same_step_and_never_changes_weights(){
  let p=plan(4,"200");let mut first=entry_fixture(&p,0,1);first.phase=Phase::Failed;let retry=entry_fixture(&p,0,2);admission(&retry,[&first].into_iter()).unwrap();
  assert!(admission(&entry_fixture(&p,1,3),[&first].into_iter()).is_err());
  let mut fake=p.clone();fake.total_input_atoms="201".into();assert!(fake.progress([&first].into_iter()).is_err());
  let mut changed=retry.clone();let mut value=serde_json::to_value(&changed.expected_basket).unwrap();value["intent"]["targets"][0]["inputAtoms"]=serde_json::json!(46);
  changed.expected_basket=Some(serde_json::from_value(value).unwrap());assert!(admission(&changed,[&first].into_iter()).is_err());
 }
 #[test]fn journal_reopen_compaction_and_two_tabs_keep_one_economic_step(){
  let p=plan(4,"200");let root=std::env::temp_dir().join(format!("stock-investment-{}",std::process::id()));std::fs::create_dir(&root).unwrap();let path=root.join("execution.log");
  let mut j=crate::journal::Journal::open(&path,20,1024*1024).unwrap();let first=entry_fixture(&p,0,1);j.insert(first.clone()).unwrap();
  assert!(j.insert(entry_fixture(&p,0,2)).is_err());assert!(j.insert(entry_fixture(&p,1,3)).is_err());j.update(&first.id,Phase::Unknown,true).unwrap();j.compact().unwrap();drop(j);
  let mut j=crate::journal::Journal::open(&path,20,1024*1024).unwrap();assert_eq!(p.progress(j.entries()).unwrap(),Progress::Pending(0));assert!(j.insert(entry_fixture(&p,1,3)).is_err());
  j.update(&first.id,Phase::Finalized,false).unwrap();assert!(j.insert(entry_fixture(&p,1,3)).is_err());j.update(&first.id,Phase::Reconciled,false).unwrap();j.insert(entry_fixture(&p,1,3)).unwrap();j.compact().unwrap();drop(j);
  let j=crate::journal::Journal::open(&path,20,1024*1024).unwrap();assert_eq!(p.progress(j.entries()).unwrap(),Progress::Pending(1));drop(j);std::fs::remove_dir_all(root).unwrap();
 }
 #[test]fn tranche_uses_original_signed_publication_not_reweighted_synthetic_document(){
  let p=publication(16);let plan=Plan::from_publication(&p,p.document.creator.clone(),"100000001".into(),"23".repeat(16),20).unwrap();
  for (i,range) in plan.ranges().iter().enumerate(){let minimums=plan.allocations[range.clone()].iter().map(|a|(a.instrument.clone(),1)).collect();
   let intent=crate::basket_wire::Intent::from_publication_range(&p,p.document.creator.parse().unwrap(),&plan.total_input_atoms,&p.document.catalog_revision,i as u64*3,100,&minimums,Some(range.clone())).unwrap();
   assert_eq!(intent.total_input_atoms,plan.tranche(i).unwrap().input().unwrap());assert_eq!(intent.retained_cash_atoms,0);assert_eq!(intent.targets.len(),range.len());
  }
 }
 fn adaptive_entry(plan:&Plan,index:usize,start:usize,count:usize,suffix:u8)->Entry{
  let mut entry=entry_fixture(plan,0,suffix);let t=plan.adaptive_tranche(index,start,count).unwrap();
  let mut e=serde_json::to_value(entry.expected_basket.as_ref().unwrap()).unwrap();
  e["intent"]["totalInputAtoms"]=serde_json::json!(t.input().unwrap());
  e["intent"]["targets"]=serde_json::json!(plan.allocations[t.range().unwrap()].iter().map(|a|serde_json::json!({"instrument":a.instrument,"inputAtoms":a.input_atoms.parse::<u64>().unwrap(),"minimumExposureQ32":1})).collect::<Vec<_>>());
  e["investment"]=serde_json::to_value(t).unwrap();entry.expected_basket=Some(serde_json::from_value(e).unwrap());entry
 }
 #[test]fn adaptive_retry_shrinks_only_capacity_and_keeps_exact_allocation(){
  let p=plan(4,"100000001");let t=p.adaptive_tranche(0,0,3).unwrap();
  for reason in ["v0 transaction packet bound","basket execution Bank resource bound","basket computational budget exceeded"]{
   let two=t.smaller_after(reason).unwrap().unwrap();assert_eq!(two.range().unwrap(),0..2);assert_eq!(two.plan,p);
   let one=two.smaller_after(reason).unwrap().unwrap();assert_eq!(one.range().unwrap(),0..1);assert!(one.smaller_after(reason).unwrap().is_none());
  }
  for reason in ["basket stock policy binding","basket simulation failed or bank changed; rebuild","provider quota exhausted","basket receipt cash/CU","basket execution market changed; rebuild"]{
   assert!(t.smaller_after(reason).unwrap().is_none());
  }
  assert!(p.adaptive_tranche(0,1,1).is_err());assert!(p.adaptive_tranche(2,1,1).is_err());assert!(p.adaptive_tranche(1,4,1).is_err());
 }
 #[test]fn vm_exhaustion_requires_runtime_error_budget_and_non_spoofable_log(){
  use serde_json::json;use crate::basket_wire::computational_exhaustion as exhausted;
  let error=json!({"InstructionError":[2,"ProgramFailedToComplete"]});
  let line="Program Am6Xc88xbowj6kNDdjmg13wPvvRvTdijcWvadFCZKFPD failed: exceeded CUs meter at BPF instruction";
  assert!(exhausted(&error,1000000,1000000,4,&json!([line])));
  assert!(!exhausted(&error,999999,1000000,4,&json!([line])));assert!(!exhausted(&error,1000000,1000000,2,&json!([line])));
  for logs in [json!([]),json!([format!("Program log: {line}")]),json!(["Program fake failed: exceeded CUs meter at BPF instruction"])]{
   assert!(!exhausted(&error,1000000,1000000,4,&logs));
  }
  for other in [json!({"InstructionError":[2,{"Custom":5}]}),json!("BlockhashNotFound"),json!({"InstructionError":[2,"InvalidAccountData"]})]{assert!(!exhausted(&other,1000000,1000000,4,&json!([line])));}
 }
 #[test]fn adaptive_single_two_single_groups_resume_exact_prefix_after_restart(){
  let p=plan(4,"100000001");let root=std::env::temp_dir().join(format!("stock-adaptive-{}",std::process::id()));std::fs::create_dir(&root).unwrap();let path=root.join("execution.log");
  let mut j=crate::journal::Journal::open(&path,20,1024*1024).unwrap();let mut first=adaptive_entry(&p,0,0,1,31);j.insert(first.clone()).unwrap();
  assert!(j.insert(adaptive_entry(&p,0,0,2,32)).is_err());assert!(j.insert(adaptive_entry(&p,1,1,2,33)).is_err());
  j.update(&first.id,Phase::Unknown,true).unwrap();j.compact().unwrap();drop(j);
  let mut j=crate::journal::Journal::open(&path,20,1024*1024).unwrap();assert_eq!(p.cursor(j.entries()).unwrap(),(Progress::Pending(0),0));
  j.update(&first.id,Phase::Finalized,false).unwrap();assert!(j.insert(adaptive_entry(&p,1,1,2,33)).is_err());
  j.update(&first.id,Phase::Reconciled,false).unwrap();assert_eq!(p.cursor(j.entries()).unwrap(),(Progress::Ready(1),1));
  let second=adaptive_entry(&p,1,1,2,33);j.insert(second.clone()).unwrap();j.update(&second.id,Phase::Failed,false).unwrap();
  let retry=adaptive_entry(&p,1,1,2,34);j.insert(retry.clone()).unwrap();j.update(&retry.id,Phase::Finalized,false).unwrap();j.update(&retry.id,Phase::Reconciled,false).unwrap();
  assert_eq!(p.cursor(j.entries()).unwrap(),(Progress::Ready(2),3));
  assert!(j.insert(adaptive_entry(&p,2,2,2,35)).is_err());let last=adaptive_entry(&p,2,3,1,36);j.insert(last.clone()).unwrap();
  j.update(&last.id,Phase::Finalized,false).unwrap();j.update(&last.id,Phase::Reconciled,false).unwrap();j.compact().unwrap();drop(j);
  let j=crate::journal::Journal::open(&path,20,1024*1024).unwrap();assert_eq!(p.cursor(j.entries()).unwrap(),(Progress::Complete,4));
  first.phase=Phase::Reconciled;let mut gap=adaptive_entry(&p,1,2,1,37);gap.phase=Phase::Reconciled;assert!(p.cursor([&first,&gap].into_iter()).is_err());
  drop(j);std::fs::remove_dir_all(root).unwrap();
 }
 #[test]fn old_fixed_records_and_new_adaptive_records_share_one_plan_without_rebuy(){
  let p=plan(7,"100000001");let mut old=entry_fixture(&p,0,41);old.phase=Phase::Reconciled;
  let serialized=serde_json::to_string(old.expected_basket.as_ref().unwrap().investment().unwrap()).unwrap();assert!(!serialized.contains("stockRange"));
  let mut next=adaptive_entry(&p,1,3,1,42);admission(&next,[&old].into_iter()).unwrap();next.phase=Phase::Reconciled;
  assert_eq!(p.cursor([&old,&next].into_iter()).unwrap(),(Progress::Ready(2),4));
  assert!(admission(&entry_fixture(&p,2,43),[&old,&next].into_iter()).is_err());
 }
}
