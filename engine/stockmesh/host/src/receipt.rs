//! Finalized RPC receipts bound to the exact approved signed wire. Simulation
//! output, agent claims and sender acknowledgements cannot create this type.
use crate::{
    journal::{Entry, Phase},
    rpc::Rpc,
    Result,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use bincode::Options;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use solana_message::VersionedMessage;
use solana_pubkey::{pubkey, Pubkey};

// A first sale may create its output ATA in the approved transaction. Missing
// preTokenBalances is not generally zero: require the signed, canonical ATA
// setup AND zero pre-lamports before admitting this one absence.
fn initialized_output(
    message: &VersionedMessage,
    keys: &[String],
    expected: &Expected,
    meta: &Value,
    output_index: usize,
) -> Result<bool> {
    const ATA: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
    const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
    const TOKEN22: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
    let owner: Pubkey = expected.owner.parse().map_err(|_| "receipt ATA owner")?;
    let mint: Pubkey = expected.output_mint.parse().map_err(|_| "receipt ATA mint")?;
    if keys.first() != Some(&expected.owner)
        || !message.is_maybe_writable(output_index, None)
        || !meta["preBalances"].as_array().is_some_and(|balances| {
            balances.len() == keys.len() && balances[output_index].as_u64() == Some(0)
        })
    {
        return Ok(false);
    }
    let mut matches = 0;
    for (position, ix) in message.instructions().iter().enumerate() {
        if keys.get(usize::from(ix.program_id_index)) != Some(&ATA.to_string()) { continue; }
        let accounts = ix.accounts.iter().map(|index| {
            keys.get(usize::from(*index)).ok_or_else(|| "receipt ATA account index".to_string())
        }).collect::<Result<Vec<_>>>()?;
        if accounts.get(1).copied() != Some(&expected.output_account) { continue; }
        if ix.data != [1] || accounts.len() != 6 || position + 1 >= message.instructions().len()
            || accounts[0] != &expected.owner || accounts[2] != &expected.owner
            || accounts[3] != &expected.output_mint || accounts[4] != &Pubkey::default().to_string()
        {
            return Err("receipt output ATA setup binding".into());
        }
        let token: Pubkey = accounts[5].parse().map_err(|_| "receipt ATA token program")?;
        if ![TOKEN, TOKEN22].contains(&token)
            || Pubkey::find_program_address(&[owner.as_ref(), token.as_ref(), mint.as_ref()], &ATA).0.to_string()
                != expected.output_account
        {
            return Err("receipt output ATA derivation".into());
        }
        matches += 1;
    }
    Ok(matches == 1)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    pub owner: String,
    pub input_account: String,
    pub input_mint: String,
    pub output_account: String,
    pub output_mint: String,
    pub input: u64,
    pub minimum_output: u64,
    pub quoted_output: u64,
    pub maximum_cu: u64,
    /// Configured route cohort, pinned by the trusted compiler's approval.
    pub route: [u8; 32],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_output: Option<crate::native_payout::NativeOutput>,
}
pub struct Verified {
    signature: String,
    route: [u8; 32],
    input: u64,
    output: u64,
    quoted: u64,
    cu: u64,
    fee_lamports: u64,
    slot: u64,
    native_output: bool,
}
impl Verified {
    pub fn signature(&self) -> &str {
        &self.signature
    }
    pub fn route(&self) -> [u8; 32] {
        self.route
    }
    pub fn input(&self) -> u64 {
        self.input
    }
    pub fn output(&self) -> u64 {
        self.output
    }
    pub fn shortfall_bps(&self) -> u16 {
        (u128::from(self.quoted.saturating_sub(self.output)) * 10000 / u128::from(self.quoted))
            as u16
    }
    pub fn summary(&self) -> Value {
        json!({"signature":self.signature,"input":self.input.to_string(),"output":self.output.to_string(),"computeUnits":self.cu,
            "feeLamports":self.fee_lamports,"slot":self.slot,"verification":if self.native_output{"finalized_exact_wire_and_native_payout"}else{"finalized_exact_wire_and_token_balances"}})
    }
}
pub fn fetch(rpc: &Rpc, entry: &Entry, expected: &Expected) -> Result<Verified> {
    if !matches!(entry.phase, Phase::Finalized | Phase::Reconciled) {
        return Err("receipt not finalized".into());
    }
    rpc.check_genesis()?;
    let value=rpc.call("getTransaction",json!([entry.signature,{"encoding":"base64","commitment":"finalized","maxSupportedTransactionVersion":0}]))?;
    verify(entry, expected, &value)
}
fn verify(e: &Entry, x: &Expected, v: &Value) -> Result<Verified> {
    if x.input == 0
        || x.minimum_output == 0
        || x.quoted_output < x.minimum_output
        || x.maximum_cu > 1_400_000
        || x.input_account == x.output_account
        || x.input_mint == x.output_mint
    {
        return Err("receipt expectation bounds".into());
    }
    let wire = STANDARD
        .decode(v["transaction"][0].as_str().ok_or("missing transaction")?)
        .map_err(|e| e.to_string())?;
    if wire != e.wire {
        return Err("receipt wire substitution".into());
    }
    // Reverify persisted approval and every owner's signature before trusting RPC metadata.
    crate::sender::authorize(
        &wire,
        crate::sender::Authorization {
            intent_id: e.id.clone(),
            message_hash: e.message_hash,
            last_valid_height: e.last_valid_height,
            resources: e.resources.clone(),
        },
    )?;
    let n = wire[0] as usize;
    let message: VersionedMessage = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(1232)
        .reject_trailing_bytes()
        .deserialize(&wire[1 + 64 * n..])
        .map_err(|e| e.to_string())?;
    if bs58::encode(&wire[1..65]).into_string() != e.signature {
        return Err("receipt signature binding".into());
    }
    let mut keys: Vec<_> = message
        .static_account_keys()
        .iter()
        .map(|k| k.to_string())
        .collect();
    let meta = v
        .get("meta")
        .filter(|m| m.is_object())
        .ok_or("missing receipt metadata")?;
    if !meta.get("err").ok_or("missing execution status")?.is_null() {
        return Err("execution failed".into());
    }
    for kind in ["writable", "readonly"] {
        if let Some(loaded) = meta["loadedAddresses"][kind].as_array() {
            if loaded.len() > 64 {
                return Err("loaded address bound".into());
            }
            for k in loaded {
                keys.push(k.as_str().ok_or("loaded address")?.to_string());
            }
        }
    }
    if keys.len() > 256 {
        return Err("receipt account bound".into());
    }
    let amount = |kind: &str, account: &str, mint: &str| -> Result<u64> {
        let idx = keys
            .iter()
            .position(|k| k == account)
            .ok_or("token account absent from wire")?;
        let balances = meta[kind].as_array().ok_or("token balances missing")?;
        let matches: Vec<_> = balances
            .iter()
            .filter(|b| b["accountIndex"].as_u64() == Some(idx as u64))
            .collect();
        if matches.is_empty() && kind == "preTokenBalances" && account == x.output_account
            && initialized_output(&message, &keys, x, meta, idx)?
        {
            return Ok(0);
        }
        if matches.len() != 1 {
            return Err("missing/duplicate token balance".into());
        }
        let b = matches[0];
        if b["mint"].as_str() != Some(mint) || b["owner"].as_str() != Some(&x.owner) {
            return Err("receipt mint/owner substitution".into());
        }
        b["uiTokenAmount"]["amount"]
            .as_str()
            .ok_or("integer token balance")?
            .parse()
            .map_err(|_| "token amount overflow".into())
    };
    let input = amount("preTokenBalances", &x.input_account, &x.input_mint)?
        .checked_sub(amount(
            "postTokenBalances",
            &x.input_account,
            &x.input_mint,
        )?)
        .ok_or("wrong input direction")?;
    let output = if let Some(native)=&x.native_output {
        if x.output_mint!=crate::native_payout::WSOL.to_string() || x.output_account!=native.payout.asset()?.token.to_string()
            || x.owner!=native.payout.owner()?.to_string() {return Err("receipt native payout identity".into());}
        native.verify_receipt(&message,&keys,meta)?
    } else {
        amount("postTokenBalances",&x.output_account,&x.output_mint)?
            .checked_sub(amount("preTokenBalances",&x.output_account,&x.output_mint)?).ok_or("wrong output direction")?
    };
    let cu = meta["computeUnitsConsumed"]
        .as_u64()
        .ok_or("missing on-chain CU")?;
    if input != x.input || output < x.minimum_output || cu == 0 || cu > x.maximum_cu {
        return Err("settlement postcondition violation".into());
    }
    Ok(Verified {
        signature: e.signature.clone(),
        route: x.route,
        input,
        output,
        quoted: x.quoted_output,
        cu,
        fee_lamports: meta["fee"].as_u64().ok_or("receipt fee")?,
        slot: v["slot"].as_u64().ok_or("receipt slot")?,
        native_output: x.native_output.is_some(),
    })
}

/// Conservative execution-cost feedback, separate from fair-value consensus.
/// Only verified actual fills enter. A fill can raise a route penalty; favorable
/// or self-generated fills cannot relax it. Offline held-out validation is needed
/// before enabling any automatic decrease or probabilistic outcome claim.
pub struct CostModel {
    epoch: u64,
    seen: std::collections::BTreeSet<String>,
    routes: std::collections::BTreeMap<[u8; 32], (u64, u16)>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CostState {
    pub epoch: u64,
    pub seen: std::collections::BTreeSet<String>,
    pub routes: Vec<([u8; 32], u64, u16)>,
}
impl Default for CostModel {
    fn default() -> Self {
        Self {
            epoch: 1,
            seen: Default::default(),
            routes: Default::default(),
        }
    }
}
impl CostModel {
    pub fn observe(&mut self, r: &Verified) -> Result<bool> {
        if self.seen.contains(r.signature()) {
            return Ok(false);
        }
        if self.seen.len() == 4096
            || (self.routes.len() == 64 && !self.routes.contains_key(&r.route()))
        {
            return Err("feedback capacity; rotate a durable evaluation epoch".into());
        }
        let old = self.routes.get(&r.route()).copied().unwrap_or((0, 0));
        let count = old.0.checked_add(1).ok_or("feedback count overflow")?;
        self.routes
            .insert(r.route(), (count, old.1.max(r.shortfall_bps())));
        self.seen.insert(r.signature().into());
        Ok(true)
    }
    pub fn penalty_bps(&self, route: [u8; 32]) -> Option<u16> {
        self.routes.get(&route).map(|v| v.1)
    }
    pub fn state(&self) -> CostState {
        CostState {
            epoch: self.epoch,
            seen: self.seen.clone(),
            routes: self
                .routes
                .iter()
                .map(|(route, (count, penalty))| (*route, *count, *penalty))
                .collect(),
        }
    }
    pub fn restore(&mut self, state: CostState) -> Result<()> {
        if state.epoch == 0
            || state.seen.len() > 4096
            || state.routes.len() > 64
            || state
                .seen
                .iter()
                .any(|signature| signature.is_empty() || signature.len() > 128)
        {
            return Err("cost model snapshot bounds".into());
        }
        let mut routes = std::collections::BTreeMap::new();
        for (route, count, penalty) in state.routes {
            if count == 0 || penalty > 10_000 || routes.insert(route, (count, penalty)).is_some() {
                return Err("cost model snapshot route".into());
            }
        }
        self.epoch = state.epoch;
        self.seen = state.seen;
        self.routes = routes;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};
    fn created_output_fixture(attack: &str) -> (Entry, Expected, Value) {
        use solana_instruction::{AccountMeta, Instruction};
        let signer = SigningKey::from_bytes(&[31; 32]);
        let owner = Pubkey::new_from_array(signer.verifying_key().to_bytes());
        let token = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
        let ata = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
        let input = Pubkey::new_from_array([51;32]);
        let input_mint = Pubkey::new_from_array([52;32]);
        let mint = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        let output = Pubkey::find_program_address(&[owner.as_ref(),token.as_ref(),mint.as_ref()],&ata).0;
        let mut setup = Instruction { program_id:ata, data:vec![1], accounts:vec![
            AccountMeta::new(owner,true),AccountMeta::new(output,false),AccountMeta::new_readonly(owner,false),
            AccountMeta::new_readonly(mint,false),AccountMeta::new_readonly(Pubkey::default(),false),AccountMeta::new_readonly(token,false),
        ]};
        match attack {
            "recipient"=>setup.accounts[2].pubkey=Pubkey::new_from_array([61;32]),
            "mint"=>setup.accounts[3].pubkey=Pubkey::new_from_array([62;32]),
            "program"=>setup.accounts[5].pubkey=Pubkey::new_from_array([63;32]),
            "opcode"=>setup.data=vec![0],
            "arity"=>setup.accounts.push(AccountMeta::new_readonly(owner,false)),
            _=>{},
        }
        // Receipt contract fixture only: this final instruction is not an SBF fill.
        let settle = Instruction { program_id:Pubkey::new_from_array([64;32]),data:vec![20],
            accounts:vec![AccountMeta::new(owner,true),AccountMeta::new(input,false),AccountMeta::new(output,false)] };
        let instructions = match attack {
            "none"=>vec![settle], "late"=>vec![settle,setup],
            "duplicate"=>vec![setup.clone(),setup,settle],_=>vec![setup,settle],
        };
        let bytes = crate::onebook_wire::compile_unsigned_v0(owner,&instructions,&[],[7;32]).unwrap();
        let message: VersionedMessage = bincode::deserialize(&bytes).unwrap();
        let keys = message.static_account_keys();
        let position=|key:Pubkey|keys.iter().position(|v|*v==key).unwrap();
        let mut wire=vec![1];wire.extend(signer.sign(&bytes).to_bytes());wire.extend(&bytes);
        let mut entry=crate::sender::authorize(&wire,crate::sender::Authorization{
            intent_id:"created-output-fixture".into(),message_hash:Sha256::digest(&bytes).into(),last_valid_height:100,resources:vec![],
        }).unwrap();entry.phase=Phase::Finalized;
        let x=Expected{owner:owner.to_string(),input_account:input.to_string(),input_mint:input_mint.to_string(),
            output_account:output.to_string(),output_mint:mint.to_string(),input:100,minimum_output:190,quoted_output:200,maximum_cu:200000,route:[5;32],native_output:None};
        let balance=|account:Pubkey,mint:Pubkey,n:&str|json!({"accountIndex":position(account),"mint":mint.to_string(),"owner":owner.to_string(),"uiTokenAmount":{"amount":n}});
        let mut pre_lamports=vec![1_000_000u64;keys.len()];pre_lamports[position(output)]=0;
        let value=json!({"slot":20,"transaction":[STANDARD.encode(wire),"base64"],"meta":{"err":null,"computeUnitsConsumed":120000,"fee":5000,
            "preBalances":pre_lamports,"preTokenBalances":[balance(input,input_mint,"1000")],
            "postTokenBalances":[balance(input,input_mint,"900"),balance(output,mint,"195")]}});
        (entry,x,value)
    }

    #[test]
    fn first_sale_accepts_only_signed_canonical_output_creation() {
        let (entry,x,value)=created_output_fixture("");
        let result=verify(&entry,&x,&value).unwrap();
        assert_eq!(result.input(),100);assert_eq!(result.output(),195);
        for attack in ["none","late","duplicate","recipient","mint","program","opcode","arity"] {
            let (entry,x,value)=created_output_fixture(attack);
            assert!(verify(&entry,&x,&value).is_err(),"{attack}");
        }
    }

    // Locally signed fixtures exercise the full receipt verifier, including
    // exact-wire authorization. They are never sent to a chain.
    pub(crate) fn native_output_fixture(attack: &str) -> (Entry, Expected, Value) {
        use crate::native_payout::{NativeOutput, NativePayout, WSOL};
        use solana_instruction::{Instruction,AccountMeta};
        let signer=SigningKey::from_bytes(&[37;32]);
        let owner=Pubkey::new_from_array(signer.verifying_key().to_bytes());
        let input=Pubkey::new_from_array([61;32]);let mint=Pubkey::new_from_array([62;32]);
        let program=Pubkey::new_from_array([63;32]);
        let payout=NativePayout::new(owner,&format!("stkq_{}","a".repeat(32)),2_039_280).unwrap();
        let output=payout.asset().unwrap().token;
        let mut instructions=payout.setup().unwrap().to_vec();
        instructions.push(Instruction{program_id:program,data:vec![20],accounts:vec![AccountMeta::new(owner,true),AccountMeta::new(input,false),AccountMeta::new(output,false)]});
        let mut close=payout.close().unwrap();
        if attack=="recipient" {close.accounts[1].pubkey=Pubkey::new_from_array([64;32]);}
        if attack!="missing_close" {instructions.push(close);}
        if attack=="extra_use" {instructions.insert(2,Instruction{program_id:program,data:vec![21],accounts:vec![AccountMeta::new(output,false)]});}
        if attack=="opcode" {instructions[2].data[0]=13;}
        let bytes=crate::onebook_wire::compile_unsigned_v0(owner,&instructions,&[],[7;32]).unwrap();
        let message:VersionedMessage=bincode::deserialize(&bytes).unwrap();
        let keys=message.static_account_keys();let pos=|key:Pubkey|keys.iter().position(|k|*k==key).unwrap();
        let mut wire=vec![1];wire.extend(signer.sign(&bytes).to_bytes());wire.extend(&bytes);
        let mut entry=crate::sender::authorize(&wire,crate::sender::Authorization{intent_id:"native-payout-fixture".into(),message_hash:Sha256::digest(&bytes).into(),last_valid_height:100,resources:vec![]}).unwrap();
        entry.phase=Phase::Finalized;
        let native=NativeOutput{payout,settlement_program:program.to_string(),created_accounts:vec![]};
        let expected=Expected{owner:owner.to_string(),input_account:input.to_string(),input_mint:mint.to_string(),output_account:output.to_string(),output_mint:WSOL.to_string(),input:100,minimum_output:190,quoted_output:200,maximum_cu:200000,route:[5;32],native_output:Some(native)};
        let balance=|n:&str|json!({"accountIndex":pos(input),"mint":mint.to_string(),"owner":owner.to_string(),"uiTokenAmount":{"amount":n}});
        let mut pre=vec![1_000_000u64;keys.len()];pre[pos(output)]=0;
        let mut post=pre.clone();post[pos(owner)]=pre[pos(owner)]-5_000+195;
        let value=json!({"slot":20,"transaction":[STANDARD.encode(wire),"base64"],"meta":{"err":null,"computeUnitsConsumed":120000,"fee":5000,"preBalances":pre,"postBalances":post,
            "preTokenBalances":[balance("1000")],"postTokenBalances":[balance("900")]}});
        (entry,expected,value)
    }

    #[test]
    fn native_sale_receipt_is_wallet_credit_not_a_missing_wrapped_token_balance() {
        let (entry,expected,value)=native_output_fixture("");
        let verified=verify(&entry,&expected,&value).unwrap();
        assert_eq!(verified.input(),100);assert_eq!(verified.output(),195);
        assert_eq!(verified.summary()["verification"],"finalized_exact_wire_and_native_payout");
        assert_eq!(verified.summary()["input"],"100");assert_eq!(verified.summary()["output"],"195");
        if let Ok(path)=std::env::var("SKEW_NATIVE_RECEIPT_VECTOR") {
            use std::io::Write;
            let path=std::path::Path::new(&path);
            assert!(path.starts_with("/srv/skew/stockmesh-direct-node-20260920/evidence") && !path.components().any(|c|matches!(c,std::path::Component::ParentDir)));
            let payload=json!({"schema":"skew.stocklana.submission/v1","quoteId":format!("stkq_{}","a".repeat(32)),"preparedId":format!("stkp_{}","b".repeat(32)),
                "signature":verified.signature(),"phase":"RECONCILED","attempts":1,"sameSignedWire":true,"verifiedSwap":verified.summary()});
            std::fs::OpenOptions::new().write(true).create_new(true).open(path).unwrap().write_all(&serde_json::to_vec_pretty(&payload).unwrap()).unwrap();
        }
        let mut legacy=expected.clone();legacy.native_output=None;
        assert!(verify(&entry,&legacy,&value).is_err());
        for attack in ["recipient","missing_close","extra_use","opcode"] {
            let (entry,expected,value)=native_output_fixture(attack);
            assert!(verify(&entry,&expected,&value).is_err(),"{attack}");
        }
        for attack in ["fee","floor","missing_balance","input","wrong_owner","duplicate_rent","wire"] {
            let (entry,mut expected,mut value)=native_output_fixture("");
            match attack {
                "fee"=>{value["meta"].as_object_mut().unwrap().remove("fee");},
                "floor"=>expected.minimum_output=196,
                "missing_balance"=>value["meta"]["postBalances"]=json!([]),
                "input"=>value["meta"]["postTokenBalances"][0]["uiTokenAmount"]["amount"]=json!("899"),
                "wrong_owner"=>expected.owner=Pubkey::new_from_array([99;32]).to_string(),
                "duplicate_rent"=>expected.native_output.as_mut().unwrap().created_accounts=vec![expected.input_account.clone();2],
                "wire"=>value["transaction"][0]=json!("AAAA"),
                _=>unreachable!(),
            }
            assert!(verify(&entry,&expected,&value).is_err(),"{attack}");
        }
    }

    #[test]
    fn pre_native_output_journal_expectations_still_round_trip() {
        let (_,expected,_)=fixture();
        let old=serde_json::to_value(&expected).unwrap();
        assert!(old.get("native_output").is_none());
        let restored:Expected=serde_json::from_value(old.clone()).unwrap();
        assert!(restored.native_output.is_none());
        assert_eq!(serde_json::to_value(restored).unwrap(),old);
    }

    #[test]
    fn output_creation_does_not_mask_missing_balances_or_changed_identity() {
        for attack in ["pre_lamports","missing_lamports","short_lamports","input","post","owner","mint","floor","duplicate"] {
            let (entry,x,mut value)=created_output_fixture("");
            let meta=&mut value["meta"];
            match attack {
                "pre_lamports"=>{for n in meta["preBalances"].as_array_mut().unwrap(){*n=json!(1);}},
                "missing_lamports"=>{meta.as_object_mut().unwrap().remove("preBalances");},
                "short_lamports"=>meta["preBalances"]=json!([]),
                "input"=>meta["preTokenBalances"]=json!([]),
                "post"=>{meta["postTokenBalances"].as_array_mut().unwrap().pop();},
                "owner"=>meta["postTokenBalances"][1]["owner"]=json!("other"),
                "mint"=>meta["postTokenBalances"][1]["mint"]=json!("other"),
                "floor"=>meta["postTokenBalances"][1]["uiTokenAmount"]["amount"]=json!("189"),
                "duplicate"=>{let row=meta["postTokenBalances"][1].clone();meta["postTokenBalances"].as_array_mut().unwrap().push(row);},
                _=>unreachable!(),
            }
            assert!(verify(&entry,&x,&value).is_err(),"{attack}");
        }
    }
    fn fixture() -> (Entry, Expected, Value) {
        let signer = SigningKey::from_bytes(&[31; 32]);
        let public = signer.verifying_key().to_bytes();
        let mut message = vec![1, 0, 2, 3];
        message.extend(public);
        message.extend([3; 32]);
        message.extend([4; 32]);
        message.extend([0; 32]);
        message.push(0);
        let mut wire = vec![1];
        wire.extend(signer.sign(&message).to_bytes());
        wire.extend(&message);
        let mut e = crate::sender::authorize(
            &wire,
            crate::sender::Authorization {
                intent_id: "fixture".into(),
                message_hash: Sha256::digest(message).into(),
                last_valid_height: 100,
                resources: vec![],
            },
        )
        .unwrap();
        e.phase = Phase::Finalized;
        let owner = bs58::encode(public).into_string();
        let input = bs58::encode([3; 32]).into_string();
        let output = bs58::encode([4; 32]).into_string();
        let x = Expected {
            owner: owner.clone(),
            input_account: input,
            input_mint: "mint-a".into(),
            output_account: output,
            output_mint: "mint-b".into(),
            input: 100,
            minimum_output: 190,
            quoted_output: 200,
            maximum_cu: 1400000,
            route: [5; 32],
            native_output: None,
        };
        let token =
            |i, m, a| json!({"accountIndex":i,"mint":m,"owner":owner,"uiTokenAmount":{"amount":a}});
        let v = json!({"slot":20,"transaction":[STANDARD.encode(wire),"base64"],"meta":{"err":null,"computeUnitsConsumed":1200000,"fee":5000,
            "preTokenBalances":[token(1,"mint-a","1000"),token(2,"mint-b","0")],
            "postTokenBalances":[token(1,"mint-a","900"),token(2,"mint-b","195")]}});
        (e, x, v)
    }
    #[test]
    fn balance_and_wire_attacks_never_become_feedback() {
        for attack in [
            "wire",
            "error",
            "owner",
            "mint",
            "input",
            "floor",
            "cu",
            "missing",
            "duplicate",
            "signature",
        ] {
            let (mut e, x, mut v) = fixture();
            match attack {
                "wire" => v["transaction"][0] = json!("AAAA"),
                "error" => v["meta"]["err"] = json!({"failed":true}),
                "owner" => v["meta"]["postTokenBalances"][0]["owner"] = json!("other"),
                "mint" => v["meta"]["postTokenBalances"][1]["mint"] = json!("other"),
                "input" => {
                    v["meta"]["postTokenBalances"][0]["uiTokenAmount"]["amount"] = json!("899")
                }
                "floor" => {
                    v["meta"]["postTokenBalances"][1]["uiTokenAmount"]["amount"] = json!("189")
                }
                "cu" => v["meta"]["computeUnitsConsumed"] = json!(1400001),
                "missing" => v["meta"]["postTokenBalances"] = json!([]),
                "duplicate" => {
                    let t = v["meta"]["postTokenBalances"][0].clone();
                    v["meta"]["postTokenBalances"]
                        .as_array_mut()
                        .unwrap()
                        .push(t);
                }
                "signature" => e.message_hash = [0; 32],
                _ => unreachable!(),
            }
            assert!(verify(&e, &x, &v).is_err(), "{attack}");
        }
    }
    #[test]
    fn duplicate_or_favorable_fills_cannot_relax_execution_penalties() {
        let (e, x, v) = fixture();
        let r = verify(&e, &x, &v).unwrap();
        let mut model = CostModel::default();
        assert!(model.observe(&r).unwrap());
        assert!(!model.observe(&r).unwrap());
        assert_eq!(model.penalty_bps(x.route), Some(250));
        let favorable = Verified {
            signature: "separate verified fixture".into(),
            output: 220,
            ..r
        };
        model.observe(&favorable).unwrap();
        assert_eq!(model.penalty_bps(x.route), Some(250));
        let state = model.state();
        let mut restored = CostModel::default();
        restored.restore(state).unwrap();
        assert!(!restored.observe(&r).unwrap());
        assert_eq!(restored.penalty_bps(x.route), Some(250));
        let mut invalid = restored.state();
        invalid.routes[0].1 = 0;
        assert!(CostModel::default().restore(invalid).is_err());
    }
}
