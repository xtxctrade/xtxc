//! A basket is a vector of distinct stock intents, not one summed exposure.
//! Each existing settlement invocation retains its own policy and floor; the
//! owner signs one bounded message. Shared pools execute sequentially in that
//! message and MUST be simulated together. Independent quote sums are not an
//! execution result. This compiler owns no key and performs no network send.
use crate::{exposure_wire::{self,DirectExposureSpec,DirectProduct},feed::{Account,Snapshot},
    native_wire,onebook_wire,swap_wire::TokenAsset,wallet_wire::WalletSetup,world::NativeSwapProposal,Result};
use serde::{Deserialize,Serialize};
use sha2::{Digest,Sha256};
use solana_instruction::Instruction;
use solana_message::AddressLookupTableAccount;
use solana_pubkey::{pubkey,Pubkey};
use std::collections::{BTreeMap,BTreeSet};

const COMPUTE:Pubkey=pubkey!("ComputeBudget111111111111111111111111111111");
const USDC:Pubkey=pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const WSOL:Pubkey=pubkey!("So11111111111111111111111111111111111111112");
const CLOCK:&str="SysvarC1ock11111111111111111111111111111111";
// First release profile, not a silent truncation of a larger composition.
pub const MAX_ATOMIC_STOCKS:usize=3;
/// Only confirmed local capacity limits allow a smaller unsigned group.
/// Price/floor, policy, state, provider, wallet and unknown failures do not.
pub(crate) fn capacity_error(error:&str)->bool{matches!(error,
    "joint basket discovery resource bound"|"basket execution Bank resource bound"|
    "basket observation bounds"|"basket resolved account capacity"|
    "v0 transaction packet bound"|"basket computational budget exceeded")}
/// Runtime errors, not program log strings or HTTP/provider errors. SBPF can
/// report ProgramFailedToComplete on meter exhaustion; require the matching
/// runtime failure line AND exact exhausted budget for that ambiguous variant.
pub fn computational_exhaustion(error:&serde_json::Value,units:u64,limit:u64,instruction_count:usize,logs:&serde_json::Value)->bool{
    let Some(v)=error.get("InstructionError").and_then(serde_json::Value::as_array)else{return false};
    if v.len()!=2||v[0].as_u64().is_none_or(|i|i>=instruction_count as u64){return false;}
    if v[1].as_str()==Some("ComputationalBudgetExceeded"){return true;}
    if v[1].as_str()!=Some("ProgramFailedToComplete")||limit==0||units!=limit{return false;}
    let Some(lines)=logs.as_array()else{return false};if lines.len()>4096{return false;}
    lines.iter().filter_map(serde_json::Value::as_str).any(|line|line.strip_prefix("Program ")
        .and_then(|s|s.strip_suffix(" failed: exceeded CUs meter at BPF instruction"))
        .is_some_and(|s|s.parse::<Pubkey>().is_ok()))
}
mod execution;
pub use execution::{ExpectedBasket, PreparedBasket};
#[cfg(test)]
pub(crate) use execution::archived_simulation;
#[cfg(test)]
pub(crate) use execution::tests::fixture as recovery_fixture;
#[cfg(test)]
pub(crate) use execution::tests::candidate_fixture;

