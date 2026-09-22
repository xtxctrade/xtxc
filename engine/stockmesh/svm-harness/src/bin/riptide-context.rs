//! Diagnose transaction-context-sensitive prices without changing captured state.
#[allow(dead_code)]
#[path = "../matrix.rs"]
mod matrix;
use matrix::*;
use serde_json::json;
use std::{fs, path::PathBuf};
fn main() {
    let root = PathBuf::from("/srv/skew/stocklana-engine-20260912");
    let mut m = runtime(&root);
    let c = Case::load(&root.join("artifacts/venue-matrix/cases/NVDAx-buy-riptide"));
    m.sysvars.clock.slot = c.slot;
    m.sysvars.clock.unix_timestamp = c.time as i64;
    let j = run(&m, &c.jup, &c.a);
    assert!(j.program_result.is_ok());
    let direct = c.lower(&j).unwrap();
    let mut rows = vec![
        json!({"context":"jupiter_original","output":amount(&j.resulting_accounts,&c.output)-START,"cu":j.compute_units_consumed}),
    ];
    for partial in [0, 1] {
        let mut ix = direct[0].0.clone();
        ix.data[11] = partial;
        let r = run(&m, &ix, &c.a);
        let mut row = json!({"partial":partial,"status":format!("{:?}",r.program_result),"cu":r.compute_units_consumed});
        if r.program_result.is_ok() {
            row["output"] = json!(amount(&r.resulting_accounts, &c.output) - START);
            row["input"] = json!(START - amount(&r.resulting_accounts, &c.input));
        }
        rows.push(row);
    }
    let report = json!({"rows":rows});
    println!("{report}");
    fs::write(
        root.join("artifacts/competition/riptide-context.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
}
