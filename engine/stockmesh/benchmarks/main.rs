//! Synthetic benchmarks. Run on AWS; never label these venues Jupiter/DFlow.
use skew_engine::clearing::{fold, FlowIntent, MAX_PIVOTS};
use skew_engine::compiler::{compile, Curve, Model, Snapshot};
use skew_engine::optimizer::tape::ResidualTape;
use skew_engine::optimizer::{search, search_reference, Allocation};
use skew_engine::reflow::{Intent, Limits};
use skew_engine::runtime::fixture::{book, rate, FixtureHost};
use skew_engine::runtime::{EdgePin, ExecutionHost};
use skew_engine::{Error, Result, WorkMeter, MAX_EDGES};
use std::fs::{self, File};
use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::time::Instant;

const UNIT: u64 = 1_000_000;

fn markets(seed: u64) -> Vec<Snapshot> {
    // Synthetic USDC has 6 decimals, synthetic stock 9. No live market data.
    let delta = seed * 701;
    let mut out = vec![
        book(
            10,
            &[
                (10_000 * UNIT, rate(1_000_000_000, 187_410_000 + delta)),
                (90_000 * UNIT, rate(1_000_000_000, 187_550_000 + delta)),
                (500_000 * UNIT, rate(1_000_000_000, 188_100_000 + delta)),
            ],
            100,
        ),
        book(
            11,
            &[(500_000 * UNIT, rate(1_000_000_000, 187_480_000))],
            100,
        ),
        book(
            12,
            &[(40_000 * UNIT, rate(1_000_000_000, 187_420_000 - delta))],
            100,
        ),
        book(
            13,
            &[(500_000 * UNIT, rate(1_000_000_000, 187_900_000))],
            100,
        ),
    ];
    out[2].expires_at_slot = 101;
    out[3].model = Model::ConstantProduct {
        reserve_in: 10_000_000 * UNIT,
        reserve_out: 53_400_000_000_000,
        fee_ppm: 100,
    };
    out
}

fn solve(venues: &[Snapshot], amount: u64, reference: bool) -> Result<(Allocation, u32)> {
    let mut meter = WorkMeter::new(1_000_000);
    let mut curves = [Curve::default(); MAX_EDGES];
    let mut mask = 0;
    for (i, s) in venues.iter().enumerate() {
        if !s.enabled || s.expires_at_slot < 100 || s.slot < 99 {
            continue;
        }
        curves[i] = compile(s, amount, &mut meter)?;
        mask |= 1 << i;
    }
    let r = if reference {
        search_reference(venues, &curves[..venues.len()], amount, mask, 4, &mut meter)?
    } else {
        search(venues, &curves[..venues.len()], amount, mask, 4, &mut meter)?
    };
    Ok((r.plan, meter.used))
}

fn static_execute(
    host: &FixtureHost,
    plan: Allocation,
    amount: u64,
    min_out: u64,
) -> Result<(u64, u8)> {
    let mut staged = *host;
    let start = staged.balances;
    for i in 0..staged.len {
        if plan.inputs[i] > 0 {
            staged.invoke(i, plan.inputs[i])?;
        }
    }
    if start.input - staged.balances.input != amount {
        return Err(Error::BalanceInvariant);
    }
    let out = staged.balances.output - start.output;
    if out < min_out {
        return Err(Error::MinOut);
    }
    Ok((out, staged.calls))
}