#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Target {pub instrument:String,pub input_atoms:u64,pub minimum_exposure_q32:u64}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Intent {
    pub owner:String,pub strategy_version:[u8;32],pub catalog_revision:[u8;32],
    pub total_input_atoms:u64,pub retained_cash_atoms:u64,pub owner_sequence:u64,
    pub deadline_slot:u64,pub maximum_cu:u32,pub targets:Vec<Target>,
}
impl Intent {
    /// The published version determines symbols, order, weights and reserve.
    /// A caller supplies only the investor's budget and per-stock quote floors.
    /// Publication authentication does not authorize that investor's spending.
    pub fn from_publication(publication:&crate::strategy::Published,owner:Pubkey,total_input_atoms:&str,
        catalog_revision:&str,owner_sequence:u64,deadline_slot:u64,minimums:&BTreeMap<String,u64>)->Result<Self>{
        Self::from_publication_range(publication,owner,total_input_atoms,catalog_revision,owner_sequence,deadline_slot,minimums,None)
    }
    pub(crate) fn from_publication_range(publication:&crate::strategy::Published,owner:Pubkey,total_input_atoms:&str,
        catalog_revision:&str,owner_sequence:u64,deadline_slot:u64,minimums:&BTreeMap<String,u64>,range:Option<std::ops::Range<usize>>)->Result<Self>{
        publication.verify()?;
        let document=&publication.document;
        let partial=range.is_some();let range=range.unwrap_or(0..document.legs.len());
        if range.start>=range.end||range.end>document.legs.len()||minimums.len()!=range.len(){return Err("basket quote set changed".into());}
        let (allocations,cash)=document.allocations(total_input_atoms)?;
        let retained_cash_atoms=if partial{0}else{cash};
        let spent=allocations[range.clone()].iter().try_fold(0u64,|s,n|s.checked_add(*n).ok_or("basket allocation overflow"))?;
        // A catalog expansion does not rewrite or invalidate an immutable
        // strategy. Its version retains the publication catalog; this new
        // execution commits the CURRENT catalog and freshly admitted quotes.
        let decode=|s:&str|->Result<[u8;32]>{if s.len()!=64||!s.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)){return Err("basket commitment encoding".into());}
            let mut result=[0;32];for (i,byte) in result.iter_mut().enumerate(){*byte=u8::from_str_radix(&s[2*i..2*i+2],16).map_err(|_|"basket commitment encoding")?;}Ok(result)};
        let intent=Self{owner:owner.to_string(),strategy_version:decode(&publication.version_hash)?,catalog_revision:decode(catalog_revision)?,
            total_input_atoms:spent.checked_add(retained_cash_atoms).ok_or("basket allocation overflow")?,retained_cash_atoms,owner_sequence,deadline_slot,maximum_cu:1_400_000,
            targets:document.legs[range.clone()].iter().zip(allocations[range].iter().copied()).map(|(leg,input_atoms)|Ok(Target{instrument:leg.instrument.clone(),input_atoms,
                minimum_exposure_q32:*minimums.get(&leg.instrument).ok_or("basket stock floor missing")?})).collect::<Result<_>>()?};
        intent.validate()?;Ok(intent)
    }
    pub fn validate(&self)->Result<u64>{
        let owner:Pubkey=self.owner.parse().map_err(|_|"basket owner")?;
        if owner==Pubkey::default() || self.strategy_version==[0;32] || self.catalog_revision==[0;32]
            || !(1..=MAX_ATOMIC_STOCKS).contains(&self.targets.len()) || self.total_input_atoms==0
            || self.total_input_atoms>stocklana_adapters::MAX_INPUT || self.deadline_slot==0
            || self.maximum_cu==0 || self.maximum_cu>1_400_000
            || self.owner_sequence.checked_add(self.targets.len() as u64).is_none_or(|v|v==u64::MAX) {
            return Err("basket intent bounds; continuation requires a separate approved plan".into());
        }
        let mut instruments=BTreeSet::new();let mut spent=0u64;
        for target in &self.targets {
            onebook_wire::instrument_id(&target.instrument)?;
            if !instruments.insert(&target.instrument)||target.input_atoms==0||target.minimum_exposure_q32==0 {
                return Err("basket duplicate or empty stock target".into());
            }
            spent=spent.checked_add(target.input_atoms).ok_or("basket funding overflow")?;
        }
        if spent.checked_add(self.retained_cash_atoms)!=Some(self.total_input_atoms){return Err("basket funding does not conserve cash".into());}
        Ok(spent)
    }
    pub fn commitment(&self)->Result<[u8;32]>{
        self.validate()?;let mut h=Sha256::new();h.update(b"XTXC_BASKET_INTENT_V1\0");
        h.update(serde_json::to_vec(self).map_err(|_|"basket intent encoding")?);Ok(h.finalize().into())
    }
}
pub struct Leg {pub products:Vec<DirectProduct>,pub proposals:Vec<NativeSwapProposal>,pub economic_reflow:bool,pub minimum_cash_atoms:Option<u64>}
/// USDC is the investor's budget. Some stocks trade against SOL, which is
/// bought and consumed inside the SAME settlement invocation. This does not
/// authorize wrapping the investor's SOL or spending existing wSOL inventory.
pub(crate) fn funding_cash(proposals:&[NativeSwapProposal])->Result<Option<(Pubkey,u64)>>{
    if proposals.is_empty(){return Err("basket proposals missing".into());}
    let funding=proposals.iter().filter(|p|p.stage==1&&p.product_id.is_none()).collect::<Vec<_>>();
    if funding.is_empty(){
        if proposals.iter().any(|p|p.stage!=1||p.product_id.is_none()){return Err("basket funding stages".into());}
        return Ok(None);
    }
    if funding.len()>3||proposals.iter().any(|p|!matches!((p.stage,p.product_id.is_some()),(1,false)|(2,true)))
        || !proposals.iter().any(|p|p.stage==2)
        || funding.iter().any(|p|p.market.input_mint!=USDC.to_string()||p.market.output_mint!=WSOL.to_string()){
        return Err("basket cash funding identity".into());
    }
    let atoms=funding.iter().try_fold(0u64,|n,p|n.checked_add(p.expected_output_atoms).ok_or("basket funding overflow"))?;
    if atoms==0||atoms>stocklana_adapters::MAX_INPUT{return Err("basket funding cash bound".into());}
    Ok(Some((WSOL,atoms)))
}
#[derive(Clone,Copy)]
pub struct ExecutionSettings {pub maximum_policy_age:u64,pub compute_unit_price_micro_lamports:u64,pub allow_underlying_closed:bool}
impl Default for ExecutionSettings {fn default()->Self{Self{maximum_policy_age:100,compute_unit_price_micro_lamports:0,allow_underlying_closed:false}}}
#[derive(Clone)]
struct Output {instrument:String,mint:String,token:String,program:String,fixed:bool,model:skew_native::ScaledUiAmount,
    numerator:u64,denominator:u64,conservative_bps:u16}
