//! Actual SBF policy publication, version transitions and guarded NVDA execution.
#[allow(dead_code)]
#[path = "../matrix.rs"]
mod matrix;
use matrix::*;
use serde_json::json;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use std::{fs, path::PathBuf};
use stocklana_adapters as dex;
fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--derive-policy") {
        assert_eq!(args.len(), 7);
        let authority = pk(&args[2]);
        let instrument = pk(&args[3]);
        let input = pk(&args[4]);
        let output = pk(&args[5]);
        let rights = pk(&args[6]);
        let (policy, _) = Pubkey::find_program_address(
            &[
                b"stock2",
                authority.as_ref(),
                instrument.as_ref(),
                input.as_ref(),
                output.as_ref(),
                rights.as_ref(),
            ],
            &SETTLE,
        );
        println!("{}", json!({"policy":policy.to_string()}));
        return;
    }

    let root = PathBuf::from("/srv/skew/stocklana-engine-20260912");
    let mut m = runtime(&root);
    let c = Case::load(&root.join("artifacts/venue-matrix/cases/NVDAx-buy-raydium_clmm"));
    m.sysvars.clock.slot = c.slot;
    m.sysvars.clock.unix_timestamp = c.time as i64;
    m.sysvars.clock.epoch = dex::u64_at(
        &get(&c.a, &pk("SysvarC1ock11111111111111111111111111111111")).data,
        16,
    )
    .unwrap();
    let baseline = run(&m, &c.jup, &c.a);
    assert!(baseline.program_result.is_ok());
    let legs = c.lower(&baseline).unwrap();
    let out = amount(&run(&m, &legs[0].0, &c.a).resulting_accounts, &c.output) - START;
    let input = dex::u64_at(&legs[0].0.data, 8).unwrap();
    let authority = Pubkey::new_from_array([122; 32]);
    let instrument = [31; 32];
    let issuer = [32; 32];
    let rights = [33; 32];
    let im = dex::key(&get(&c.a, &c.input).data, 0).unwrap();
    let om = dex::key(&get(&c.a, &c.output).data, 0).unwrap();
    let (policy, _) = Pubkey::find_program_address(
        &[
            b"stock2",
            authority.as_ref(),
            &instrument,
            &im,
            &om,
            &rights,
        ],
        &SETTLE,
    );
    let mut a = c.a.clone();
    put(
        &mut a,
        authority,
        Account {
            lamports: 1_000_000_000,
            ..Account::default()
        },
    );
    put(&mut a, policy, Account::default());
    put(
        &mut a,
        Pubkey::default(),
        Account {
            lamports: 1,
            owner: pk("NativeLoader1111111111111111111111111111111"),
            executable: true,
            ..Account::default()
        },
    );
    let mut data = vec![19];
    for k in [instrument, issuer, im, om] {
        data.extend_from_slice(&k);
    }
    data.extend_from_slice(&rights);
    data.extend_from_slice(&1u64.to_le_bytes());
    data.extend_from_slice(&(c.slot + 200).to_le_bytes());
    data.push(17);
    let publish = Instruction {
        program_id: SETTLE,
        accounts: vec![
            AccountMeta::new(authority, true),
            AccountMeta::new(policy, false),
            AccountMeta::new_readonly(Pubkey::default(), false),
        ],
        data,
    };
    let result = run(&m, &publish, &a);
    assert!(result.program_result.is_ok(), "{:?}", result.program_result);
    let initialized = result.resulting_accounts;
    let mut graph = c.graph(&legs, input, out).unwrap();
    graph.data[28..36].copy_from_slice(&(c.slot + 100).to_le_bytes());
    let pi = graph.accounts.len();
    graph
        .accounts
        .push(AccountMeta::new_readonly(policy, false));
    let mut data = vec![7, pi as u8, 0, 0];
    data.extend_from_slice(&1u64.to_le_bytes());
    data.extend_from_slice(&150u64.to_le_bytes());
    data.extend_from_slice(&graph.data);
    let guarded = Instruction {
        program_id: SETTLE,
        accounts: graph.accounts,
        data,
    };
    let mut rows = vec![json!({"case":"policy_initialized","cu":result.compute_units_consumed})];
    let good = run(&m, &guarded, &initialized);
    assert!(good.program_result.is_ok(), "{:?}", good.program_result);
    assert_eq!(&good.return_data[..8], b"SKEWSTK2");
    assert_eq!(dex::u64_at(&good.return_data, 8).unwrap(), 1);
    assert_eq!(START - amount(&good.resulting_accounts, &c.input), input);
    assert_eq!(amount(&good.resulting_accounts, &c.output) - START, out);
    rows.push(json!({"case":"guarded_stock_swap","input":input,"output":out,"cu":good.compute_units_consumed}));

    // A large intent is committed in a plan PDA and advances through exact,
    // owner-signed stock graphs. The two successful capsules prove resumable
    // progress; duplicate sequence and control-account smuggling fail before
    // any state can survive the transaction.
    let plan_id = [55; 32];
    let (plan, _) = Pubkey::find_program_address(&[b"capsule", WALLET.as_ref(), &plan_id], &SETTLE);
    let mut plan_bank = initialized.clone();
    put(&mut plan_bank, plan, Account::default());
    let mut plan_data = vec![10];
    plan_data.extend_from_slice(&plan_id);
    plan_data.extend_from_slice(&input.checked_mul(2).unwrap().to_le_bytes());
    plan_data.extend_from_slice(&2u64.to_le_bytes());
    plan_data.extend_from_slice(&input.to_le_bytes());
    plan_data.extend_from_slice(&1u64.to_le_bytes());
    plan_data.extend_from_slice(&(c.slot + 200).to_le_bytes());
    let initialize_plan = Instruction {
        program_id: SETTLE,
        accounts: vec![
            AccountMeta::new(WALLET, true),
            AccountMeta::new(plan, false),
            AccountMeta::new_readonly(policy, false),
            AccountMeta::new_readonly(Pubkey::new_from_array(im), false),
            AccountMeta::new_readonly(Pubkey::new_from_array(om), false),
            AccountMeta::new_readonly(Pubkey::default(), false),
        ],
        data: plan_data,
    };
    let plan_initialized = run(&m, &initialize_plan, &plan_bank);
    assert!(
        plan_initialized.program_result.is_ok(),
        "{:?}",
        plan_initialized.program_result
    );
    assert_eq!(
        &get(&plan_initialized.resulting_accounts, &plan).data[..8],
        b"SKEWCAP1"
    );
    rows.push(json!({"case":"flow_capsule_plan_initialized","cu":plan_initialized.compute_units_consumed,"onchain_plan":true,"cross_transaction_atomic":false}));

    let make_capsule = |graph_sequence: u64, capsule_sequence: u64| {
        let mut graph = c.graph(&legs, input, 1).unwrap();
        graph.data[4..12].copy_from_slice(&graph_sequence.to_le_bytes());
        graph.data[28..36].copy_from_slice(&(c.slot + 100).to_le_bytes());
        let policy_index = graph.accounts.len();
        graph
            .accounts
            .push(AccountMeta::new_readonly(policy, false));
        let plan_index = graph.accounts.len();
        graph.accounts.push(AccountMeta::new(plan, false));
        let mut data = vec![11, plan_index as u8, policy_index as u8, 0];
        data.extend_from_slice(&plan_id);
        data.extend_from_slice(&capsule_sequence.to_le_bytes());
        data.extend_from_slice(&1u64.to_le_bytes());
        data.extend_from_slice(&150u64.to_le_bytes());
        data.extend_from_slice(&graph.data);
        Instruction {
            program_id: SETTLE,
            accounts: graph.accounts,
            data,
        }
    };
    let capsule_one = make_capsule(0, 1);
    let capsule_one_result = run(&m, &capsule_one, &plan_initialized.resulting_accounts);
    assert!(
        capsule_one_result.program_result.is_ok(),
        "{:?}",
        capsule_one_result.program_result
    );
    assert_eq!(&capsule_one_result.return_data[..8], b"SKEWCAP1");
    assert_eq!(dex::u64_at(&capsule_one_result.return_data, 40).unwrap(), 1);
    rows.push(json!({"case":"flow_capsule_one","cu":capsule_one_result.compute_units_consumed,"input":dex::u64_at(&capsule_one_result.return_data,48).unwrap(),"output":dex::u64_at(&capsule_one_result.return_data,56).unwrap()}));

    let duplicate = run(&m, &capsule_one, &capsule_one_result.resulting_accounts);
    assert!(duplicate.program_result.is_err());
    assert_eq!(
        duplicate.resulting_accounts,
        capsule_one_result.resulting_accounts
    );
    rows.push(json!({"case":"flow_capsule_duplicate_sequence","cu":duplicate.compute_units_consumed,"rollback":true}));

    let mut smuggled = make_capsule(1, 2);
    let plan_index = smuggled.data[1];
    smuggled.data[60 + dex::graph::HEADER_LEN] = plan_index;
    let rejected = run(&m, &smuggled, &capsule_one_result.resulting_accounts);
    assert!(rejected.program_result.is_err());
    assert_eq!(
        rejected.resulting_accounts,
        capsule_one_result.resulting_accounts
    );
    rows.push(json!({"case":"flow_capsule_control_account_smuggling","cu":rejected.compute_units_consumed,"rollback":true}));

    let capsule_two = make_capsule(1, 2);
    let capsule_two_result = run(&m, &capsule_two, &capsule_one_result.resulting_accounts);
    assert!(
        capsule_two_result.program_result.is_ok(),
        "{:?}",
        capsule_two_result.program_result
    );
    assert_eq!(dex::u64_at(&capsule_two_result.return_data, 80).unwrap(), 1);
    let final_plan = &get(&capsule_two_result.resulting_accounts, &plan).data;
    assert_eq!(dex::u64_at(final_plan, 192).unwrap(), input * 2);
    assert_eq!(dex::u64_at(final_plan, 224).unwrap(), 2);
    rows.push(json!({"case":"flow_capsule_complete","cu":capsule_two_result.compute_units_consumed,"capsules":2,"onchain_cumulative_floor":true}));
    for name in [
        "wrong_version",
        "writable_policy",
        "expired_deadline",
        "policy_age",
        "wrong_mint",
        "wrong_rights",
    ] {
        let mut bad = guarded.clone();
        let mut bank = initialized.clone();
        match name {
            "wrong_version" => bad.data[4..12].copy_from_slice(&2u64.to_le_bytes()),
            "writable_policy" => bad.accounts[pi].is_writable = true,
            "expired_deadline" => {
                bad.data[20 + 28..20 + 36].copy_from_slice(&(c.slot + 201).to_le_bytes())
            }
            "policy_age" => {
                // The captured bank also contains Clock. Keep its bytes and
                // Mollusk's syscall cache at the same advanced slot.
                m.sysvars.clock.slot = c.slot + 151;
                let clock = pk("SysvarC1ock11111111111111111111111111111111");
                let mut account = get(&bank, &clock).clone();
                account.data[..8].copy_from_slice(&(c.slot + 151).to_le_bytes());
                put(&mut bank, clock, account);
            }
            "wrong_mint" => {
                let mut p = get(&bank, &policy).clone();
                p.data[104..136].copy_from_slice(&[9; 32]);
                put(&mut bank, policy, p);
            }
            "wrong_rights" => {
                let mut p = get(&bank, &policy).clone();
                p.data[168..200].copy_from_slice(&[9; 32]);
                put(&mut bank, policy, p);
            }
            _ => unreachable!(),
        }
        let r = run(&m, &bad, &bank);
        assert!(r.program_result.is_err(), "{name}");
        assert_eq!(r.resulting_accounts, bank);
        rows.push(json!({"case":name,"rollback":true,"cu":r.compute_units_consumed}));
        m.sysvars.clock.slot = c.slot;
    }
    let mut halt = publish.clone();
    halt.data[161..169].copy_from_slice(&2u64.to_le_bytes());
    halt.data[177] = 25;
    let changed = run(&m, &halt, &initialized);
    assert!(changed.program_result.is_ok());
    let mut new = guarded.clone();
    new.data[4..12].copy_from_slice(&2u64.to_le_bytes());
    for (name, ix) in [
        ("old_version_after_update", &guarded),
        ("corporate_action_halt", &new),
    ] {
        let r = run(&m, ix, &changed.resulting_accounts);
        assert!(r.program_result.is_err());
        assert_eq!(r.resulting_accounts, changed.resulting_accounts);
        rows.push(json!({"case":name,"rollback":true,"cu":r.compute_units_consumed}));
    }
    let mut reopen = publish.clone();
    reopen.data[161..169].copy_from_slice(&3u64.to_le_bytes());
    reopen.data[177] = 1;
    let closed = run(&m, &reopen, &changed.resulting_accounts);
    assert!(closed.program_result.is_ok());
    new.data[4..12].copy_from_slice(&3u64.to_le_bytes());
    let r = run(&m, &new, &closed.resulting_accounts);
    assert!(r.program_result.is_err());
    assert_eq!(r.resulting_accounts, closed.resulting_accounts);
    rows.push(json!({"case":"closed_underlying_requires_signed_opt_in","rollback":true,"cu":r.compute_units_consumed}));
    new.data[2] = 1;
    let r = run(&m, &new, &closed.resulting_accounts);
    assert!(r.program_result.is_ok());
    rows.push(json!({"case":"closed_underlying_secondary_opt_in","cu":r.compute_units_consumed}));
    let mut unsigned = reopen.clone();
    unsigned.accounts[0].is_signer = false;
    for (name, ix) in [
        ("unsigned_policy_update", &unsigned),
        ("policy_version_replay", &halt),
    ] {
        let r = run(&m, ix, &closed.resulting_accounts);
        assert!(r.program_result.is_err());
        assert_eq!(r.resulting_accounts, closed.resulting_accounts);
        rows.push(json!({"case":name,"rollback":true,"cu":r.compute_units_consumed}));
    }
    fs::write(root.join(if std::env::args().any(|x|x=="--fair-execution") {"artifacts/fair-execution/stock-proof.json"} else {"artifacts/seven-gates/stock-proof.json"}),serde_json::to_vec_pretty(&json!({"scope":"user-selected policy authority, fixture publication and captured NVDA SBF; not issuer membership or live corporate-action data","rows":rows})).unwrap()).unwrap();
}
