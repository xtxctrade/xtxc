//! Fail-closed SBF financial checks, plus the CPMM graph on its direct snapshot.
#[allow(dead_code)]
#[path = "../main.rs"]
mod legacy;
#[allow(dead_code)]
#[path = "../matrix.rs"]
mod matrix;
use matrix::*;
use serde_json::json;
use solana_instruction::AccountMeta;
use solana_pubkey::Pubkey;
use std::{fs, path::PathBuf};
use stocklana_adapters as dex;

fn main() {
    let root = PathBuf::from("/srv/skew/stocklana-engine-20260912");
    let mut rows = Vec::new();
    let f = legacy::Fixture::load(root.clone());
    for sell in [false, true] {
        for input in [100_000u64, 1_000_000, 100_000_000] {
            let ix = f.ray(sell, input, 1);
            let direct = run(&f.m, &ix, &f.a);
            assert!(direct.program_result.is_ok());
            let source = if sell { f.keys[2] } else { f.keys[3] };
            let destination = if sell { f.keys[3] } else { f.keys[2] };
            let output = amount(&direct.resulting_accounts, &destination) - START;
            let c = Case {
                name: "CPMM-direct-snapshot".into(),
                a: f.a.clone(),
                venue: dex::graph::Venue::RaydiumCpmm,
                quote: json!({}),
                jup: ix.clone(),
                input: source,
                output: destination,
                slot: f.slot,
                time: f.time,
            };
            let graph = c.graph(&[(ix, sell)], input, output).unwrap();
            let r = run(&f.m, &graph, &c.a);
            assert!(r.program_result.is_ok());
            assert_eq!(START - amount(&r.resulting_accounts, &source), input);
            assert_eq!(amount(&r.resulting_accounts, &destination) - START, output);
            rows.push(json!({"case":"cpmm_graph","sell":sell,"input":input,"output":output,"cu":r.compute_units_consumed}));
        }
    }
    // The CLOB's indivisible lots leave real input dust. The second venue
    // receives the observed remainder in the same SBF invocation.
    for input in [100_000_000u64, 1_000_000_000, 10_000_000_000] {
        let phoenix = f.phoenix(false, input, 16);
        let first = run(&f.m, &phoenix, &f.a);
        assert!(first.program_result.is_ok());
        let spent = START - amount(&first.resulting_accounts, &f.keys[3]);
        let residual = input - spent;
        assert!(residual > 0);
        let ray = f.ray(false, residual, 1);
        let last = run(&f.m, &ray, &first.resulting_accounts);
        assert!(last.program_result.is_ok());
        let output = amount(&last.resulting_accounts, &f.keys[2]) - START;
        let c = Case {
            name: "Phoenix lot remainder to CPMM".into(),
            a: f.a.clone(),
            venue: dex::graph::Venue::Phoenix,
            quote: json!({}),
            jup: phoenix.clone(),
            input: f.keys[3],
            output: f.keys[2],
            slot: f.slot,
            time: f.time,
        };
        let graph = c
            .graph_mixed(
                &[
                    (dex::graph::Venue::Phoenix, phoenix, false),
                    (dex::graph::Venue::RaydiumCpmm, ray, false),
                ],
                input,
                output,
            )
            .unwrap();
        let result = run(&f.m, &graph, &f.a);
        assert!(result.program_result.is_ok());
        assert_eq!(START - amount(&result.resulting_accounts, &c.input), input);
        assert_eq!(
            amount(&result.resulting_accounts, &c.output) - START,
            output
        );
        rows.push(json!({"case":"phoenix_lot_dust_reflow_to_cpmm","input":input,"first_actual_input":spent,"observed_residual":residual,"output":output,"cu":result.compute_units_consumed,"legs":2}));
    }
    let mut m = runtime(&root);
    let c = Case::load(&root.join("artifacts/venue-matrix/cases/NVDAx-buy-raydium_clmm"));
    m.sysvars.clock.slot = c.slot;
    m.sysvars.clock.unix_timestamp = c.time as i64;
    let jup = run(&m, &c.jup, &c.a);
    assert!(jup.program_result.is_ok());
    let legs = c.lower(&jup).unwrap();
    let direct = run(&m, &legs[0].0, &c.a);
    assert!(direct.program_result.is_ok());
    let out = amount(&direct.resulting_accounts, &c.output) - START;
    let graph = c.graph(&legs, 100_000_000, out).unwrap();
    let mut faults = Vec::new();
    let mut no_signer = graph.clone();
    no_signer.accounts[0].is_signer = false;
    faults.push(("missing_signer", no_signer, c.a.clone()));
    let mut mint_write = graph.clone();
    mint_write.accounts[graph.data[37] as usize].is_writable = true;
    faults.push(("writable_mint", mint_write, c.a.clone()));
    let mut alias = graph.clone();
    alias.data[39] = alias.data[36];
    faults.push(("asset_alias", alias, c.a.clone()));
    let mut overspend = graph.clone();
    overspend.data[12..20].copy_from_slice(&(START + 1).to_le_bytes());
    faults.push(("input_over_balance", overspend, c.a.clone()));
    let mut expiry = graph.clone();
    expiry.data[28..36].copy_from_slice(&(c.slot - 1).to_le_bytes());
    faults.push(("expired", expiry, c.a.clone()));
    let mut replays = graph.clone();
    replays.data[4..12].copy_from_slice(&1u64.to_le_bytes());
    faults.push(("wrong_sequence", replays, c.a.clone()));
    let mut budget = graph.clone();
    budget.data[46..54].fill(0);
    faults.push(("zero_leg_budget", budget, c.a.clone()));
    let mut cycle = graph.clone();
    cycle.data[43] = 1;
    faults.push(("cyclic_edge", cycle, c.a.clone()));
    let mut extra = graph.clone();
    let other = Pubkey::new_from_array([101; 32]);
    extra.accounts.push(AccountMeta::new(other, false));
    let mut a = c.a.clone();
    let t = get(&a, &c.output).clone();
    put(&mut a, other, t);
    faults.push(("undeclared_wallet_balance", extra, a));
    let om = graph.accounts[graph.data[40] as usize].pubkey;
    for (kind, offset, name) in [(26, 32, "paused_mint"), (14, 32, "active_transfer_hook")] {
        let mut a = c.a.clone();
        let mut mint = get(&a, &om).clone();
        let mut cursor = 166;
        let mut found = false;
        while cursor + 4 <= mint.data.len() {
            let k = u16::from_le_bytes([mint.data[cursor], mint.data[cursor + 1]]);
            let len = u16::from_le_bytes([mint.data[cursor + 2], mint.data[cursor + 3]]) as usize;
            if k == kind {
                mint.data[cursor + 4 + offset] = 1;
                found = true;
                break;
            }
            cursor += 4 + len;
        }
        assert!(found);
        put(&mut a, om, mint);
        faults.push((name, graph.clone(), a));
    }
    for (name, ix, a) in faults {
        let r = run(&m, &ix, &a);
        assert!(r.program_result.is_err(), "fault accepted: {name}");
        assert_eq!(r.resulting_accounts, a, "partial mutation: {name}");
        rows.push(json!({"fault":name,"error":format!("{:?}",r.program_result),"cu":r.compute_units_consumed,"all_accounts_unchanged":true}));
    }
    // Every truncated wire and deterministic malformed byte stream must reject
    // before obtaining authority. SBF execution catches indexing panics too.
    for len in 0..graph.data.len() {
        let mut bad = graph.clone();
        bad.data.truncate(len);
        let r = run(&m, &bad, &c.a);
        assert!(r.program_result.is_err());
        assert_eq!(r.resulting_accounts, c.a);
    }
    rows.push(json!({"fault":"all_wire_truncations","count":graph.data.len(),"all_accounts_unchanged":true}));
    m.compute_budget.compute_unit_limit = 20_000;
    let r = run(&m, &graph, &c.a);
    assert!(r.program_result.is_err());
    assert_eq!(r.resulting_accounts, c.a);
    rows.push(json!({"fault":"CU_exhaustion","cu":r.compute_units_consumed,"all_accounts_unchanged":true}));
    let report = json!({"rows":rows});
    println!("{report}");
    fs::write(
        root.join(if std::env::args().any(|x| x == "--fair-execution") {
            "artifacts/fair-execution/graph-adversarial.json"
        } else if std::env::args().any(|x| x == "--seven-gates") {
            "artifacts/seven-gates/graph-adversarial.json"
        } else {
            "artifacts/venue-matrix/graph-adversarial.json"
        }),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
}
