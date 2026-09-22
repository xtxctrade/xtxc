//! Experimental, fixed-unit, in-kind basket vault. NOT deployed or admitted.
//! One share atom represents the immutable units of EACH constituent. No NAV
//! oracle, administrator withdrawal, rebalance, curve, swap or fee instruction.
//! Legacy SPL only: Token-2022 requires a separately admitted extension adapter.
use solana_program::{account_info::AccountInfo, entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction}, program::{invoke, invoke_signed},
    program_error::ProgramError, pubkey::Pubkey, rent::Rent, sysvar::Sysvar};

pub const TOKEN: Pubkey = solana_program::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const HEADER: usize = 112;
pub const LEG: usize = 72;
pub const RECEIPT: usize = 128;
pub const MAGIC: &[u8;8] = b"XTXCBV01";
const BAD: ProgramError = ProgramError::Custom(1);
const AUTH: ProgramError = ProgramError::Custom(2);
const REPLAY: ProgramError = ProgramError::Custom(3);
const INSOLVENT: ProgramError = ProgramError::Custom(4);
const DELTA: ProgramError = ProgramError::Custom(5);
#[cfg(not(feature="no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

fn u64_at(d:&[u8], p:usize)->Result<u64,ProgramError>{Ok(u64::from_le_bytes(d.get(p..p+8).ok_or(BAD)?.try_into().map_err(|_|BAD)?))}
fn key_at(d:&[u8], p:usize)->Result<Pubkey,ProgramError>{Ok(Pubkey::new_from_array(d.get(p..p+32).ok_or(BAD)?.try_into().map_err(|_|BAD)?))}
fn need(ok:bool,e:ProgramError)->ProgramResult{if ok{Ok(())}else{Err(e)}}
fn mul(a:u64,b:u64)->Result<u64,ProgramError>{a.checked_mul(b).ok_or(ProgramError::ArithmeticOverflow)}
fn zero_account(a:&AccountInfo,owner:&Pubkey,len:usize)->ProgramResult{
    need(a.owner==owner&&a.is_writable&&a.is_signer&&!a.executable,AUTH)?;
    let d=a.try_borrow_data()?;need(d.len()==len&&d.iter().all(|x|*x==0),REPLAY)?;
    need(Rent::get()?.is_exempt(a.lamports(),len),BAD)
}
fn mint(a:&AccountInfo)->Result<(u64,u8),ProgramError>{
    need(a.owner==&TOKEN&&!a.executable,BAD)?;let d=a.try_borrow_data()?;
    need(d.len()==82&&d[45]==1,BAD)?;Ok((u64_at(&d,36)?,d[44]))
}
fn share_mint(a:&AccountInfo,authority:&Pubkey,decimals:u8)->Result<u64,ProgramError>{
    let (supply,d)=mint(a)?;let data=a.try_borrow_data()?;
    need(d==decimals&&data[..4]==[1,0,0,0]&&key_at(&data,4)?==*authority&&data[46..50]==[0,0,0,0],AUTH)?;Ok(supply)
}
fn token(a:&AccountInfo,mint:&Pubkey,owner:&Pubkey)->Result<u64,ProgramError>{
    need(a.owner==&TOKEN&&a.is_writable&&!a.executable,BAD)?;
    let d=a.try_borrow_data()?;
    need(d.len()==165&&key_at(&d,0)?==*mint&&key_at(&d,32)?==*owner&&d[108]==1
        &&d[72..76]==[0,0,0,0]&&d[109..113]==[0,0,0,0]&&d[129..133]==[0,0,0,0],AUTH)?;
    u64_at(&d,64)
}
fn unique(accounts:&[AccountInfo])->ProgramResult{
    // Bounded <=55 accounts; alias checks are cheaper than confused-deputy CPI.
    for i in 0..accounts.len(){for j in 0..i{need(accounts[i].key!=accounts[j].key,BAD)?;}}Ok(())
}
pub fn process_instruction(program:&Pubkey,a:&[AccountInfo],data:&[u8])->ProgramResult{
    match data.first(){Some(0)=>initialize(program,a,data),Some(1|2)=>exchange(program,a,data),_=>Err(BAD)}
}

