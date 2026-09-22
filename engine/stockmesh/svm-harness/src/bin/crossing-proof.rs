//! Fully funded opposing intents vs sequential actual DEX execution.
#[allow(dead_code)]
#[path = "../matrix.rs"]
mod matrix;
use base64::{engine::general_purpose::STANDARD, Engine};
use matrix::*;
use serde_json::{json, Value};
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{pubkey, Pubkey};
use std::{fs, path::PathBuf};
use stocklana_adapters as dex;

#[derive(Clone, Copy)]
struct Participant {
    owner: Pubkey,
    nonce: Pubkey,
    source: Pubkey,
    destination: Pubkey,
    im: Pubkey,
    om: Pubkey,
    input: u64,
    min_out: u64,
}
fn participant(
    a: &mut Accounts,
    owner: Pubkey,
    im: Pubkey,
    om: Pubkey,
    input: u64,
    min_out: u64,
) -> Participant {
    put(
        a,
        owner,
        Account {
            lamports: 1_000_000_000_000,
            ..Account::default()
        },
    );
    let mut keys = Vec::new();
    for mint in [im, om] {
        let program = get(a, &mint).owner;
        let (ata, _) = Pubkey::find_program_address(
            &[owner.as_ref(), program.as_ref(), mint.as_ref()],
            &pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
        );
        let mut t = token(mint, program, &get(a, &mint).data);
        t.data[32..64].copy_from_slice(owner.as_ref());
        put(a, ata, t);
        keys.push(ata);
    }
    let (nonce, _) = Pubkey::find_program_address(&[b"stocklana", owner.as_ref()], &SETTLE);
    let mut d = vec![0u8; 64];
    d[..8].copy_from_slice(b"SKEWSEQ1");
    d[8..40].copy_from_slice(owner.as_ref());
    put(
        a,
        nonce,
        Account {
            lamports: 10_000_000,
            owner: SETTLE,
            data: d,
            executable: false,
            rent_epoch: 0,
        },
    );
    Participant {
        owner,
        nonce,
        source: keys[0],
        destination: keys[1],
        im,
        om,
        input,
        min_out,
    }
}
fn compile(a: &Accounts, p: &[Participant], slot: u64) -> Instruction {
    let mut metas = Vec::<AccountMeta>::new();
    let mut key = |k: Pubkey, w: bool, s: bool| -> u8 {
        if let Some(i) = metas.iter().position(|x| x.pubkey == k) {
            metas[i].is_writable |= w;
            metas[i].is_signer |= s;
            i as u8
        } else {
            let i = metas.len();
            metas.push(AccountMeta {
                pubkey: k,
                is_writable: w,
                is_signer: s,
            });
            i as u8
        }
    };
    let mut d = vec![3, p.len() as u8, p.len() as u8, 0];
    d.extend_from_slice(&(slot + 1000).to_le_bytes());
    for x in p {
        for (k, w, s) in [
            (x.owner, false, true),
            (x.nonce, true, false),
            (x.source, true, false),
            (x.destination, true, false),
            (x.im, false, false),
            (x.om, false, false),
            (get(a, &x.im).owner, false, false),
            (get(a, &x.om).owner, false, false),
        ] {
            d.push(key(k, w, s));
        }
        for n in [0, x.input, x.min_out] {
            d.extend_from_slice(&n.to_le_bytes());
        }
    }
    for (i, x) in p.iter().enumerate() {
        d.extend_from_slice(&[
            i as u8,
            if p.len() == 3 {
                ((i + 1) % 3) as u8
            } else {
                (i ^ 1) as u8
            },
        ]);
        d.extend_from_slice(&x.input.to_le_bytes());
    }
    Instruction {
        program_id: SETTLE,
        accounts: metas,
        data: d,
    }
}
fn wire_size(ix: &Instruction, cu: u32) -> usize {
    let budget = Instruction {
        program_id: pubkey!("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data: [vec![2], cu.to_le_bytes().to_vec()].concat(),
    };
    let m = solana_message::Message::new(&[budget, ix.clone()], Some(&WALLET));
    1 + 64 * m.header.num_required_signatures as usize + m.serialize().len()
}
fn main() {
    let root = PathBuf::from("/srv/skew/stocklana-engine-20260912");
    let mut m = runtime(&root);
    let c = Case::load(&root.join("artifacts/venue-matrix/cases/NVDAx-buy-raydium_clmm"));
    m.sysvars.clock.slot = c.slot;
    m.sysvars.clock.unix_timestamp = c.time as i64;
    m.sysvars.clock.epoch = dex::u64_at(
        &get(
            &c.a,
            &pubkey!("SysvarC1ock11111111111111111111111111111111"),
        )
        .data,
        16,
    )
    .unwrap();
    let jup = run(&m, &c.jup, &c.a);
    assert!(jup.program_result.is_ok());
    let lower = c.lower(&jup).unwrap();
    assert_eq!(lower.len(), 1);
    let im = pk(c.quote["inputMint"].as_str().unwrap());
    let om = pk(c.quote["outputMint"].as_str().unwrap());
    if std::env::args().any(|x| x == "--partial") {
        partial(&root, &m, &c, &lower, im, om);
        return;
    }
    let mut rows = Vec::<Value>::new();
    for usd in [100u64, 1_000, 10_000, 50_000, 100_000, 500_000] {
        let input = usd * 1_000_000;
        let mut buy = lower[0].0.clone();
        buy.data[8..16].copy_from_slice(&input.to_le_bytes());
        let first = run(&m, &buy, &c.a);
        if first.program_result.is_err() {
            rows.push(json!({"usd":usd,"baseline_status":format!("{:?}",first.program_result),"status":"BASELINE_CAPACITY_OR_STATE_REJECTED"}));
            continue;
        }
        let shares = amount(&first.resulting_accounts, &c.output) - START;
        for count in [2usize, 4] {
            let mut a = c.a.clone();
            let mut ps = Vec::new();
            for i in 0..count {
                let owner = if i == 0 {
                    WALLET
                } else {
                    Pubkey::new_from_array([120 + i as u8; 32])
                };
                ps.push(if i % 2 == 0 {
                    participant(&mut a, owner, im, om, input, shares)
                } else {
                    participant(&mut a, owner, om, im, shares, input)
                });
            }
            let mut direct = Vec::new();
            for (i, p) in ps.iter().enumerate() {
                let mut ix = buy.clone();
                ix.accounts[0].pubkey = p.owner;
                ix.accounts[3].pubkey = p.source;
                ix.accounts[4].pubkey = p.destination;
                if i % 2 == 1 {
                    ix.accounts.swap(5, 6);
                    ix.accounts.swap(11, 12);
                    ix.data[8..16].copy_from_slice(&shares.to_le_bytes());
                }
                direct.push(ix);
            }
            let reference = m.process_transaction_instructions(&direct, &a);
            let ix = compile(&a, &ps, c.slot);
            let result = run(&m, &ix, &a);
            assert!(
                result.program_result.is_ok(),
                "crossing {usd} / {count}: {:?}",
                result.program_result
            );
            assert_eq!(&result.return_data[..8], b"SKEWCOW1");
            for p in &ps {
                assert_eq!(
                    START - amount(&result.resulting_accounts, &p.source),
                    p.input
                );
                assert_eq!(
                    amount(&result.resulting_accounts, &p.destination) - START,
                    p.min_out
                );
                assert_eq!(
                    dex::u64_at(&get(&result.resulting_accounts, &p.nonce).data, 40).unwrap(),
                    1
                );
            }
            let mut row = json!({"usd_per_buyer":usd,"intents":count,"transfers":count,"stock_atoms_per_seller":shares,"cu":result.compute_units_consumed,
                "direct_dex_status":format!("{:?}",reference.program_result),"direct_dex_cu":reference.compute_units_consumed,
                "legacy_wire_bytes_with_budget":wire_size(&ix,150_000),"signed_minima_met":true,"external_dex_calls":0,"return_data":result.return_data});
            if reference.program_result.is_ok() {
                let buyer = amount(&reference.resulting_accounts, &ps[0].destination) - START;
                let seller = amount(&reference.resulting_accounts, &ps[1].destination) - START;
                row["first_buyer_direct_output"] = json!(buyer);
                row["first_seller_direct_output_usdc_atoms"] = json!(seller);
                row["first_seller_crossing_output_usdc_atoms"] = json!(input);
                assert_eq!(buyer, shares);
                row["first_seller_improvement_bps"] =
                    json!((input as f64 / seller as f64 - 1.0) * 10_000.0);
            }
            if usd == 100 && count == 2 {
                let mut faults = Vec::new();
                let mut missing = ix.clone();
                missing
                    .accounts
                    .iter_mut()
                    .find(|a| a.pubkey == ps[1].owner)
                    .unwrap()
                    .is_signer = false;
                faults.push(("missing_second_signer", missing));
                let mut min = ix.clone();
                min.data[12 + 24..12 + 32].copy_from_slice(&(shares + 1).to_le_bytes());
                faults.push(("late_min_out", min));
                let mut debit = ix.clone();
                debit.data[12 + count * 32 + 2..12 + count * 32 + 10]
                    .copy_from_slice(&(input + 1).to_le_bytes());
                faults.push(("over_debit", debit));
                let mut nonce = ix.clone();
                nonce.data[12 + 8..12 + 16].copy_from_slice(&1u64.to_le_bytes());
                faults.push(("wrong_nonce", nonce));
                let mut expiry = ix.clone();
                expiry.data[4..12].copy_from_slice(&(c.slot - 1).to_le_bytes());
                faults.push(("expired", expiry));
                let mut duplicate = ix.clone();
                duplicate.data[44] = duplicate.data[12];
                faults.push(("duplicate_owner", duplicate));
                for (name, fault) in faults {
                    let r = run(&m, &fault, &a);
                    assert!(r.program_result.is_err(), "{name}");
                    assert_eq!(
                        r.resulting_accounts, a,
                        "{name} rolls back all balances/nonces"
                    );
                    rows.push(json!({"fault":name,"status":format!("{:?}",r.program_result),"cu":r.compute_units_consumed,"all_accounts_unchanged":true}));
                }
                let replay = run(&m, &ix, &result.resulting_accounts);
                assert!(replay.program_result.is_err());
                assert_eq!(replay.resulting_accounts, result.resulting_accounts);
                rows.push(json!({"fault":"nonce_replay","all_accounts_unchanged":true}));
                let fixture = json!({"snapshot_case":c.name,"wallets":ps.iter().map(|p|p.owner.to_string()).collect::<Vec<_>>(),"instruction":{"programId":SETTLE.to_string(),"data":STANDARD.encode(&ix.data),"accounts":ix.accounts.iter().map(|a|json!({"pubkey":a.pubkey.to_string(),"isSigner":a.is_signer,"isWritable":a.is_writable})).collect::<Vec<_>>()},"accounts":a.iter().filter(|(key,_)|ix.accounts.iter().any(|a|a.pubkey==*key)).map(|(key,v)|json!({"pubkey":key.to_string(),"owner":v.owner.to_string(),"lamports":v.lamports,"executable":v.executable,"data":STANDARD.encode(&v.data)})).collect::<Vec<_>>()});
                fs::write(
                    root.join("artifacts/venue-matrix/crossing-fixture.json"),
                    serde_json::to_vec_pretty(&fixture).unwrap(),
                )
                .unwrap();
            }
            println!("{row}");
            rows.push(row);
        }
    }
    // USDC -> NVDAx -> SOL -> USDC, settled as three signed transfers.
    let sol = pubkey!("So11111111111111111111111111111111111111112");
    let mut a = c.a.clone();
    let sol_bytes = fs::read(root.join(format!("artifacts/snapshots/{sol}.bin"))).unwrap();
    put(
        &mut a,
        sol,
        Account {
            lamports: 10_000_000,
            data: sol_bytes,
            owner: pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
            executable: false,
            rent_epoch: 0,
        },
    );
    let ps = [
        participant(&mut a, WALLET, im, om, 100_000_000, 45_000_000),
        participant(
            &mut a,
            Pubkey::new_from_array([121; 32]),
            sol,
            im,
            1_000_000_000,
            100_000_000,
        ),
        participant(
            &mut a,
            Pubkey::new_from_array([122; 32]),
            om,
            sol,
            45_000_000,
            1_000_000_000,
        ),
    ];
    let ix = compile(&a, &ps, c.slot);
    let result = run(&m, &ix, &a);
    assert!(result.program_result.is_ok());
    for p in &ps {
        assert_eq!(
            START - amount(&result.resulting_accounts, &p.source),
            p.input
        );
        assert_eq!(
            amount(&result.resulting_accounts, &p.destination) - START,
            p.min_out
        );
    }
    for (key, value) in &a {
        if !ps
            .iter()
            .any(|p| [p.source, p.destination, p.nonce].contains(key))
        {
            assert_eq!(get(&result.resulting_accounts, key), value);
        }
    }
    let mut bad = ix.clone();
    bad.data[36..44].copy_from_slice(&u64::MAX.to_le_bytes());
    let failure = run(&m, &bad, &a);
    assert!(failure.program_result.is_err());
    assert_eq!(failure.resulting_accounts, a);
    rows.push(json!({"case":"three_asset_signed_cycle","intents":3,"assets":3,"external_dex_calls":0,"cu":result.compute_units_consumed,"legacy_wire_bytes_with_budget":wire_size(&ix,150_000),"signed_minima_met":true,"rollback_verified":true}));
    fs::write(root.join("artifacts/venue-matrix/crossing-proof.json"),serde_json::to_vec_pretty(&json!({"environment":"Mollusk; actual deployed SPL Token/Token-2022; captured NVDAx mint; synthetic funded signed-message fixtures","baseline":"sequential direct Raydium CLMM swaps; not unrestricted Jupiter/DFlow","rows":rows})).unwrap()).unwrap();
}

fn partial(
    root: &std::path::Path,
    m: &mollusk_svm::Mollusk,
    c: &Case,
    lower: &[(Instruction, bool)],
    im: Pubkey,
    om: Pubkey,
) {
    let fair_mode = std::env::args().any(|x| x == "--fair-execution");
    let out_dir = root.join(if fair_mode {
        "artifacts/fair-execution"
    } else {
        "artifacts/seven-gates"
    });
    fs::create_dir_all(&out_dir).unwrap();
    let mut rows = Vec::new();
    for usd in [100u64, 1000, 10_000, 50_000, 100_000] {
        let input = usd * 1_000_000;
        let cross_input = if fair_mode {
            input / 3 / 2 * 2
        } else {
            input / 3
        };
        let residual = input - cross_input;
        let mut small = lower[0].0.clone();
        small.data[8..16].copy_from_slice(&cross_input.to_le_bytes());
        let r = run(m, &small, &c.a);
        assert!(r.program_result.is_ok());
        let mut shares = amount(&r.resulting_accounts, &c.output) - START;
        let mut model = serde_json::Value::Null;
        if fair_mode {
            use skew_engine::fair::{self, Interval};
            let price = 2 * skew_engine::SCALE as u64;
            let band = fair::consensus(
                &[
                    Interval {
                        group: 1,
                        low: price,
                        high: price,
                    },
                    Interval {
                        group: 2,
                        low: price,
                        high: price,
                    },
                    Interval {
                        group: 3,
                        low: price * 2,
                        high: price * 2,
                    },
                ],
                1,
                100,
            )
            .unwrap();
            let cross = fair::cross(band, input, 1, cross_input / 2, cross_input).unwrap();
            shares = cross.base;
            assert_eq!(
                (
                    cross.quote,
                    cross.buyer_quote_residual,
                    cross.seller_base_residual
                ),
                (cross_input, residual, 0)
            );
            model = json!({"source":"synthetic independent-group fixture, not live fair value","low_q32":band.low,"high_q32":band.high,"cross_price_q32":cross.price_q32,"one_outlier_ignored":true,"integer_crossing_inside_band":true});
        }
        let mut a = c.a.clone();
        let buyer = participant(&mut a, WALLET, im, om, input, shares);
        let seller = participant(
            &mut a,
            Pubkey::new_from_array([121; 32]),
            om,
            im,
            shares,
            cross_input,
        );
        let mut ix = compile(&a, &[buyer, seller], c.slot);
        ix.data[0] = 5;
        let transfers = 12 + 2 * 32;
        ix.data[transfers + 2..transfers + 10].copy_from_slice(&cross_input.to_le_bytes());
        let mut direct = lower[0].0.clone();
        direct.accounts[3].pubkey = buyer.source;
        direct.accounts[4].pubkey = buyer.destination;
        let local = Case {
            name: "partial clearing residual".into(),
            a: a.clone(),
            input: buyer.source,
            output: buyer.destination,
            jup: direct.clone(),
            venue: c.venue,
            quote: json!({}),
            slot: c.slot,
            time: c.time,
        };
        let mut graph = local.graph(&[(direct, lower[0].1)], residual, 1).unwrap();
        let mut remap = Vec::new();
        for meta in &graph.accounts {
            let index = if let Some(i) = ix.accounts.iter().position(|m| m.pubkey == meta.pubkey) {
                ix.accounts[i].is_writable |= meta.is_writable;
                ix.accounts[i].is_signer |= meta.is_signer;
                i
            } else {
                let i = ix.accounts.len();
                ix.accounts.push(meta.clone());
                i
            };
            remap.push(index as u8);
        }
        assert_eq!(&remap[..2], &[0, 1]);
        assert!(ix.accounts.len() <= 64);
        for i in 36..42 {
            graph.data[i] = remap[graph.data[i] as usize];
        }
        let mut pos = 42;
        for _ in 0..graph.data[2] {
            graph.data[pos + 12] = remap[graph.data[pos + 12] as usize];
            let count = graph.data[pos + 13] as usize;
            for i in pos + 14..pos + 14 + count {
                graph.data[i] = remap[graph.data[i] as usize];
            }
            pos += 14 + count;
        }
        let graph_start = ix.data.len() + 2;
        ix.data
            .extend_from_slice(&(graph.data.len() as u16).to_le_bytes());
        ix.data.extend_from_slice(&graph.data);
        let result = run(m, &ix, &a);
        assert!(
            result.program_result.is_ok(),
            "{usd}: {:?}",
            result.program_result
        );
        assert_eq!(&result.return_data[..8], b"SKEWNET1");
        assert_eq!(
            START - amount(&result.resulting_accounts, &buyer.source),
            input
        );
        assert_eq!(
            START - amount(&result.resulting_accounts, &seller.source),
            shares
        );
        assert_eq!(
            amount(&result.resulting_accounts, &seller.destination) - START,
            cross_input
        );
        let received = amount(&result.resulting_accounts, &buyer.destination) - START;
        assert!(received >= shares);
        for p in [buyer, seller] {
            assert_eq!(
                dex::u64_at(&get(&result.resulting_accounts, &p.nonce).data, 40).unwrap(),
                1
            );
        }
        let mut faults = Vec::new();
        for name in [
            "late_min_out",
            "residual_mismatch",
            "missing_seller_signature",
            "nonce_replay",
            "foreign_owner_cpi",
        ] {
            let mut bad = ix.clone();
            let mut bank = a.clone();
            match name {
                "late_min_out" => bad.data[36..44].copy_from_slice(&(received + 1).to_le_bytes()),
                "residual_mismatch" => bad.data[graph_start + 12..graph_start + 20]
                    .copy_from_slice(&(residual + 1).to_le_bytes()),
                "missing_seller_signature" => {
                    bad.accounts
                        .iter_mut()
                        .find(|m| m.pubkey == seller.owner)
                        .unwrap()
                        .is_signer = false
                }
                "nonce_replay" => bank = result.resulting_accounts.clone(),
                "foreign_owner_cpi" => {
                    let idx = bad
                        .accounts
                        .iter()
                        .position(|m| m.pubkey == seller.source)
                        .unwrap() as u8;
                    bad.data[graph_start + 42 + 14 + 5] = idx;
                }
                _ => unreachable!(),
            }
            let rr = run(m, &bad, &bank);
            assert!(rr.program_result.is_err(), "{name}");
            assert_eq!(rr.resulting_accounts, bank, "{name}");
            faults.push(json!({"name":name,"rollback":true,"cu":rr.compute_units_consumed,"status":format!("{:?}",rr.program_result)}));
        }
        rows.push(json!({"fair_model":model,"usd":usd,"input":input,"crossed_input":cross_input,"external_input":residual,"buyer_output":received,"seller_output":cross_input,"cu":result.compute_units_consumed,"status":"PARTIAL_CROSS_AND_EXTERNAL_SETTLEMENT","faults":faults}));
        if usd == 50_000 {
            let external_floor =
                u64::try_from(u128::from(received - shares) * 9980 / 10_000).unwrap();
            let floor = shares.checked_add(external_floor).unwrap();
            ix.data[36..44].copy_from_slice(&floor.to_le_bytes());
            ix.data[graph_start + 20..graph_start + 28]
                .copy_from_slice(&external_floor.to_le_bytes());
            let bounded = run(m, &ix, &a);
            assert!(bounded.program_result.is_ok());
            assert_eq!(
                amount(&bounded.resulting_accounts, &buyer.destination) - START,
                received
            );
            let row = rows.last_mut().unwrap();
            row["private_fixture_buyer_min_out"] = json!(floor);
            row["private_fixture_external_min_out"] = json!(external_floor);
            row["private_fixture_external_slippage_bps"] = json!(20);
            let fixture = json!({"wallets":[buyer.owner.to_string(),seller.owner.to_string()],"instruction":{"programId":SETTLE.to_string(),"data":STANDARD.encode(&ix.data),"accounts":ix.accounts.iter().map(|a|json!({"pubkey":a.pubkey.to_string(),"isSigner":a.is_signer,"isWritable":a.is_writable})).collect::<Vec<_>>()},"accounts":a.iter().map(|(k,a)|json!({"pubkey":k.to_string(),"owner":a.owner.to_string(),"lamports":a.lamports,"executable":a.executable,"data":STANDARD.encode(&a.data)})).collect::<Vec<_>>()});
            fs::write(
                out_dir.join("partial-fixture.json"),
                serde_json::to_vec_pretty(&fixture).unwrap(),
            )
            .unwrap();
        }
    }
    fs::write(out_dir.join("partial-proof.json"),serde_json::to_vec_pretty(&json!({"scope":"two signing owners; one residual owner; actual captured Raydium CLMM SBF; no mainnet submission","rows":rows})).unwrap()).unwrap();
}
