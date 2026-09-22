#[allow(dead_code)]
#[path = "../matrix.rs"]
mod matrix;
#[path = "../native_bridge.rs"]
mod native_bridge;
use matrix::*;
use native_bridge::Curve;
use serde_json::json;
use std::{fs, hint::black_box, path::PathBuf, time::Instant};
use stocklana_adapters::{self as dex, graph::Venue};
fn main() {
    let root = PathBuf::from("/srv/skew/stocklana-engine-20260912");
    let mut m = runtime(&root);
    m.compute_budget.compute_unit_limit = 1_400_000;
    let mut rows = Vec::new();
    let mut mismatches = 0;
    let mut resource_rejections = 0;
    let mut dirs: Vec<_> = fs::read_dir(root.join("artifacts/venue-matrix/cases"))
        .unwrap()
        .map(|x| x.unwrap().path())
        .collect();
    dirs.sort();
    for path in dirs {
        if !path.join("manifest.json").exists() {
            continue;
        }
        let mut c = Case::load(&path);
        if ![
            Venue::OrcaWhirlpool,
            Venue::RaydiumClmm,
            Venue::ByrealClmm,
            Venue::RaydiumAmmV4,
            Venue::RaydiumCpmm,
            Venue::Phoenix,
            Venue::MeteoraDlmm,
            Venue::MeteoraDammV2,
        ]
        .contains(&c.venue)
        {
            continue;
        }
        if std::env::args().any(|s| s == "--orca") && c.venue != Venue::OrcaWhirlpool {
            continue;
        }
        m.sysvars.clock.slot = c.slot;
        m.sysvars.clock.unix_timestamp = c.time as i64;
        let clock = &get(&c.a, &pk("SysvarC1ock11111111111111111111111111111111")).data;
        m.sysvars.clock.epoch = dex::u64_at(clock, 16).unwrap();
        let j = run(&m, &c.jup, &c.a);
        if !j.program_result.is_ok() {
            continue;
        }
        let Ok(legs) = c.lower(&j) else {
            continue;
        };
        let times = if std::env::args().any(|s| s == "--orca-time") {
            vec![0u64, 30, 31, 600, 601, 3600, 3601]
        } else {
            vec![0]
        };
        for offset in times {
            m.sysvars.clock.unix_timestamp = (c.time + offset) as i64;
            let clock_key = pk("SysvarC1ock11111111111111111111111111111111");
            c.a.iter_mut().find(|a| a.0 == clock_key).unwrap().1.data[32..40]
                .copy_from_slice(&(c.time + offset).to_le_bytes());
            for (edge, (ix, dir)) in legs.iter().enumerate() {
                let compiled = Curve::decode(
                    &c.a,
                    c.venue,
                    ix,
                    *dir,
                    c.slot,
                    c.time + offset,
                    m.sysvars.clock.epoch,
                );
                let curve = match compiled {
                    Ok(x) => x,
                    Err(e) => {
                        rows.push(json!({"case":c.name,"edge":edge,"decode":format!("{e:?}")}));
                        continue;
                    }
                };
                let q = if c.venue == Venue::Phoenix {
                    c.quote["inAmount"]
                        .as_str()
                        .unwrap()
                        .parse::<u64>()
                        .unwrap()
                } else {
                    dex::u64_at(&ix.data, if c.venue == Venue::RaydiumAmmV4 { 1 } else { 8 })
                        .unwrap()
                };
                let mut row = json!({"case":c.name,"edge":edge,"time_offset":offset,"decode":"NATIVE_PRICE_PROPOSAL","checked":0,"success_equal":0,"native_reject":0,"native_reject_sbf_success":0,"resource_rejections":[],"mismatches":[]});
                let (_, _, src, dst) = c.venue.bindings(*dir);
                for i in 1..=96u64 {
                    let qty = if i <= 48 {
                        (q as u128 * i as u128 / 12).max(1) as u64
                    } else {
                        ((q as u128)
                            * (10u128.pow(((i - 49) / 8) as u32))
                            * (1 + (i - 49) % 8) as u128)
                            .min(dex::MAX_INPUT as u128) as u64
                    };
                    let native = curve.quote(qty);
                    let mut call = ix.clone();
                    if c.venue == Venue::Phoenix {
                        let h = dex::PhoenixHeader::decode(&get(&c.a, &ix.accounts[2].pubkey).data)
                            .unwrap();
                        let Ok(b) = dex::phoenix_ioc(&h, *dir, qty, 16, c.slot + 100) else {
                            continue;
                        };
                        call.data = b[..dex::PHOENIX_IOC_LEN].to_vec();
                    } else {
                        let mut b = [0; 80];
                        let n = c.venue.swap_data(qty, *dir, &mut b).unwrap();
                        call.data = b[..n].to_vec();
                    }
                    let result = run(&m, &call, &c.a);
                    row["checked"] = json!(row["checked"].as_u64().unwrap() + 1);
                    match native {
                        Ok(out) => {
                            let actual = if result.program_result.is_ok() {
                                Some((
                                    START
                                        - amount(
                                            &result.resulting_accounts,
                                            &ix.accounts[src].pubkey,
                                        ),
                                    amount(&result.resulting_accounts, &ix.accounts[dst].pubkey)
                                        - START,
                                ))
                            } else {
                                None
                            };
                            if actual == Some((qty, out)) {
                                row["success_equal"] =
                                    json!(row["success_equal"].as_u64().unwrap() + 1);
                            } else if result.program_result.is_err()
                                && result.compute_units_consumed == 1_400_000
                            {
                                resource_rejections += 1;
                                row["resource_rejections"].as_array_mut().unwrap().push(json!({"input":qty,"native_price_proposal":out,"cu":result.compute_units_consumed,"execution_admitted":false,"status":format!("{:?}",result.program_result)}));
                            } else {
                                mismatches += 1;
                                row["mismatches"].as_array_mut().unwrap().push(json!({"input":qty,"native":out,"actual":actual,"cu":result.compute_units_consumed,"status":format!("{:?}",result.program_result)}));
                            }
                        }
                        Err(_) => {
                            row["native_reject"] =
                                json!(row["native_reject"].as_u64().unwrap() + 1);
                            if result.program_result.is_ok() {
                                row["native_reject_sbf_success"] =
                                    json!(row["native_reject_sbf_success"].as_u64().unwrap() + 1);
                            }
                        }
                    }
                }
                let mut times = Vec::with_capacity(10000);
                for i in 0..10000 {
                    let qty = q.saturating_add(i % 17);
                    let t = Instant::now();
                    black_box(curve.quote(black_box(qty))).ok();
                    times.push(t.elapsed().as_nanos() as u64);
                }
                times.sort_unstable();
                row["quote_ns"] = json!({"p50":times[5000],"p99":times[9900],"max":times[9999]});
                println!("{}", row);
                rows.push(row);
            }
        }
    }
    let out = root.join(if std::env::args().any(|s| s == "--fair-execution") {
        "artifacts/fair-execution"
    } else if std::env::args().any(|s| s == "--seven-gates") {
        "artifacts/seven-gates"
    } else if std::env::args().any(|s| s == "--expansion") {
        "artifacts/native-expansion"
    } else {
        "artifacts/native-search"
    });
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join(if std::env::args().any(|s|s=="--orca-time") {"orca-time-proof.json"} else if std::env::args().any(|s|s=="--orca") {"orca-kernel-proof.json"} else {"kernel-proof.json"}),serde_json::to_vec_pretty(&json!({"scope":"native price proposals versus deployed SBF, captured states; native pricing is not a compute-budget admission gate","rows":rows,"output_or_semantic_mismatches":mismatches,"native_predictions_requiring_resource_rejection":resource_rejections})).unwrap()).unwrap();
    assert_eq!(mismatches, 0);
}
