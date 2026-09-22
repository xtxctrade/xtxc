//! Warm native quote timing. Run only on AWS; no SVM/RPC latency claim.
use sha2::{Digest, Sha256};
use skew_native_quoters::riptide::{Context, RiptideCurve};
use std::{fs, hint::black_box, time::Instant};
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let bytes = fs::read(&args[1]).unwrap();
    let slot: u64 = args[2].parse().unwrap();
    let penalty: u32 = args[3].parse().unwrap();
    let context = Context {
        state_slot: slot,
        execution_slot: slot,
        state_sha256: Sha256::digest(&bytes).into(),
        execution_fingerprint: Sha256::digest(
            b"direct Riptide; B to A; no partial fill; guarded output",
        )
        .into(),
        penalty_per_million: penalty,
    };
    let curve = RiptideCurve::decode(&bytes, context).unwrap();
    for i in 0..10_000u64 {
        black_box(curve.quote(black_box(100_000_000 + i % 1024), false, black_box(context)))
            .unwrap();
    }
    let count = 100_000;
    let mut times = Vec::with_capacity(count);
    let mut output = 0;
    let total = Instant::now();
    for i in 0..count as u64 {
        let start = Instant::now();
        let quoted =
            black_box(curve.quote(black_box(100_000_000 + i % 1024), false, black_box(context)));
        times.push(start.elapsed().as_nanos() as u64);
        output ^= quoted.unwrap();
    }
    let elapsed = total.elapsed().as_nanos();
    times.sort_unstable();
    println!("{{\"scope\":\"warm decoded Riptide native quote only; all quotes successful\",\"iterations\":{count},\"p50_ns\":{},\"p99_ns\":{},\"max_ns\":{},\"elapsed_ns\":{elapsed},\"checksum\":{output}}}", times[count/2],times[count*99/100],times[count-1]);
}
