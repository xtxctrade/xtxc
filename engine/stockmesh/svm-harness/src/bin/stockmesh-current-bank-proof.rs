//! Exact SBF execution of all ten StockMesh lanes from one captured full bank.
//!
//! State is a mainnet account capture. Wallet balances, owner and nonce are
//! synthetic; the harness has no RPC, keypair, signature or submission path.

#[allow(dead_code)]
#[path = "../matrix.rs"]
mod matrix;
use base64::{engine::general_purpose::STANDARD, Engine};
use matrix::*;
use mollusk_svm::Mollusk;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use skew_execution_host::{
    feed::{Account as HostAccount, Snapshot},
    market::{MarketConfig, USDC_MINT, WSOL_MINT},
    native_wire,
    swap_wire::{self, AccountView, SwapGraph},
    world::{NativeSwapProposal, WorldConfig},
};
use solana_account::Account;
use solana_pubkey::Pubkey;
use std::{collections::BTreeSet, fs, path::Path, path::PathBuf, time::Instant};
use stocklana_adapters as dex;

const BANK: &str = "artifacts/stockmesh-full-execution-banks-v38-20260915";
const ALLOCATION: &str = "artifacts/stockmesh-v38-full-bank/frozen-allocation.json";
const OUTPUT: &str = "artifacts/stockmesh-v38-full-bank/current-bank-sbf.json";

fn sha256(path: &Path) -> String {
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}

fn account_from_rpc(_key: &str, value: &Value) -> Account {
    assert!(!value.is_null() && value["data"][1] == "base64");
    Account {
        lamports: value["lamports"].as_u64().unwrap(),
        data: STANDARD.decode(value["data"][0].as_str().unwrap()).unwrap(),
        owner: pk(value["owner"].as_str().unwrap()),
        executable: value["executable"].as_bool().unwrap(),
        rent_epoch: 0,
    }
}

fn host_snapshot(accounts: &Accounts, slot: u64) -> Snapshot {
    Snapshot {
        slot,
        generation: 1,
        hash: [7; 32],
        revision: 1,
        observed: Instant::now(),
        slot_advanced: Instant::now(),
        accounts: accounts
            .iter()
            .map(|(key, account)| HostAccount {
                key: key.to_string(),
                owner: account.owner.to_string(),
                executable: account.executable,
                lamports: account.lamports,
                data: account.data.clone(),
            })
            .collect(),
    }
}

