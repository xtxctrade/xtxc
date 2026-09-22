//! Same-source, same-toolchain baseline/candidate microbenchmark. No RPC or CU.
use skew_engine::compiler::{compile, Curve, Model, Snapshot};
use skew_engine::optimizer::tape::ResidualTape;
use skew_engine::optimizer::{search, search_tape};
use skew_engine::runtime::fixture::{book, rate};
use skew_engine::{WorkMeter, MAX_ATOMS, MAX_EDGES};
use std::hint::black_box;
use std::time::Instant;

fn meter() -> WorkMeter {
    WorkMeter::new(10_000_000)
}

fn books(n: usize, bands: usize) -> Vec<Snapshot> {
    (0..n)
        .map(|i| {
            let levels: Vec<_> = (0..bands)
                .map(|j| {
                    (
                        100 + i as u64 * 7,
                        rate(10_000 - j as u64 * 100 - i as u64, 700),
                    )
                })
                .collect();
            book(10 + i as u8, &levels, 100)
        })
        .collect()
}

fn compiled(s: &[Snapshot], amount: u64) -> Vec<Curve> {
    s.iter()
        .map(|v| compile(v, amount, &mut meter()).unwrap())
        .collect()
}

fn measure<F: FnMut(usize) -> u32>(name: &str, mut f: F) {
    for i in 0..256 {
        black_box(f(i));
    }
    let mut times = Vec::with_capacity(2048);
    let mut work = 0u64;
    for i in 0..2048 {
        let start = Instant::now();
        work += u64::from(black_box(f(black_box(i))));
        times.push(start.elapsed().as_nanos());
    }
    times.sort_unstable();
    println!(
        "{name},{},{},{},{},{},{}",
        times[1024],
        times[1945],
        times[2027],
        times[0],
        times[2047],
        work / 2048
    );
}

fn benchmark() {
    println!("case,p50_ns,p95_ns,p99_ns,min_ns,max_ns,work_mean");
    let dense = books(8, 16);
    let small = books(2, 4);
    let dense_c = compiled(&dense, 6000);
    let small_c = compiled(&small, 600);
    let dense_t = ResidualTape::build(&dense_c, 255, &mut meter()).unwrap();
    let small_t = ResidualTape::build(&small_c, 3, &mut meter()).unwrap();
    let mut cp = dense.clone();
    for (i, s) in cp.iter_mut().enumerate() {
        s.capacity = 20_000;
        s.model = Model::ConstantProduct {
            reserve_in: 90_000 + i as u64 * 317,
            reserve_out: 140_000,
            fee_ppm: 3000,
        };
    }
    measure("cp_compile", |i| {
        let mut m = meter();
        black_box(
            compile(
                black_box(&cp[i % 8]),
                black_box(6000 + i as u64 % 61),
                &mut m,
            )
            .unwrap(),
        );
        m.used
    });
    measure("book_compile", |i| {
        let mut m = meter();
        black_box(compile(black_box(&dense[i % 8]), black_box(6000), &mut m).unwrap());
        m.used
    });
    measure("tape_build_8x16", |_| {
        let mut m = meter();
        black_box(ResidualTape::build(black_box(&dense_c), black_box(255), &mut m).unwrap());
        m.used
    });
    measure("allocate_8x16", |i| {
        let mut m = meter();
        black_box(
            dense_t
                .allocate(black_box(6000 + i as u64 % 61), black_box(255), &mut m)
                .unwrap(),
        );
        m.used
    });
    measure("search_warm_8x16", |i| {
        let mut m = meter();
        black_box(
            search_tape(
                black_box(&dense),
                black_box(&dense_c),
                black_box(&dense_t),
                black_box(6000 + i as u64 % 61),
                255,
                4,
                &mut m,
            )
            .unwrap(),
        );
        m.used
    });
    measure("search_cold_8x16", |i| {
        let mut m = meter();
        black_box(
            search(
                black_box(&dense),
                black_box(&dense_c),
                black_box(6000 + i as u64 % 61),
                255,
                4,
                &mut m,
            )
            .unwrap(),
        );
        m.used
    });
    measure("search_warm_2x4", |i| {
        let mut m = meter();
        black_box(
            search_tape(
                black_box(&small),
                black_box(&small_c),
                black_box(&small_t),
                black_box(600 + i as u64 % 61),
                3,
                2,
                &mut m,
            )
            .unwrap(),
        );
        m.used
    });
    measure("search_warm_sparse", |i| {
        let mut m = meter();
        black_box(
            search_tape(
                black_box(&dense),
                black_box(&dense_c),
                black_box(&dense_t),
                black_box(6000 + i as u64 % 61),
                0x55,
                4,
                &mut m,
            )
            .unwrap(),
        );
        m.used
    });
    measure("cp8_compile_and_search", |i| {
        let amount = black_box(6000 + i as u64 % 61);
        let mut m = meter();
        let mut c = [Curve::default(); MAX_EDGES];
        for (j, s) in black_box(&cp).iter().enumerate() {
            c[j] = compile(s, amount, &mut m).unwrap();
        }
        black_box(search(black_box(&cp), black_box(&c), amount, 255, 4, &mut m).unwrap());
        m.used
    });
    // Small/common books matter too: a dense-book win must not hide a
    // regression on one venue or one level.
    for (edges, bands) in [(1usize, 1usize), (1, 16), (2, 1), (2, 16), (4, 16), (8, 1)] {
        let s = books(edges, bands);
        let amount = (edges.min(4) * bands * 55) as u64;
        let c = compiled(&s, amount * 2);
        let mask = ((1u16 << edges) - 1) as u8;
        let t = ResidualTape::build(&c, mask, &mut meter()).unwrap();
        measure(&format!("build_{edges}x{bands}"), |_| {
            let mut m = meter();
            black_box(ResidualTape::build(black_box(&c), black_box(mask), &mut m).unwrap());
            m.used
        });
        measure(&format!("warm_{edges}x{bands}"), |_| {
            let mut m = meter();
            black_box(
                search_tape(
                    black_box(&s),
                    black_box(&c),
                    black_box(&t),
                    black_box(amount),
                    mask,
                    edges.min(4),
                    &mut m,
                )
                .unwrap(),
            );
            m.used
        });
    }
    eprintln!("ResidualTape_bytes={}", std::mem::size_of::<ResidualTape>());
}

