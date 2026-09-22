//! Per-sale, owner-derived native SOL output. No custodial key or old wSOL ATA
//! is used. The account is created, initialized, filled and closed in one wire.
use crate::{feed::Account, swap_wire::{AccountView, TokenAsset}, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::VersionedMessage;
use solana_pubkey::{pubkey, Pubkey};

pub const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
pub const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const SYSTEM: Pubkey = Pubkey::new_from_array([0;32]);
const MAX_RENT: u64 = 10_000_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativePayout {
    owner: String,
    seed: String,
    rent_lamports: u64,
}
impl NativePayout {
    pub fn new(owner: Pubkey, quote_id: &str, rent_lamports: u64) -> Result<Self> {
        let seed = quote_id.strip_prefix("stkq_").ok_or("native payout quote")?;
        if owner == SYSTEM || seed.len()!=32 || !seed.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b))
            || !(1..=MAX_RENT).contains(&rent_lamports) {
            return Err("native payout bounds".into());
        }
        Ok(Self {owner:owner.to_string(),seed:seed.into(),rent_lamports})
    }
    pub fn owner(&self) -> Result<Pubkey> { self.owner.parse().map_err(|_|"native payout owner".into()) }
    pub fn rent(&self) -> u64 { self.rent_lamports }
    pub fn asset(&self) -> Result<TokenAsset> {
        let owner=self.owner()?;
        // Validate deserialized expectations as strictly as fresh construction.
        Self::new(owner,&format!("stkq_{}",self.seed),self.rent_lamports)?;
        let token=Pubkey::create_with_seed(&owner,&self.seed,&TOKEN).map_err(|_|"native payout derivation")?;
        Ok(TokenAsset {token,mint:WSOL,token_program:TOKEN})
    }
    pub fn validate_absent(&self, account: AccountView<'_>) -> Result<()> {
        if account.owner!=SYSTEM || account.executable || !account.data.is_empty() {return Err("native payout account already exists".into());}
        Ok(())
    }
    pub fn validate_initialized(&self, account: AccountView<'_>) -> Result<()> {
        let d=account.data;
        if account.owner!=TOKEN || account.executable || d.len()!=165 || d[..32]!=WSOL.to_bytes()
            || d[32..64]!=self.owner()?.to_bytes() || d[64..108]!=[0;44] || d[108]!=1
            || d[109..113]!=1u32.to_le_bytes() || d[113..121]!=self.rent_lamports.to_le_bytes()
            || d[121..]!=[0;44] {return Err("native payout initialized identity or reserve".into());}
        Ok(())
    }
    pub fn setup(&self) -> Result<[Instruction;2]> {
        let owner=self.owner()?;let asset=self.asset()?;
        // SystemInstruction::CreateAccountWithSeed (bincode fixed integers).
        let mut data=3u32.to_le_bytes().to_vec();data.extend(owner.as_ref());
        data.extend((self.seed.len() as u64).to_le_bytes());data.extend(self.seed.as_bytes());
        data.extend(self.rent_lamports.to_le_bytes());data.extend(165u64.to_le_bytes());data.extend(TOKEN.as_ref());
        let create=Instruction {program_id:SYSTEM,data,accounts:vec![AccountMeta::new(owner,true),
            AccountMeta::new(asset.token,false),AccountMeta::new_readonly(owner,true)]};
        // InitializeAccount3 needs no rent sysvar account; token program reads it.
        let mut data=vec![18];data.extend(owner.as_ref());
        let initialize=Instruction {program_id:TOKEN,data,accounts:vec![AccountMeta::new(asset.token,false),AccountMeta::new_readonly(WSOL,false)]};
        Ok([create,initialize])
    }
    pub fn close(&self) -> Result<Instruction> {
        let owner=self.owner()?;
        Ok(Instruction {program_id:TOKEN,data:vec![9],accounts:vec![AccountMeta::new(self.asset()?.token,false),
            AccountMeta::new(owner,false),AccountMeta::new_readonly(owner,true)]})
    }
}

