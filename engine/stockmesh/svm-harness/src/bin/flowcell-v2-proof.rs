//! Multi-owner residual settlement proof for opcode 12.
//!
//! This uses two resource-disjoint deployed venue programs and archived market
//! accounts. The fixture is an ABI/CU/rollback proof; a separate coherent-state
//! capture is required before making route-quality or live-market claims.
#[allow(dead_code)]
#[path = "../matrix.rs"]
mod matrix;

use matrix::*;
use serde_json::json;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{pubkey, Pubkey};
use std::{fs, path::PathBuf};
use stocklana_adapters as dex;

const CROSS_INPUT: u64 = 50_000_000;
const TOTAL_INPUT: u64 = 100_000_000;

#[derive(Clone, Copy)]
struct Participant {
    owner: Pubkey,
    nonce: Pubkey,
    source: Pubkey,
    destination: Pubkey,
    input_mint: Pubkey,
    output_mint: Pubkey,
    input: u64,
    min_out: u64,
}

fn participant(
    accounts: &mut Accounts,
    owner: Pubkey,
    input_mint: Pubkey,
    output_mint: Pubkey,
    input: u64,
    min_out: u64,
) -> Participant {
    put(
        accounts,
        owner,
        Account {
            lamports: 1_000_000_000_000,
            ..Account::default()
        },
    );
    let mut token_accounts = Vec::new();
    for mint in [input_mint, output_mint] {
        let token_program = get(accounts, &mint).owner;
        let (address, _) = Pubkey::find_program_address(
            &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
            &pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
        );
        let mut account = token(mint, token_program, &get(accounts, &mint).data);
        account.data[32..64].copy_from_slice(owner.as_ref());
        put(accounts, address, account);
        token_accounts.push(address);
    }
    let (nonce, _) = Pubkey::find_program_address(&[b"stocklana", owner.as_ref()], &SETTLE);
    let mut nonce_data = vec![0u8; 64];
    nonce_data[..8].copy_from_slice(b"SKEWSEQ1");
    nonce_data[8..40].copy_from_slice(owner.as_ref());
    put(
        accounts,
        nonce,
        Account {
            lamports: 10_000_000,
            owner: SETTLE,
            data: nonce_data,
            executable: false,
            rent_epoch: 0,
        },
    );
    Participant {
        owner,
        nonce,
        source: token_accounts[0],
        destination: token_accounts[1],
        input_mint,
        output_mint,
        input,
        min_out,
    }
}

fn merge_accounts(left: &mut Accounts, right: &Accounts) {
    for (key, value) in right {
        if let Some(existing) = left.iter_mut().find(|row| row.0 == *key) {
            // The two archived markets were captured at different slots. Shared
            // sysvars use the newer fixture; market-owned accounts are disjoint.
            if *key == pubkey!("SysvarC1ock11111111111111111111111111111111") {
                if dex::u64_at(&value.data, 0).unwrap() > dex::u64_at(&existing.1.data, 0).unwrap()
                {
                    existing.1 = value.clone();
                }
            } else if existing.1 != *value {
                // Wallet token accounts are replaced below with deterministic
                // participant accounts. Shared immutable accounts must match.
                let token_owner = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
                let token_2022 = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
                assert!(
                    [token_owner, token_2022].contains(&existing.1.owner),
                    "conflicting shared non-token account {key}"
                );
            }
        } else {
            left.push((*key, value.clone()));
        }
    }
}

fn owner_graph(
    case: &Case,
    runtime: &mollusk_svm::Mollusk,
    participant: &Participant,
    input: u64,
) -> Instruction {
    let original = run(runtime, &case.jup, &case.a);
    assert!(original.program_result.is_ok());
    let direct = case.lower(&original).unwrap();
    assert_eq!(direct.len(), 1);
    let mut graph = case.graph(&direct, input, 1).unwrap();
    graph.data[28..36]
        .copy_from_slice(&runtime.sysvars.clock.slot.saturating_add(100).to_le_bytes());
    graph.accounts[0] = AccountMeta::new_readonly(participant.owner, true);
    graph.accounts[1] = AccountMeta::new(participant.nonce, false);
    let assets = graph.data[1] as usize;
    let first_token_index = graph.data[36] as usize;
    let final_token_index = graph.data[36 + (assets - 1) * 3] as usize;
    graph.accounts[first_token_index].pubkey = participant.source;
    graph.accounts[final_token_index].pubkey = participant.destination;
    graph
}