fn matrix(dir: &str) -> std::io::Result<()> {
    let mut csv = BufWriter::new(File::create(format!("{dir}/matrix.csv"))?);
    writeln!(csv, "provenance,scenario,seed,usdc,algorithm,quoted_output_atoms,actual_output_atoms,success,error,latency_ns,work_units,legs,reflows,tape_builds,compute_units")?;
    for scenario in [
        "normal",
        "high_volatility",
        "stale_quote",
        "venue_unavailable",
        "liquidity_shift",
        "rfq_expiration",
        "partial_fill",
        "fatal_cpi",
    ] {
        for seed in 0..16 {
            for dollars in [100, 1000, 10_000, 50_000, 100_000, 500_000] {
                let amount = dollars * UNIT;
                let quoted = markets(seed);
                let (initial, _) = solve(&quoted, amount, false).unwrap();
                let min_out = initial.output * 9950 / 10_000;
                let mut live = quoted.clone();
                match scenario {
                    "high_volatility" => {
                        for s in &mut live {
                            if let Model::Book { levels, len } = &mut s.model {
                                for l in &mut levels[..*len as usize] {
                                    l.marginal_q32 = l.marginal_q32 * (9975 + seed % 51) / 10_000;
                                }
                            }
                        }
                    }
                    "stale_quote" => live[0].slot = 95,
                    "venue_unavailable" => live[0].enabled = false,
                    "liquidity_shift" => {
                        if let Model::Book { levels, len } = &mut live[0].model {
                            for l in &mut levels[..*len as usize] {
                                l.marginal_q32 = l.marginal_q32 * 9950 / 10_000;
                            }
                        }
                        live[0].generation += 1;
                    }
                    "rfq_expiration" => live[2].expires_at_slot = 99,
                    _ => {}
                }
                let mut host = FixtureHost::new(&live, amount, 100).unwrap();
                if scenario == "partial_fill" {
                    host.settlement_caps[0] = 5_000 * UNIT;
                }
                if scenario == "fatal_cpi" {
                    host.fatal = 1;
                }
                let pins: Vec<EdgePin> = quoted.iter().copied().map(Into::into).collect();
                for algorithm in [
                    "frozen_split",
                    "fresh_split_reference",
                    "best_single",
                    "skew_reflow_tape",
                ] {
                    let started = Instant::now();
                    let (mut work, mut reflows, mut builds) = (0, 0, 0);
                    let mut expected = initial.output;
                    let result: Result<(u64, u8)> = match algorithm {
                        "frozen_split" => static_execute(&host, initial, amount, min_out),
                        "fresh_split_reference" => solve(&live, amount, true).and_then(|(p, w)| {
                            work = w;
                            expected = p.output;
                            static_execute(&host, p, amount, min_out)
                        }),
                        "best_single" => {
                            let mut best = None;
                            for (i, s) in live.iter().enumerate() {
                                if !s.enabled
                                    || s.expires_at_slot < 100
                                    || s.slot < 99
                                    || s.capacity < amount
                                {
                                    continue;
                                }
                                if let Ok(out) = s.exact_quote(amount) {
                                    if best.is_none_or(|(_, b)| out > b) {
                                        best = Some((i, out));
                                    }
                                }
                            }
                            best.ok_or(Error::Capacity).and_then(|(i, out)| {
                                expected = out;
                                let mut p = Allocation {
                                    output: out,
                                    mask: 1 << i,
                                    ..Allocation::default()
                                };
                                p.inputs[i] = amount;
                                static_execute(&host, p, amount, min_out)
                            })
                        }
                        _ => {
                            let mut h = host;
                            h.atomic(
                                &pins,
                                Intent {
                                    input_mint: [1; 32],
                                    output_mint: [2; 32],
                                    amount_in: amount,
                                    min_out,
                                    expires_at_slot: 101,
                                    max_snapshot_age: 1,
                                },
                                Limits::default(),
                            )
                            .map(|r| {
                                work = r.work_units;
                                reflows = r.reflows;
                                builds = r.tape_builds;
                                expected = r.initial.plan.output;
                                (r.output_received, r.legs)
                            })
                        }
                    };
                    let latency = started.elapsed().as_nanos();
                    let (out, success, error, legs) = match result {
                        Ok((out, legs)) => (out.to_string(), true, String::new(), legs),
                        Err(error) => (String::new(), false, format!("{error:?}"), 0),
                    };
                    writeln!(csv, "synthetic_native,{scenario},{seed},{dollars},{algorithm},{expected},{out},{success},{error},{latency},{work},{legs},{reflows},{builds},")?;
                }
            }
        }
    }
    Ok(())
}

fn ablation(dir: &str) -> std::io::Result<()> {
    let mut csv = BufWriter::new(File::create(format!("{dir}/tape_ablation.csv"))?);
    writeln!(
        csv,
        "bands,amount,iterations,reference_ns,tape_ns,reference_work,tape_work,tape_build_ns"
    )?;
    let snapshots: Vec<_> = (0..8)
        .map(|i| {
            let levels: Vec<_> = (0..16)
                .map(|j| (10_000 * UNIT, rate(10_000 - j * 8 - i, 2000)))
                .collect();
            book(10 + i as u8, &levels, 100)
        })
        .collect();
    let curves: Vec<_> = snapshots
        .iter()
        .map(|s| compile(s, 1_000_000 * UNIT, &mut WorkMeter::new(10000)).unwrap())
        .collect();
    let t = Instant::now();
    let tape = ResidualTape::build(&curves, 255, &mut WorkMeter::new(10000)).unwrap();
    let build_ns = t.elapsed().as_nanos();
    for amount in [100, 10_000, 100_000, 500_000, 1_000_000] {
        let n = 5000;
        let q = amount * UNIT;
        let mut slow = WorkMeter::new(u32::MAX);
        let mut fast = WorkMeter::new(u32::MAX);
        let t = Instant::now();
        let mut last = Allocation::default();
        for _ in 0..n {
            last = black_box(
                skew_engine::optimizer::waterfill(black_box(&curves), black_box(q), 255, &mut slow)
                    .unwrap(),
            );
        }
        let slow_ns = t.elapsed().as_nanos();
        let t = Instant::now();
        for _ in 0..n {
            assert_eq!(
                last,
                black_box(tape.allocate(black_box(q), 255, &mut fast).unwrap())
            );
        }
        let fast_ns = t.elapsed().as_nanos();
        writeln!(
            csv,
            "128,{amount},{n},{slow_ns},{fast_ns},{},{},{build_ns}",
            slow.used, fast.used
        )?;
    }
    Ok(())
}