pub struct Compiled {
    pub instructions:Vec<Instruction>,pub message:Vec<u8>,pub intent_commitment:[u8;32],
    pub shared_pools:Vec<String>,pub settlement_indices:Vec<usize>,
    intent:Intent,program:Pubkey,nonce:Pubkey,source:TokenAsset,outputs:Vec<Output>,transits:Vec<TokenAsset>,
    message_hash:[u8;32],policies:BTreeMap<String,[u8;32]>,prepared_slot:u64,state_pins:BTreeMap<String,[u8;32]>,
}
#[derive(Debug,Serialize)]
#[serde(rename_all="camelCase")]
pub struct StockDelta {pub instrument:String,pub exposure_q32:String,pub products:Vec<ProductDelta>}
#[derive(Debug,Serialize)]
#[serde(rename_all="camelCase")]
pub struct ProductDelta {pub mint:String,pub raw_atoms:String}
#[derive(Debug,Serialize)]
#[serde(rename_all="camelCase")]
pub struct SimulationOutcome {pub spent_atoms:String,pub retained_cash_atoms:String,pub stocks:Vec<StockDelta>,pub compute_units:u64}

fn account<'a>(bank:&'a Snapshot,key:&str)->Result<&'a Account>{
    let mut matches=bank.accounts.iter().filter(|a|a.key==key);
    let row=matches.next().ok_or("basket account missing")?;
    if matches.next().is_some(){return Err("basket account ambiguous".into());}Ok(row)
}
fn integer(data:&[u8],offset:usize)->Result<u64>{
    Ok(u64::from_le_bytes(data.get(offset..offset+8).ok_or("basket account short")?.try_into().map_err(|_|"basket integer")?))
}
fn token_amount(bank:&Snapshot,address:&str,mint:&str,owner:&str,program:&str,allow_absent:bool)->Result<u64>{
    let row=account(bank,address)?;
    if allow_absent&&row.owner==Pubkey::default().to_string()&&!row.executable&&row.data.is_empty(){return Ok(0);}
    let mint:Pubkey=mint.parse().map_err(|_|"basket mint")?;let owner:Pubkey=owner.parse().map_err(|_|"basket owner")?;
    if row.executable||row.owner!=program||row.data.len()<165||row.data[..32]!=mint.to_bytes()
        || row.data[32..64]!=owner.to_bytes()||row.data[108]!=1 {return Err("basket token binding".into());}
    integer(&row.data,64)
}
fn compute(tag:u8,n:u32)->Instruction{let mut data=vec![tag];data.extend_from_slice(&n.to_le_bytes());Instruction{program_id:COMPUTE,accounts:vec![],data}}
fn state_pin(a:&Account)->[u8;32]{let mut h=Sha256::new();h.update(a.owner.as_bytes());h.update([u8::from(a.executable)]);h.update((a.data.len()as u64).to_le_bytes());h.update(&a.data);h.finalize().into()}