// Every line includes the full curve / allocation / certificate, not just an
// economic total that could hide a changed tie-break or chosen provider.
fn corpus() {
    for r in [1, 17, 1000, 10_000_000, MAX_ATOMS] {
        for y in [1, 9, 1000, 91_234, MAX_ATOMS] {
            for fee in [0, 1, 3000, 100_000, 999_999] {
                for cap in [1, 2, 3, 15, 16, 17, 1000, 10_000, MAX_ATOMS] {
                    let mut s = books(1, 1)[0];
                    s.capacity = cap;
                    s.model = Model::ConstantProduct {
                        reserve_in: r,
                        reserve_out: y,
                        fee_ppm: fee,
                    };
                    println!(
                        "CP {r} {y} {fee} {cap} {:?}",
                        compile(&s, cap, &mut meter())
                    );
                }
            }
        }
    }
    for seed in 0u64..32 {
        let mut s = books(8, 16);
        for (i, v) in s.iter_mut().enumerate() {
            if (i as u64 + seed) % 3 == 0 {
                v.model = Model::ConstantProduct {
                    reserve_in: 10_000 + seed * 37,
                    reserve_out: 14_000 + i as u64 * 71,
                    fee_ppm: (seed as u32 * 317) % 10_000,
                };
            }
            if seed % 4 == 0 {
                v.writable_resources = 1 << (i / 2);
            }
        }
        let c = compiled(&s, 10_000);
        let tape = ResidualTape::build(&c, 255, &mut meter()).unwrap();
        for mask in 1..=255 {
            for amount in [1, 107, 1600, 6000, 16_000] {
                println!(
                    "A {seed} {mask} {amount} {:?}",
                    tape.allocate(amount, mask, &mut meter())
                );
                for legs in [1, 2, 4] {
                    println!(
                        "S {seed} {mask} {amount} {legs} {:?}",
                        search_tape(&s, &c, &tape, amount, mask, legs, &mut meter())
                    );
                }
            }
        }
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--corpus") {
        corpus();
    } else {
        benchmark();
    }
}
