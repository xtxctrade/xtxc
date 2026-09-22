//! Owner-specific simulation, externally signed admission and vector receipts.
//! Finalized receipts use EACH top-level settlement return, not the final
//! returnData register or a stale prepared multiplier. No key or sender here.
use super::*;
use crate::{feed::Feed,journal::{Entry,Phase},rpc::Rpc,sender::{self,Authorization}};
use base64::{engine::general_purpose::STANDARD,Engine};
use serde_json::{json,Value};

#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Token {
    account:String,mint:String,program:String,decimals:u8,instrument:Option<String>,
    fixed:bool,numerator:u64,denominator:u64,conservative_bps:u16,
}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct ReturnLayout {opcode:u8,tag:String,header:usize,
    #[serde(default,skip_serializing_if="Option::is_none")]
    minimum_cash:Option<u64>}
/// Created only from the trusted compiler. Never accept this from a request.
/// Optional journal field preserves old single-stock records unchanged.
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedBasket {
    intent:Intent,program:String,nonce:String,message_hash:[u8;32],keys:Vec<String>,
    resources:Vec<[u8;32]>,prepared_slot:u64,tokens:Vec<Token>,created:Vec<String>,
    returns:Vec<ReturnLayout>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    investment:Option<crate::investment::Tranche>,
}

pub struct PreparedBasket {
    expected:ExpectedBasket,unsigned_wire:Vec<u8>,state_hash:[u8;32],revision:u64,
    last_valid_height:u64,summary:Value,
}

impl Compiled {
    fn expected(&self,before:&Snapshot)->Result<ExpectedBasket>{
        if self.message_hash!=<[u8;32]>::from(Sha256::digest(&self.message))||before.slot!=self.prepared_slot {
            return Err("basket compiled identity changed".into());
        }
        let message=crate::pipeline::decode(&self.message)?;
        let keys=crate::pipeline::resolved(&message,before)?;
        if keys.len()>64{return Err("basket resolved account capacity".into());}
        let resources=keys.iter().enumerate().filter(|(i,_)|message.is_maybe_writable(*i,None))
            .map(|(_,k)|k.parse::<Pubkey>().map(|v|v.to_bytes()).map_err(|_|"basket resource".into())).collect::<Result<Vec<_>>>()?;
        let mut tokens=vec![Token{account:self.source.token.to_string(),mint:USDC.to_string(),program:self.source.token_program.to_string(),
            decimals:account(before,&USDC.to_string())?.data.get(44).copied().ok_or("basket cash decimals")?,instrument:None,
            fixed:true,numerator:1,denominator:1,conservative_bps:10000}];
        tokens.extend(self.outputs.iter().map(|o|Token{account:o.token.clone(),mint:o.mint.clone(),program:o.program.clone(),
            decimals:o.model.decimals,instrument:Some(o.instrument.clone()),fixed:o.fixed,numerator:o.numerator,denominator:o.denominator,conservative_bps:o.conservative_bps}));
        tokens.extend(self.transits.iter().map(|a|Token{account:a.token.to_string(),mint:a.mint.to_string(),program:a.token_program.to_string(),
            decimals:9,instrument:None,fixed:true,numerator:1,denominator:1,conservative_bps:10000}));
        let mut created=Vec::new();
        for name in tokens.iter().map(|t|&t.account).chain(std::iter::once(&self.nonce.to_string())){
            let a=account(before,name)?;
            if a.owner==Pubkey::default().to_string()&&a.data.is_empty()&&!a.executable&&a.lamports==0{created.push(name.clone());}
        }
        let mut returns=Vec::new();
        // The instructions and public indices can be inspected by callers;
        // decode from the HASH-BOUND wire rather than trusting mutable fields.
        for ix in message.instructions(){
            if keys.get(usize::from(ix.program_id_index))!=Some(&self.program.to_string())||ix.data==[0]{continue;}
            let opcode=*ix.data.first().ok_or("basket empty settlement")?;
            let (tag,header,minimum_cash)=match opcode{
                13=>("SKEWEXP1",40,None),
                14=>(if ix.data.get(5)==Some(&1){"SKEWMSR1"}else{"SKEWMSH1"},48,None),
                18=>{
                    if ix.data.len()<8||!matches!(ix.data[1],1|2){return Err("basket return ABI not admitted".into());}
                    let end=8+usize::from(u16::from_le_bytes([ix.data[4],ix.data[5]]));
                    if end.checked_add(usize::from(u16::from_le_bytes([ix.data[6],ix.data[7]])))!=Some(ix.data.len()){
                        return Err("basket funding wire length".into());}
                    let graph=stocklana_adapters::graph::Graph::decode(ix.data.get(8..end).ok_or("basket funding wire")?,ix.accounts.len()).map_err(|_|"basket funding graph")?;
                    (if ix.data[1]==1{"SKEWMSF1"}else{"SKEWMSF2"},56,Some(graph.min_out))
                },
                _=>return Err("basket return opcode not admitted".into()),
            };
            returns.push(ReturnLayout{opcode,tag:tag.into(),header,minimum_cash});
        }
        let expected=ExpectedBasket{intent:self.intent.clone(),program:self.program.to_string(),nonce:self.nonce.to_string(),message_hash:self.message_hash,
            keys,resources,prepared_slot:before.slot,tokens,created,returns,investment:None};
        expected.validate()?;Ok(expected)
    }
}

