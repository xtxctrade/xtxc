//! Signed immutable composition metadata. This is NOT an investment, trading
//! mandate, follower allocation, receipt ledger or permission to collect fees.
use crate::{catalog::{valid_instrument, BasketLeg, BasketRequest}, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use ed25519_dalek::{Signature, VerifyingKey};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::{BTreeMap, BTreeSet}, fs::File, io::{Read, Seek, SeekFrom, Write}, path::Path};

const MAX_RECORD:usize=16*1024;
const MAX_LOG:u64=64*1024*1024;
const MAX_VERSIONS:usize=10_000;
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(deny_unknown_fields,rename_all="camelCase")]
pub struct Leg {pub instrument:String,pub weight_bps:u16}
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(deny_unknown_fields,rename_all="camelCase")]
pub struct Document {
 pub creator:String,pub nonce:String,pub version:u32,pub previous_version_hash:Option<String>,
 pub catalog_revision:String,pub name:String,pub description:String,pub legs:Vec<Leg>,
 pub cash_weight_bps:u16,pub creator_fee_bps:u16,pub issued_at:u64,pub expires_at:u64,
}
fn hex(bytes:impl AsRef<[u8]>)->String{bytes.as_ref().iter().map(|b|format!("{b:02x}")).collect()}
fn hash(s:&[u8])->String{hex(Sha256::digest(s))}
fn hex_field(s:&str,n:usize)->bool{s.len()==n&&s.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b))&&s.bytes().any(|b|b!=b'0')}
fn text(s:&str,max:usize,empty:bool)->bool{(empty||!s.is_empty())&&s.len()<=max&&s.trim()==s&&!s.chars().any(|c|c.is_control()||('\u{2028}'..='\u{202e}').contains(&c)||('\u{2066}'..='\u{2069}').contains(&c))}
impl Document {
 pub fn validate(&self)->Result<()> {
  let key: [u8;32]=bs58::decode(&self.creator).into_vec().map_err(|_|"creator key")?.try_into().map_err(|_|"creator key")?;
  if bs58::encode(key).into_string()!=self.creator||VerifyingKey::from_bytes(&key).map_err(|_|"creator key")?.is_weak()
   ||!hex_field(&self.nonce,32)||!hex_field(&self.catalog_revision,64)||!(1..=1000).contains(&self.version)
   ||!text(&self.name,64,false)||!text(&self.description,280,true)||!(2..=16).contains(&self.legs.len())
   ||self.creator_fee_bps>1000||self.cash_weight_bps>=10000||self.issued_at==0
   ||self.expires_at<=self.issued_at||self.expires_at-self.issued_at>600
   ||(self.version==1)!=self.previous_version_hash.is_none()||self.previous_version_hash.as_ref().is_some_and(|s|!hex_field(s,64))
  {return Err("strategy document bounds".into())}
  let mut seen=BTreeSet::new();let mut weight=u32::from(self.cash_weight_bps);
  for leg in &self.legs {if !valid_instrument(&leg.instrument)||leg.instrument=="XTXCCASHRESERVE"||leg.weight_bps==0||!seen.insert(&leg.instrument){return Err("strategy duplicate or invalid stock".into())}weight+=u32::from(leg.weight_bps);}
  if weight!=10000{return Err("strategy weights must total 10000".into())}Ok(())
 }
 pub fn strategy_id(&self)->Result<String>{self.validate()?;Ok(format!("stks_{}",hash(format!("XTXC_STRATEGY_V1\0{}\0{}",self.creator,self.nonce).as_bytes())))}
 pub fn message(&self)->Result<String>{
  self.validate()?;
  let legs=self.legs.iter().map(|l|format!("{}: {} bps",l.instrument,l.weight_bps)).collect::<Vec<_>>().join("\n");
  Ok(format!("XTXC Strategy Publication\nNetwork: solana:mainnet\nCreator: {}\nStrategy nonce: {}\nVersion: {}\nPrevious version: {}\nCatalog: {}\nName: {}\nDescription: {}\n{}\nCash: {} bps\nCreator fee: {} bps of new profits\nIssued at: {}\nExpires at: {}\nPublish composition only. No trade, transfer or spending permission.",self.creator,self.nonce,self.version,self.previous_version_hash.as_deref().unwrap_or("none"),self.catalog_revision,self.name,self.description,legs,self.cash_weight_bps,self.creator_fee_bps,self.issued_at,self.expires_at))
 }
 pub fn version_hash(&self)->Result<String>{Ok(hash(self.message()?.as_bytes()))}
 /// Include cash in largest-remainder allocation rather than rounding a
 /// second independent budget. Keep per-stock Q32 values as a vector.
 pub fn allocations(&self,total:&str)->Result<(Vec<u64>,u64)> {
  self.validate()?;let mut legs=self.legs.iter().map(|l|BasketLeg{instrument:l.instrument.clone(),weight_bps:l.weight_bps}).collect::<Vec<_>>();
  // The identifier is reserved by document validation, never a quoted stock.
  let cash= self.cash_weight_bps>0;
  if cash {legs.push(BasketLeg{instrument:"XTXCCASHRESERVE".into(),weight_bps:self.cash_weight_bps});}
  let mut allocation=BasketRequest{catalog_revision:self.catalog_revision.clone(),input_atoms:total.into(),max_slippage_bps:20,legs}.allocate_with_limit(&self.catalog_revision,17)?;
  let reserve=if cash{allocation.pop().ok_or("cash allocation")?}else{0};Ok((allocation,reserve))
 }
}
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(deny_unknown_fields,rename_all="camelCase")]
pub struct Published {pub strategy_id:String,pub version_hash:String,pub document:Document,pub signature:String}
impl Published {
 pub fn verify(&self)->Result<()> {
  if self.strategy_id!=self.document.strategy_id()?||self.version_hash!=self.document.version_hash()?{return Err("strategy identity mismatch".into())}
  let key:[u8;32]=bs58::decode(&self.document.creator).into_vec().map_err(|_|"creator key")?.try_into().map_err(|_|"creator key")?;
  let bytes=STANDARD.decode(&self.signature).map_err(|_|"publication signature")?;
  if STANDARD.encode(&bytes)!=self.signature{return Err("publication signature encoding".into())}
  let signature=Signature::from_slice(&bytes).map_err(|_|"publication signature")?;
  VerifyingKey::from_bytes(&key).map_err(|_|"creator key")?.verify_strict(self.document.message()?.as_bytes(),&signature).map_err(|_|"publication signature mismatch".into())
 }
}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Record{sequence:u64,previous:String,publication:Published}
pub struct StrategyStore {file:File,rows:BTreeMap<String,Published>,latest:BTreeMap<String,String>,bytes:u64,sequence:u64,head:String,poisoned:bool}
impl StrategyStore {
 pub fn open(path:&Path)->Result<Self>{
  let mut file=crate::journal::open_private_regular(path,false)?;file.try_lock_exclusive().map_err(|_|"strategy store already owned")?;
  File::open(path.parent().ok_or("strategy parent")?).and_then(|p|p.sync_all()).map_err(|e|e.to_string())?;
  if file.metadata().map_err(|e|e.to_string())?.len()>MAX_LOG{return Err("strategy log size".into())}
  let mut bytes=Vec::new();file.read_to_end(&mut bytes).map_err(|e|e.to_string())?;
  let mut store=Self{file,rows:BTreeMap::new(),latest:BTreeMap::new(),bytes:0,sequence:0,head:"0".repeat(64),poisoned:false};
  let mut at=0;while at+4<=bytes.len(){
   let len=u32::from_le_bytes(bytes[at..at+4].try_into().unwrap()) as usize;
   if len==0||len>MAX_RECORD{return Err("strategy record size".into())}
   if at+4+len+32>bytes.len(){break}
   let body=&bytes[at+4..at+4+len];let digest=Sha256::digest(body);
   if digest.as_slice()!=&bytes[at+4+len..at+4+len+32]{return Err("strategy log corruption".into())}
   let record:Record=serde_json::from_slice(body).map_err(|_|"strategy record encoding")?;
   if record.sequence!=store.sequence+1||record.previous!=store.head{return Err("strategy log chain".into())}
   record.publication.verify()?;store.check_next(&record.publication)?;
   if store.rows.contains_key(&record.publication.version_hash){return Err("strategy duplicate record".into())}
   store.install(record.publication);store.sequence=record.sequence;store.head=hex(digest);at+=4+len+32;
  }
  // Only an incomplete final frame is recoverable. Never discard a complete
  // invalid signature/hash/version. Acknowledged records were fsynced first.
  if at!=bytes.len(){store.file.set_len(at as u64).and_then(|_|store.file.sync_all()).map_err(|e|e.to_string())?;}
  store.file.seek(SeekFrom::End(0)).map_err(|e|e.to_string())?;store.bytes=at as u64;Ok(store)
 }
 fn check_next(&self,p:&Published)->Result<()> {
  if self.rows.len()>=MAX_VERSIONS{return Err("strategy version capacity".into())}
  if let Some(hash)=self.latest.get(&p.strategy_id){let old=&self.rows[hash];if p.document.version!=old.document.version+1||p.document.previous_version_hash.as_ref()!=Some(hash)||p.document.creator!=old.document.creator||p.document.nonce!=old.document.nonce{return Err("strategy version conflict".into())}}
  else if p.document.version!=1||p.document.previous_version_hash.is_some(){return Err("strategy first version".into())}
  Ok(())
 }
 fn install(&mut self,p:Published){self.latest.insert(p.strategy_id.clone(),p.version_hash.clone());self.rows.insert(p.version_hash.clone(),p);}
 pub fn publish(&mut self,p:Published,now:u64)->Result<Published>{
  if self.poisoned{return Err("strategy store requires recovery".into())}p.verify()?;
  if let Some(old)=self.rows.get(&p.version_hash){if old==&p{return Ok(old.clone())}return Err("strategy publication collision".into())}
  if p.document.issued_at>now.saturating_add(30)||p.document.expires_at<now{return Err("strategy publication expired".into())}
  self.check_next(&p)?;
  let body=serde_json::to_vec(&Record{sequence:self.sequence+1,previous:self.head.clone(),publication:p.clone()}).map_err(|e|e.to_string())?;
  if body.len()>MAX_RECORD||self.bytes+body.len() as u64+36>MAX_LOG{return Err("strategy log capacity".into())}
  let digest=Sha256::digest(&body);let mut frame=(body.len() as u32).to_le_bytes().to_vec();frame.extend(&body);frame.extend_from_slice(&digest);
  self.poisoned=true;self.file.write_all(&frame).and_then(|_|self.file.sync_all()).map_err(|e|e.to_string())?;
  self.bytes+=frame.len() as u64;self.sequence+=1;self.head=hex(digest);self.install(p.clone());self.poisoned=false;Ok(p)
 }
 pub fn get(&self,id:&str,version:Option<&str>)->Option<&Published>{let hash=version.or_else(||self.latest.get(id).map(String::as_str))?;self.rows.get(hash).filter(|r|r.strategy_id==id)}
 pub fn list(&self,offset:usize)->Vec<Published>{self.latest.values().skip(offset).take(50).map(|h|self.rows[h].clone()).collect()}
 pub fn count(&self)->usize{self.latest.len()}
 pub fn revision(&self)->&str{&self.head}
}