fn remap_graph(graph: &mut Instruction, cell_accounts: &mut Vec<AccountMeta>) {
    let mut remap = Vec::with_capacity(graph.accounts.len());
    for meta in &graph.accounts {
        let index = if let Some(index) = cell_accounts
            .iter()
            .position(|candidate| candidate.pubkey == meta.pubkey)
        {
            cell_accounts[index].is_writable |= meta.is_writable;
            cell_accounts[index].is_signer |= meta.is_signer;
            index
        } else {
            let index = cell_accounts.len();
            cell_accounts.push(meta.clone());
            index
        };
        remap.push(index as u8);
    }
    let assets = graph.data[1] as usize;
    for index in 0..assets {
        let cursor = 36 + index * 3;
        for offset in 0..3 {
            graph.data[cursor + offset] = remap[graph.data[cursor + offset] as usize];
        }
    }
    let mut cursor = 36 + assets * 3;
    if graph.data[0] == 9 {
        cursor += 8;
    }
    for _ in 0..graph.data[2] {
        graph.data[cursor + 12] = remap[graph.data[cursor + 12] as usize];
        let account_count = graph.data[cursor + 13] as usize;
        for index in cursor + 14..cursor + 14 + account_count {
            graph.data[index] = remap[graph.data[index] as usize];
        }
        cursor += 14 + account_count;
    }
    assert_eq!(cursor, graph.data.len());
}

fn compile_cell(
    accounts: &Accounts,
    participants: &[Participant],
    transfers: &[(usize, usize, u64)],
    mut residuals: Vec<(usize, Instruction)>,
    deadline: u64,
) -> Instruction {
    let mut metas = Vec::<AccountMeta>::new();
    let mut key = |pubkey: Pubkey, writable: bool, signer: bool| -> u8 {
        if let Some(index) = metas.iter().position(|meta| meta.pubkey == pubkey) {
            metas[index].is_writable |= writable;
            metas[index].is_signer |= signer;
            index as u8
        } else {
            let index = metas.len();
            metas.push(AccountMeta {
                pubkey,
                is_writable: writable,
                is_signer: signer,
            });
            index as u8
        }
    };
    let mut data = vec![
        12,
        participants.len() as u8,
        transfers.len() as u8,
        residuals.len() as u8,
    ];
    data.extend_from_slice(&deadline.to_le_bytes());
    for participant in participants {
        for (pubkey, writable, signer) in [
            (participant.owner, false, true),
            (participant.nonce, true, false),
            (participant.source, true, false),
            (participant.destination, true, false),
            (participant.input_mint, false, false),
            (participant.output_mint, false, false),
            (get(accounts, &participant.input_mint).owner, false, false),
            (get(accounts, &participant.output_mint).owner, false, false),
        ] {
            data.push(key(pubkey, writable, signer));
        }
        for value in [0, participant.input, participant.min_out] {
            data.extend_from_slice(&value.to_le_bytes());
        }
    }
    for (from, to, amount) in transfers {
        data.extend_from_slice(&[*from as u8, *to as u8]);
        data.extend_from_slice(&amount.to_le_bytes());
    }
    for (owner_slot, graph) in &mut residuals {
        remap_graph(graph, &mut metas);
        data.push(*owner_slot as u8);
        data.extend_from_slice(&(graph.data.len() as u16).to_le_bytes());
        data.extend_from_slice(&graph.data);
    }
    assert!(metas.len() <= 64);
    Instruction {
        program_id: SETTLE,
        accounts: metas,
        data,
    }
}