fn clearing(dir: &str) -> std::io::Result<()> {
    let mut csv = BufWriter::new(File::create(format!("{dir}/clearing.csv"))?);
    writeln!(csv, "provenance,intents,seed,requested_value,internal_value,residual_value,netting_bps,pivots,reverse_pivots,optimal_at_fixed_prices,latency_ns,work_units")?;
    for n in [4, 8, 16, 32] {
        for seed in 0..100u64 {
            let mut state = seed + 7743;
            let orders: Vec<_> = (0..n)
                .map(|i| {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let a = ((state >> 32) % 4) as u8;
                    let b = (a + 1 + ((state >> 40) % 3) as u8) % 4;
                    let amount = 100 + (state >> 48) % 900;
                    FlowIntent {
                        owner: [i as u8 + 1; 32],
                        nonce: 1,
                        sell: a,
                        buy: b,
                        amount,
                        min_out: amount,
                        expires_at_slot: 100,
                    }
                })
                .collect();
            let mut meter = WorkMeter::new(1_000_000);
            let t = Instant::now();
            let r = fold(&orders, &[1, 1, 1, 1], 100, MAX_PIVOTS, &mut meter).unwrap();
            let latency = t.elapsed().as_nanos();
            writeln!(
                csv,
                "synthetic_equal_value_atoms,{n},{seed},{},{},{},{},{},{},{},{latency},{}",
                r.requested_value,
                r.internally_cleared_value,
                r.requested_value - r.internally_cleared_value,
                r.internally_cleared_value * 10000 / r.requested_value,
                r.pivots,
                r.reverse_edge_pivots,
                r.optimal_at_fixed_prices,
                meter.used
            )?;
        }
    }
    Ok(())
}

fn load(dir: &str) -> std::io::Result<()> {
    use skew_engine::runtime::admission::{Admission, Request};
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    };
    let lane = Arc::new(Mutex::new(Admission::default()));
    let admitted = Arc::new(AtomicU64::new(0));
    let completed = Arc::new(AtomicU64::new(0));
    let rejected = Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    let threads: Vec<_> = (0..8)
        .map(|owner| {
            let lane = lane.clone();
            let admitted = admitted.clone();
            let completed = completed.clone();
            let rejected = rejected.clone();
            std::thread::spawn(move || {
                for nonce in 0..10_000 {
                    let mut queue = lane.lock().unwrap();
                    let request = Request {
                        owner: [owner + 1; 32],
                        nonce,
                        read_resources: 0,
                        write_resources: 1 << (nonce % 8),
                        deadline: 100,
                    };
                    if queue.admit(request, 1).is_ok() {
                        admitted.fetch_add(1, Ordering::Relaxed);
                    } else {
                        rejected.fetch_add(1, Ordering::Relaxed);
                    }
                    if let Some((ticket, _)) = queue.dispatch(1) {
                        queue.complete(ticket).unwrap();
                        completed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let elapsed = start.elapsed().as_nanos();
    // Fixed clock deliberately saturates replay tombstones: backpressure only.
    fs::write(format!("{dir}/admission_load.json"), format!(
        "{{\"provenance\":\"native_fixed_clock_saturation\",\"threads\":8,\"offered\":80000,\"admitted\":{},\"completed\":{},\"rejected\":{},\"elapsed_ns\":{},\"landed_transactions\":null}}\n",
        admitted.load(Ordering::Relaxed), completed.load(Ordering::Relaxed), rejected.load(Ordering::Relaxed), elapsed))?;
    Ok(())
}

fn main() -> std::io::Result<()> {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "benchmarks/results".to_string());
    fs::create_dir_all(&dir)?;
    matrix(&dir)?;
    ablation(&dir)?;
    clearing(&dir)?;
    load(&dir)?;
    fs::write(format!("{dir}/provenance.json"), "{\"kind\":\"synthetic_native\",\"network_submission\":false,\"live_venue_adapters\":false,\"solana_compute_units\":null,\"jupiter_comparison\":null,\"dflow_comparison\":null}\n")?;
    println!("Wrote deterministic fixtures and native measurements to {dir}");
    Ok(())
}