#[cfg(test)]
mod tests {
 use super::*;use ed25519_dalek::{Signer,SigningKey};use std::sync::atomic::{AtomicU64,Ordering};
 fn document()->Document{Document{creator:bs58::encode(SigningKey::from_bytes(&[7;32]).verifying_key().as_bytes()).into_string(),nonce:"12".repeat(16),version:1,previous_version_hash:None,catalog_revision:"ab".repeat(32),name:"AI portfolio".into(),description:"Chips and software".into(),legs:vec![Leg{instrument:"NVDA".into(),weight_bps:4500},Leg{instrument:"MSFT".into(),weight_bps:4500}],cash_weight_bps:1000,creator_fee_bps:500,issued_at:1000,expires_at:1600}}
 fn signed(d:Document)->Published{Published{strategy_id:d.strategy_id().unwrap(),version_hash:d.version_hash().unwrap(),signature:STANDARD.encode(SigningKey::from_bytes(&[7;32]).sign(d.message().unwrap().as_bytes()).to_bytes()),document:d}}
 struct Temp(std::path::PathBuf);impl Temp{fn new()->Self{static N:AtomicU64=AtomicU64::new(0);let path=std::env::temp_dir().join(format!("stock-strategy-{}-{}",std::process::id(),N.fetch_add(1,Ordering::Relaxed)));std::fs::create_dir(&path).unwrap();Self(path)}fn path(&self)->std::path::PathBuf{self.0.join("compositions.log")}}impl Drop for Temp{fn drop(&mut self){let _=std::fs::remove_dir_all(&self.0);}}
 #[test]fn signature_covers_every_reviewed_field(){let p=signed(document());p.verify().unwrap();let mut d=p.document.clone();d.name="Other".into();let q=Published{strategy_id:d.strategy_id().unwrap(),version_hash:d.version_hash().unwrap(),document:d,..p};assert!(q.verify().is_err());}
 #[test]fn allocations_have_one_exact_cash_budget(){let d=document();assert_eq!(d.allocations("100000001").unwrap(),(vec![45000000,45000001],10000000));let mut reverse=d.clone();reverse.legs.reverse();assert_eq!(reverse.allocations("100000001").unwrap(),(vec![45000001,45000000],10000000));assert!(d.allocations("1").is_err());assert!(d.allocations("1e6").is_err());}
 #[test]fn ambiguous_and_unbounded_compositions_are_rejected(){for name in ["x\ny","x\u{2028}y","x\u{202e}y"," x"]{let mut d=document();d.name=name.into();assert!(d.validate().is_err());}let mut d=document();d.legs[1].instrument="NVDA".into();assert!(d.validate().is_err());let mut d=document();d.legs[1].instrument="XTXCCASHRESERVE".into();assert!(d.validate().is_err());let mut d=document();d.creator_fee_bps=1001;assert!(d.validate().is_err());let mut d=document();d.legs[0].weight_bps=4501;assert!(d.validate().is_err());}
 #[test]fn publication_retry_version_conflict_and_reopen(){let tmp=Temp::new();let mut s=StrategyStore::open(&tmp.path()).unwrap();assert!(StrategyStore::open(&tmp.path()).is_err());let p=signed(document());s.publish(p.clone(),1000).unwrap();assert_eq!(s.publish(p.clone(),9000).unwrap(),p);let mut d=p.document.clone();d.version=2;d.previous_version_hash=Some(p.version_hash.clone());d.name="AI portfolio II".into();let p2=signed(d);s.publish(p2.clone(),1000).unwrap();let mut bad=p2.document.clone();bad.name="Conflicting update".into();assert!(s.publish(signed(bad),1000).is_err());assert_eq!(s.get(&p.strategy_id,None),Some(&p2));assert_eq!(s.get(&p.strategy_id,Some(&p.version_hash)),Some(&p));let rev=s.revision().to_owned();drop(s);let s=StrategyStore::open(&tmp.path()).unwrap();assert_eq!(s.count(),1);assert_eq!(s.revision(),rev);assert_eq!(s.get(&p.strategy_id,None),Some(&p2));}
 #[test]fn only_incomplete_unacknowledged_tail_is_recovered(){let tmp=Temp::new();let p=signed(document());{let mut s=StrategyStore::open(&tmp.path()).unwrap();s.publish(p.clone(),1000).unwrap();}let len=std::fs::metadata(tmp.path()).unwrap().len();{let mut f=std::fs::OpenOptions::new().append(true).open(tmp.path()).unwrap();f.write_all(&[100,0,0,0,1,2,3]).unwrap();}let s=StrategyStore::open(&tmp.path()).unwrap();assert_eq!(std::fs::metadata(tmp.path()).unwrap().len(),len);assert_eq!(s.get(&p.strategy_id,None),Some(&p));drop(s);{let mut f=std::fs::OpenOptions::new().write(true).open(tmp.path()).unwrap();f.seek(SeekFrom::Start(10)).unwrap();f.write_all(&[0xff]).unwrap();}assert!(StrategyStore::open(&tmp.path()).is_err());}
 #[test]fn failed_signature_never_appends(){let tmp=Temp::new();let mut s=StrategyStore::open(&tmp.path()).unwrap();let mut p=signed(document());p.signature=STANDARD.encode([0;64]);assert!(s.publish(p,1000).is_err());assert_eq!(std::fs::metadata(tmp.path()).unwrap().len(),0);let mut expired=document();expired.issued_at=1;expired.expires_at=100;assert!(s.publish(signed(expired),1000).is_err());}
}
