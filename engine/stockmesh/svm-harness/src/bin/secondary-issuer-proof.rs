//! Direct secondary-market proof with official mint identities and a fresh,
//! coherent public RPC bank. Wallet balances/nonce are explicit local fixtures.
//! No issuer-policy admission, signing, network simulation, or submission.
#[allow(dead_code)]
#[path = "../matrix.rs"]
mod matrix;
use base64::{engine::general_purpose::STANDARD, Engine};
use matrix::{amount, get, put, token, Accounts, SETTLE, WALLET};
use mollusk_svm::Mollusk;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use skew_execution_host::{
    feed::{Account as HostAccount, Snapshot},
    market::{MarketConfig, Venue},
    native_wire,
    rpc::Rpc,
    swap_wire::{self, AccountView, Budget, SwapGraph},
    world::NativeSwapProposal,
};
use skew_native::{dlmm::DlmmCurve, ScaledUiAmount, TransferFee};
use solana_account::Account;
use solana_pubkey::Pubkey;
use std::{
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

const PROGRAM: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
const CLOCK: &str = "SysvarC1ock11111111111111111111111111111111";
const MAINNET: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
fn pk(value: &str) -> Pubkey {
    value
        .parse()
        .unwrap_or_else(|error| panic!("invalid public key {value}: {error}"))
}
fn snapshot(a: &Accounts, slot: u64) -> Snapshot {
    Snapshot {
        slot,
        generation: 1,
        revision: 1,
        hash: [0; 32],
        observed: Instant::now(),
        slot_advanced: Instant::now(),
        accounts: a
            .iter()
            .map(|(k, a)| HostAccount {
                key: k.to_string(),
                owner: a.owner.to_string(),
                executable: a.executable,
                lamports: a.lamports,
                data: a.data.clone(),
            })
            .collect(),
    }
}
fn decoded(keys: &[String], bank: &Value, event: Pubkey) -> Accounts {
    assert_eq!(keys.len(), bank["value"].as_array().unwrap().len());
    keys.iter()
        .zip(bank["value"].as_array().unwrap())
        .map(|(key, row)| {
            let key = pk(key);
            let account = if row.is_null() {
                assert_eq!(key, event, "only canonical event authority may be absent");
                Account::default()
            } else {
                Account {
                    lamports: row["lamports"].as_u64().unwrap(),
                    owner: pk(row["owner"].as_str().unwrap()),
                    executable: row["executable"].as_bool().unwrap(),
                    data: STANDARD.decode(row["data"][0].as_str().unwrap()).unwrap(),
                    rent_epoch: 0,
                }
            };
            (key, account)
        })
        .collect()
}
fn evidence_dir(root: &Path) -> PathBuf {
    let out = std::env::var_os("SKEW_ISSUER_DISCOVERY_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("artifacts/issuer-discovery-v10"));
    let out = out.canonicalize().expect("issuer evidence directory");
    assert!(out.starts_with(root.join("artifacts")));
    out
}
fn prove(out: &Path, symbol: &str) -> Value {
    let path = out.join(format!("{symbol}-secondary-bank.json"));
    let discovery = matrix::read(&path);
    if discovery["pools"].as_array().unwrap().is_empty() {
        return json!({"symbol":symbol,"status":"NO_DLMM_POOL_OBSERVED","rows":[]});
    }
    let event = Pubkey::find_program_address(&[b"__event_authority"], &pk(PROGRAM)).0;
    let mut keys: Vec<String> = serde_json::from_value(discovery["keys"].clone()).unwrap();
    keys.push(event.to_string());
    keys.sort();
    keys.dedup();
    assert!(keys.len() <= 100);
    let saved_path = out.join(format!("{symbol}-execution-bank.json"));
    let bank = if std::env::var_os("SKEW_SECONDARY_REPLAY").is_some() {
        let saved = matrix::read(&saved_path);
        assert_eq!(saved["keys"], json!(keys));
        saved["bank"].clone()
    } else {
        let rpc =
            Rpc::pinned("https://api.mainnet-beta.solana.com".into(), MAINNET.into()).unwrap();
        rpc.call(
            "getMultipleAccounts",
            json!([keys,{"encoding":"base64","commitment":"confirmed",
        "minContextSlot":discovery["bank"]["context"]["slot"]}]),
        )
        .unwrap()
    };
    let raw = serde_json::to_vec_pretty(&json!({"keys":keys,"bank":bank})).unwrap();
    fs::write(saved_path, &raw).unwrap();
    let slot = bank["context"]["slot"].as_u64().unwrap();
    let mut accounts = decoded(&keys, &bank, event);
    let mut m = Mollusk::new(&SETTLE, "stocklana_settle");
    m.compute_budget.compute_unit_limit = 1_400_000;
    let mut programs = Vec::new();
    for program in [
        PROGRAM,
        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
        "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
        "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr",
    ] {
        let a = get(&accounts, &pk(program));
        assert!(a.executable);
        let elf = if a.owner == pk("BPFLoaderUpgradeab1e11111111111111111111111") {
            assert_eq!(a.data.len(), 36);
            assert_eq!(&a.data[..4], &[2, 0, 0, 0]);
            let pd = Pubkey::new_from_array(a.data[4..36].try_into().unwrap());
            assert_eq!(
                Some(pd.to_string().as_str()),
                discovery["programData"][program].as_str()
            );
            let program_data = get(&accounts, &pd);
            assert_eq!(program_data.owner, a.owner);
            assert!(!program_data.executable);
            let data = &program_data.data;
            assert_eq!(&data[..4], &[3, 0, 0, 0]);
            &data[45..]
        } else {
            &a.data
        };
        assert_eq!(&elf[..4], b"\x7fELF");
        m.add_program_with_loader_and_elf(&pk(program), &a.owner, elf);
        programs.push(json!({"program":program,"sha256":format!("{:x}",Sha256::digest(elf))}));
    }
    let clock = &get(&accounts, &pk(CLOCK)).data;
    let time = stocklana_adapters::u64_at(clock, 32).unwrap();
    let epoch = stocklana_adapters::u64_at(clock, 16).unwrap();
    assert_eq!(stocklana_adapters::u64_at(clock, 0).unwrap(), slot);
    m.sysvars.clock.slot = slot;
    m.sysvars.clock.unix_timestamp = time as i64;
    m.sysvars.clock.epoch = epoch;
    put(
        &mut accounts,
        WALLET,
        Account {
            lamports: 100_000_000_000_000,
            ..Account::default()
        },
    );
    put(
        &mut accounts,
        SETTLE,
        mollusk_svm::program::create_program_account_loader_v3(&SETTLE),
    );
    let nonce = Pubkey::find_program_address(&[b"stocklana", WALLET.as_ref()], &SETTLE).0;
    let mut data = vec![0u8; 64];
    data[..8].copy_from_slice(b"SKEWSEQ1");
    data[8..40].copy_from_slice(WALLET.as_ref());
    put(
        &mut accounts,
        nonce,
        Account {
            lamports: 10_000_000,
            data,
            owner: SETTLE,
            ..Account::default()
        },
    );
    let target = pk(discovery["mint"].as_str().unwrap());
    let scaled = ScaledUiAmount::decode(&get(&accounts, &target).data, time as i64).unwrap();
    let mut rows = Vec::new();
    let mut pool_liquidity = Vec::new();
    for plan in discovery["pools"].as_array().unwrap() {
        let pool = pk(plan["pool"].as_str().unwrap());
        let pool_data = get(&accounts, &pool).data.clone();
        let mints = [
            Pubkey::new_from_array(stocklana_adapters::key(&pool_data, 88).unwrap()),
            Pubkey::new_from_array(stocklana_adapters::key(&pool_data, 120).unwrap()),
        ];
        assert!(mints.contains(&target));
        let arrays: Vec<String> = serde_json::from_value(plan["tick_arrays"].clone()).unwrap();
        let bins = arrays
            .iter()
            .map(|k| get(&accounts, &pk(k)).data.as_slice())
            .collect::<Vec<_>>();
        let mut liquidity = [0u128; 4];
        for data in &bins {
            for index in 0..70 {
                for (side, offset) in [0, 8, 112, 128].into_iter().enumerate() {
                    liquidity[side] = liquidity[side]
                        .checked_add(u128::from(
                            stocklana_adapters::u64_at(data, 56 + index * 144 + offset).unwrap(),
                        ))
                        .unwrap();
                }
            }
        }
        pool_liquidity.push(json!({"pool":pool.to_string(),"selectedArrays":arrays.len(),
            "discoveredArrays":plan["discoveredArrays"],
            "selectedBinAmmAndLimitLiquidityAtoms":liquidity.map(|q|q.to_string()),
            "allDiscoveredArraysIncluded":plan["discoveredArrays"].as_u64()==Some(arrays.len() as u64)}));
        let fees = mints.map(|k| TransferFee::decode(&get(&accounts, &k).data, epoch).unwrap());
        let curve = DlmmCurve::decode(pool.to_bytes(), &pool_data, &bins, fees, slot, time);
        for input_index in [0usize, 1] {
            let input = mints[input_index];
            let output = mints[1 - input_index];
            let decimals = get(&accounts, &input).data[44];
            let base = 10u64.checked_pow(u32::from(decimals)).unwrap();
            for qty in [base / 100, base, base * 100, base * 1000] {
                let mut row = json!({"symbol":symbol,"pool":pool.to_string(),"slot":slot,
                    "inputMint":input.to_string(),"outputMint":output.to_string(),"inputAtoms":qty.to_string()});
                let expected = match curve
                    .as_ref()
                    .map_err(|e| *e)
                    .and_then(|c| c.quote(qty, input_index == 0))
                {
                    Ok(v) if v > 0 => v,
                    other => {
                        row["status"] = json!("NOT_NATIVE_ADMITTED");
                        row["reason"] = json!(format!("{other:?}"));
                        rows.push(row);
                        continue;
                    }
                };
                let result = (|| -> Result<Value, String> {
                    let mut a = accounts.clone();
                    let view = snapshot(&a, slot);
                    let source = native_wire::wallet_asset(WALLET, input, &view)?;
                    let destination = native_wire::wallet_asset(WALLET, output, &view)?;
                    for asset in [source, destination] {
                        let md = &get(&a, &asset.mint).data;
                        let wallet = token(asset.mint, asset.token_program, md);
                        put(&mut a, asset.token, wallet);
                    }
                    let view = snapshot(&a, slot);
                    let config = MarketConfig {
                        venue: Venue::MeteoraDlmm,
                        program: PROGRAM.into(),
                        pool: pool.to_string(),
                        config: String::new(),
                        input_mint: input.to_string(),
                        output_mint: output.to_string(),
                        tick_arrays: arrays.clone(),
                        array_capacity: None,
                        clock: CLOCK.into(),
                    };
                    let proposal = NativeSwapProposal {
                        market: config,
                        stage: 1,
                        product_id: None,
                        input_atoms: qty,
                        expected_output_atoms: expected,
                    };
                    let leg = native_wire::lower_native_leg(
                        &proposal,
                        &view,
                        WALLET,
                        source,
                        destination,
                        Budget::Remaining,
                    )?;
                    let spec = SwapGraph {
                        owner: WALLET,
                        sequence: 0,
                        input_atoms: qty,
                        minimum_output_atoms: expected,
                        deadline_slot: slot + 100,
                        input: source,
                        output: destination,
                        intermediates: vec![],
                        legs: vec![leg],
                    };
                    let ix = swap_wire::compile_swap_graph(SETTLE, &spec, |k| {
                        let a = a
                            .iter()
                            .find(|(key, _)| key == k)
                            .ok_or("missing compiler dependency")?;
                        Ok(AccountView {
                            owner: a.1.owner,
                            executable: a.1.executable,
                            data: &a.1.data,
                        })
                    })?;
                    let execution = matrix::run(&m, &ix, &a);
                    if execution.program_result.is_err() {
                        return Err(format!("SBF {:?}", execution.program_result));
                    }
                    if amount(&a, &source.token)
                        - amount(&execution.resulting_accounts, &source.token)
                        != qty
                        || amount(&execution.resulting_accounts, &destination.token)
                            - amount(&a, &destination.token)
                            != expected
                    {
                        return Err("native/exact SBF mismatch".into());
                    }
                    let mut fail = ix.clone();
                    fail.data[20..28].copy_from_slice(&(expected + 1).to_le_bytes());
                    let rejected = matrix::run(&m, &fail, &a);
                    if rejected.program_result.is_ok() || rejected.resulting_accounts != a {
                        return Err("floor rollback".into());
                    }
                    let replay = matrix::run(&m, &ix, &execution.resulting_accounts);
                    if replay.program_result.is_ok()
                        || replay.resulting_accounts != execution.resulting_accounts
                    {
                        return Err("nonce replay".into());
                    }
                    Ok(
                        json!({"status":"PASSED_CAPTURED_SBF","outputAtoms":expected.to_string(),
                        "computeUnits":execution.compute_units_consumed,"floorRollback":true,"nonceRollback":true,
                        "scaledShareUnitsQ32":if output==target {Some(scaled.exposure_q32(expected,1,1,10_000).unwrap().to_string())}else{None}}),
                    )
                })();
                let value = result.unwrap_or_else(
                    |error| json!({"status":"LOWERING_OR_SBF_FAILURE","error":error}),
                );
                row.as_object_mut()
                    .unwrap()
                    .extend(value.as_object().unwrap().clone());
                rows.push(row);
            }
        }
    }
    json!({"symbol":symbol,"rows":rows,"poolLiquidity":pool_liquidity,"programs":programs,"bankSlot":slot,
        "bankSha256":format!("{:x}",Sha256::digest(&raw)),"multiplierQ32":scaled.multiplier_q32.to_string()})
}
fn main() {
    let root = Path::new("/srv/skew/stocklana-engine-20260912");
    assert_eq!(std::env::current_dir().unwrap(), root);
    let out = evidence_dir(root);
    // Always exercise the current reproducible deployment candidate. A stale
    // historical ELF can prove an adapter that production would never run.
    std::env::set_var("SBF_OUT_DIR", root.join("artifacts/sbf"));
    let symbols = std::env::args().skip(1).collect::<Vec<_>>();
    assert!(!symbols.is_empty());
    let mut products = Vec::new();
    for symbol in symbols {
        assert!(["NVDAon", "TSLAon", "COINon", "QQQon", "SPYon"].contains(&symbol.as_str()));
        let product = prove(&out, &symbol);
        println!(
            "{}",
            json!({"symbol":symbol,"rows":product["rows"].as_array().unwrap().len()})
        );
        products.push(product);
    }
    let rows = products
        .iter()
        .flat_map(|p| p["rows"].as_array().unwrap())
        .collect::<Vec<_>>();
    let passed = rows
        .iter()
        .filter(|r| r["status"] == "PASSED_CAPTURED_SBF")
        .count();
    let rejected = rows
        .iter()
        .filter(|r| r["status"] == "NOT_NATIVE_ADMITTED")
        .count();
    let failed = rows.len() - passed - rejected;
    let report = json!({"schema":"skew.direct-secondary-sbf/v1","scope":"Actual mint/pool/program bank; synthetic wallet balances; no issuer-policy admission, network signing or submission",
        "executionProven":passed>0 && failed==0,
        "status":if failed>0 {"EXECUTION_MISMATCH"} else if passed>0 {"CAPTURED_SBF_EXECUTION_PROVEN"} else {"NO_EXECUTABLE_SAMPLED_INPUTS"},
        "passed":passed,"notNativeAdmitted":rejected,"failed":failed,"products":products});
    fs::write(
        out.join("direct-secondary-proof.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    println!(
        "{}",
        json!({"passed":passed,"notNativeAdmitted":rejected,"failed":failed})
    );
    assert_eq!(failed, 0);
    // Empty markets are a recorded execution blocker, never a passing fill.
    if passed == 0 {
        assert!(products
            .iter()
            .flat_map(|p| p["poolLiquidity"].as_array().into_iter().flatten())
            .all(|p| p["allDiscoveredArraysIncluded"] == true
                && p["selectedBinAmmAndLimitLiquidityAtoms"] == json!(["0", "0", "0", "0"])));
    }
}