impl PreparedBasket {
    pub fn simulate(feed:&Feed,before:&Snapshot,rpc:&Rpc,compiled:&Compiled,last_valid_height:u64)->Result<Self>{
        feed.validate_fence(before)?;
        if last_valid_height==0{return Err("basket block height".into());}
        let expected=compiled.expected(before)?;
        let mut addresses=expected.tokens.iter().map(|t|t.account.clone()).collect::<BTreeSet<_>>();
        addresses.insert(expected.intent.owner.clone());addresses.insert(expected.nonce.clone());
        addresses.extend(compiled.outputs.iter().map(|o|o.mint.clone()));addresses.extend(compiled.policies.keys().cloned());
        let addresses=addresses.into_iter().collect::<Vec<_>>();
        if addresses.len()>32{return Err("basket observation bounds".into());}
        let unsigned_wire=unsigned(&compiled.message);
        rpc.check_genesis()?;
        let result=rpc.call("simulateTransaction",json!([STANDARD.encode(&unsigned_wire),{
            "encoding":"base64","sigVerify":false,"replaceRecentBlockhash":false,"commitment":"confirmed","minContextSlot":before.slot,
            "accounts":{"encoding":"base64","addresses":addresses}}]))?;
        let summary=Self::check_simulation(compiled,&expected,before,&addresses,&result)?;
        feed.validate_fence(before)?;
        Ok(Self{expected,unsigned_wire,state_hash:before.hash,revision:before.revision,last_valid_height,summary})
    }
    fn check_simulation(compiled:&Compiled,expected:&ExpectedBasket,before:&Snapshot,addresses:&[String],result:&Value)->Result<Value>{
        if result["context"]["slot"].as_u64()!=Some(before.slot){
            return Err("basket simulation failed or bank changed; rebuild".into());}
        let error=result["value"].get("err").ok_or("basket simulation status")?;
        if !error.is_null(){
            let exhausted=computational_exhaustion(error,result["value"]["unitsConsumed"].as_u64().unwrap_or(0),
                u64::from(compiled.intent.maximum_cu),compiled.instructions.len(),&result["value"]["logs"]);
            return Err(if exhausted{"basket computational budget exceeded"}else{"basket simulation failed or bank changed; rebuild"}.into());
        }
        let rows=crate::exposure_pipeline::returned_accounts(result,addresses,"basket simulation")?;
        let mut after=before.clone();
        for row in rows{let target=after.accounts.iter_mut().find(|a|a.key==row.key).ok_or("basket observation outside Bank")?;*target=row;}
        let cu=result["value"]["unitsConsumed"].as_u64().ok_or("basket simulation CU")?;
        let outcome=compiled.verify_simulated(&compiled.message,before,&after,cu)?;
        let amounts=|s:&Snapshot|expected.tokens.iter().map(|t|token_amount(s,&t.account,&t.mint,&expected.intent.owner,&t.program,expected.created.contains(&t.account))).collect::<Result<Vec<_>>>();
        let stocks=expected.stock_returns(&result["value"]["logs"],&amounts(before)?,&amounts(&after)?,cu)?;
        let balances=|s:&Snapshot|expected.keys.iter().map(|k|account(s,k).map(|a|a.lamports)).collect::<Result<Vec<_>>>();
        let fee=crate::exposure_pipeline::simulation_fee(result)?;
        let rent=expected.native_cost(&balances(before)?,&balances(&after)?,fee)?;
        Ok(json!({"schema":"skew.stockmesh.prepared-basket/v1","owner":expected.intent.owner,
            "strategyVersion":hex(&expected.intent.strategy_version),"catalogRevision":hex(&expected.intent.catalog_revision),
            "inputAtoms":outcome.spent_atoms,"retainedCashAtoms":outcome.retained_cash_atoms,"stocks":stocks,
            "simulationCU":cu,"feeLamports":fee.to_string(),"rentLamports":rent.to_string(),"stateSlot":before.slot,
            "ownerSequence":expected.intent.owner_sequence.to_string(),"nextOwnerSequence":(expected.intent.owner_sequence+expected.intent.targets.len()as u64).to_string(),
            "messageHash":hex(&expected.message_hash),"requiresOwnerSignatures":true,"submitted":false}))
    }
    pub fn review(&self)->Value{
        json!({"execution":self.summary,"unsignedTransactionBase64":STANDARD.encode(&self.unsigned_wire),"lastValidBlockHeight":self.last_valid_height})
    }
    pub fn expected(&self)->&ExpectedBasket{&self.expected}
    pub(crate) fn bind_investment(&mut self,tranche:crate::investment::Tranche)->Result<()>{
        tranche.validate(&self.expected)?;self.expected.investment=Some(tranche);Ok(())
    }
    pub fn prepared_id(&self,quote_id:&str)->Result<String>{self.expected.prepared_id(quote_id)}
    pub fn validate_state(&self,feed:&Feed,snapshot:&Snapshot)->Result<()>{
        feed.validate_fence(snapshot)?;
        if snapshot.hash!=self.state_hash||snapshot.revision!=self.revision||snapshot.slot!=self.expected.prepared_slot {
            return Err("basket prepared Bank revoked".into());}Ok(())
    }
    pub fn authorize(&self,feed:&Feed,snapshot:&Snapshot,wire:&[u8],quote_id:String)->Result<Entry>{
        self.validate_state(feed,snapshot)?;
        self.expected.authorize(wire,quote_id,self.last_valid_height)
    }
}