// INIT: config(new program-owned signer), authority PDA, creator signer,
// share mint(preinitialized under authority PDA), SPL program, then (mint,vault).
// Payload 0,n,shareDecimals,rightsPolicyHash[32],unitsPerShareAtom[n]*u64 LE.
fn initialize(program:&Pubkey,a:&[AccountInfo],data:&[u8])->ProgramResult{
    need(data.len()>=35,BAD)?;let n=data[1] as usize;
    need((2..=16).contains(&n)&&data[2]<=9&&data.len()==35+8*n&&a.len()==5+2*n,BAD)?;
    unique(a)?;zero_account(&a[0],program,HEADER+n*LEG)?;
    let (authority,bump)=Pubkey::find_program_address(&[b"xtxc-basket-v1",a[0].key.as_ref()],program);
    need(a[1].key==&authority&&a[2].is_signer&&a[4].key==&TOKEN&&a[4].executable,AUTH)?;
    need(data[3..35].iter().any(|x|*x!=0),BAD)?;
    need(share_mint(&a[3],&authority,data[2])?==0,BAD)?;
    for i in 0..n{let m=&a[5+2*i];let v=&a[6+2*i];mint(m)?;
        need(m.key!=a[3].key&&u64_at(data,35+8*i)?>0,BAD)?;
        // Empty vaults guarantee deterministic initial backing. Later donations
        // do not alter unit claims or dilute holders; surplus is not NAV equity.
        need(token(v,m.key,&authority)?==0,BAD)?;
    }
    let mut cfg=a[0].try_borrow_mut_data()?;
    cfg[..8].copy_from_slice(MAGIC);cfg[8..40].copy_from_slice(a[3].key.as_ref());
    cfg[40..72].copy_from_slice(a[2].key.as_ref());cfg[72..104].copy_from_slice(&data[3..35]);
    cfg[104]=bump;cfg[105]=n as u8;cfg[106]=data[2];cfg[107]=1;
    for i in 0..n{let p=HEADER+i*LEG;cfg[p..p+32].copy_from_slice(a[5+2*i].key.as_ref());
        cfg[p+32..p+64].copy_from_slice(a[6+2*i].key.as_ref());cfg[p+64..p+72].copy_from_slice(&data[35+8*i..43+8*i]);}
    Ok(())
}

