//! Same-bank, independent-pool allocation using actual deployed SBF as oracle.
//! This is a correctness/quality harness, not a low-latency production quoter.
#[path = "../fast_search.rs"]
mod fast_search;
#[allow(dead_code)]
#[path = "../matrix.rs"]
mod matrix;
#[path = "../native_bridge.rs"]
mod native_bridge;
#[path = "../reflow_proof.rs"]
mod reflow_proof;
use base64::{engine::general_purpose::STANDARD, Engine};
use matrix::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use skew_engine::{optimizer::oracle::refine_bounded, Error};
use solana_account::Account;
use solana_instruction::Instruction;
use solana_pubkey::{pubkey, Pubkey};
use std::{collections::BTreeMap, fs, path::PathBuf, time::Instant};
use stocklana_adapters::{self as dex, graph::Venue};

fn main() {
    let root = PathBuf::from("/srv/skew/stocklana-engine-20260912");
    if std::env::args().any(|x| x == "--matched-input") {
        let prior = read(&root.join("artifacts/paired-builders/500000-proof.json"));
        let base = prior["rows"][0]["baselines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["source"] == "jupiter")
            .unwrap();
        let amount = base["actual_input"].as_u64().unwrap();
        assert!(amount > 0 && amount < 500_000_000_000);
        prove(
            &root,
            &root.join("artifacts/paired-builders/500000.json"),
            &[500_000],
            true,
            Some(amount),
        );
    } else if std::env::args().any(|x| x == "--paired") {
        for usd in [100u64, 1_000, 10_000, 50_000, 100_000, 500_000] {
            let file = root.join(format!("artifacts/paired-builders/{usd}.json"));
            prove(&root, &file, &[usd], true, None);
        }
    } else {
        prove(
            &root,
            &root.join("artifacts/competition/coherent-market.json"),
            &[100, 1_000, 10_000, 50_000, 100_000, 500_000],
            false,
            None,
        );
    }
}
fn prove(
    root: &std::path::Path,
    file: &std::path::Path,
    sizes: &[u64],
    paired: bool,
    input_override: Option<u64>,
) {
    let fast = std::env::args().any(|x| x == "--fast");
    let performance = std::env::args().any(|x| x == "--performance");
    let latency = std::env::args().any(|x| x == "--latency");
    let world = std::env::args().any(|x| x == "--world");
    assert!(!world || (!fast && !paired && !latency && !performance));
    assert!(
        !latency || (fast && !paired),
        "latency mode requires --fast coherent bank"
    );
    let native_out = root.join(if std::env::args().any(|x| x == "--fair-execution") {
        "artifacts/fair-execution"
    } else if std::env::args().any(|x| x == "--seven-gates") {
        "artifacts/seven-gates"
    } else if std::env::args().any(|x| x == "--expansion") {
        "artifacts/native-expansion"
    } else {
        "artifacts/native-search"
    });
    fs::create_dir_all(&native_out).unwrap();
    let world_proof = world.then(|| read(&native_out.join("world-proof.json")));
    let state = read(file);
    let mut a = Vec::new();
    for (key, v) in state["keys"]
        .as_array()
        .unwrap()
        .iter()
        .zip(state["accounts"].as_array().unwrap())
    {
        if key == "Sysvar1nstructions1111111111111111111111111" {
            continue;
        }
        let account = if v.is_null() {
            Account::default()
        } else {
            Account {
                lamports: v["lamports"].as_u64().unwrap(),
                data: STANDARD.decode(v["data"][0].as_str().unwrap()).unwrap(),
                owner: pk(v["owner"].as_str().unwrap()),
                executable: v["executable"].as_bool().unwrap(),
                rent_epoch: 0,
            }
        };
        put(&mut a, pk(key.as_str().unwrap()), account);
    }
    let templates = state["templates"].as_array().unwrap();
    let input = pk(templates[0]["input_account"].as_str().unwrap());
    let output = pk(templates[0]["output_account"].as_str().unwrap());
    let im = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
    let om = pubkey!("Xsc9qvGR1efVDFGLrVsmkzv3qi45LTBjeUKSPmx9qEh");
    for (key, mint) in [(input, im), (output, om)] {
        let t = token(mint, get(&a, &mint).owner, &get(&a, &mint).data);
        put(&mut a, key, t);
    }
    put(
        &mut a,
        WALLET,
        Account {
            lamports: 1_000_000_000_000,
            ..Account::default()
        },
    );
    let (nonce, _) = Pubkey::find_program_address(&[b"stocklana", WALLET.as_ref()], &SETTLE);
    let mut n = vec![0u8; 64];
    n[..8].copy_from_slice(b"SKEWSEQ1");
    n[8..40].copy_from_slice(WALLET.as_ref());
    put(
        &mut a,
        nonce,
        Account {
            lamports: 10_000_000,
            owner: SETTLE,
            data: n,
            executable: false,
            rent_epoch: 0,
        },
    );
    put(
        &mut a,
        SETTLE,
        mollusk_svm::program::create_program_account_loader_v3(&SETTLE),
    );
    let legs: Vec<_> = templates
        .iter()
        .map(|v| {
            assert_eq!(v["input_account"], input.to_string());
            assert_eq!(v["output_account"], output.to_string());
            let direct = &v["direct"][0];
            (
                Venue::try_from(v["kind"].as_u64().unwrap() as u8).unwrap(),
                instruction(direct),
                direct["direction"].as_bool().unwrap(),
            )
        })
        .collect();
    // No market write may affect another oracle's read or write set.
    for (i, (_, x, _)) in legs.iter().enumerate() {
        for (_, y, _) in &legs[..i] {
            for p in &x.accounts {
                if [WALLET, input, output].contains(&p.pubkey) {
                    continue;
                }
                assert!(
                    !y.accounts
                        .iter()
                        .any(|q| q.pubkey == p.pubkey && (q.is_writable || p.is_writable)),
                    "shared market dependency: {}",
                    p.pubkey
                );
            }
        }
    }
    let mut m = runtime(root);
    m.compute_budget.compute_unit_limit = 1_400_000;
    let clock = &get(&a, &pubkey!("SysvarC1ock11111111111111111111111111111111")).data;
    m.sysvars.clock.slot = dex::u64_at(clock, 0).unwrap();
    m.sysvars.clock.epoch = dex::u64_at(clock, 16).unwrap();
    m.sysvars.clock.unix_timestamp = dex::u64_at(clock, 32).unwrap() as i64;
    assert_eq!(
        m.sysvars.clock.slot,
        state["context"]["slot"].as_u64().unwrap()
    );
    let c = Case {
        name: "coherent-NVDAx-five-venue".into(),
        a,
        venue: legs[0].0,
        quote: json!({}),
        jup: legs[0].1.clone(),
        input,
        output,
        slot: m.sysvars.clock.slot,
        time: m.sysvars.clock.unix_timestamp as u64,
    };
    if std::env::args().any(|x| x == "--reflow") {
        if std::env::args().any(|x| x == "--reflow-profile") {
            m.compute_budget.compute_unit_limit = 5_000_000; // Diagnostic only, never execution admission.
            m.logger = Some(std::rc::Rc::new(
                std::cell::RefCell::new(Default::default()),
            ));
        }
        reflow_proof::prove(root, &m, &c, &legs);
        return;
    }
    let compiled = fast.then(|| fast_search::Compiled::build(&m, &c, &legs, 500_000_000_000));
    let mut rows = Vec::new();
    let repeats = if latency { 200 } else { 1 };
    for (sample, &usd) in (0..repeats).flat_map(|sample| sizes.iter().map(move |usd| (sample, usd)))
    {
        let q = input_override.unwrap_or(usd * 1_000_000);
        let mut cache = BTreeMap::new();
        let mut simulation_ns = Vec::new();
        let mut oracle = |edge: usize, qty: u64| {
            if let Some(value) = cache.get(&(edge, qty)) {
                return *value;
            }
            let (venue, ix, dir) = &legs[edge];
            let mut ix = ix.clone();
            let mut bytes = [0u8; 80];
            let len = venue.swap_data(qty, *dir, &mut bytes).unwrap();
            ix.data = bytes[..len].to_vec();
            let start = Instant::now();
            let r = run(&m, &ix, &c.a);
            simulation_ns.push(start.elapsed().as_nanos() as u64);
            let value = if r.program_result.is_ok()
                && START.checked_sub(amount(&r.resulting_accounts, &input)) == Some(qty)
            {
                amount(&r.resulting_accounts, &output)
                    .checked_sub(START)
                    .filter(|v| *v > 0)
                    .map(|out| (out, r.compute_units_consumed + 20_000))
                    .ok_or(Error::Capacity)
            } else {
                Err(Error::VenueUnavailable)
            };
            cache.insert((edge, qty), value);
            value
        };
        // Exact single-venue quotes are benchmark comparators, not work the
        // compiled production search performs. Keep their evidence but exclude
        // it from the warm route-search timer.
        let singles: Vec<_> = (0..legs.len()).map(|i| oracle(i, q)).collect();
        let started = Instant::now();
        let mut fast_metrics = json!(null);
        let plan = if let Some(world) = &world_proof {
            let row = world["rows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["inputAtoms"] == q)
                .unwrap();
            assert_eq!(row["slot"], c.slot);
            assert_eq!(row["executionAdmitted"], false);
            let mut best = skew_engine::optimizer::oracle::ExactPlan::default();
            let mut checked = 0;
            let mut rejected = 0;
            let alternatives = row["alternatives"].as_array().unwrap();
            assert!(alternatives.len() <= 3);
            for candidate in std::iter::once(row).chain(alternatives) {
                let mut p = skew_engine::optimizer::oracle::ExactPlan::default();
                let mut direct = Vec::new();
                for l in candidate["legs"].as_array().unwrap() {
                    let i = legs
                        .iter()
                        .position(|(v, ix, d)| {
                            ix.accounts[v.bindings(*d).0].pubkey.to_string()
                                == l["pool"].as_str().unwrap()
                        })
                        .unwrap();
                    assert_eq!(p.inputs[i], 0, "duplicate world leg");
                    p.inputs[i] = l["inputAtoms"].as_u64().unwrap();
                    p.outputs[i] = l["outputAtoms"].as_u64().unwrap();
                    p.output = p.output.checked_add(p.outputs[i]).unwrap();
                    let (v, mut ix, d) = legs[i].clone();
                    let mut b = [0; 80];
                    let n = v.swap_data(p.inputs[i], d, &mut b).unwrap();
                    ix.data = b[..n].to_vec();
                    direct.push((v, ix, d));
                }
                assert_eq!(p.inputs.iter().sum::<u64>(), q);
                assert_eq!(p.output, candidate["outputAtoms"].as_u64().unwrap());
                let graph = c.graph_mixed(&direct, q, 1).unwrap();
                let r = run(&m, &graph, &c.a);
                checked += 1;
                if r.program_result.is_err() {
                    rejected += 1;
                    continue;
                }
                assert_eq!(START - amount(&r.resulting_accounts, &input), q);
                assert_eq!(
                    amount(&r.resulting_accounts, &output) - START,
                    p.output,
                    "world native output mismatch"
                );
                p.cost = r.compute_units_consumed;
                if p.output > best.output {
                    best = p;
                }
            }
            fast_metrics = json!({"world_exact_candidates":checked,"world_rejected_candidates":rejected,"fallback_selected":best.output!=row["outputAtoms"].as_u64().unwrap()});
            if best.output == 0 {
                Err(Error::Capacity)
            } else {
                Ok(best)
            }
        } else if let Some(compiled) = &compiled {
            compiled.solve(q).map(|(p, stats)| {
                fast_metrics = stats;
                p
            })
        } else {
            refine_bounded(legs.len(), q, 4096, 1_350_000, &mut oracle)
        };
        let plan = match plan {
            Ok(p) => p,
            Err(e) => {
                rows.push(json!({"usd":usd,"status":format!("{e:?}")}));
                continue;
            }
        };
        let mut baseline_rows = Vec::new();
        if paired {
            for b in state["baselines"].as_array().unwrap() {
                let mut row = json!({"source":b["source"],"capture_status":b["status"],"scope":"actual swap instruction; preinitialized fixture ATAs; original builder slippage"});
                if b["status"] == "BUILT_NOT_EXECUTED" {
                    let ix = instruction(&b["build"]["swapInstruction"]);
                    let r = run(&m, &ix, &c.a);
                    row["status"] = json!(format!("{:?}", r.program_result));
                    row["cu"] = json!(r.compute_units_consumed);
                    if r.program_result.is_ok() {
                        let spent = START - amount(&r.resulting_accounts, &input);
                        let out = amount(&r.resulting_accounts, &output) - START;
                        row["same_input_comparable"] = json!(spent == q);
                        row["actual_input"] = json!(spent);
                        row["actual_output"] = json!(out);
                        if spent == q {
                            row["skew_improvement_atoms"] =
                                json!(plan.output as i128 - out as i128);
                            row["skew_improvement_bps"] =
                                json!((plan.output as f64 / out as f64 - 1.0) * 10_000.0);
                        }
                    }
                }
                baseline_rows.push(row);
            }
        }
        let best_single = singles
            .iter()
            .filter_map(|v| v.ok().map(|x| x.0))
            .max()
            .unwrap_or(0);
        let mut direct = Vec::new();
        for (i, leg) in legs.iter().enumerate() {
            if plan.inputs[i] > 0 {
                let (v, mut ix, dir) = leg.clone();
                let mut b = [0u8; 80];
                let len = v.swap_data(plan.inputs[i], dir, &mut b).unwrap();
                ix.data = b[..len].to_vec();
                direct.push((v, ix, dir));
            }
        }
        let graph = c.graph_mixed(&direct, q, plan.output).unwrap();
        let result = run(&m, &graph, &c.a);
        if result.program_result.is_err() {
            let row = json!({"usd":usd,"status":"FINAL_SBF_RESOURCE_OR_STATE_REJECTED","error":format!("{:?}",result.program_result),"cu":result.compute_units_consumed,"inputs":&plan.inputs[..legs.len()],"proposed_output":plan.output});
            println!("{row}");
            rows.push(row);
            fs::write(
                if fast || performance || world {
                    native_out.join("split-rejected.json")
                } else {
                    root.join("artifacts/competition/split-rejected.json")
                },
                serde_json::to_vec_pretty(&rows).unwrap(),
            )
            .unwrap();
            continue;
        }
        assert_eq!(START - amount(&result.resulting_accounts, &input), q);
        assert_eq!(
            amount(&result.resulting_accounts, &output) - START,
            plan.output
        );
        // A CU-constrained optimum can be below an unconstrained single-pool quote.
        let budget = Instruction {
            program_id: pubkey!("ComputeBudget111111111111111111111111111111"),
            accounts: vec![],
            data: [vec![2], 1_400_000u32.to_le_bytes().to_vec()].concat(),
        };
        let msg = solana_message::Message::new(&[budget.clone(), graph.clone()], Some(&WALLET));
        let table = solana_message::AddressLookupTableAccount {
            key: Pubkey::new_from_array([91; 32]),
            addresses: graph
                .accounts
                .iter()
                .filter(|x| !x.is_signer)
                .map(|x| x.pubkey)
                .collect(),
        };
        let v0 = solana_message::v0::Message::try_compile(
            &WALLET,
            &[budget, graph.clone()],
            std::slice::from_ref(&table),
            Default::default(),
        )
        .unwrap();
        let versioned = solana_message::VersionedMessage::V0(v0);
        let wire = 65 + versioned.serialize().len();
        assert!(wire <= 1232, "ALT transaction packet bound");
        simulation_ns.sort_unstable();
        let row = json!({"sample":sample,"fast_search":fast_metrics,"usd":usd,"input_atoms":q,"matched_to_baseline_executed_input":input_override.is_some(),"status":"EXACT_SPLIT_SBF_VERIFIED","inputs":&plan.inputs[..legs.len()],"output":plan.output,"best_single_output":best_single,
            "improvement_atoms":plan.output as i128-best_single as i128,"estimated_cu_with_leg_margins":plan.cost,"improvement_bps":if best_single>0{Some((plan.output as f64/best_single as f64-1.0)*10_000.0)}else{None},
            "cu":result.compute_units_consumed,"legs":direct.len(),"baselines":baseline_rows,"oracle_requests":plan.oracle_calls,"unique_sbf_quotes":cache.len(),"budget_exhausted":plan.exhausted,"final_step_atoms":plan.final_step,
            "search_elapsed_ns":started.elapsed().as_nanos() as u64,"sbf_quote_p50_ns":simulation_ns[simulation_ns.len()/2],"sbf_quote_p99_ns":simulation_ns[(simulation_ns.len()-1)*99/100],
            "legacy_wire_bytes_with_budget":65+msg.serialize().len(),"v0_wire_bytes_with_fixture_lookup":wire,
            "optimality":"best observed; no global certificate for opaque/rounded curves"});
        let serialize_ix = |ix: &Instruction| json!({"programId":ix.program_id.to_string(),"data":STANDARD.encode(&ix.data),"accounts":ix.accounts.iter().map(|x|json!({"pubkey":x.pubkey.to_string(),"isSigner":x.is_signer,"isWritable":x.is_writable})).collect::<Vec<_>>()});
        if usd == 50_000 && !paired && !performance && !latency && !world {
            let fixture = json!({"snapshot_slot":c.slot,"wallets":[WALLET.to_string()],"input":q,"min_out":plan.output,"instruction":serialize_ix(&graph),
                "lookup_table":{"key":table.key.to_string(),"addresses":table.addresses.iter().map(ToString::to_string).collect::<Vec<_>>()},
                "accounts":c.a.iter().filter(|(key,_)|graph.accounts.iter().any(|x|x.pubkey==*key)).map(|(key,v)|json!({"pubkey":key.to_string(),"owner":v.owner.to_string(),"lamports":v.lamports,"executable":v.executable,"data":STANDARD.encode(&v.data)})).collect::<Vec<_>>()});
            fs::write(
                if fast {
                    native_out.join("split-fixture.json")
                } else {
                    root.join("artifacts/competition/split-fixture.json")
                },
                serde_json::to_vec_pretty(&fixture).unwrap(),
            )
            .unwrap();
        }
        if !latency {
            println!("{row}");
        }
        rows.push(row);
    }
    let usd = sizes[0];
    let report = json!({"world_native_proposals":world,"cold_compilation":compiled.as_ref().map(|x|&x.metrics),"scope":"five captured independent NVDAx/USDC pools; --fast uses native/sampled proposals with exact SBF finalists; warm search latency includes allocation, exact finalist verification, final exact wire simulation and serialization while exact single-venue comparison quotes are sampled outside that timer; --world verifies host native allocations; not an end-to-end landing latency claim; paired mode replays Jupiter build and DFlow imperative swap, not RFQ/meta landing",
            "slot":c.slot,"snapshot_sha256":format!("{:x}",Sha256::digest(fs::read(file).unwrap())),"settlement_elf_sha256":format!("{:x}",Sha256::digest(fs::read(root.join("artifacts/sbf/stocklana_settle.so")).unwrap())),
            "venues":templates.iter().map(|v|v["case"].clone()).collect::<Vec<_>>(),"rows":rows});
    fs::write(
        if world {
            native_out.join("world-sbf-proof.json")
        } else if latency {
            native_out.join("route-latency.json")
        } else if performance {
            native_out.join("exhaustive-release.json")
        } else if fast {
            native_out.join(format!(
                "{}{}-proof.json",
                if paired {
                    usd.to_string()
                } else {
                    "coherent".into()
                },
                if input_override.is_some() {
                    "-matched-input"
                } else {
                    ""
                }
            ))
        } else if paired {
            root.join(format!(
                "artifacts/paired-builders/{usd}{}-proof.json",
                if input_override.is_some() {
                    "-matched-input"
                } else {
                    ""
                }
            ))
        } else {
            root.join("artifacts/competition/split-proof.json")
        },
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
}