impl ExpectedBasket {
    pub(crate) fn intent(&self)->&Intent{&self.intent}
    pub(crate) fn investment(&self)->Option<&crate::investment::Tranche>{self.investment.as_ref()}
    pub(crate) fn validate_investment(&self)->Result<()>{if let Some(t)=&self.investment{t.validate(self)?;}Ok(())}
    pub(crate) fn wallet_owner(&self)->&str{&self.intent.owner}
    fn validate(&self)->Result<()>{
        self.intent.validate()?;
        self.validate_investment()?;
        if self.returns.len()!=self.intent.targets.len()||self.tokens.len()<2||self.tokens.len()>13||self.keys.len()>64
            ||self.keys.first()!=Some(&self.intent.owner)||!self.keys.contains(&self.nonce)||self.resources.is_empty()
            ||self.tokens.iter().map(|t|&t.account).collect::<BTreeSet<_>>().len()!=self.tokens.len()
            ||self.keys.iter().collect::<BTreeSet<_>>().len()!=self.keys.len(){return Err("basket expectation shape".into());}
        if self.tokens[0].mint!=USDC.to_string()||self.tokens[0].instrument.is_some(){return Err("basket cash identity".into());}
        let transit=self.tokens.iter().skip(1).filter(|t|t.instrument.is_none()).collect::<Vec<_>>();
        if transit.len()>1||transit.iter().any(|t|t.mint!=WSOL.to_string()||t.decimals!=9||t.program!="TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"){
            return Err("basket transit identity".into());}
        if self.returns.iter().any(|r|(r.opcode==18)!=(r.minimum_cash.is_some())||r.minimum_cash.is_some_and(|n|n==0||n>stocklana_adapters::MAX_INPUT))
            ||self.returns.iter().any(|r|r.opcode==18)!=!transit.is_empty(){return Err("basket funding return binding".into());}
        for t in &self.tokens{if !self.keys.contains(&t.account)||!self.resources.contains(&t.account.parse::<Pubkey>().map_err(|_|"basket token resource")?.to_bytes()){
            return Err("basket token not reserved".into());}}
        if !self.resources.contains(&self.nonce.parse::<Pubkey>().map_err(|_|"basket nonce resource")?.to_bytes()){return Err("basket nonce range not reserved".into());}
        Ok(())
    }
    fn prepared_id(&self,quote_id:&str)->Result<String>{
        self.validate()?;
        if quote_id.len()!=37||!quote_id.starts_with("stkq_")||!quote_id[5..].bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)){
            return Err("basket quote identity".into());}
        let mut h=Sha256::new();h.update(b"XTXC_PREPARED_BASKET_V1\0");h.update(quote_id.as_bytes());h.update(self.intent.commitment()?);h.update(self.message_hash);
        Ok(format!("stkp_{}",hex(&h.finalize()[..16])))
    }
    fn authorize(&self,wire:&[u8],quote_id:String,last_valid_height:u64)->Result<Entry>{
        let mut entry=sender::authorize(wire,Authorization{intent_id:self.prepared_id(&quote_id)?,
            message_hash:self.message_hash,last_valid_height,resources:self.resources.clone()})?;
        if wire[0]!=1||crate::pipeline::decode(&wire[65..])?.static_account_keys().first().map(ToString::to_string).as_deref()!=Some(&self.intent.owner){return Err("basket signing owner".into());}
        entry.quote_id=Some(quote_id);entry.expected_basket=Some(self.clone());
        if serde_json::to_vec(&entry).map_err(|_|"basket journal encoding")?.len()>16*1024-128{return Err("basket durable record bound".into());}
        Ok(entry)
    }
    fn stock_returns(&self,logs:&Value,before:&[u64],after:&[u64],cu:u64)->Result<Vec<Value>>{
        self.validate()?;
        if before.len()!=self.tokens.len()||after.len()!=before.len()||cu==0||cu>u64::from(self.intent.maximum_cu)
            ||before[0]<self.intent.total_input_atoms||before[0].checked_sub(after[0])!=Some(self.intent.validate()?){return Err("basket receipt cash/CU".into());}
        let returns=top_level_returns(logs,&self.program)?;
        if returns.len()!=self.returns.len(){return Err("basket all stock returns required".into());}
        for (i,t) in self.tokens.iter().enumerate().skip(1){
            if t.instrument.is_none()&&before[i]!=after[i]{return Err("basket existing funding inventory changed".into());}
        }
        let mut stocks=Vec::new();
        for (i,((target,layout),d)) in self.intent.targets.iter().zip(&self.returns).zip(returns).enumerate(){
            let products=self.tokens.iter().enumerate().filter(|(_,t)|t.instrument.as_deref()==Some(&target.instrument)).collect::<Vec<_>>();
            if products.is_empty()||d.len()!=layout.header+products.len()*16||d.get(..8)!=Some(layout.tag.as_bytes())
                ||integer(&d,8)?!=self.intent.owner_sequence+i as u64||integer(&d,16)?!=target.input_atoms||integer(&d,32)?!=products.len()as u64
                ||(layout.opcode!=13&&integer(&d,40)?!=0){return Err("basket stock return binding".into());}
            if let Some(floor)=layout.minimum_cash{let received=integer(&d,48)?;
                if received<floor||received>stocklana_adapters::MAX_INPUT{return Err("basket funded cash receipt floor".into());}}
            let mut total=0u64;let mut deltas=Vec::new();
            for (p,(index,t)) in products.iter().enumerate(){
                let raw=integer(&d,layout.header+p*16)?;let exposure=integer(&d,layout.header+p*16+8)?;
                if after[*index].checked_sub(before[*index])!=Some(raw)||(raw==0)!=(exposure==0){return Err("basket stock balance delta".into());}
                if t.fixed&&raw>0{let model=skew_native::ScaledUiAmount{decimals:t.decimals,multiplier_q32:1<<32,next_multiplier_effective_timestamp:i64::MAX,next_multiplier_q32:1<<32};
                    if model.exposure_q32(raw,t.numerator,t.denominator,t.conservative_bps).map_err(|_|"basket fixed conversion")?!=exposure{return Err("basket fixed exposure mismatch".into());}}
                total=total.checked_add(exposure).ok_or("basket stock total overflow")?;
                deltas.push(json!({"mint":t.mint,"rawAtoms":raw.to_string(),"exposureQ32":exposure.to_string()}));
            }
            if integer(&d,24)?!=total||total<target.minimum_exposure_q32{return Err("basket individual stock receipt floor".into());}
            stocks.push(json!({"instrument":target.instrument,"inputAtoms":target.input_atoms.to_string(),"actualExposureQ32":total.to_string(),"products":deltas}));
        }Ok(stocks)
    }
    fn native_cost(&self,pre:&[u64],post:&[u64],fee:u64)->Result<u64>{
        if pre.len()!=self.keys.len()||post.len()!=pre.len()||fee>10_000_000{return Err("basket native balance shape".into());}
        let mut rent=0u64;
        for name in self.tokens.iter().map(|t|&t.account).chain(std::iter::once(&self.nonce)){
            let index=self.keys.iter().position(|k|k==name).ok_or("basket native account index")?;
            if self.created.contains(name){if pre[index]!=0||post[index]==0{return Err("basket created account rent".into());}
                rent=rent.checked_add(post[index]).ok_or("basket rent overflow")?;
            }else if pre[index]!=post[index]{return Err("basket existing account rent changed".into());}
        }
        if pre[0].checked_sub(post[0])!=fee.checked_add(rent){return Err("basket payer debit exceeds fee and rent".into());}Ok(rent)
    }
    pub fn fetch(&self,rpc:&Rpc,entry:&Entry)->Result<Value>{
        if !matches!(entry.phase,Phase::Finalized|Phase::Reconciled){return Err("basket receipt not finalized".into());}
        rpc.check_genesis()?;
        let value=rpc.call("getTransaction",json!([entry.signature,{"encoding":"base64","commitment":"finalized","maxSupportedTransactionVersion":0}]))?;
        self.verify(entry,&value)
    }
    pub(crate) fn verify(&self,entry:&Entry,v:&Value)->Result<Value>{
        self.validate()?;
        if !matches!(entry.phase,Phase::Finalized|Phase::Reconciled)||entry.message_hash!=self.message_hash||entry.resources!=self.resources
            ||v["transaction"][1].as_str()!=Some("base64")||STANDARD.decode(v["transaction"][0].as_str().ok_or("basket receipt transaction")?).map_err(|_|"basket receipt encoding")?!=entry.wire{
            return Err("basket receipt phase/wire binding".into());}
        let authorized=sender::authorize(&entry.wire,Authorization{intent_id:entry.id.clone(),message_hash:self.message_hash,last_valid_height:entry.last_valid_height,resources:self.resources.clone()})?;
        if authorized.signature!=entry.signature{return Err("basket receipt signature".into());}
        let message=crate::pipeline::decode(&entry.wire[65..])?;
        let meta=&v["meta"];if !meta.get("err").ok_or("basket receipt error missing")?.is_null(){return Err("basket transaction failed".into());}
        let mut keys=message.static_account_keys().iter().map(ToString::to_string).collect::<Vec<_>>();
        for (kind,writable) in [("writable",true),("readonly",false)]{
            let count:usize=message.address_table_lookups().map_or(0,|tables|tables.iter().map(|t|if writable{t.writable_indexes.len()}else{t.readonly_indexes.len()}).sum());
            let rows=meta["loadedAddresses"][kind].as_array();if rows.map_or(0,Vec::len)!=count{return Err("basket receipt ALT count".into());}
            for row in rows.into_iter().flatten(){keys.push(row.as_str().ok_or("basket receipt ALT key")?.into());}
        }
        if keys!=self.keys{return Err("basket receipt resolved keys".into());}
        let balances=|kind:&str|meta[kind].as_array().ok_or("basket lamports missing")?.iter().map(|v|v.as_u64().ok_or_else(||"basket lamports integer".into())).collect::<Result<Vec<_>>>();
        let pre=balances("preBalances")?;let post=balances("postBalances")?;
        let fee=meta["fee"].as_u64().ok_or("basket fee")?;let rent=self.native_cost(&pre,&post,fee)?;
        let amounts=|kind:&str|->Result<Vec<u64>>{
            let rows=meta[kind].as_array().ok_or("basket token balances missing")?;
            self.tokens.iter().map(|t|{
                let index=keys.iter().position(|k|k==&t.account).ok_or("basket token index")?;
                let mut matching=rows.iter().filter(|r|r["accountIndex"].as_u64()==Some(index as u64));
                let Some(row)=matching.next() else {return if kind=="preTokenBalances"&&self.created.contains(&t.account)&&pre[index]==0{Ok(0)}else{Err("basket token balance absent".into())};};
                if matching.next().is_some()||row["mint"].as_str()!=Some(&t.mint)||row["owner"].as_str()!=Some(&self.intent.owner)
                    ||row["programId"].as_str()!=Some(&t.program)||row["uiTokenAmount"]["decimals"].as_u64()!=Some(u64::from(t.decimals)){return Err("basket token metadata binding".into());}
                let s=row["uiTokenAmount"]["amount"].as_str().ok_or("basket amount string")?;
                if s.is_empty()||s.len()>20||!s.bytes().all(|b|b.is_ascii_digit())||(s.len()>1&&s.starts_with('0')){return Err("basket amount canonicality".into());}
                s.parse().map_err(|_|"basket amount overflow".into())
            }).collect()
        };
        let cu=meta["computeUnitsConsumed"].as_u64().ok_or("basket CU missing")?;
        let stocks=self.stock_returns(&meta["logMessages"],&amounts("preTokenBalances")?,&amounts("postTokenBalances")?,cu)?;
        let slot=v["slot"].as_u64().filter(|s|*s>=self.prepared_slot&&*s<=self.intent.deadline_slot).ok_or("basket receipt slot")?;
        Ok(json!({"schema":"skew.stockmesh.basket-receipt/v1","signature":entry.signature,"owner":self.intent.owner,
            "strategyVersion":hex(&self.intent.strategy_version),"inputAtoms":self.intent.validate()?.to_string(),
            "retainedCashAtoms":self.intent.retained_cash_atoms.to_string(),"stocks":stocks,"computeUnits":cu,"feeLamports":fee.to_string(),"rentLamports":rent.to_string(),"slot":slot,
            "verification":"finalized_exact_wire_balances_and_each_sbf_stock_return"}))
    }
}

