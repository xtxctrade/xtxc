//! Opcode-13 multi-product economic-exposure proof.
//!
//! Two distinct Token-2022 stock mints execute through two resource-disjoint
//! deployed CLMM programs under one owner nonce. The policies deliberately bind
//! both fixture products to one synthetic instrument identity; this proves the
//! settlement ABI, aggregate Q32 postcondition, rollback and CU, not a claim
//! that these two captured products are the same real-world stock.
#[allow(dead_code)]
#[path = "../matrix.rs"]
mod matrix;

pub type Result<T> = std::result::Result<T, String>;
use skew_execution_host::onebook_wire as wire;

use base64::{engine::general_purpose::STANDARD, Engine};
use matrix::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{pubkey, Pubkey};
use std::{fs, path::PathBuf};
use stocklana_adapters as dex;

const INPUT_PER_PRODUCT: u64 = 100_000_000;
const POLICY_LEN: usize = 225;

#[derive(Clone, Copy)]
struct ProductWallet {
    owner: Pubkey,
    nonce: Pubkey,
    source: Pubkey,
    destination: Pubkey,
    output_mint: Pubkey,
}

fn wallet_product(
    accounts: &mut Accounts,
    owner: Pubkey,
    input_mint: Pubkey,
    output_mint: Pubkey,
) -> ProductWallet {
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
    ProductWallet {
        owner,
        nonce,
        source: token_accounts[0],
        destination: token_accounts[1],
        output_mint,
    }
}

fn merge_accounts(left: &mut Accounts, right: &Accounts) {
    for (key, value) in right {
        if let Some(existing) = left.iter_mut().find(|row| row.0 == *key) {
            if *key == pubkey!("SysvarC1ock11111111111111111111111111111111") {
                if dex::u64_at(&value.data, 0).unwrap() > dex::u64_at(&existing.1.data, 0).unwrap()
                {
                    existing.1 = value.clone();
                }
            } else if existing.1 != *value {
                let token = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
                let token_2022 = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
                assert!([token, token_2022].contains(&existing.1.owner));
            }
        } else {
            left.push((*key, value.clone()));
        }
    }
}

fn product_graph(
    case: &Case,
    runtime: &mollusk_svm::Mollusk,
    wallet: &ProductWallet,
) -> Instruction {
    let original = run(runtime, &case.jup, &case.a);
    assert!(original.program_result.is_ok());
    let direct = case.lower(&original).unwrap();
    assert_eq!(direct.len(), 1);
    let mut graph = case.graph(&direct, INPUT_PER_PRODUCT, 1).unwrap();
    graph.data[28..36]
        .copy_from_slice(&runtime.sysvars.clock.slot.saturating_add(100).to_le_bytes());
    graph.accounts[0] = AccountMeta::new_readonly(wallet.owner, true);
    graph.accounts[1] = AccountMeta::new(wallet.nonce, false);
    let assets = graph.data[1] as usize;
    let input_token = graph.data[36] as usize;
    let output_token = graph.data[36 + (assets - 1) * 3] as usize;
    let output_mint = graph.data[36 + (assets - 1) * 3 + 1] as usize;
    graph.accounts[input_token].pubkey = wallet.source;
    graph.accounts[output_token].pubkey = wallet.destination;
    assert_eq!(graph.accounts[output_mint].pubkey, wallet.output_mint);
    graph
}

