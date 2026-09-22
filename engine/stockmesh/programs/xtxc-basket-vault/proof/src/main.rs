//! Local VM + archived SPL ELF. No RPC, signatures, network or customer funds.
use mollusk_svm::{Mollusk,program::create_program_account_loader_v3};
use solana_account::Account;
use solana_instruction::{Instruction,AccountMeta};
use solana_pubkey::{Pubkey,pubkey};
use serde_json::{json,Value};
use sha2::{Sha256,Digest};
const P:Pubkey=Pubkey::new_from_array([193;32]);
const T:Pubkey=pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const CFG:Pubkey=Pubkey::new_from_array([194;32]);
const U:Pubkey=Pubkey::new_from_array([195;32]);
const SHARE:Pubkey=Pubkey::new_from_array([196;32]);
const UT:Pubkey=Pubkey::new_from_array([197;32]);
const R:Pubkey=Pubkey::new_from_array([198;32]);
type Accounts=Vec<(Pubkey,Account)>;
fn k(n:u8)->Pubkey{Pubkey::new_from_array([n;32])}
fn a(owner:Pubkey,data:Vec<u8>)->Account{Account{lamports:100_000_000,data,owner,executable:false,rent_epoch:0}}
fn mint(authority:Pubkey,dec:u8,supply:u64)->Account{let mut d=vec![0;82];d[..4].copy_from_slice(&1u32.to_le_bytes());d[4..36].copy_from_slice(authority.as_ref());d[36..44].copy_from_slice(&supply.to_le_bytes());d[44]=dec;d[45]=1;a(T,d)}
fn tok(mint:Pubkey,owner:Pubkey,amount:u64)->Account{let mut d=vec![0;165];d[..32].copy_from_slice(mint.as_ref());d[32..64].copy_from_slice(owner.as_ref());d[64..72].copy_from_slice(&amount.to_le_bytes());d[108]=1;a(T,d)}
fn get<'a>(a:&'a Accounts,k:Pubkey)->&'a Account{&a.iter().find(|(p,_)|*p==k).unwrap().1}
fn get_mut<'a>(a:&'a mut Accounts,k:Pubkey)->&'a mut Account{&mut a.iter_mut().find(|(p,_)|*p==k).unwrap().1}
fn u64_at(a:&Accounts,k:Pubkey,p:usize)->u64{u64::from_le_bytes(get(a,k).data[p..p+8].try_into().unwrap())}
fn base(n:u8)->(Accounts,Instruction,Pubkey){
 let (auth,_)=Pubkey::find_program_address(&[b"xtxc-basket-v1",CFG.as_ref()],&P);
 let mut accounts=vec![(CFG,a(P,vec![0;112+n as usize*72])),(U,a(Pubkey::default(),vec![])),(auth,a(Pubkey::default(),vec![])),
  (SHARE,mint(auth,6,0)),(UT,tok(SHARE,U,0)),(R,a(P,vec![0;128])),(P,create_program_account_loader_v3(&P)),(T,create_program_account_loader_v3(&T))];
 let mut metas=vec![AccountMeta::new(CFG,true),AccountMeta::new_readonly(auth,false),AccountMeta::new_readonly(U,true),AccountMeta::new_readonly(SHARE,false),AccountMeta::new_readonly(T,false)];
 let mut data=vec![0,n,6];data.extend_from_slice(&[53;32]);
 for i in 0..n{let m=k(10+i*3);let v=k(11+i*3);let ut=k(12+i*3);accounts.extend([(m,mint(U,6,1_000_000)),(v,tok(m,auth,0)),(ut,tok(m,U,10_000))]);metas.extend([AccountMeta::new_readonly(m,false),AccountMeta::new(v,false)]);data.extend_from_slice(&(100u64+i as u64).to_le_bytes());}
 (accounts,Instruction{program_id:P,accounts:metas,data},auth)
}
fn exchange(n:u8,auth:Pubkey,op:u8,shares:u64,receipt:Pubkey)->Instruction{
 let mut metas=vec![AccountMeta::new_readonly(CFG,false),AccountMeta::new_readonly(auth,false),AccountMeta::new_readonly(U,true),AccountMeta::new(SHARE,false),AccountMeta::new(UT,false),AccountMeta::new(receipt,true),AccountMeta::new_readonly(T,false)];
 for i in 0..n{metas.extend([AccountMeta::new_readonly(k(10+i*3),false),AccountMeta::new(k(12+i*3),false),AccountMeta::new(k(11+i*3),false)]);}
 let mut data=vec![op];data.extend_from_slice(&shares.to_le_bytes());Instruction{program_id:P,accounts:metas,data}
}
fn run(m:&Mollusk,label:&str,ix:&Instruction,before:&Accounts,success:bool,rows:&mut Vec<Value>)->Accounts{
 let r=m.process_transaction_instructions(&[ix.clone()],before);
 assert_eq!(r.program_result.is_ok(),success,"{label}: {:?}",r.program_result);
 if !success{assert_eq!(&r.resulting_accounts,before,"{label}: failed transaction changed accounts");}
 rows.push(json!({"case":label,"success":success,"computeUnits":r.compute_units_consumed,"result":format!("{:?}",r.program_result),"fullRollback":!success}));r.resulting_accounts
}
fn main(){
 let root=std::env::var("XTXC_PROOF_DIR").expect("isolated evidence directory");
 assert!(root.starts_with("/srv/skew/stockmesh-direct-node-20260920/evidence/launch-runtime-"));
 std::env::set_var("SBF_OUT_DIR",format!("{root}/sbf"));let mut m=Mollusk::new(&P,"xtxc_basket_vault");m.add_program(&T,"spl_token");m.compute_budget.compute_unit_limit=400_000;
 let mut rows=Vec::new();let (b,init,auth)=base(3);let initialized=run(&m,"initialize-three",&init,&b,true,&mut rows);
 run(&m,"no-reinitialize",&init,&initialized,false,&mut rows);
 let issue=exchange(3,auth,1,10,R);let minted=run(&m,"deposit-mint",&issue,&initialized,true,&mut rows);
 assert_eq!(u64_at(&minted,SHARE,36),10);assert_eq!(u64_at(&minted,UT,64),10);
 for i in 0..3{assert_eq!(u64_at(&minted,k(11+i*3),64),10*(100+i as u64));assert_eq!(u64_at(&minted,k(12+i*3),64),10_000-10*(100+i as u64));}
 run(&m,"receipt-replay",&issue,&minted,false,&mut rows);
 let mut close=minted.clone();close.push((k(199),a(P,vec![0;128])));let redeem=exchange(3,auth,2,4,k(199));let reduced=run(&m,"redeem-partial",&redeem,&close,true,&mut rows);assert_eq!(u64_at(&reduced,SHARE,36),6);
 let mut end=reduced.clone();end.push((k(200),a(P,vec![0;128])));let end=run(&m,"redeem-all",&exchange(3,auth,2,6,k(200)),&end,true,&mut rows);assert_eq!(u64_at(&end,SHARE,36),0);
 for i in 0..3{assert_eq!(u64_at(&end,k(11+i*3),64),0);assert_eq!(u64_at(&end,k(12+i*3),64),10_000);}
 let mut insufficient=initialized.clone();get_mut(&mut insufficient,k(18)).data[64..72].copy_from_slice(&0u64.to_le_bytes());run(&m,"insufficient-last-asset",&issue,&insufficient,false,&mut rows);
 let mut wrong=initialized.clone();get_mut(&mut wrong,UT).data[32..64].copy_from_slice(k(220).as_ref());run(&m,"foreign-share-recipient",&issue,&wrong,false,&mut rows);
 let mut frozen=initialized.clone();get_mut(&mut frozen,k(12)).data[108]=2;run(&m,"frozen-constituent",&issue,&frozen,false,&mut rows);
 let mut extension=initialized.clone();get_mut(&mut extension,k(10)).owner=pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");run(&m,"unadmitted-token2022",&issue,&extension,false,&mut rows);
 let mut insolvent=close.clone();get_mut(&mut insolvent,k(11)).data[64..72].copy_from_slice(&1u64.to_le_bytes());run(&m,"insolvent-constituent",&redeem,&insolvent,false,&mut rows);
 let mut unauthorized=issue.clone();unauthorized.accounts[2].is_signer=false;run(&m,"missing-user-signature",&unauthorized,&initialized,false,&mut rows);
 let mut duplicate=issue.clone();duplicate.accounts[10].pubkey=duplicate.accounts[7].pubkey;run(&m,"aliased-constituent",&duplicate,&initialized,false,&mut rows);
 run(&m,"zero-share-mint",&exchange(3,auth,1,0,R),&initialized,false,&mut rows);
 run(&m,"overflow-quantity",&exchange(3,auth,1,u64::MAX,R),&initialized,false,&mut rows);
 let mut donation=initialized.clone();get_mut(&mut donation,k(11)).data[64..72].copy_from_slice(&7u64.to_le_bytes());let after=run(&m,"donation-not-share-inflation",&issue,&donation,true,&mut rows);assert_eq!(u64_at(&after,k(11),64),1007);assert_eq!(u64_at(&after,SHARE,36),10);
 let mut foreign_receipt=initialized.clone();get_mut(&mut foreign_receipt,R).owner=U;run(&m,"foreign-receipt-owner",&issue,&foreign_receipt,false,&mut rows);
 let mut unsigned_receipt=issue.clone();unsigned_receipt.accounts[5].is_signer=false;run(&m,"unsigned-receipt",&unsigned_receipt,&initialized,false,&mut rows);
 let mut delegated=initialized.clone();get_mut(&mut delegated,k(11)).data[72..76].copy_from_slice(&1u32.to_le_bytes());run(&m,"delegated-vault",&issue,&delegated,false,&mut rows);
 let mut share_freeze=initialized.clone();get_mut(&mut share_freeze,SHARE).data[46..50].copy_from_slice(&1u32.to_le_bytes());run(&m,"share-freeze-authority",&issue,&share_freeze,false,&mut rows);
 let mut wrong_vault=initialized.clone();get_mut(&mut wrong_vault,k(11)).data[32..64].copy_from_slice(U.as_ref());run(&m,"foreign-vault-owner",&issue,&wrong_vault,false,&mut rows);
 let mut imm=initialized.clone();get_mut(&mut imm,CFG).owner=U;run(&m,"foreign-config-owner",&issue,&imm,false,&mut rows);
 m.compute_budget.compute_unit_limit=14000;run(&m,"mid-cpi-cutoff-rollback",&issue,&initialized,false,&mut rows);m.compute_budget.compute_unit_limit=400_000;
 m.compute_budget.compute_unit_limit=10000;run(&m,"compute-cutoff-rollback",&issue,&initialized,false,&mut rows);m.compute_budget.compute_unit_limit=400_000;
 for n in [2,8,16]{let (a,ix,auth)=base(n);let a=run(&m,&format!("initialize-{n}"),&ix,&a,true,&mut rows);let mut a=run(&m,&format!("deposit-{n}"),&exchange(n,auth,1,3,R),&a,true,&mut rows);assert_eq!(u64_at(&a,SHARE,36),3);a.push((k(199),self::a(P,vec![0;128])));let a=run(&m,&format!("redeem-{n}"),&exchange(n,auth,2,3,k(199)),&a,true,&mut rows);assert_eq!(u64_at(&a,SHARE,36),0);for i in 0..n{assert_eq!(u64_at(&a,k(11+i*3),64),0);assert_eq!(u64_at(&a,k(12+i*3),64),10000);}}
 let hashes=["xtxc_basket_vault","spl_token"].map(|name|json!({"program":name,"sha256":format!("{:x}",Sha256::digest(std::fs::read(format!("{root}/sbf/{name}.so")).unwrap()))}));
 let report=json!({"schema":"xtxc.basket-sbf-proof/v1","scope":"new experimental SBF plus archived SPL token ELF; all mint/account/balance inputs are fixtures; no mainnet sends","mainnetDeployed":false,"programs":hashes,"cases":rows});
 std::fs::write(format!("{root}/proof.json"),serde_json::to_vec_pretty(&report).unwrap()).unwrap();println!("{} cases passed",report["cases"].as_array().unwrap().len());
}