/// Persisted with the exact-wire journal entry; old entries omit this field.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeOutput {
    pub payout: NativePayout,
    pub settlement_program: String,
    pub created_accounts: Vec<String>,
}
impl NativeOutput {
    pub fn validate_wire(&self, message:&VersionedMessage, keys:&[String]) -> Result<()> {
        let owner=self.payout.owner()?;let temp=self.payout.asset()?.token.to_string();
        let program:Pubkey=self.settlement_program.parse().map_err(|_|"native payout settlement")?;
        if message.header().num_required_signatures!=1 || keys.first()!=Some(&owner.to_string())
            || program==SYSTEM || self.created_accounts.len()>4
            || self.created_accounts.iter().collect::<std::collections::BTreeSet<_>>().len()!=self.created_accounts.len()
            || self.created_accounts.iter().any(|k|k==&temp||k==&owner.to_string()) {
            return Err("native payout wire identity".into());
        }
        let ix=message.instructions();
        if ix.len()<4 {return Err("native payout wire length".into());}
        let matches=|actual:&solana_message::compiled_instruction::CompiledInstruction,expected:&Instruction| {
            actual.data==expected.data && keys.get(usize::from(actual.program_id_index))==Some(&expected.program_id.to_string())
                && actual.accounts.len()==expected.accounts.len()
                && actual.accounts.iter().zip(&expected.accounts).all(|(i,a)| {
                    let i=usize::from(*i);
                    keys.get(i)==Some(&a.pubkey.to_string())
                        && (!a.is_signer || i<usize::from(message.header().num_required_signatures))
                        && (!a.is_writable || message.is_maybe_writable(i,None))
                })
        };
        let setup=self.payout.setup()?;
        let starts=(0..ix.len()-2).filter(|i|matches(&ix[*i],&setup[0])&&matches(&ix[*i+1],&setup[1])).collect::<Vec<_>>();
        if starts.len()!=1 || starts[0]+2>ix.len()-2 || !matches(ix.last().ok_or("native payout close")?,&self.payout.close()?)
            || ix[ix.len()-2].data.first()!=Some(&20)
            || keys.get(usize::from(ix[ix.len()-2].program_id_index))!=Some(&self.settlement_program) {
            return Err("native payout setup settlement close sequence".into());
        }
        // No other top-level action may borrow the temporary account.
        for (i,instruction) in ix.iter().enumerate() {
            if [starts[0],starts[0]+1,ix.len()-2,ix.len()-1].contains(&i) {continue;}
            if instruction.accounts.iter().any(|k|keys.get(usize::from(*k))==Some(&temp)) {
                return Err("native payout extra account use".into());
            }
        }
        Ok(())
    }
    pub fn received(&self, before_owner:u64, after_owner:u64, fee:u64, rent:u64) -> Result<u64> {
        let value=i128::from(after_owner)-i128::from(before_owner)+i128::from(fee)+i128::from(rent);
        u64::try_from(value).map_err(|_|"native payout wallet delta".into())
    }
    pub fn verify_receipt(&self,message:&VersionedMessage,keys:&[String],meta:&Value) -> Result<u64> {
        self.validate_wire(message,keys)?;
        let position=|key:&str|keys.iter().position(|v|v==key).ok_or("native payout account absent");
        let balances=|name:&str|meta[name].as_array().filter(|v|v.len()==keys.len()).ok_or("native payout balances incomplete");
        let pre=balances("preBalances")?;let post=balances("postBalances")?;
        let amount=|values:&[Value],index:usize|values[index].as_u64().ok_or("native payout integer balance");
        let temp=position(&self.payout.asset()?.token.to_string())?;
        if amount(pre,temp)?!=0 || amount(post,temp)?!=0 {return Err("native payout not created and closed".into());}
        let mut rent=0u64;
        for key in &self.created_accounts {
            let index=position(key)?;let paid=amount(post,index)?;
            if amount(pre,index)?!=0 || paid==0 || paid>MAX_RENT {return Err("native payout persistent rent".into());}
            rent=rent.checked_add(paid).ok_or("native payout rent overflow")?;
        }
        let owner=position(&self.payout.owner)?;
        self.received(amount(pre,owner)?,amount(post,owner)?,meta["fee"].as_u64().ok_or("native payout fee")?,rent)
    }
    pub fn verify_simulation(&self, original:&crate::feed::Snapshot, returned:&[Account], fee:u64) -> Result<u64> {
        let get=|rows:&[Account],key:&str|rows.iter().find(|a|a.key==key).cloned().ok_or("native payout observed account");
        let before=get(&original.accounts,&self.payout.owner)?;let after=get(returned,&self.payout.owner)?;
        if before.owner!=SYSTEM.to_string() || after.owner!=before.owner || !before.data.is_empty()
            || !after.data.is_empty() || before.executable || after.executable {return Err("native payout wallet identity".into());}
        let temp=self.payout.asset()?.token.to_string();
        for a in [get(&original.accounts,&temp)?,get(returned,&temp)?] {
            if a.owner!=SYSTEM.to_string() || a.lamports!=0 || a.executable || !a.data.is_empty() {return Err("native payout account not empty".into());}
        }
        let mut rent=0u64;
        for key in &self.created_accounts {
            let a=get(&original.accounts,key)?;let b=get(returned,key)?;
            if a.lamports!=0 || !a.data.is_empty() || a.owner!=SYSTEM.to_string() || b.lamports==0 || b.lamports>MAX_RENT {
                return Err("native payout setup rent".into());
            }
            rent=rent.checked_add(b.lamports).ok_or("native payout rent overflow")?;
        }
        // Agave deducts the simulated fee from the payer's returned balance.
        // Use the simulation's reported fee, never a hard-coded signature fee.
        self.received(before.lamports,after.lamports,fee,rent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn payout_is_per_owner_per_quote_and_never_an_associated_account() {
        let owner=Pubkey::new_from_array([42;32]);
        let quote=format!("stkq_{}","a".repeat(32));
        let p=NativePayout::new(owner,&quote,2_039_280).unwrap();
        let ata=Pubkey::find_program_address(&[owner.as_ref(),TOKEN.as_ref(),WSOL.as_ref()],&pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")).0;
        assert_ne!(p.asset().unwrap().token,ata);
        assert_ne!(p.asset().unwrap().token,NativePayout::new(owner,&format!("stkq_{}","b".repeat(32)),2_039_280).unwrap().asset().unwrap().token);
        assert_ne!(p.asset().unwrap().token,NativePayout::new(Pubkey::new_from_array([43;32]),&quote,2_039_280).unwrap().asset().unwrap().token);
        for quote in ["","stkq_","stkq_../../secret"] {assert!(NativePayout::new(owner,quote,2_039_280).is_err());}
        for rent in [0,MAX_RENT+1,u64::MAX] {assert!(NativePayout::new(owner,&quote,rent).is_err());}
    }
    #[test]
    fn temporary_account_requires_exact_native_state_and_no_existing_authority() {
        let p=NativePayout::new(Pubkey::new_from_array([42;32]),&format!("stkq_{}","a".repeat(32)),2_039_280).unwrap();
        let mut d=vec![0u8;165];d[..32].copy_from_slice(WSOL.as_ref());d[32..64].copy_from_slice(p.owner().unwrap().as_ref());
        d[108]=1;d[109..113].copy_from_slice(&1u32.to_le_bytes());d[113..121].copy_from_slice(&p.rent().to_le_bytes());
        fn view(data:&[u8])->AccountView<'_>{AccountView{owner:TOKEN,data,executable:false}}
        p.validate_initialized(view(&d)).unwrap();
        for byte in [0,32,64,72,108,109,113,121,129,164] {
            let mut bad=d.clone();bad[byte]^=1;assert!(p.validate_initialized(view(&bad)).is_err(),"{byte}");
        }
        assert!(p.validate_absent(view(&d)).is_err());
        assert!(p.validate_initialized(AccountView{owner:TOKEN,data:&d,executable:true}).is_err());
    }
    #[test]
    fn native_delta_separates_refundable_reserve_fees_and_persistent_rent() {
        let n=NativeOutput{payout:NativePayout::new(Pubkey::new_from_array([42;32]),&format!("stkq_{}","a".repeat(32)),2_039_280).unwrap(),settlement_program:Pubkey::new_from_array([43;32]).to_string(),created_accounts:vec![]};
        assert_eq!(n.received(10_000_000,9_000_000,5_000,2_000_000).unwrap(),1_005_000);
        assert_eq!(n.received(10_000_000,10_000_000,5_000,0).unwrap(),5_000);
        assert!(n.received(10_000_000,0,0,0).is_err());
        assert!(n.received(0,u64::MAX,u64::MAX,u64::MAX).is_err());
        for fee in [serde_json::Value::Null,json_value("5000"),serde_json::json!(-1),serde_json::json!(10_000_001)] {
            assert!(crate::exposure_pipeline::simulation_fee(&serde_json::json!({"value":{"fee":fee}})).is_err());
        }
        for fee in [0,5000,10_000_000] {assert_eq!(crate::exposure_pipeline::simulation_fee(&serde_json::json!({"value":{"fee":fee}})).unwrap(),fee);}
    }
    fn json_value(s:&str)->serde_json::Value {serde_json::Value::String(s.into())}
}