fn unsigned(message:&[u8])->Vec<u8>{let mut wire=vec![0;65];wire[0]=1;wire.extend_from_slice(message);wire}
fn hex(v:&[u8])->String{v.iter().map(|b|format!("{b:02x}")).collect()}
fn top_level_returns(logs:&Value,program:&str)->Result<Vec<Vec<u8>>>{
    let rows=logs.as_array().filter(|r|r.len()<=4096).ok_or("basket complete runtime logs required")?;
    let mut stack:Vec<String>=Vec::new();let mut returns=Vec::new();let mut bytes=0usize;let mut returned_top=false;
    for row in rows{
        let line=row.as_str().ok_or("basket log text")?;bytes=bytes.checked_add(line.len()).ok_or("basket log size")?;
        if bytes>512*1024{return Err("basket logs too large".into());}
        if let Some(s)=line.strip_prefix("Program return: "){
            let (p,data)=s.split_once(' ').ok_or("basket runtime return")?;
            if stack.last().map(String::as_str)!=Some(p){return Err("basket return outside invocation".into());}
            if stack.len()==1&&p==program{if returned_top{return Err("basket multiple returns from one stock invocation".into());}returned_top=true;returns.push(STANDARD.decode(data).map_err(|_|"basket return encoding")?);}
        }else if let Some(s)=line.strip_prefix("Program "){
            if let Some((p,depth))=s.split_once(" invoke ["){
                let d=depth.strip_suffix(']').and_then(|v|v.parse::<usize>().ok()).ok_or("basket invocation depth")?;
                if d!=stack.len()+1||d>8{return Err("basket invocation nesting".into());}if d==1{returned_top=false;}stack.push(p.into());
            }else if let Some(p)=s.strip_suffix(" success"){
                if stack.pop().as_deref()!=Some(p){return Err("basket unbalanced runtime success".into());}
            }else if s.contains(" failed:"){return Err("basket failed runtime invocation".into());}
        }
    }
    if !stack.is_empty(){return Err("basket truncated runtime logs".into());}Ok(returns)
}

