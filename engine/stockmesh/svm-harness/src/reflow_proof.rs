use crate::{matrix::*, native_bridge::Curve};
use base64::{engine::general_purpose::STANDARD, Engine};
use mollusk_svm::Mollusk;
use serde_json::json;
use solana_instruction::Instruction;
use std::{
    fs,
    path::{Path, PathBuf},
};
use stocklana_adapters::{self as dex, graph::Venue};

pub fn prove(root: &Path, m: &Mollusk, c: &Case, all: &[(Venue, Instruction, bool)]) {
    let out_dir = std::env::var_os("SKEW_REFLOW_PROOF_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            root.join(if std::env::args().any(|x| x == "--fair-execution") {
                "artifacts/fair-execution"
            } else {
                "artifacts/seven-gates"
            })
        });
    fs::create_dir_all(&out_dir).unwrap();
    let legs: Vec<_> = all
        .iter()
        .filter(|x| matches!(x.0, Venue::RaydiumClmm | Venue::ByrealClmm))
        .cloned()
        .collect();
    assert_eq!(legs.len(), 2);
    let mut rows = Vec::new();
    let mut negatives = Vec::new();
    let seeded = std::env::args().any(|x| x == "--seeded");
    let seeds = if seeded {
        vec![425_000u64, 437_500, 445_000, 450_000, 475_000]
    } else {
        vec![0]
    };
    for seed in seeds {
        for usd in [100u64, 1000, 10_000, 50_000, 100_000, 500_000] {
            for calls in [8u8, 16, 32, 64] {
                if std::env::args().any(|x| x == "--reflow-profile")
                    && (usd != 500_000 || calls != 8)
                {
                    continue;
                }
                if seeded && (usd != 500_000 || calls != 8) {
                    continue;
                }
                let input = usd * 1_000_000;
                let mut ix = c
                    .reflow(&legs, input, 1, calls, seeded.then_some(seed * 1_000_000))
                    .unwrap();
                let r = run(m, &ix, &c.a);
                if std::env::args().any(|x| x == "--reflow-profile") {
                    for line in m.logger.as_ref().unwrap().borrow().get_recorded_content() {
                        println!("{line}");
                    }
                }
                let mut row = json!({"seed_usd":seed,"usd":usd,"oracle_call_limit_per_round":calls,"status":format!("{:?}",r.program_result),"cu":r.compute_units_consumed});
                if r.program_result.is_ok() {
                    assert_eq!(START - amount(&r.resulting_accounts, &c.input), input);
                    let output = amount(&r.resulting_accounts, &c.output) - START;
                    let count = dex::u64_at(&r.return_data, 24).unwrap() as usize;
                    assert!((1..=4).contains(&count));
                    assert_eq!(r.return_data.len(), 32 + count * 48);
                    let mut replay = c.a.clone();
                    let mut remaining = input;
                    let mut trace = Vec::new();
                    for step in 0..count {
                        let b = &r.return_data[32 + step * 48..32 + (step + 1) * 48];
                        let budget = dex::u64_at(b, 8).unwrap();
                        let spent = dex::u64_at(b, 16).unwrap();
                        let received = dex::u64_at(b, 24).unwrap();
                        let chosen = dex::u64_at(b, 32).unwrap() as usize;
                        let queries = dex::u64_at(b, 40).unwrap();
                        assert!(chosen < legs.len() && queries <= u64::from(calls));
                        // Independent off-chain replay decodes ALL curves from each
                        // resulting bank; it does not share the on-chain head cache.
                        let curves: Vec<_> = legs
                            .iter()
                            .map(|(v, i, d)| {
                                Curve::decode(
                                    &replay,
                                    *v,
                                    i,
                                    *d,
                                    c.slot,
                                    c.time,
                                    m.sysvars.clock.epoch,
                                )
                                .unwrap()
                            })
                            .collect();
                        let (expected, allocation) = if step == 0 && seeded {
                            assert_eq!(queries, 0);
                            (0, seed * 1_000_000)
                        } else if step == 3 {
                            let best = (0..curves.len())
                                .filter_map(|i| curves[i].quote(remaining).ok().map(|q| (i, q)))
                                .max_by_key(|(i, q)| (*q, std::cmp::Reverse(*i)))
                                .unwrap();
                            (best.0, remaining)
                        } else {
                            let plan = skew_engine::optimizer::oracle::refine(
                                curves.len(),
                                remaining,
                                u32::from(calls),
                                |i, q| curves[i].quote(q).map_err(|_| skew_engine::Error::Capacity),
                            )
                            .unwrap();
                            let selected = (0..curves.len())
                                .max_by_key(|i| (plan.inputs[*i], std::cmp::Reverse(*i)))
                                .unwrap();
                            (selected, plan.inputs[selected])
                        };
                        assert_eq!(
                            (chosen, budget),
                            (expected, allocation),
                            "fresh residual allocation mismatch"
                        );
                        let (v, mut direct, d) = legs[chosen].clone();
                        let mut bytes = [0; 80];
                        let n = v.swap_data(budget, d, &mut bytes).unwrap();
                        direct.data = bytes[..n].to_vec();
                        let rr = run(m, &direct, &replay);
                        assert!(rr.program_result.is_ok());
                        assert_eq!(
                            amount(&replay, &c.input) - amount(&rr.resulting_accounts, &c.input),
                            spent
                        );
                        assert_eq!(
                            amount(&rr.resulting_accounts, &c.output) - amount(&replay, &c.output),
                            received
                        );
                        assert_eq!(curves[chosen].quote(budget).unwrap(), received);
                        replay = rr.resulting_accounts;
                        remaining -= spent;
                        trace.push(json!({"step":step,"candidate":chosen,"venue":v as u8,"budget":budget,"spent":spent,"received":received,"remaining":remaining,"oracle_calls":queries,"fresh_bank_replay_matches":true}));
                    }
                    assert_eq!(remaining, 0);
                    assert_eq!(
                        amount(&replay, &c.output),
                        amount(&r.resulting_accounts, &c.output)
                    );
                    row["output_atoms"] = json!(output);
                    row["execution_legs"] = json!(count);
                    row["reflows"] = json!(count - 1);
                    row["trace"] = json!(trace);
                    if usd == 50_000 && calls == 8 {
                        for fault in [
                            "late_min_out",
                            "nonce_replay",
                            "alias_pool",
                            "unsigned_owner",
                            "expired",
                            "unsupported_venue",
                            "work_limit",
                        ] {
                            let mut bad = ix.clone();
                            let mut bank = c.a.clone();
                            match fault {
                                "late_min_out" => {
                                    bad.data[20..28].copy_from_slice(&(output + 1).to_le_bytes())
                                }
                                "nonce_replay" => bank = r.resulting_accounts.clone(),
                                "alias_pool" => {
                                    let first =
                                        bad.data[42..42 + 14 + legs[0].1.accounts.len()].to_vec();
                                    bad.data.truncate(42);
                                    bad.data.extend_from_slice(&first);
                                    bad.data.extend_from_slice(&first);
                                }
                                "unsigned_owner" => bad.accounts[0].is_signer = false,
                                "expired" => {
                                    bad.data[28..36].copy_from_slice(&(c.slot - 1).to_le_bytes())
                                }
                                "unsupported_venue" => bad.data[42] = 8,
                                "work_limit" => bad.data[3] = 65,
                                _ => unreachable!(),
                            }
                            let rr = run(m, &bad, &bank);
                            assert!(rr.program_result.is_err(), "{fault}");
                            assert_eq!(rr.resulting_accounts, bank, "{fault}: rollback");
                            negatives.push(json!({"fault":fault,"status":format!("{:?}",rr.program_result),"cu":rr.compute_units_consumed,"rollback":true}));
                        }
                    }
                    if seeded && seed == 450_000 {
                        for fault in [
                            "seed_zero",
                            "seed_full",
                            "seed_overflow",
                            "seed_cap",
                            "truncated",
                            "trailing",
                            "bad_skipped_candidate",
                            "late_floor",
                            "nonce_replay",
                            "unsigned_owner",
                            "expired",
                            "work_limit",
                        ] {
                            let mut bad = ix.clone();
                            let mut bank = c.a.clone();
                            match fault {
                                "seed_zero" => bad.data[42..50].fill(0),
                                "seed_full" => {
                                    bad.data[42..50].copy_from_slice(&input.to_le_bytes())
                                }
                                "seed_overflow" => bad.data[42..50].fill(255),
                                "seed_cap" => bad.data[54..62].copy_from_slice(&1u64.to_le_bytes()),
                                "truncated" => {
                                    bad.data.truncate(49);
                                }
                                "trailing" => bad.data.push(0),
                                "bad_skipped_candidate" => {
                                    let p = 50 + 14 + bad.data[63] as usize;
                                    bad.data[p] = 8;
                                }
                                "late_floor" => {
                                    bad.data[20..28].copy_from_slice(&(output + 1).to_le_bytes())
                                }
                                "nonce_replay" => bank = r.resulting_accounts.clone(),
                                "unsigned_owner" => bad.accounts[0].is_signer = false,
                                "expired" => {
                                    bad.data[28..36].copy_from_slice(&(c.slot - 1).to_le_bytes())
                                }
                                "work_limit" => bad.data[3] = 65,
                                _ => unreachable!(),
                            }
                            let rr = run(m, &bad, &bank);
                            assert!(rr.program_result.is_err(), "seeded {fault}");
                            assert_eq!(rr.resulting_accounts, bank, "seeded rollback {fault}");
                            negatives.push(json!({"fault":fault,"status":format!("{:?}",rr.program_result),"cu":rr.compute_units_consumed,"rollback":true}));
                        }
                    }
                    if (usd == 100_000 && calls == 8) || (seeded && seed == 450_000) {
                        assert!(count > 1, "signed fixture must actually reflow");
                        // The signed private-chain fixture keeps a meaningful
                        // output floor; the exploratory run above is not authority.
                        let floor = u64::try_from(u128::from(output) * 9980 / 10_000).unwrap();
                        ix.data[20..28].copy_from_slice(&floor.to_le_bytes());
                        let bounded = run(m, &ix, &c.a);
                        assert!(bounded.program_result.is_ok());
                        assert_eq!(
                            amount(&bounded.resulting_accounts, &c.output) - START,
                            output
                        );
                        row["private_fixture_min_out"] = json!(floor);
                        row["private_fixture_slippage_bps"] = json!(20);
                        let fixture = json!({"wallets":[WALLET.to_string()],"instruction":{"programId":SETTLE.to_string(),"data":STANDARD.encode(&ix.data),"accounts":ix.accounts.iter().map(|a|json!({"pubkey":a.pubkey.to_string(),"isSigner":a.is_signer,"isWritable":a.is_writable})).collect::<Vec<_>>()},"accounts":c.a.iter().map(|(k,a)|json!({"pubkey":k.to_string(),"owner":a.owner.to_string(),"lamports":a.lamports,"executable":a.executable,"data":STANDARD.encode(&a.data)})).collect::<Vec<_>>()});
                        fs::write(
                            out_dir.join(if seeded {
                                "seeded-fixture.json"
                            } else {
                                "reflow-fixture.json"
                            }),
                            serde_json::to_vec_pretty(&fixture).unwrap(),
                        )
                        .unwrap();
                    }
                } else {
                    assert_eq!(r.resulting_accounts, c.a);
                }
                println!("{row}");
                rows.push(row);
            }
        }
    }
    fs::write(out_dir.join(if seeded { "seeded-proof.json" } else { "reflow-proof.json" }),serde_json::to_vec_pretty(&json!({"scope":"two independent captured NVDAx Raydium/Byreal pools; actual on-chain allocation and CPI; no mainnet submission","rows":rows,"negatives":negatives})).unwrap()).unwrap();
}