fn main() {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "/srv/skew/stocklana-engine-20260912".into()),
    );
    let mut runtime = runtime(&root);
    runtime.compute_budget.compute_unit_limit = 1_400_000;
    let raydium = Case::load(&root.join("artifacts/venue-matrix/cases/NVDAx-buy-raydium_clmm"));
    let byreal = Case::load(&root.join("artifacts/venue-matrix/cases/NVDAx-buy-byreal"));
    let usdc = pk(raydium.quote["inputMint"].as_str().unwrap());
    let nvdax = pk(raydium.quote["outputMint"].as_str().unwrap());
    assert_eq!(
        (usdc, nvdax),
        (
            pk(byreal.quote["inputMint"].as_str().unwrap()),
            pk(byreal.quote["outputMint"].as_str().unwrap())
        )
    );

    let mut bank = raydium.a.clone();
    merge_accounts(&mut bank, &byreal.a);
    let clock = dex::u64_at(
        &get(
            &bank,
            &pubkey!("SysvarC1ock11111111111111111111111111111111"),
        )
        .data,
        0,
    )
    .unwrap();
    runtime.sysvars.clock.slot = clock;
    runtime.sysvars.clock.unix_timestamp = i64::from_le_bytes(
        get(
            &bank,
            &pubkey!("SysvarC1ock11111111111111111111111111111111"),
        )
        .data[32..40]
            .try_into()
            .unwrap(),
    );

    let mut buyers = [
        participant(&mut bank, WALLET, usdc, nvdax, TOTAL_INPUT, 1),
        participant(
            &mut bank,
            Pubkey::new_from_array([141; 32]),
            usdc,
            nvdax,
            TOTAL_INPUT,
            1,
        ),
    ];
    let mut graphs = [
        owner_graph(&raydium, &runtime, &buyers[0], TOTAL_INPUT - CROSS_INPUT),
        owner_graph(&byreal, &runtime, &buyers[1], TOTAL_INPUT - CROSS_INPUT),
    ];
    let mut observed_external = [0u64; 2];
    for index in 0..2 {
        let result = run(&runtime, &graphs[index], &bank);
        assert!(
            result.program_result.is_ok(),
            "standalone graph {index}: {:?}",
            result.program_result
        );
        observed_external[index] = amount(&result.resulting_accounts, &buyers[index].destination)
            .checked_sub(START)
            .unwrap();
        let floor = u64::try_from(u128::from(observed_external[index]) * 9_980 / 10_000).unwrap();
        graphs[index].data[20..28].copy_from_slice(&floor.to_le_bytes());
    }

    let cross_shares = [observed_external[0], observed_external[1]];
    buyers[0].min_out = cross_shares[0]
        .checked_add(dex::u64_at(&graphs[0].data, 20).unwrap())
        .unwrap();
    buyers[1].min_out = cross_shares[1]
        .checked_add(dex::u64_at(&graphs[1].data, 20).unwrap())
        .unwrap();
    let sellers = [
        participant(
            &mut bank,
            Pubkey::new_from_array([142; 32]),
            nvdax,
            usdc,
            cross_shares[0],
            CROSS_INPUT,
        ),
        participant(
            &mut bank,
            Pubkey::new_from_array([143; 32]),
            nvdax,
            usdc,
            cross_shares[1],
            CROSS_INPUT,
        ),
    ];
    let participants = [buyers[0], buyers[1], sellers[0], sellers[1]];
    let transfers = [
        (0, 2, CROSS_INPUT),
        (2, 0, cross_shares[0]),
        (1, 3, CROSS_INPUT),
        (3, 1, cross_shares[1]),
    ];
    let instruction = compile_cell(
        &bank,
        &participants,
        &transfers,
        vec![(0, graphs[0].clone()), (1, graphs[1].clone())],
        clock + 1000,
    );
    let result = run(&runtime, &instruction, &bank);
    assert!(
        result.program_result.is_ok(),
        "flow cell: {:?}",
        result.program_result
    );
    assert_eq!(&result.return_data[..8], b"SKEWCEL2");
    for participant in &participants {
        assert_eq!(
            START - amount(&result.resulting_accounts, &participant.source),
            participant.input
        );
        assert!(
            amount(&result.resulting_accounts, &participant.destination) - START
                >= participant.min_out
        );
        assert_eq!(
            dex::u64_at(
                &get(&result.resulting_accounts, &participant.nonce).data,
                40
            )
            .unwrap(),
            1
        );
    }

    let mut faults = Vec::new();
    for name in [
        "missing_second_owner_signature",
        "duplicate_residual_owner",
        "residual_input_mismatch",
        "late_owner_minimum",
        "shared_writable_venue_state",
    ] {
        let mut bad = instruction.clone();
        match name {
            "missing_second_owner_signature" => {
                bad.accounts
                    .iter_mut()
                    .find(|meta| meta.pubkey == participants[1].owner)
                    .unwrap()
                    .is_signer = false;
            }
            "duplicate_residual_owner" => {
                let second = 12 + 4 * 32 + 4 * 10 + 3 + graphs[0].data.len();
                bad.data[second] = 0;
            }
            "residual_input_mismatch" => {
                let first_graph = 12 + 4 * 32 + 4 * 10 + 3;
                bad.data[first_graph + 12..first_graph + 20]
                    .copy_from_slice(&(TOTAL_INPUT - CROSS_INPUT + 1).to_le_bytes());
            }
            "late_owner_minimum" => {
                let second_intent_minimum = 12 + 32 + 24;
                bad.data[second_intent_minimum..second_intent_minimum + 8]
                    .copy_from_slice(&u64::MAX.to_le_bytes());
            }
            "shared_writable_venue_state" => {
                let first_graph_start = 12 + 4 * 32 + 4 * 10 + 3;
                let second_record = first_graph_start + graphs[0].data.len();
                let second_graph_start = second_record + 3;
                let first_leg_start = first_graph_start + 36 + 2 * 3;
                let second_leg_start = second_graph_start + 36 + 2 * 3;
                let first_pool_account_position = first_leg_start + 14 + 2;
                let second_pool_account_position = second_leg_start + 14 + 2;
                bad.data[second_pool_account_position] = bad.data[first_pool_account_position];
            }
            _ => unreachable!(),
        }
        let failure = run(&runtime, &bad, &bank);
        assert!(failure.program_result.is_err(), "{name}");
        assert_eq!(failure.resulting_accounts, bank, "{name} rollback");
        faults.push(json!({
            "case": name,
            "rollback": true,
            "cu": failure.compute_units_consumed,
            "status": format!("{:?}", failure.program_result),
        }));
    }

    // Three economic intents form one exact asset cycle. No router leg is
    // needed: each signed input is another owner's signed output. This is the
    // concrete SOL -> USDC, USDC -> NVDAx, NVDAx -> SOL Flow Folding case.
    let wsol = pubkey!("So11111111111111111111111111111111111111112");
    let mut cycle_bank = bank.clone();
    let mut wsol_mint_data = vec![0u8; 82];
    wsol_mint_data[44] = 9;
    wsol_mint_data[45] = 1;
    put(
        &mut cycle_bank,
        wsol,
        Account {
            lamports: 1_461_600,
            data: wsol_mint_data,
            owner: pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
            executable: false,
            rent_epoch: 0,
        },
    );
    let sol_atoms = 1_000_000_000;
    let usdc_atoms = 185_000_000;
    let nvda_atoms = 1_000_000;
    let cycle_participants = [
        participant(
            &mut cycle_bank,
            Pubkey::new_from_array([151; 32]),
            wsol,
            usdc,
            sol_atoms,
            usdc_atoms,
        ),
        participant(
            &mut cycle_bank,
            Pubkey::new_from_array([152; 32]),
            usdc,
            nvdax,
            usdc_atoms,
            nvda_atoms,
        ),
        participant(
            &mut cycle_bank,
            Pubkey::new_from_array([153; 32]),
            nvdax,
            wsol,
            nvda_atoms,
            sol_atoms,
        ),
    ];
    let cycle_transfers = [(0, 2, sol_atoms), (1, 0, usdc_atoms), (2, 1, nvda_atoms)];
    let cycle_instruction = compile_cell(
        &cycle_bank,
        &cycle_participants,
        &cycle_transfers,
        Vec::new(),
        clock + 1000,
    );
    let cycle_result = run(&runtime, &cycle_instruction, &cycle_bank);
    assert!(
        cycle_result.program_result.is_ok(),
        "three-asset cycle: {:?}",
        cycle_result.program_result
    );
    for participant in &cycle_participants {
        assert_eq!(
            START - amount(&cycle_result.resulting_accounts, &participant.source),
            participant.input
        );
        assert_eq!(
            amount(&cycle_result.resulting_accounts, &participant.destination) - START,
            participant.min_out
        );
    }

    let evidence = json!({
        "scope": "opcode 12 ABI, owner isolation, two resource-disjoint deployed DEX CPIs, SBF rollback and CU",
        "market_state": "archived venue states captured at different slots; not a coherent quote or route-quality proof",
        "mainnet_submission": false,
        "intents": 4,
        "internal_transfers": 4,
        "residual_owners": 2,
        "external_legs": 2,
        "venues": ["raydium_clmm", "byreal_clmm"],
        "cu": result.compute_units_consumed,
        "account_count": instruction.accounts.len(),
        "instruction_data_bytes": instruction.data.len(),
        "signed_owner_minima_met": true,
        "three_asset_cycle": {
            "intents": ["SOL->USDC", "USDC->NVDAx", "NVDAx->SOL"],
            "external_legs": 0,
            "cu": cycle_result.compute_units_consumed,
            "account_count": cycle_instruction.accounts.len(),
            "instruction_data_bytes": cycle_instruction.data.len(),
            "exact_owner_postconditions": true,
        },
        "return_data": result.return_data,
        "faults": faults,
    });
    let output = root.join("artifacts/flowcell-v2");
    fs::create_dir_all(&output).unwrap();
    fs::write(
        output.join("proof.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    println!("{evidence}");
}
