//! Re-run the admitted captured DEX corpus through the actual host compiler.
//! This never contacts RPC, signs, submits, or overwrites the original evidence.
#[allow(dead_code)]
#[path = "../matrix.rs"]
mod matrix;
use matrix::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).expect("AWS repository path"));
    let output = std::env::var_os("SKEW_COMPILER_PROOF_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("artifacts/stockmesh-compiler-v7"));
    fs::create_dir_all(&output).unwrap();
    let sbf_dir = std::env::var_os("SKEW_SETTLE_SBF_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("artifacts/sbf-stockmesh-v5"));
    std::env::set_var("SKEW_SETTLE_SBF_DIR", &sbf_dir);
    let mut runtime = runtime(&root);
    let corpus = read(&root.join("artifacts/venue-matrix/proof.json"));
    let admitted = corpus["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["status"] == "DIRECT_AND_SBF_VERIFIED")
        .map(|row| row["case"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    let expected_venues = corpus["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["status"] == "DIRECT_AND_SBF_VERIFIED")
        .map(|row| row["venue"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    let excluded = corpus["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["status"] != "DIRECT_AND_SBF_VERIFIED")
        .map(|row| row["case"].clone())
        .collect::<Vec<_>>();
    let mut paths = fs::read_dir(root.join("artifacts/venue-matrix/cases"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.join("manifest.json").exists()
                && admitted.contains(path.file_name().unwrap().to_str().unwrap())
        })
        .collect::<Vec<_>>();
    paths.sort();
    let mut rows = Vec::new();
    for path in paths {
        let c = Case::load(&path);
        runtime.sysvars.clock.slot = c.slot;
        runtime.sysvars.clock.unix_timestamp = c.time as i64;
        runtime.sysvars.clock.epoch = stocklana_adapters::u64_at(
            &get(&c.a, &pk("SysvarC1ock11111111111111111111111111111111")).data,
            16,
        )
        .unwrap();
        let result = (|| -> Result<_, String> {
            let original = run(&runtime, &c.jup, &c.a);
            if original.program_result.is_err() {
                return Err(format!("captured source: {:?}", original.program_result));
            }
            let direct = c.lower(&original)?;
            let baseline = runtime.process_transaction_instructions(
                &direct.iter().map(|(ix, _)| ix.clone()).collect::<Vec<_>>(),
                &c.a,
            );
            if baseline.program_result.is_err() {
                return Err(format!("captured direct: {:?}", baseline.program_result));
            }
            let spent = amount(&c.a, &c.input)
                .checked_sub(amount(&baseline.resulting_accounts, &c.input))
                .ok_or("input delta")?;
            let received = amount(&baseline.resulting_accounts, &c.output)
                .checked_sub(amount(&c.a, &c.output))
                .ok_or("output delta")?;
            let ix = c.graph(&direct, spent, received)?;
            let execution = run(&runtime, &ix, &c.a);
            if execution.program_result.is_err() {
                return Err(format!("compiled SBF: {:?}", execution.program_result));
            }
            if amount(&execution.resulting_accounts, &c.input)
                != amount(&baseline.resulting_accounts, &c.input)
                || amount(&execution.resulting_accounts, &c.output)
                    != amount(&baseline.resulting_accounts, &c.output)
            {
                return Err("direct versus compiled raw delta mismatch".into());
            }
            let mut fail = ix.clone();
            fail.data[20..28].copy_from_slice(
                &received
                    .checked_add(1)
                    .ok_or("floor overflow")?
                    .to_le_bytes(),
            );
            let rejected = run(&runtime, &fail, &c.a);
            if rejected.program_result.is_ok() || rejected.resulting_accounts != c.a {
                return Err("minimum output rollback".into());
            }
            let replay = run(&runtime, &ix, &execution.resulting_accounts);
            if replay.program_result.is_ok()
                || replay.resulting_accounts != execution.resulting_accounts
            {
                return Err("nonce replay rollback".into());
            }
            Ok(
                json!({"case":c.name,"venue":c.venue as u8,"slot":c.slot,"inputAtoms":spent.to_string(),"outputAtoms":received.to_string(),"computeUnits":execution.compute_units_consumed,"accounts":ix.accounts.len(),"legs":direct.len(),"minimumOutputRollback":true,"nonceReplayRollback":true,"passed":true}),
            )
        })();
        let row = result.unwrap_or_else(
            |error| json!({"case":c.name,"venue":c.venue as u8,"passed":false,"error":error}),
        );
        println!("{row}");
        rows.push(row);
    }
    let passed = rows.iter().filter(|row| row["passed"] == true).count();
    let venues = rows
        .iter()
        .filter(|row| row["passed"] == true)
        .map(|row| row["venue"].as_u64().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    let complete = passed == rows.len()
        && rows.len() == admitted.len()
        && passed >= 63
        && venues.len() == expected_venues.len();
    let report = json!({"schema":"skew.typed-graph-compiler-proof/v1","environment":"AWS Mollusk; captured deployed ELFs/accounts; synthetic wallet balances; no network submission","compiler":"host/src/swap_wire.rs::compile_swap_graph","compilerSha256":format!("{:x}",Sha256::digest(fs::read(root.join("host/src/swap_wire.rs")).unwrap())),"settlementElfSha256":format!("{:x}",Sha256::digest(fs::read(sbf_dir.join("stocklana_settle.so")).unwrap())),"passed":complete,"caseCount":rows.len(),"excludedPreviouslyUnadmitted":excluded,"passedCount":passed,"typedVenues":venues.len(),"supportedAbiVenues":9,"rows":rows});
    fs::write(
        output.join("sbf-corpus.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    assert!(
        complete,
        "typed compiler must preserve every admitted captured case"
    );
}