fn policy(
    accounts: &mut Accounts,
    authority: Pubkey,
    instrument: [u8; 32],
    issuer: [u8; 32],
    input: Pubkey,
    output: Pubkey,
    slot: u64,
) -> Pubkey {
    let rights: [u8; 32] = Sha256::digest(
        [
            b"SKEW_FIXTURE_RIGHTS_V2".as_slice(),
            &instrument,
            &issuer,
            input.as_ref(),
            output.as_ref(),
        ]
        .concat(),
    )
    .into();
    let (address, _) = Pubkey::find_program_address(
        &[
            b"stock2",
            authority.as_ref(),
            &instrument,
            input.as_ref(),
            output.as_ref(),
            &rights,
        ],
        &SETTLE,
    );
    let mut data = vec![0u8; POLICY_LEN];
    data[..8].copy_from_slice(b"SKEWSTK2");
    data[8..40].copy_from_slice(authority.as_ref());
    data[40..72].copy_from_slice(&instrument);
    data[72..104].copy_from_slice(&issuer);
    data[104..136].copy_from_slice(input.as_ref());
    data[136..168].copy_from_slice(output.as_ref());
    data[168..200].copy_from_slice(&rights);
    data[200..208].copy_from_slice(&1u64.to_le_bytes());
    data[208..216].copy_from_slice(&slot.to_le_bytes());
    data[216..224].copy_from_slice(&slot.saturating_add(1_000).to_le_bytes());
    data[224] = 1 | 16;
    put(
        accounts,
        address,
        Account {
            lamports: 10_000_000,
            data,
            owner: SETTLE,
            executable: false,
            rent_epoch: 0,
        },
    );
    address
}

fn compile(
    policies: &[Pubkey],
    graphs: &[Instruction],
    deadline: u64,
    minimum_exposure_q32: u64,
) -> Instruction {
    let first = dex::graph::Graph::decode(&graphs[0].data, graphs[0].accounts.len()).unwrap();
    let source = first.assets[0];
    wire::compile_exposure_fill(
        SETTLE,
        wire::ExposureFillSpec {
            buyer: graphs[0].accounts[0].pubkey,
            buyer_nonce: graphs[0].accounts[1].pubkey,
            buyer_source: graphs[0].accounts[source.token as usize].pubkey,
            input_mint: graphs[0].accounts[source.mint as usize].pubkey,
            input_token_program: graphs[0].accounts[source.program as usize].pubkey,
            buyer_sequence: first.sequence,
            input_atoms: INPUT_PER_PRODUCT * graphs.len() as u64,
            minimum_exposure_q32,
            deadline_slot: deadline,
            maximum_policy_age: 150,
            allow_underlying_closed: false,
            allocations: policies
                .iter()
                .zip(graphs)
                .map(|(policy, graph)| {
                    let decoded =
                        dex::graph::Graph::decode(&graph.data, graph.accounts.len()).unwrap();
                    let sink = decoded.assets[decoded.asset_count - 1];
                    wire::ExposureAllocation {
                        product: wire::MeshProduct {
                            policy: *policy,
                            claim: None,
                            destination: graph.accounts[sink.token as usize].pubkey,
                            mint: graph.accounts[sink.mint as usize].pubkey,
                            token_program: graph.accounts[sink.program as usize].pubkey,
                            model: 0,
                            conservative_bps: 10000,
                            policy_version: 1,
                            numerator: 1,
                            denominator: 1,
                        },
                        graph: graph.clone(),
                    }
                })
                .collect(),
        },
    )
    .unwrap()
}

fn q32(raw: u64, decimals: u8) -> u64 {
    u64::try_from(u128::from(raw) * (1u128 << 32) / 10u128.pow(u32::from(decimals))).unwrap()
}