// MINT/REDEEM: config, authority, user signer, share mint, user's shares,
// fresh receipt(program-owned signer), SPL program, then (mint,user ATA,vault).
// Payload operation(1 mint/2 redeem), shareAtoms u64 LE. Receipt key is part of
// the user's signed instruction. Retry must reuse it; a committed receipt can
// never execute twice. An unknown transaction must be reconciled, not rebuilt
// with a new receipt by the client.
fn exchange(program:&Pubkey,a:&[AccountInfo],data:&[u8])->ProgramResult{
    need(data.len()==9&&a.len()>=13,BAD)?;let shares=u64_at(data,1)?;need(shares>0,BAD)?;
    need(a[0].owner==program&&!a[0].executable,AUTH)?;
    let cfg=a[0].try_borrow_data()?;need(cfg.len()>=HEADER&&&cfg[..8]==MAGIC&&cfg[107]==1,BAD)?;
    let n=cfg[105] as usize;need((2..=16).contains(&n)&&cfg.len()==HEADER+n*LEG&&a.len()==7+3*n,BAD)?;
    unique(a)?;
    let bump=[cfg[104]];let seeds:&[&[u8]]=&[b"xtxc-basket-v1",a[0].key.as_ref(),&bump];
    let authority=Pubkey::create_program_address(seeds,program).map_err(|_|AUTH)?;
    need(a[1].key==&authority&&a[2].is_signer&&a[3].is_writable&&a[3].key==&key_at(&cfg,8)?
        &&a[6].key==&TOKEN&&a[6].executable,AUTH)?;
    zero_account(&a[5],program,RECEIPT)?;
    let supply=share_mint(&a[3],&authority,cfg[106])?;
    let owned=token(&a[4],a[3].key,a[2].key)?;
    let issue=data[0]==1;
    let target=if issue{supply.checked_add(shares)}else{supply.checked_sub(shares)}.ok_or(BAD)?;
    if !issue{need(owned>=shares,BAD)?;}else{owned.checked_add(shares).ok_or(BAD)?;}
    let mut units=[0u64;16];let mut before=[0u64;16];let mut user_before=[0u64;16];
    for i in 0..n{let p=HEADER+i*LEG;let m=&a[7+3*i];let user=&a[8+3*i];let vault=&a[9+3*i];
        need(m.key==&key_at(&cfg,p)?&&vault.key==&key_at(&cfg,p+32)?,BAD)?;mint(m)?;
        units[i]=u64_at(&cfg,p+64)?;let amount=mul(shares,units[i])?;
        before[i]=token(vault,m.key,&authority)?;user_before[i]=token(user,m.key,a[2].key)?;
        need(before[i]>=mul(supply,units[i])?,INSOLVENT)?;
        mul(target,units[i])?;
        if issue{need(user_before[i]>=amount,BAD)?;before[i].checked_add(amount).ok_or(BAD)?;}
        else{user_before[i].checked_add(amount).ok_or(BAD)?;}
    }
    for i in 0..n{let m=&a[7+3*i];let user=&a[8+3*i];let vault=&a[9+3*i];let amount=mul(shares,units[i])?;
        let (_,decimals)=mint(m)?;
        if issue{transfer(user,m,vault,&a[2],&a[6],amount,decimals,None)?;}
        else{transfer(vault,m,user,&a[1],&a[6],amount,decimals,Some(seeds))?;}
        let expected_vault=if issue{before[i]+amount}else{before[i]-amount};
        let expected_user=if issue{user_before[i]-amount}else{user_before[i]+amount};
        need(token(vault,m.key,&authority)?==expected_vault&&token(user,m.key,a[2].key)?==expected_user,DELTA)?;
    }
    let mut bytes=vec![if issue{14}else{15}];bytes.extend_from_slice(&shares.to_le_bytes());bytes.push(cfg[106]);
    let infos=if issue{vec![a[3].clone(),a[4].clone(),a[1].clone(),a[6].clone()]}else{vec![a[4].clone(),a[3].clone(),a[2].clone(),a[6].clone()]};
    let ix=Instruction{program_id:TOKEN,accounts:vec![AccountMeta::new(*infos[0].key,false),AccountMeta::new(*infos[1].key,false),AccountMeta::new_readonly(*infos[2].key,true)],data:bytes};
    if issue{invoke_signed(&ix,&infos,&[seeds])?;}else{invoke(&ix,&infos)?;}
    need(share_mint(&a[3],&authority,cfg[106])?==target&&token(&a[4],a[3].key,a[2].key)?==if issue{owned+shares}else{owned-shares},DELTA)?;
    let mut receipt=a[5].try_borrow_mut_data()?;receipt[..8].copy_from_slice(b"XTXCBR01");
    receipt[8..40].copy_from_slice(a[0].key.as_ref());receipt[40..72].copy_from_slice(a[2].key.as_ref());
    receipt[72..104].copy_from_slice(&cfg[72..104]);receipt[104]=data[0];
    receipt[112..120].copy_from_slice(&shares.to_le_bytes());receipt[120..128].copy_from_slice(&target.to_le_bytes());
    Ok(())
}
fn transfer<'a>(from:&AccountInfo<'a>,mint:&AccountInfo<'a>,to:&AccountInfo<'a>,owner:&AccountInfo<'a>,program:&AccountInfo<'a>,amount:u64,decimals:u8,seeds:Option<&[&[u8]]>)->ProgramResult{
    let mut data=vec![12];data.extend_from_slice(&amount.to_le_bytes());data.push(decimals);
    let ix=Instruction{program_id:TOKEN,accounts:vec![AccountMeta::new(*from.key,false),AccountMeta::new_readonly(*mint.key,false),AccountMeta::new(*to.key,false),AccountMeta::new_readonly(*owner.key,true)],data};
    let infos=[from.clone(),mint.clone(),to.clone(),owner.clone(),program.clone()];
    match seeds{Some(s)=>invoke_signed(&ix,&infos,&[s]),None=>invoke(&ix,&infos)}
}
