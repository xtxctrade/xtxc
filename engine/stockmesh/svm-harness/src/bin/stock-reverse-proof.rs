//! Execute every admitted captured top-five xStock sell case through the
//! ProductPolicy-guarded reverse ABI in the actual SBF program. This proof uses
//! captured deployed venue programs/accounts and synthetic wallet balances; it
//! never signs or submits a network transaction.
#[allow(dead_code)]
#[path = "../matrix.rs"]
mod matrix;

use matrix::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use std::{collections::BTreeSet, fs, path::PathBuf};
use stocklana_adapters as dex;

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).expect("AWS repository path"));
    let output = std::env::var_os("SKEW_REVERSE_PROOF_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("artifacts/stockmesh-reverse-v45"));
    fs::create_dir_all(&output).unwrap();
    let sbf_dir = std::env::var_os("SKEW_SETTLE_SBF_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("artifacts/sbf"));
    std::env::set_var("SKEW_SETTLE_SBF_DIR", &sbf_dir);

    let corpus = read(&root.join("artifacts/venue-matrix/proof.json"));
    let admitted = corpus["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["status"] == "DIRECT_AND_SBF_VERIFIED")
        .filter_map(|row| row["case"].as_str())
        .collect::<BTreeSet<_>>();
    let symbols = ["NVDAx", "TSLAx", "SPYx", "QQQx", "COINx"];
    let mut paths = fs::read_dir(root.join("artifacts/venue-matrix/cases"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            path.join("manifest.json").exists()
                && admitted.contains(name)
                && name.contains("-sell-")
                && symbols.iter().any(|symbol| name.starts_with(symbol))
        })
        .collect::<Vec<_>>();
    paths.sort();

    let mut runtime = runtime(&root);
    let mut rows = Vec::with_capacity(paths.len());
    let mut proved_symbols = BTreeSet::new();
    let mut proved_venues = BTreeSet::new();
    for path in paths {
        let c = Case::load(&path);
        runtime.sysvars.clock.slot = c.slot;
        runtime.sysvars.clock.unix_timestamp = c.time as i64;
        runtime.sysvars.clock.epoch = dex::u64_at(
            &get(&c.a, &pk("SysvarC1ock11111111111111111111111111111111")).data,
            16,
        )
        .unwrap();

        let source = run(&runtime, &c.jup, &c.a);
        assert!(source.program_result.is_ok(), "{} source", c.name);
        let direct = c.lower(&source).unwrap();
        let baseline = runtime.process_transaction_instructions(
            &direct
                .iter()
                .map(|(instruction, _)| instruction.clone())
                .collect::<Vec<_>>(),
            &c.a,
        );
        assert!(baseline.program_result.is_ok(), "{} direct", c.name);
        let input = amount(&c.a, &c.input)
            .checked_sub(amount(&baseline.resulting_accounts, &c.input))
            .expect("sell input delta");
        let output_atoms = amount(&baseline.resulting_accounts, &c.output)
            .checked_sub(amount(&c.a, &c.output))
            .expect("sell output delta");
        assert!(input > 0 && output_atoms > 0, "{} deltas", c.name);

        let product_mint = dex::key(&get(&c.a, &c.input).data, 0).unwrap();
        let cash_mint = dex::key(&get(&c.a, &c.output).data, 0).unwrap();
        let authority = Pubkey::new_from_array([122; 32]);
        let instrument = [31; 32];
        let issuer = [32; 32];
        let rights = [33; 32];
        let (policy, _) = Pubkey::find_program_address(
            &[
                b"stock2",
                authority.as_ref(),
                &instrument,
                &cash_mint,
                &product_mint,
                &rights,
            ],
            &SETTLE,
        );
        let mut bank = c.a.clone();
        put(
            &mut bank,
            authority,
            Account {
                lamports: 1_000_000_000,
                ..Account::default()
            },
        );
        put(&mut bank, policy, Account::default());
        put(
            &mut bank,
            Pubkey::default(),
            Account {
                lamports: 1,
                owner: pk("NativeLoader1111111111111111111111111111111"),
                executable: true,
                ..Account::default()
            },
        );
        let mut publish_data = vec![19];
        for value in [instrument, issuer, cash_mint, product_mint, rights] {
            publish_data.extend_from_slice(&value);
        }
        publish_data.extend_from_slice(&1u64.to_le_bytes());
        publish_data.extend_from_slice(&(c.slot + 200).to_le_bytes());
        publish_data.push(17);
        let publish = Instruction {
            program_id: SETTLE,
            accounts: vec![
                AccountMeta::new(authority, true),
                AccountMeta::new(policy, false),
                AccountMeta::new_readonly(Pubkey::default(), false),
            ],
            data: publish_data,
        };
        let initialized = run(&runtime, &publish, &bank);
        assert!(initialized.program_result.is_ok(), "{} policy", c.name);

        let mut graph = c.graph(&direct, input, output_atoms).unwrap();
        graph.data[28..36].copy_from_slice(&(c.slot + 100).to_le_bytes());
        let policy_index = graph.accounts.len();
        graph
            .accounts
            .push(AccountMeta::new_readonly(policy, false));
        let mut data = vec![20, policy_index as u8, 0, 0];
        data.extend_from_slice(&1u64.to_le_bytes());
        data.extend_from_slice(&150u64.to_le_bytes());
        data.extend_from_slice(&graph.data);
        let guarded = Instruction {
            program_id: SETTLE,
            accounts: graph.accounts,
            data,
        };
        let execution = run(&runtime, &guarded, &initialized.resulting_accounts);
        assert!(
            execution.program_result.is_ok(),
            "{} reverse: {:?}",
            c.name,
            execution.program_result
        );
        assert_eq!(&execution.return_data[..8], b"SKEWSTK2");
        assert_eq!(
            amount(&initialized.resulting_accounts, &c.input)
                - amount(&execution.resulting_accounts, &c.input),
            input,
            "{} product debit",
            c.name,
        );
        assert_eq!(
            amount(&execution.resulting_accounts, &c.output)
                - amount(&initialized.resulting_accounts, &c.output),
            output_atoms,
            "{} cash credit",
            c.name,
        );

        let mut floor = guarded.clone();
        floor.data[40..48].copy_from_slice(&(output_atoms + 1).to_le_bytes());
        let floor_result = run(&runtime, &floor, &initialized.resulting_accounts);
        assert!(floor_result.program_result.is_err(), "{} floor", c.name);
        assert_eq!(
            floor_result.resulting_accounts,
            initialized.resulting_accounts
        );

        let mut forward_opcode = guarded.clone();
        forward_opcode.data[0] = 7;
        let direction_result = run(&runtime, &forward_opcode, &initialized.resulting_accounts);
        assert!(
            direction_result.program_result.is_err(),
            "{} direction",
            c.name
        );
        assert_eq!(
            direction_result.resulting_accounts,
            initialized.resulting_accounts
        );

        let mut writable_policy = guarded.clone();
        writable_policy.accounts[policy_index].is_writable = true;
        let writable_result = run(&runtime, &writable_policy, &initialized.resulting_accounts);
        assert!(
            writable_result.program_result.is_err(),
            "{} writable policy",
            c.name
        );
        assert_eq!(
            writable_result.resulting_accounts,
            initialized.resulting_accounts
        );

        let symbol = c.name.split('-').next().unwrap().to_string();
        proved_symbols.insert(symbol.clone());
        proved_venues.insert(c.venue as u8);
        rows.push(json!({
            "case":c.name,"symbol":symbol,"venue":c.venue as u8,"slot":c.slot,
            "inputProductAtoms":input.to_string(),"outputCashAtoms":output_atoms.to_string(),
            "computeUnits":execution.compute_units_consumed,"accounts":guarded.accounts.len(),
            "legs":direct.len(),"minimumOutputRollback":true,"wrongDirectionRollback":true,
            "writablePolicyRollback":true,"passed":true
        }));
    }

    let complete = rows.len() >= 20 && proved_symbols.len() == 5 && proved_venues.len() >= 4;
    let report = json!({
        "schema":"skew.stockmesh.reverse-sbf-proof/v1",
        "environment":"AWS Mollusk; captured deployed venue ELFs/accounts; synthetic wallet balances; no network signature, submission, or fill",
        "settlementElfSha256":format!("{:x}", Sha256::digest(fs::read(sbf_dir.join("stocklana_settle.so")).unwrap())),
        "passed":complete,"caseCount":rows.len(),"stockCount":proved_symbols.len(),
        "venueCount":proved_venues.len(),"symbols":proved_symbols,"venues":proved_venues,"rows":rows
    });
    fs::write(
        output.join("reverse-sbf.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    assert!(
        complete,
        "reverse SBF proof must cover every top-five xStock and at least four venues"
    );
}