fn main() {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "/srv/skew/stocklana-engine-20260912".into()),
    );
    if std::env::var_os("SKEW_SETTLE_SBF_DIR").is_none() {
        std::env::set_var(
            "SKEW_SETTLE_SBF_DIR",
            root.join("artifacts/sbf-stockmesh-v5"),
        );
    }
    let mut runtime = runtime(&root);
    runtime.compute_budget.compute_unit_limit = 1_400_000;
    let first = Case::load(&root.join("artifacts/venue-matrix/cases/NVDAx-buy-raydium_clmm"));
    let second = Case::load(&root.join("artifacts/venue-matrix/cases/TSLAx-buy-byreal"));
    let usdc = pk(first.quote["inputMint"].as_str().unwrap());
    assert_eq!(usdc, pk(second.quote["inputMint"].as_str().unwrap()));
    let output_mints = [
        pk(first.quote["outputMint"].as_str().unwrap()),
        pk(second.quote["outputMint"].as_str().unwrap()),
    ];

    let mut bank = first.a.clone();
    merge_accounts(&mut bank, &second.a);
    let clock_account = pubkey!("SysvarC1ock11111111111111111111111111111111");
    let slot = dex::u64_at(&get(&bank, &clock_account).data, 0).unwrap();
    runtime.sysvars.clock.slot = slot;
    runtime.sysvars.clock.unix_timestamp =
        i64::from_le_bytes(get(&bank, &clock_account).data[32..40].try_into().unwrap());
    let owner = WALLET;
    let wallets = [
        wallet_product(&mut bank, owner, usdc, output_mints[0]),
        wallet_product(&mut bank, owner, usdc, output_mints[1]),
    ];
    assert_eq!(wallets[0].source, wallets[1].source);
    assert_eq!(wallets[0].nonce, wallets[1].nonce);
    let mut graphs = [
        product_graph(&first, &runtime, &wallets[0]),
        product_graph(&second, &runtime, &wallets[1]),
    ];
    let mut raw = [0u64; 2];
    for index in 0..2 {
        let result = run(&runtime, &graphs[index], &bank);
        assert!(
            result.program_result.is_ok(),
            "standalone {index}: {:?}",
            result.program_result
        );
        raw[index] = amount(&result.resulting_accounts, &wallets[index].destination) - START;
        let floor = u64::try_from(u128::from(raw[index]) * 9_980 / 10_000).unwrap();
        graphs[index].data[20..28].copy_from_slice(&floor.to_le_bytes());
    }
    let instrument = wire::instrument_id("NVDA").unwrap();
    let authority = Pubkey::new_from_array([18u8; 32]);
    let policies = [
        policy(
            &mut bank,
            authority,
            instrument,
            wire::issuer_id("SBF-0").unwrap(),
            usdc,
            output_mints[0],
            slot,
        ),
        policy(
            &mut bank,
            authority,
            instrument,
            wire::issuer_id("SBF-1").unwrap(),
            usdc,
            output_mints[1],
            slot,
        ),
    ];
    let expected_exposure = q32(raw[0], get(&bank, &output_mints[0]).data[44])
        .checked_add(q32(raw[1], get(&bank, &output_mints[1]).data[44]))
        .unwrap();
    let minimum_exposure = u64::try_from(u128::from(expected_exposure) * 9_980 / 10_000).unwrap();
    let instruction = compile(&policies, &graphs, slot + 100, minimum_exposure);
    let result = run(&runtime, &instruction, &bank);
    assert!(
        result.program_result.is_ok(),
        "exposure settlement: {:?}",
        result.program_result
    );
    assert_eq!(&result.return_data[..8], b"SKEWEXP1");
    assert_eq!(
        START - amount(&result.resulting_accounts, &wallets[0].source),
        INPUT_PER_PRODUCT * 2
    );
    for index in 0..2 {
        assert_eq!(
            amount(&result.resulting_accounts, &wallets[index].destination) - START,
            raw[index]
        );
    }
    assert_eq!(
        dex::u64_at(&get(&result.resulting_accounts, &wallets[0].nonce).data, 40).unwrap(),
        1
    );

    let mut faults = Vec::new();
    for name in [
        "aggregate_minimum",
        "policy_version",
        "shared_writable_market",
        "descriptor_reserved",
    ] {
        let mut bad = instruction.clone();
        match name {
            "aggregate_minimum" => {
                bad.data[12..20].copy_from_slice(&(expected_exposure + 1).to_le_bytes())
            }
            "policy_version" => bad.data[32..40].copy_from_slice(&2u64.to_le_bytes()),
            "shared_writable_market" => {
                let first_graph = 28 + 32;
                let second_descriptor = first_graph + graphs[0].data.len();
                let second_graph = second_descriptor + 32;
                let first_leg = first_graph + 36 + graphs[0].data[1] as usize * 3;
                let second_leg = second_graph + 36 + graphs[1].data[1] as usize * 3;
                bad.data[second_leg + 14 + 2] = bad.data[first_leg + 14 + 2];
            }
            "descriptor_reserved" => bad.data[58] = 1,
            _ => unreachable!(),
        }
        let failed = run(&runtime, &bad, &bank);
        assert!(failed.program_result.is_err(), "{name}");
        assert_eq!(failed.resulting_accounts, bank, "{name} rollback");
        faults.push(json!({"case":name,"rollback":true,"cu":failed.compute_units_consumed,"status":format!("{:?}",failed.program_result)}));
    }

    let table = solana_message::AddressLookupTableAccount {
        key: Pubkey::new_from_array([81; 32]),
        addresses: instruction
            .accounts
            .iter()
            .filter(|a| !a.is_signer)
            .map(|a| a.pubkey)
            .collect(),
    };
    let compute = Instruction {
        program_id: pubkey!("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data: [vec![2], 1_400_000u32.to_le_bytes().to_vec()].concat(),
    };
    let message = wire::compile_unsigned_v0(
        wallets[0].owner,
        &[compute, instruction.clone()],
        std::slice::from_ref(&table),
        [42; 32],
    )
    .unwrap();
    let mut alt = vec![0u8; 56];
    alt[..4].copy_from_slice(&1u32.to_le_bytes());
    alt[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
    for address in &table.addresses {
        alt.extend_from_slice(address.as_ref());
    }
    put(
        &mut bank,
        table.key,
        Account {
            lamports: 10_000_000,
            data: alt,
            owner: pubkey!("AddressLookupTab1e1111111111111111111111111"),
            executable: false,
            rent_epoch: 0,
        },
    );
    let vector = json!({
        "scope":"captured SBF outcome and shared production compiler; not an RPC or mainnet receipt",
        "slot":slot,"program":SETTLE.to_string(),"owner":wallets[0].owner.to_string(),"inputMint":usdc.to_string(),
        "inputAtoms":INPUT_PER_PRODUCT * 2,"floor":minimum_exposure,"deadline":slot+100,
        "policies":policies.iter().map(|k|k.to_string()).collect::<Vec<_>>(),
        "productMints":output_mints.iter().map(|k|k.to_string()).collect::<Vec<_>>(),
        "message":STANDARD.encode(&message),"computeUnits":result.compute_units_consumed,
        "returnData":{"programId":SETTLE.to_string(),"data":[STANDARD.encode(&result.return_data),"base64"]},
        "before":bank.iter().map(|(k,a)|json!({"key":k.to_string(),"owner":a.owner.to_string(),"lamports":a.lamports,"executable":a.executable,"data":[STANDARD.encode(&a.data),"base64"]})).collect::<Vec<_>>(),
        "after":result.resulting_accounts.iter().map(|(k,a)|json!({"key":k.to_string(),"owner":a.owner.to_string(),"lamports":a.lamports,"executable":a.executable,"data":[STANDARD.encode(&a.data),"base64"]})).collect::<Vec<_>>()
    });
    let vector_bytes = serde_json::to_vec(&vector).unwrap();
    let evidence = json!({
        "scope":"opcode 13 multi-product aggregate exposure ABI, deployed DEX CPI, SBF rollback and CU",
        "economic_identity":"synthetic same-instrument policies over two distinct captured product mints",
        "market_state":"archived venue states; not a coherent quote or route-quality proof",
        "mainnet_submission":false,
        "products":2,
        "external_legs":2,
        "input_atoms":INPUT_PER_PRODUCT * 2,
        "minimum_exposure_q32":minimum_exposure,
        "actual_exposure_q32":expected_exposure,
        "cu":result.compute_units_consumed,
        "account_count":instruction.accounts.len(),
        "instruction_data_bytes":instruction.data.len(),
        "host_compiler":"host/src/onebook_wire.rs::compile_exposure_fill",
        "graph_compiler":"host/src/swap_wire.rs::compile_swap_graph",
        "graph_compiler_sha256":format!("{:x}",Sha256::digest(fs::read(root.join("host/src/swap_wire.rs")).unwrap())),
        "message_bytes":message.len(),"signed_packet_bytes":message.len()+65,
        "host_vector_sha256":format!("{:x}",Sha256::digest(&vector_bytes)),
        "return_data":result.return_data,
        "faults":faults,
    });
    let output = std::env::var_os("SKEW_PROOF_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("artifacts/stockmesh-exposure-v5"));
    fs::create_dir_all(&output).unwrap();
    fs::write(output.join("host-vector.json"), vector_bytes).unwrap();
    fs::write(
        output.join("proof.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    println!("{evidence}");
}