fn load_bank(root: &Path, bank_path: &str, manifest_bank: &Value) -> (Accounts, u64, u64, u64) {
    let bank_root = root.join(bank_path);
    let worlds = manifest_bank["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|lane| lane["worlds"].as_array().unwrap())
        .map(|world| world.as_str().unwrap())
        .collect::<BTreeSet<_>>();
    let keys = worlds
        .iter()
        .flat_map(|world| {
            let config: WorldConfig =
                serde_json::from_slice(&fs::read(bank_root.join(world)).unwrap()).unwrap();
            config.keys().unwrap()
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let snapshot_name = manifest_bank["snapshot"].as_str().unwrap();
    let response = read(&bank_root.join(snapshot_name));
    let values = response["value"].as_array().unwrap();
    assert_eq!(keys.len(), values.len());
    let mut accounts = keys
        .iter()
        .zip(values)
        .map(|(key, value)| (pk(key), account_from_rpc(key, value)))
        .collect::<Accounts>();
    let clock = get(
        &accounts,
        &pk("SysvarC1ock11111111111111111111111111111111"),
    );
    let slot = dex::u64_at(&clock.data, 0).unwrap();
    let epoch = dex::u64_at(&clock.data, 16).unwrap();
    let unix_time = dex::u64_at(&clock.data, 32).unwrap();
    assert_eq!(slot, response["context"]["slot"].as_u64().unwrap());
    put(
        &mut accounts,
        WALLET,
        Account {
            lamports: START + 1_000_000_000_000,
            ..Account::default()
        },
    );
    put(
        &mut accounts,
        SETTLE,
        mollusk_svm::program::create_program_account_loader_v3(&SETTLE),
    );
    let nonce = Pubkey::find_program_address(&[b"stocklana", WALLET.as_ref()], &SETTLE).0;
    let mut nonce_data = vec![0u8; 64];
    nonce_data[..8].copy_from_slice(b"SKEWSEQ1");
    nonce_data[8..40].copy_from_slice(WALLET.as_ref());
    put(
        &mut accounts,
        nonce,
        Account {
            lamports: 10_000_000,
            data: nonce_data,
            owner: SETTLE,
            executable: false,
            rent_epoch: 0,
        },
    );
    (accounts, slot, unix_time, epoch)
}

fn proposal(value: &Value) -> NativeSwapProposal {
    NativeSwapProposal {
        market: serde_json::from_value::<MarketConfig>(value["market"].clone()).unwrap(),
        stage: value["stage"].as_u64().unwrap() as usize,
        product_id: value
            .get("productId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        input_atoms: value["inputAtoms"].as_str().unwrap().parse().unwrap(),
        expected_output_atoms: value["expectedOutputAtoms"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
    }
}

fn add_wallet_assets(accounts: &mut Accounts, proposals: &[NativeSwapProposal]) {
    let mints = proposals
        .iter()
        .flat_map(|proposal| [&proposal.market.input_mint, &proposal.market.output_mint])
        .map(|mint| pk(mint))
        .collect::<BTreeSet<_>>();
    for mint in mints {
        let owner = get(accounts, &mint).owner;
        let address = Pubkey::find_program_address(
            &[WALLET.as_ref(), owner.as_ref(), mint.as_ref()],
            &pk("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
        )
        .0;
        let mint_data = get(accounts, &mint).data.clone();
        let wallet_token = token(mint, owner, &mint_data);
        put(accounts, address, wallet_token);
    }
}

fn read_account<'a>(accounts: &'a Accounts, key: &Pubkey) -> Result<AccountView<'a>, String> {
    let account = accounts
        .iter()
        .find(|(candidate, _)| candidate == key)
        .map(|(_, account)| account)
        .ok_or_else(|| format!("execution bank missing {key}"))?;
    Ok(AccountView {
        owner: account.owner,
        executable: account.executable,
        data: &account.data,
    })
}

fn execute_lane(
    runtime: &mut Mollusk,
    root: &Path,
    bank_path: &str,
    manifest: &Value,
    lane: &Value,
) -> Result<Value, String> {
    let instrument = lane["instrument"].as_str().ok_or("lane instrument")?;
    let input_symbol = lane["inputSymbol"].as_str().ok_or("lane input")?;
    let bank_manifest = manifest["banks"]
        .as_array()
        .ok_or("manifest banks")?
        .iter()
        .find(|bank| bank["name"] == instrument)
        .ok_or("instrument bank")?;
    let (mut accounts, slot, unix_time, epoch) = load_bank(root, bank_path, bank_manifest);
    runtime.sysvars.clock.slot = slot;
    runtime.sysvars.clock.unix_timestamp = unix_time as i64;
    runtime.sysvars.clock.epoch = epoch;
    let proposals = lane["allocation"]["legs"]
        .as_array()
        .ok_or("allocation legs")?
        .iter()
        .map(proposal)
        .collect::<Vec<_>>();
    if proposals.is_empty() || proposals.len() > 4 {
        return Err("allocation leg bound".into());
    }
    add_wallet_assets(&mut accounts, &proposals);
    let bank = host_snapshot(&accounts, slot);
    let input_mint = pk(match input_symbol {
        "USDC" => USDC_MINT,
        "SOL" => WSOL_MINT,
        _ => return Err("input symbol".into()),
    });
    let product_mints = proposals
        .iter()
        .filter(|proposal| proposal.product_id.is_some())
        .map(|proposal| pk(&proposal.market.output_mint))
        .collect::<BTreeSet<_>>();
    if product_mints.len() != 1 {
        return Err("one exact terminal product required".into());
    }
    let output_mint = *product_mints.iter().next().unwrap();
    let mut intermediate_mints = proposals
        .iter()
        .flat_map(|proposal| {
            [
                pk(&proposal.market.input_mint),
                pk(&proposal.market.output_mint),
            ]
        })
        .filter(|mint| *mint != input_mint && *mint != output_mint)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if intermediate_mints.len() > 1 {
        return Err("unexpected deep captured path".into());
    }
    let input = native_wire::wallet_asset(WALLET, input_mint, &bank)?;
    let output = native_wire::wallet_asset(WALLET, output_mint, &bank)?;
    let intermediates = intermediate_mints
        .drain(..)
        .map(|mint| native_wire::wallet_asset(WALLET, mint, &bank))
        .collect::<Result<Vec<_>, _>>()?;
    let input_atoms = proposals
        .iter()
        .filter(|proposal| pk(&proposal.market.input_mint) == input_mint)
        .try_fold(0u64, |total, proposal| {
            total
                .checked_add(proposal.input_atoms)
                .ok_or("input overflow")
        })?;
    let output_atoms = proposals
        .iter()
        .filter(|proposal| pk(&proposal.market.output_mint) == output_mint)
        .try_fold(0u64, |total, proposal| {
            total
                .checked_add(proposal.expected_output_atoms)
                .ok_or("output overflow")
        })?;
    let header = SwapGraph {
        owner: WALLET,
        sequence: 0,
        input_atoms,
        minimum_output_atoms: output_atoms,
        deadline_slot: slot + 1000,
        input,
        output,
        intermediates,
        legs: vec![],
    };
    let graph = native_wire::lower_native_graph(header, &proposals, &bank)?;
    let instruction =
        swap_wire::compile_swap_graph(SETTLE, &graph, |key| read_account(&accounts, key))?;
    let before_input = amount(&accounts, &input.token);
    let before_output = amount(&accounts, &output.token);
    let execution = run(runtime, &instruction, &accounts);
    if execution.program_result.is_err() {
        return Err(format!(
            "exact SBF {:?} CU {}",
            execution.program_result, execution.compute_units_consumed
        ));
    }
    if before_input - amount(&execution.resulting_accounts, &input.token) != input_atoms
        || amount(&execution.resulting_accounts, &output.token) - before_output != output_atoms
        || graph.intermediates.iter().any(|asset| {
            amount(&accounts, &asset.token) != amount(&execution.resulting_accounts, &asset.token)
        })
    {
        return Err("exact SBF balance conservation".into());
    }
    let mut floor = instruction.clone();
    floor.data[20..28].copy_from_slice(
        &output_atoms
            .checked_add(1)
            .ok_or("floor overflow")?
            .to_le_bytes(),
    );
    let rejected = run(runtime, &floor, &accounts);
    if rejected.program_result.is_ok() || rejected.resulting_accounts != accounts {
        return Err("minimum exposure leg rollback".into());
    }
    let replay = run(runtime, &instruction, &execution.resulting_accounts);
    if replay.program_result.is_ok() || replay.resulting_accounts != execution.resulting_accounts {
        return Err("nonce replay rollback".into());
    }
    Ok(json!({
        "instrument":instrument,
        "inputSymbol":input_symbol,
        "slot":slot,
        "inputAtoms":input_atoms.to_string(),
        "outputAtoms":output_atoms.to_string(),
        "legs":proposals.len(),
        "venues":proposals.iter().map(|proposal|format!("{:?}",proposal.market.venue)).collect::<Vec<_>>(),
        "instructionAccounts":instruction.accounts.len(),
        "instructionDataBytes":instruction.data.len(),
        "computeUnits":execution.compute_units_consumed,
        "intermediatePrebalancePreserved":true,
        "minimumOutputRollback":true,
        "nonceReplayRollback":true,
        "passed":true
    }))
}

fn main() {
    let root = PathBuf::from("/srv/skew/stocklana-engine-20260912");
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let (bank_path, allocation_path, output_path) = match arguments.as_slice() {
        [] => (BANK, ALLOCATION, OUTPUT),
        [bank, allocation, output] => (bank.as_str(), allocation.as_str(), output.as_str()),
        _ => panic!("usage: stockmesh-current-bank-proof [BANK ALLOCATION OUTPUT]"),
    };
    let bank_root = root.join(bank_path);
    let manifest = read(&bank_root.join("fixture-manifest.json"));
    let allocation = read(&root.join(allocation_path));
    let settle_dir = root.join("artifacts/sbf-stockmesh-v12-direct-path-reflow-20260915");
    std::env::set_var("SKEW_SETTLE_SBF_DIR", &settle_dir);
    let mut runtime = runtime(&root);
    runtime.compute_budget.compute_unit_limit = 1_400_000;
    let mut rows = Vec::new();
    for lane in allocation["quoteLanes"].as_array().unwrap() {
        let result = execute_lane(&mut runtime, &root, bank_path, &manifest, lane);
        let row = result.unwrap_or_else(|error| {
            json!({"instrument":lane["instrument"],"inputSymbol":lane["inputSymbol"],"passed":false,"error":error})
        });
        println!("{row}");
        rows.push(row);
    }
    let passed = rows.iter().filter(|row| row["passed"] == true).count();
    let max_cu = rows
        .iter()
        .filter_map(|row| row["computeUnits"].as_u64())
        .max()
        .unwrap_or(0);
    let report = json!({
        "schema":"skew.stockmesh.current-full-bank-sbf/v1",
        "environment":"AWS Mollusk; captured mainnet accounts/program ELFs; synthetic wallet and nonce; no RPC/signature/submission",
        "fixtureManifestSha256":sha256(&bank_root.join("fixture-manifest.json")),
        "allocationProofSha256":sha256(&root.join(allocation_path)),
        "settlementElfSha256":sha256(&settle_dir.join("stocklana_settle.so")),
        "laneCount":rows.len(),
        "passedCount":passed,
        "maximumComputeUnits":max_cu,
        "computeCeiling":1_400_000,
        "rows":rows,
        "signedTransactions":0,
        "submittedTransactions":0,
        "fundsMoved":false,
        "evidenceBoundary":"Exact captured-bank SBF execution with synthetic wallet balances; not a current live-bank simulation or mainnet fill."
    });
    let output = root.join(output_path);
    fs::write(&output, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    assert_eq!(passed, 10, "every economic input lane must execute exactly");
    assert!(
        max_cu <= 1_400_000,
        "captured execution exceeded Solana CU ceiling"
    );
}