#[cfg(test)]
pub(crate) fn archived_simulation(compiled:&Compiled,before:&Snapshot,after:&Snapshot,table:&AddressLookupTableAccount,logs:&Value,cu:u64)->Result<Value>{
    let mut before=before.clone();let mut after=after.clone();
    // The existing archived SBF runner does not execute a real Bank's fee
    // collection or lookup loading. Add EXPLICIT fixture lookup/fee metadata
    // only for host boundary checks; never label these as provider receipts.
    let mut data=vec![0;56];data[..4].copy_from_slice(&1u32.to_le_bytes());data[4..12].copy_from_slice(&u64::MAX.to_le_bytes());data[12..20].copy_from_slice(&(before.slot-1).to_le_bytes());
    for k in &table.addresses{data.extend_from_slice(k.as_ref());}
    let alt=Account{key:table.key.to_string(),owner:"AddressLookupTab1e1111111111111111111111111".into(),lamports:1,executable:false,data};
    for bank in [&mut before,&mut after]{bank.accounts.push(alt.clone());
        for key in crate::pipeline::decode(&compiled.message)?.static_account_keys(){if !bank.accounts.iter().any(|a|a.key==key.to_string()){
            bank.accounts.push(Account{key:key.to_string(),owner:Pubkey::default().to_string(),lamports:0,executable:false,data:vec![]});}}
    }
    let expected=compiled.expected(&before)?;
    let payer=after.accounts.iter_mut().find(|a|a.key==expected.intent.owner).ok_or("fixture payer")?;payer.lamports=payer.lamports.checked_sub(5000).ok_or("fixture fee")?;
    let addresses=expected.tokens.iter().map(|t|t.account.clone()).chain([expected.intent.owner.clone(),expected.nonce.clone()])
        .chain(compiled.outputs.iter().map(|o|o.mint.clone())).chain(compiled.policies.keys().cloned()).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
    let rows=addresses.iter().map(|k|account(&after,k).map(|a|json!({"owner":a.owner,"lamports":a.lamports,"executable":a.executable,"data":[STANDARD.encode(&a.data),"base64"]}))).collect::<Result<Vec<_>>>()?;
    let result=json!({"context":{"slot":before.slot},"value":{"err":null,"accounts":rows,"logs":logs,"unitsConsumed":cu,"fee":5000}});
    let checked=PreparedBasket::check_simulation(compiled,&expected,&before,&addresses,&result)?;
    let mut exhausted=result.clone();exhausted["value"]["err"]=json!({"InstructionError":[compiled.settlement_indices[0],"ComputationalBudgetExceeded"]});
    assert_eq!(PreparedBasket::check_simulation(compiled,&expected,&before,&addresses,&exhausted).unwrap_err(),"basket computational budget exceeded");
    exhausted["context"]["slot"]=json!(before.slot+1);
    assert!(!capacity_error(&PreparedBasket::check_simulation(compiled,&expected,&before,&addresses,&exhausted).unwrap_err()));
    exhausted["context"]["slot"]=json!(before.slot);exhausted["value"]["err"]=json!({"InstructionError":[compiled.settlement_indices[0],{"Custom":5}]});
    assert!(!capacity_error(&PreparedBasket::check_simulation(compiled,&expected,&before,&addresses,&exhausted).unwrap_err()));
    let mut missing_fee=result.clone();missing_fee["value"].as_object_mut().unwrap().remove("fee");
    assert!(PreparedBasket::check_simulation(compiled,&expected,&before,&addresses,&missing_fee).is_err());
    let mut overcharge=result.clone();overcharge["value"]["fee"]=json!(4999);
    assert!(PreparedBasket::check_simulation(compiled,&expected,&before,&addresses,&overcharge).is_err());
    let mut wrong_bank=result.clone();wrong_bank["context"]["slot"]=json!(before.slot+1);
    assert!(PreparedBasket::check_simulation(compiled,&expected,&before,&addresses,&wrong_bank).is_err());
    let mut missing_stock=result.clone();missing_stock["value"]["logs"]=json!([]);
    assert!(PreparedBasket::check_simulation(compiled,&expected,&before,&addresses,&missing_stock).is_err());
    let feed=Feed::new(before.accounts.iter().map(|a|a.key.clone()).collect(),std::time::Duration::from_secs(60),4*1024*1024)?;
    let bank=feed.publish(&json!({"context":{"slot":before.slot},"value":before.accounts.iter().map(|a|json!({"owner":a.owner,"lamports":a.lamports,"executable":a.executable,"data":[STANDARD.encode(&a.data),"base64"]})).collect::<Vec<_>>()}))?;
    let genesis="5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
    let (rpc,worker)=crate::stockmesh_api::submission_recovery_tests::rpc_script(vec![("getGenesisHash",json!(genesis)),("getGenesisHash",json!(genesis)),("simulateTransaction",result)]);
    let prepared=PreparedBasket::simulate(&feed,&bank,&rpc,compiled,1234)?;worker.join().unwrap();
    assert_eq!(prepared.review()["execution"],checked);assert_eq!(prepared.review()["unsignedTransactionBase64"],STANDARD.encode(unsigned(&compiled.message)));
    feed.invalidate()?;
    assert!(prepared.authorize(&feed,&bank,&unsigned(&compiled.message),format!("stkq_{}","1".repeat(32))).is_err());
    Ok(json!({"execution":checked,"expectedBytes":serde_json::to_vec(&expected).unwrap().len(),"feeCollection":"EXPLICIT_5000_LAMPORT_METADATA_FIXTURE_NOT_BANK_MEASUREMENT",
        "missingFeeRejected":true,"extraDebitRejected":true,"changedBankRejected":true,"missingStockReturnRejected":true,"actualPrepareRpcMethodChecked":true,"revokedBankRejected":true}))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ed25519_dalek::{Signer,SigningKey};
    use solana_instruction::AccountMeta;
    use crate::journal::Journal;
    use std::sync::Arc;
    pub(crate) fn candidate_fixture()->(crate::direct_prepare::BasketCandidate,Entry){
        let (entry,value)=fixture();let expected=entry.expected_basket.clone().unwrap();
        let keys=expected.keys.clone();let response=json!({"context":{"slot":100},"value":keys.iter().map(|_|json!({"owner":Pubkey::default().to_string(),"executable":false,"lamports":0,"data":["","base64"]})).collect::<Vec<_>>()});
        let feed=Arc::new(Feed::new(keys,std::time::Duration::from_secs(30),16384).unwrap());let snapshot=feed.publish(&response).unwrap();
        let mut summary=expected.verify(&entry,&value).unwrap();summary["schema"]=json!("skew.stockmesh.prepared-basket/v1");
        summary["catalogRevision"]=json!(hex(&expected.intent.catalog_revision));summary["inputAtoms"]=json!(90);summary["retainedCashAtoms"]=json!(10);
        summary["simulationCU"]=json!(200000);summary["ownerSequence"]=json!("8");summary["nextOwnerSequence"]=json!("10");summary["stateSlot"]=json!(100);
        summary["messageHash"]=json!(hex(&expected.message_hash));summary["requiresOwnerSignatures"]=json!(true);summary["submitted"]=json!(false);
        let prepared=PreparedBasket{expected,unsigned_wire:unsigned(&entry.wire[65..]),state_hash:snapshot.hash,revision:snapshot.revision,last_valid_height:120,summary};
        (crate::direct_prepare::BasketCandidate{feed,snapshot,prepared},entry)
    }
    pub(crate) fn fixture()->(Entry,Value){
        let signer=SigningKey::from_bytes(&[0xab;32]);let owner=Pubkey::new_from_array(signer.verifying_key().to_bytes());
        let program=Pubkey::new_from_array([2;32]);let nonce=Pubkey::new_from_array([3;32]);
        let token_program="TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
        let tokens=(0..3).map(|i|Token{account:Pubkey::new_from_array([4+i;32]).to_string(),mint:if i==0{USDC.to_string()}else{Pubkey::new_from_array([7+i;32]).to_string()},program:token_program.into(),decimals:0,
            instrument:match i{0=>None,1=>Some("NVDA".into()),_=>Some("MSFT".into())},fixed:true,numerator:1,denominator:1,conservative_bps:10000}).collect::<Vec<_>>();
        let intent=Intent{owner:owner.to_string(),strategy_version:[8;32],catalog_revision:[9;32],total_input_atoms:100,retained_cash_atoms:10,owner_sequence:8,deadline_slot:150,maximum_cu:1_400_000,
            targets:["NVDA","MSFT"].iter().map(|n|Target{instrument:n.to_string(),input_atoms:45,minimum_exposure_q32:45<<32}).collect()};
        // Deliberately synthetic instruction: exercises receipt/journal identity
        // only. Native economics are tested separately with archived real ELFs.
        let ix=Instruction{program_id:program,accounts:std::iter::once(AccountMeta::new(owner,true)).chain(std::iter::once(AccountMeta::new(nonce,false)))
            .chain(tokens.iter().map(|t|AccountMeta::new(t.account.parse().unwrap(),false))).collect(),data:vec![13]};
        let message=onebook_wire::compile_unsigned_v0(owner,&[ix],&[],[10;32]).unwrap();let msg=crate::pipeline::decode(&message).unwrap();
        let keys=msg.static_account_keys().iter().map(ToString::to_string).collect::<Vec<_>>();
        let resources=keys.iter().enumerate().filter(|(i,_)|msg.is_maybe_writable(*i,None)).map(|(_,k)|k.parse::<Pubkey>().unwrap().to_bytes()).collect();
        let expected=ExpectedBasket{intent,program:program.to_string(),nonce:nonce.to_string(),message_hash:Sha256::digest(&message).into(),keys,resources,prepared_slot:100,tokens,created:vec![],
            returns:(0..2).map(|_|ReturnLayout{opcode:13,tag:"SKEWEXP1".into(),header:40,minimum_cash:None}).collect(),investment:None};
        let mut wire=vec![1];wire.extend_from_slice(&signer.sign(&message).to_bytes());wire.extend_from_slice(&message);
        let mut entry=expected.authorize(&wire,format!("stkq_{}","b".repeat(32)),120).unwrap();entry.phase=Phase::Finalized;
        let mut logs=Vec::new();
        for i in 0..2u64 {let mut d=b"SKEWEXP1".to_vec();for n in [8+i,45,45<<32,1,45,45<<32]{d.extend_from_slice(&n.to_le_bytes());}
            logs.extend([format!("Program {program} invoke [1]"),format!("Program return: {program} {}",STANDARD.encode(d)),format!("Program {program} success")]);}
        let balances=|post:bool|expected.tokens.iter().enumerate().map(|(i,t)|json!({"accountIndex":expected.keys.iter().position(|k|k==&t.account).unwrap(),"mint":t.mint,"owner":owner.to_string(),"programId":t.program,
            "uiTokenAmount":{"decimals":t.decimals,"amount":match (i,post){(0,false)=>"100",(0,true)=>"10",(_,false)=>"0",_=>"45"}}})).collect::<Vec<_>>();
        let mut pre=vec![1;expected.keys.len()];let mut post=pre.clone();pre[0]=10_000_000;post[0]=9_995_000;
        let value=json!({"transaction":[STANDARD.encode(&wire),"base64"],"slot":110,"meta":{"err":null,"fee":5000,"computeUnitsConsumed":200000,"loadedAddresses":{"writable":[],"readonly":[]},
            "preBalances":pre,"postBalances":post,"preTokenBalances":balances(false),"postTokenBalances":balances(true),"logMessages":logs}});
        (entry,value)
    }
    #[test]
    fn funded_basket_receipts_preserve_inventory_and_use_funding_units(){
        let (entry,value)=fixture();let mut e=entry.expected_basket.unwrap();
        let transit=Token{account:Pubkey::new_from_array([55;32]).to_string(),mint:WSOL.to_string(),
            program:"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".into(),decimals:9,
            instrument:None,fixed:true,numerator:1,denominator:1,conservative_bps:10000};
        e.keys.push(transit.account.clone());e.resources.push(transit.account.parse::<Pubkey>().unwrap().to_bytes());e.tokens.push(transit);
        let mut logs=value["meta"]["logMessages"].clone();
        for version in [1,2]{
            e.returns[0]=ReturnLayout{opcode:18,tag:format!("SKEWMSF{version}"),header:56,minimum_cash:Some(120)};
            let encode=|received:u64|{let mut d=format!("SKEWMSF{version}").into_bytes();
                for n in [8,45,45<<32,1,0,received,45,45<<32]{d.extend_from_slice(&n.to_le_bytes());}
                json!(format!("Program return: {} {}",e.program,STANDARD.encode(d)))};
            logs[1]=encode(125); // SOL cash atoms are NOT USDC input atoms.
            let before=[100,0,0,123456789];let after=[10,45,45,123456789];
            assert!(e.stock_returns(&logs,&before,&after,200000).is_ok());
            let restored:ExpectedBasket=serde_json::from_slice(&serde_json::to_vec(&e).unwrap()).unwrap();
            assert!(restored.stock_returns(&logs,&before,&after,200000).is_ok());
            for changed in [123456788,123456790]{let mut bad=after;bad[3]=changed;assert!(e.stock_returns(&logs,&before,&bad,200000).is_err());}
            logs[1]=encode(119);assert!(e.stock_returns(&logs,&before,&after,200000).is_err());
            logs[1]=encode(stocklana_adapters::MAX_INPUT+1);assert!(e.stock_returns(&logs,&before,&after,200000).is_err());
            let mut missing=e.clone();missing.tokens.pop();assert!(missing.validate().is_err());
            let mut wrong=e.clone();wrong.tokens[3].mint=USDC.to_string();assert!(wrong.validate().is_err());
        }
    }
    #[test]
    fn basket_receipt_checks_every_stock_nonce_wire_fee_and_runtime_return(){
        let (entry,value)=fixture();let e=entry.expected_basket.as_ref().unwrap();
        assert_eq!(e.verify(&entry,&value).unwrap()["stocks"].as_array().unwrap().len(),2);
        for field in ["logMessages","fee","postTokenBalances"] {let mut bad=value.clone();bad["meta"].as_object_mut().unwrap().remove(field);assert!(e.verify(&entry,&bad).is_err());}
        let mut bad=value.clone();bad["meta"]["logMessages"].as_array_mut().unwrap().drain(..3);assert!(e.verify(&entry,&bad).is_err());
        let mut bad=value.clone();bad["meta"]["postTokenBalances"][2]["uiTokenAmount"]["amount"]=json!("0");assert!(e.verify(&entry,&bad).is_err());
        let mut bad=value.clone();bad["meta"]["postTokenBalances"][1]["uiTokenAmount"]["amount"]=json!("90");bad["meta"]["postTokenBalances"][2]["uiTokenAmount"]["amount"]=json!("0");assert!(e.verify(&entry,&bad).is_err());
        let mut bad=value.clone();let duplicate=bad["meta"]["postTokenBalances"][1].clone();bad["meta"]["postTokenBalances"].as_array_mut().unwrap().push(duplicate);assert!(e.verify(&entry,&bad).is_err());
        for field in ["mint","owner","programId"] {let mut bad=value.clone();bad["meta"]["postTokenBalances"][1][field]=json!(USDC.to_string());assert!(e.verify(&entry,&bad).is_err());}
        let mut bad=value.clone();bad["meta"]["postBalances"][0]=json!(9_994_999u64);assert!(e.verify(&entry,&bad).is_err());
        let mut bad=value.clone();bad["slot"]=json!(151);assert!(e.verify(&entry,&bad).is_err());
        let mut bad=entry.clone();bad.phase=Phase::Unknown;assert!(e.verify(&bad,&value).is_err());
        let mut bad=entry.clone();bad.wire[1]^=1;assert!(e.verify(&bad,&value).is_err());
        let mut bad=e.clone();bad.intent.owner_sequence+=1;assert!(bad.verify(&entry,&value).is_err());
        let encoded=serde_json::to_vec(e).unwrap();let recovered:ExpectedBasket=serde_json::from_slice(&encoded).unwrap();assert_eq!(recovered.verify(&entry,&value).unwrap(),e.verify(&entry,&value).unwrap());
    }
    #[test]
    fn basket_logs_cannot_use_nested_spoofed_truncated_or_last_only_returns(){
        let (entry,value)=fixture();let e=entry.expected_basket.as_ref().unwrap();let return_line=value["meta"]["logMessages"][1].as_str().unwrap();
        assert!(top_level_returns(&json!([return_line]),&e.program).is_err());
        let spoof=json!([format!("Program {} invoke [1]",e.program),format!("Program log: {return_line}"),format!("Program {} success",e.program)]);
        assert!(top_level_returns(&spoof,&e.program).unwrap().is_empty());
        let nested=json!(["Program other invoke [1]",format!("Program {} invoke [2]",e.program),return_line,format!("Program {} success",e.program),"Program other success"]);
        assert!(top_level_returns(&nested,&e.program).unwrap().is_empty());
        assert!(top_level_returns(&json!([format!("Program {} invoke [1]",e.program),return_line]),&e.program).is_err());
    }
    #[test]
    fn basket_unknown_nonce_range_survives_compaction_and_blocks_new_spending(){
        let (mut entry,value)=fixture();entry.phase=Phase::Prepared;
        let dir=std::env::temp_dir().join(format!("basket-journal-{}-{}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir(&dir).unwrap();let path=dir.join("isolated.wal");
        {let mut j=Journal::open(&path,8,65536).unwrap();j.insert(entry.clone()).unwrap();j.update(&entry.id,Phase::Unknown,true).unwrap();j.compact().unwrap();}
        {let mut j=Journal::open(&path,8,65536).unwrap();let saved=j.get(&entry.id).unwrap();assert_eq!(saved.phase,Phase::Unknown);assert_eq!(saved.wire,entry.wire);assert_eq!(saved.wallet_owner(),entry.wallet_owner());
            let mut other=entry.clone();other.id="second".into();other.signature="second".into();assert!(j.insert(other.clone()).is_err());
            j.update(&entry.id,Phase::Finalized,false).unwrap();let saved=j.get(&entry.id).unwrap();assert!(saved.expected_basket.as_ref().unwrap().verify(saved,&value).is_ok());
            j.update(&entry.id,Phase::Reconciled,false).unwrap();assert!(j.insert(other).is_ok());}
        for name in ["isolated.wal","isolated.wal.lock"] {std::fs::remove_file(dir.join(name)).unwrap();}std::fs::remove_dir(dir).unwrap();
    }
}