/// `bank` is ONE complete Bank read (or its checked setup projection), never
/// per-stock snapshots stitched together. Caller must validate deployed policy
/// authority/ELF and frozen ALTs, as in PrepareRuntime, before wallet admission.
pub fn compile(program:Pubkey,intent:Intent,legs:Vec<Leg>,bank:&Snapshot,setup:&WalletSetup,
    tables:&[AddressLookupTableAccount],blockhash:[u8;32])->Result<Compiled>{
    compile_configured(program,intent,legs,bank,setup,tables,blockhash,ExecutionSettings::default())
}
#[allow(clippy::too_many_arguments)]
pub fn compile_configured(program:Pubkey,intent:Intent,legs:Vec<Leg>,bank:&Snapshot,setup:&WalletSetup,
    tables:&[AddressLookupTableAccount],blockhash:[u8;32],settings:ExecutionSettings)->Result<Compiled>{
    intent.validate()?;
    if !(1..=150).contains(&settings.maximum_policy_age)||settings.compute_unit_price_micro_lamports>1_000_000{return Err("basket execution settings".into());}
    if program==Pubkey::default()||legs.len()!=intent.targets.len()||bank.slot>intent.deadline_slot
        || intent.deadline_slot-bank.slot>150||setup.wrap_lamports()!=0||setup.native_payout().is_some(){return Err("basket compiler bounds".into());}
    let owner:Pubkey=intent.owner.parse().map_err(|_|"basket owner")?;
    let source=native_wire::wallet_asset(owner,USDC,bank)?;
    let nonce=Pubkey::find_program_address(&[b"stocklana",owner.as_ref()],&program).0;
    let observed_nonce=account(bank,&nonce.to_string())?;
    if observed_nonce.owner!=program.to_string()||observed_nonce.executable||observed_nonce.data.len()!=64
        || &observed_nonce.data[..8]!=b"SKEWSEQ1"||observed_nonce.data[8..40]!=owner.to_bytes()
        ||integer(&observed_nonce.data,40)?!=intent.owner_sequence{return Err("basket lowering nonce binding".into());}
    let mut instructions=vec![compute(2,intent.maximum_cu),compute(1,262_144)];
    if settings.compute_unit_price_micro_lamports>0{let mut data=vec![3];data.extend_from_slice(&settings.compute_unit_price_micro_lamports.to_le_bytes());instructions.push(Instruction{program_id:COMPUTE,accounts:vec![],data});}
    instructions.extend_from_slice(setup.instructions());
    let mut outputs=Vec::new();let mut mints=BTreeSet::new();let mut pools=BTreeMap::new();let mut shared=BTreeSet::new();let mut indices=Vec::new();let mut policies=BTreeMap::new();let mut transits=BTreeMap::new();
    for (index,(target,leg)) in intent.targets.iter().zip(legs).enumerate(){
        let funding=funding_cash(&leg.proposals)?;
        let cash=if let Some((mint,quoted))=funding{
            if leg.minimum_cash_atoms.is_none_or(|n|n==0||n>quoted){return Err("basket funding floor".into());}
            let asset=native_wire::wallet_asset(owner,mint,bank)?;transits.insert(asset.token,asset);asset
        }else{if leg.minimum_cash_atoms.is_some(){return Err("basket unexpected funding floor".into());}source};
        let mut used=BTreeSet::new();
        for proposal in &leg.proposals{
            // Direction can differ, but shared physical state comes from this
            // single Bank. Never count a shared pool as independent liquidity.
            if used.insert(proposal.market.pool.clone())&&pools.insert(proposal.market.pool.clone(),index).is_some(){shared.insert(proposal.market.pool.clone());}
        }
        for product in &leg.products{
            let p=&product.product;
            if !mints.insert(p.mint)||[USDC,WSOL].contains(&p.mint){return Err("basket output overlaps another stock or cash".into());}
            let mint=account(bank,&p.mint.to_string())?;
            if mint.owner!=p.token_program.to_string()||mint.executable||mint.data.len()<82{return Err("basket mint binding".into());}
            let model=match p.model{
                0=>skew_native::ScaledUiAmount{decimals:mint.data[44],multiplier_q32:1<<32,next_multiplier_effective_timestamp:i64::MAX,next_multiplier_q32:1<<32},
                1=>{let clock=account(bank,CLOCK)?;let timestamp=integer(&clock.data,32)? as i64;
                    let model=skew_native::ScaledUiAmount::decode(&mint.data,timestamp).map_err(|_|"basket scaled exposure")?;
                    if model.next_multiplier_effective_timestamp>0&&(i128::from(timestamp)-i128::from(model.next_multiplier_effective_timestamp)).abs()<=900{return Err("basket corporate action window".into());}model},
                _=>return Err("basket exposure model".into()),
            };
            // A missing policy is not a fixture in the production compiler.
            let policy=account(bank,&p.policy.to_string())?;let d=&policy.data;
            if policy.owner!=program.to_string()||policy.executable||d.len()!=onebook_wire::STOCK_POLICY_V2_LEN
                || &d[..8]!=onebook_wire::STOCK_POLICY_V2_TAG||d[40..72]!=onebook_wire::instrument_id(&target.instrument)?
                || d[104..136]!=cash.mint.to_bytes()||d[136..168]!=p.mint.to_bytes()||integer(d,200)?!=p.policy_version {
                return Err("basket stock policy binding".into());
            }
            policies.insert(p.policy.to_string(),Sha256::digest(d).into());
            outputs.push(Output{instrument:target.instrument.clone(),mint:p.mint.to_string(),token:p.destination.to_string(),fixed:p.model==0,
                program:p.token_program.to_string(),model,numerator:p.numerator,denominator:p.denominator,conservative_bps:p.conservative_bps});
        }
        let spec=DirectExposureSpec{buyer:owner,buyer_nonce:nonce,input:source,buyer_sequence:intent.owner_sequence+index as u64,
            input_atoms:target.input_atoms,minimum_exposure_q32:target.minimum_exposure_q32,deadline_slot:intent.deadline_slot,
            maximum_policy_age:settings.maximum_policy_age,allow_underlying_closed:settings.allow_underlying_closed,products:leg.products};
        // The next invocation observes the previous invocation's nonce inside
        // this same transaction. Project ONLY that known sequence transition
        // for the existing compiler's precondition, never hypothetical pool
        // balances or a claimed execution result. Full-message simulation is
        // still mandatory and any preceding failure rolls everything back.
        let mut lowering=bank.clone();lowering.accounts.iter_mut().find(|a|a.key==nonce.to_string()).ok_or("basket nonce absent")?.data[40..48]
            .copy_from_slice(&(intent.owner_sequence+index as u64).to_le_bytes());
        let ix=if let Some(minimum_cash_atoms)=leg.minimum_cash_atoms{
            exposure_wire::compile_funded_exposure_reflow(program,exposure_wire::FundedExposureSpec{
                buyer:owner,buyer_nonce:nonce,input:source,cash,buyer_sequence:spec.buyer_sequence,input_atoms:target.input_atoms,
                minimum_cash_atoms,minimum_exposure_q32:target.minimum_exposure_q32,reflow_oracle_calls:16,
                deadline_slot:intent.deadline_slot,maximum_policy_age:settings.maximum_policy_age,
                allow_underlying_closed:settings.allow_underlying_closed,products:spec.products},&leg.proposals,&lowering)?
        }else if leg.economic_reflow{exposure_wire::compile_direct_exposure_reflow(program,spec,&leg.proposals,&lowering)?}
            else{exposure_wire::compile_direct_exposure(program,spec,&leg.proposals,&lowering)?};
        indices.push(instructions.len());instructions.push(ix);
    }
    // WalletSetup has private fields and typed constructors, but a caller can
    // still pass a setup planned for a different owner/program or extra mint.
    // Admit only this basket's canonical ATA creations and its nonce.
    let allowed=outputs.iter().map(|o|(o.token.clone(),(o.mint.clone(),o.program.clone())))
        .chain(transits.values().map(|a|(a.token.to_string(),(a.mint.to_string(),a.token_program.to_string()))))
        .chain(std::iter::once((source.token.to_string(),(USDC.to_string(),source.token_program.to_string())))).collect::<BTreeMap<_,_>>();
    for ix in setup.instructions(){
        if ix.program_id==program&&ix.data==[0]&&ix.accounts.len()==3&&ix.accounts[0].pubkey==owner&&ix.accounts[1].pubkey==nonce&&ix.accounts[2].pubkey==Pubkey::default(){continue;}
        if ix.program_id!=pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")||ix.data!=[1]||ix.accounts.len()!=6
            ||ix.accounts[0].pubkey!=owner||ix.accounts[2].pubkey!=owner||ix.accounts[4].pubkey!=Pubkey::default()
            ||allowed.get(&ix.accounts[1].pubkey.to_string())!=Some(&(ix.accounts[3].pubkey.to_string(),ix.accounts[5].pubkey.to_string())){return Err("basket setup outside approved assets".into());}
    }
    let message=onebook_wire::compile_unsigned_v0(owner,&instructions,tables,blockhash)?;
    let decoded=crate::pipeline::decode(&message)?;
    if decoded.header().num_required_signatures!=1||decoded.static_account_keys().first()!=Some(&owner){return Err("basket signer boundary".into());}
    let mut projected_wallet=allowed.keys().cloned().collect::<BTreeSet<_>>();projected_wallet.insert(owner.to_string());projected_wallet.insert(nonce.to_string());
    // Wallet setup changes only declared wallet accounts. All market, mint,
    // clock and policy bytes must still be from this exact common Bank.
    let state_pins=bank.accounts.iter().filter(|a|!a.executable&&!projected_wallet.contains(&a.key)).map(|a|(a.key.clone(),state_pin(a))).collect();
    Ok(Compiled{prepared_slot:bank.slot,state_pins,message_hash:Sha256::digest(&message).into(),policies,intent_commitment:intent.commitment()?,intent,program,nonce,source,outputs,transits:transits.into_values().collect(),instructions,message,shared_pools:shared.into_iter().collect(),settlement_indices:indices})
}
impl Compiled {
    /// Call only after success of the EXACT combined message in one Bank.
    /// No total-exposure shortcut: 1 extra NVDA cannot cover missing MSFT.
    /// This validates simulation deltas, not chain finality or a customer fill.
    pub fn verify_simulated(&self,executed_message:&[u8],before:&Snapshot,after:&Snapshot,compute_units:u64)->Result<SimulationOutcome>{
        if <[u8;32]>::from(Sha256::digest(executed_message))!=self.message_hash{return Err("basket simulation message changed".into());}
        if before.slot!=self.prepared_slot||before.slot!=after.slot||before.slot>self.intent.deadline_slot||compute_units==0||compute_units>u64::from(self.intent.maximum_cu){return Err("basket simulation bank/CU".into());}
        for (key,pin) in &self.state_pins {if state_pin(account(before,key)?)!=*pin{return Err("basket simulation prestate changed".into());}}
        for (key,hash) in &self.policies {for bank in [before,after]{let p=account(bank,key)?;if p.owner!=self.program.to_string()||p.executable||<[u8;32]>::from(Sha256::digest(&p.data))!=*hash{return Err("basket simulation policy changed".into());}}}
        let before_nonce=account(before,&self.nonce.to_string())?;
        let seq=if before_nonce.owner==Pubkey::default().to_string()&&before_nonce.data.is_empty(){0}else{
            if before_nonce.owner!=self.program.to_string()||before_nonce.executable||before_nonce.data.len()!=64||&before_nonce.data[..8]!=b"SKEWSEQ1"
                ||bs58::encode(&before_nonce.data[8..40]).into_string()!=self.intent.owner {return Err("basket pre-nonce".into());}integer(&before_nonce.data,40)?};
        let nonce=account(after,&self.nonce.to_string())?;
        let owner:Pubkey=self.intent.owner.parse().map_err(|_|"basket owner")?;
        if seq!=self.intent.owner_sequence||nonce.owner!=self.program.to_string()||nonce.executable||nonce.data.len()!=64
            || &nonce.data[..8]!=b"SKEWSEQ1"||nonce.data[8..40]!=owner.to_bytes()
            || integer(&nonce.data,40)?!=seq+self.intent.targets.len() as u64{return Err("basket nonce progression".into());}
        let source=|bank:&Snapshot|token_amount(bank,&self.source.token.to_string(),&USDC.to_string(),&self.intent.owner,&self.source.token_program.to_string(),false);
        let balance=source(before)?;let remaining=source(after)?;let spent=self.intent.validate()?;
        if balance<self.intent.total_input_atoms||balance.checked_sub(remaining)!=Some(spent){return Err("basket cash conservation".into());}
        for transit in &self.transits{
            let amount=|bank,absent|token_amount(bank,&transit.token.to_string(),&transit.mint.to_string(),&self.intent.owner,&transit.token_program.to_string(),absent);
            if amount(before,true)?!=amount(after,false)?{return Err("basket existing funding inventory changed".into());}
        }
        let mut stocks=Vec::new();
        for target in &self.intent.targets{
            let mut exposure=0u64;let mut products=Vec::new();
            for output in self.outputs.iter().filter(|p|p.instrument==target.instrument){
                let pre=account(before,&output.mint)?;let post=account(after,&output.mint)?;
                if pre.owner!=post.owner||pre.data!=post.data||pre.executable!=post.executable{return Err("basket mint changed during execution".into());}
                let amount=|bank,absent|token_amount(bank,&output.token,&output.mint,&self.intent.owner,&output.program,absent);
                let raw=amount(after,false)?.checked_sub(amount(before,true)?).ok_or("basket product debit")?;
                let gained=if raw==0{0}else{output.model.exposure_q32(raw,output.numerator,output.denominator,output.conservative_bps).map_err(|_|"basket exposure overflow")?};
                exposure=exposure.checked_add(gained).ok_or("basket exposure sum")?;
                products.push(ProductDelta{mint:output.mint.clone(),raw_atoms:raw.to_string()});
            }
            if exposure<target.minimum_exposure_q32{return Err("basket individual stock underfilled".into());}
            stocks.push(StockDelta{instrument:target.instrument.clone(),exposure_q32:exposure.to_string(),products});
        }
        Ok(SimulationOutcome{spent_atoms:spent.to_string(),retained_cash_atoms:self.intent.retained_cash_atoms.to_string(),stocks,compute_units})
    }
}
#[cfg(test)]mod tests{
    use super::*;
    #[test]fn funded_basket_shape_cannot_spend_native_sol_or_invent_stages(){
        let make=|stage,product:Option<&str>,source:Pubkey,destination:Pubkey|NativeSwapProposal{
            market:crate::market::MarketConfig{venue:crate::market::Venue::RaydiumClmm,program:crate::market::Venue::RaydiumClmm.program().into(),
                pool:Pubkey::new_unique().to_string(),config:String::new(),input_mint:source.to_string(),output_mint:destination.to_string(),tick_arrays:vec![],array_capacity:None,clock:String::new()},
            stage,product_id:product.map(str::to_owned),input_atoms:100,expected_output_atoms:90};
        let stock=Pubkey::new_unique();let path=vec![make(1,None,USDC,WSOL),make(2,Some("product"),WSOL,stock)];
        assert_eq!(funding_cash(&path).unwrap(),Some((WSOL,90)));
        assert_eq!(funding_cash(&[make(1,Some("product"),USDC,stock)]).unwrap(),None);
        for i in 0..6{let mut bad=path.clone();match i{
            0=>bad[0].market.input_mint=WSOL.to_string(),1=>bad[0].market.output_mint=stock.to_string(),
            2=>bad[1].stage=1,3=>bad[0].product_id=Some("product".into()),4=>bad[0].expected_output_atoms=0,_=>{bad.pop();}}
            assert!(funding_cash(&bad).is_err());}
    }
    fn intent()->Intent{Intent{owner:Pubkey::new_from_array([7;32]).to_string(),strategy_version:[1;32],catalog_revision:[2;32],
        total_input_atoms:101,retained_cash_atoms:11,owner_sequence:8,deadline_slot:99,maximum_cu:1_400_000,
        targets:vec![Target{instrument:"NVDA".into(),input_atoms:45,minimum_exposure_q32:9},Target{instrument:"MSFT".into(),input_atoms:45,minimum_exposure_q32:7}]}}
    #[test]fn one_cash_budget_preserves_vector_and_version(){let i=intent();assert_eq!(i.validate().unwrap(),90);let h=i.commitment().unwrap();
        for n in 0..4{let mut v=i.clone();match n{0=>v.strategy_version[0]+=1,1=>v.targets.swap(0,1),2=>v.targets[0].minimum_exposure_q32+=1,_=>v.owner_sequence+=1};assert_ne!(h,v.commitment().unwrap());}}
    #[test]fn cannot_overspend_duplicate_or_silently_truncate(){let i=intent();for n in 0..5{let mut v=i.clone();match n{0=>v.targets[1].instrument="NVDA".into(),1=>v.targets[0].input_atoms+=1,2=>v.retained_cash_atoms-=1,3=>v.owner_sequence=u64::MAX-1,_=>v.targets.extend(i.targets.clone())};assert!(v.validate().is_err());}}
    #[test]fn published_weights_are_the_only_budget_authority(){
        use ed25519_dalek::{Signer,SigningKey};use base64::{Engine,engine::general_purpose::STANDARD};
        let key=SigningKey::from_bytes(&[31;32]);let document=crate::strategy::Document{creator:bs58::encode(key.verifying_key().to_bytes()).into_string(),
            nonce:"11".repeat(16),version:1,previous_version_hash:None,catalog_revision:"22".repeat(32),name:"Technology".into(),description:String::new(),
            legs:vec![crate::strategy::Leg{instrument:"NVDA".into(),weight_bps:4500},crate::strategy::Leg{instrument:"MSFT".into(),weight_bps:4500}],cash_weight_bps:1000,creator_fee_bps:500,issued_at:100,expires_at:200};
        let publication=crate::strategy::Published{strategy_id:document.strategy_id().unwrap(),version_hash:document.version_hash().unwrap(),signature:STANDARD.encode(key.sign(document.message().unwrap().as_bytes()).to_bytes()),document};
        let owner=Pubkey::new_from_array([5;32]);let minimums=BTreeMap::from([("NVDA".into(),12),("MSFT".into(),9)]);
        let make=|p:&crate::strategy::Published,m:&BTreeMap<String,u64>|Intent::from_publication(p,owner,"100000001",&publication.document.catalog_revision,0,400,m);
        let i=make(&publication,&minimums).unwrap();assert_eq!(i.retained_cash_atoms,10_000_000);assert_eq!(i.targets.iter().map(|t|t.input_atoms).sum::<u64>(),90_000_001);
        let expanded=Intent::from_publication(&publication,owner,"100000001",&"33".repeat(32),0,400,&minimums).unwrap();
        assert_eq!(i.strategy_version,expanded.strategy_version);assert_ne!(i.commitment().unwrap(),expanded.commitment().unwrap());
        let mut tampered=publication.clone();tampered.document.legs.swap(0,1);assert!(make(&tampered,&minimums).is_err());
        let mut missing=minimums;missing.remove("MSFT");missing.insert("TSLA".into(),9);assert!(make(&publication,&missing).is_err());
    }
}
