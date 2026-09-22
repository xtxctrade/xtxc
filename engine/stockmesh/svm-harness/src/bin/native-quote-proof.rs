//! Differential native-quote conformance; no network or submitted transactions.
#[allow(dead_code)]
#[path = "../matrix.rs"]
mod matrix;
use matrix::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use skew_native_quoters::riptide::{Context, RiptideCurve};
use std::{fs, hint::black_box, path::PathBuf, time::Instant};
fn main() {
    let root = PathBuf::from("/srv/skew/stocklana-engine-20260912");
    let mut m = runtime(&root);
    let mut rows = Vec::new();
    for asset in ["NVDAx", "TSLAx", "SPYx", "QQQx"] {
        for side in ["buy", "sell"] {
            let c = Case::load(&root.join(format!(
                "artifacts/venue-matrix/cases/{asset}-{side}-riptide"
            )));
            m.sysvars.clock.slot = c.slot;
            m.sysvars.clock.unix_timestamp = c.time as i64;
            let jup = run(&m, &c.jup, &c.a);
            assert!(jup.program_result.is_ok());
            let direct = c.lower(&jup).unwrap();
            assert_eq!(direct.len(), 1);
            let (template, dir) = &direct[0];
            let bytes = &get(&c.a, &template.accounts[1].pubkey).data;
            let market = riptide_amm::Market::from_bytes(bytes).unwrap();
            // Explicit hypothesis, tested on every admitted amount below. The
            // execution-context policy is not inferred from a favourable quote.
            let context = Context {
                state_slot: c.slot,
                execution_slot: c.slot,
                state_sha256: Sha256::digest(bytes).into(),
                execution_fingerprint: Sha256::digest(&template.data).into(),
                penalty_per_million: market.arb_penalty_per_m,
            };
            let curve = RiptideCurve::decode(bytes, context).unwrap();
            let mut checked = 0;
            let mut rejected = 0;
            let mut false_accepts = Vec::new();
            let mut mismatches = Vec::new();
            let mut nanos = Vec::new();
            for i in 1u64..=128 {
                let amount = if side == "buy" {
                    i * 1_000_000_000
                } else {
                    i * 100_000_000
                };
                let mut ix = template.clone();
                ix.data[1..9].copy_from_slice(&amount.to_le_bytes());
                let r = run(&m, &ix, &c.a);
                let predicted = curve.quote(amount, *dir, context);
                if r.program_result.is_ok()
                    && START - amount_of(&r.resulting_accounts, &c.input) == amount
                {
                    let exact = amount_of(&r.resulting_accounts, &c.output) - START;
                    if predicted == Ok(exact) {
                        checked += 1;
                    } else {
                        mismatches.push(json!({"input":amount,"native":format!("{predicted:?}"),"actual":exact}));
                    }
                } else {
                    rejected += 1;
                    if let Ok(predicted_output) = predicted {
                        false_accepts.push(json!({"input":amount,"native":predicted_output,"sbf":format!("{:?}",r.program_result)}));
                    }
                }
            }
            for i in 0u64..10_000 {
                let amount = (1 + i % 128)
                    * if side == "buy" {
                        1_000_000_000
                    } else {
                        100_000_000
                    };
                let start = Instant::now();
                let _ = black_box(curve.quote(black_box(amount), *dir, black_box(context)));
                nanos.push(start.elapsed().as_nanos() as u64);
            }
            nanos.sort_unstable();
            let mut stale = context;
            stale.execution_slot += 1;
            assert!(curve.quote(1_000_000, *dir, stale).is_err());
            let row = json!({"case":c.name,"state_slot":c.slot,"penalty_hypothesis_per_million":context.penalty_per_million,"matched":checked,"svm_rejected_or_partial":rejected,"mismatches":mismatches,"false_accepts":false_accepts,
            "native_quote_p50_ns":nanos[5000],"native_quote_p99_ns":nanos[9900],"status":if mismatches.is_empty() && false_accepts.is_empty() && checked>0{"MATCHED_ON_ADMITTED_CORPUS"}else{"QUARANTINED"}});
            println!("{row}");
            rows.push(row);
            fs::write(root.join("artifacts/venue-matrix/native-quote-proof.json"),serde_json::to_vec_pretty(&json!({"scope":"Riptide SDK integer model; conditional execution penalty; captured market; not every venue or future state","rows":rows})).unwrap()).unwrap();
        }
    }
}
fn amount_of(a: &Accounts, k: &solana_pubkey::Pubkey) -> u64 {
    matrix::amount(a, k)
}
