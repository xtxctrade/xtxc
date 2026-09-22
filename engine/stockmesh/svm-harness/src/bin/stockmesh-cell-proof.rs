//! Opcode-14 StockMesh FlowCell proof.
//!
//! One buyer receives two distinct Token-2022 issuer products from two internal
//! sellers and two deployed CLMM residual CPIs. Synthetic same-instrument policy
//! records prove the ABI, aggregate exposure, CU and rollback; they do not claim
//! that the captured NVDAx and TSLAx fixtures represent one real instrument.
#[allow(dead_code)]
#[path = "../matrix.rs"]
mod matrix;

pub type Result<T> = std::result::Result<T, String>;
use base64::{engine::general_purpose::STANDARD, Engine};
use sha2::{Digest, Sha256};
use skew_execution_host::onebook_wire as wire;

use matrix::*;
use serde_json::json;
use skew_execution_host::{
    swap_wire::{AccountView, TokenAsset},
    wallet_wire::WalletSetup,
};
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{pubkey, Pubkey};
use std::{collections::BTreeSet, fs, path::PathBuf};
use stocklana_adapters as dex;

const RESIDUAL_INPUT: u64 = 50_000_000;
const INTERNAL_CASH: u64 = 1_000_000;
const INTERNAL_STOCK: [u64; 2] = [100_000, 120_000];
const POLICY_LEN: usize = 225;
const CONSERVATIVE_BPS: u64 = 9_980;

#[derive(Clone, Copy)]
struct WalletProduct {
    source: Pubkey,
    destination: Pubkey,
    nonce: Pubkey,
}

#[derive(Clone, Copy)]
struct SellerWallet {
    owner: Pubkey,
    nonce: Pubkey,
    stock_source: Pubkey,
    cash_destination: Pubkey,
}

fn token_account(accounts: &mut Accounts, owner: Pubkey, mint: Pubkey) -> Pubkey {
    let token_program = get(accounts, &mint).owner;
    let (address, _) = Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
    );
    let mut account = token(mint, token_program, &get(accounts, &mint).data);
    account.data[32..64].copy_from_slice(owner.as_ref());
    put(accounts, address, account);
    address
}

fn empty_token_account(accounts: &mut Accounts, address: Pubkey, authority: Pubkey, mint: Pubkey) {
    let token_program = get(accounts, &mint).owner;
    let mut account = token(mint, token_program, &get(accounts, &mint).data);
    account.data[32..64].copy_from_slice(authority.as_ref());
    account.data[64..72].fill(0);
    put(accounts, address, account);
}

fn account_view<'a>(accounts: &'a Accounts, key: &Pubkey) -> Result<AccountView<'a>> {
    let account = accounts
        .iter()
        .find(|(address, _)| address == key)
        .map(|(_, account)| account)
        .ok_or_else(|| format!("missing setup account {key}"))?;
    Ok(AccountView {
        owner: account.owner,
        executable: account.executable,
        data: &account.data,
    })
}

fn token_asset(accounts: &Accounts, token: Pubkey) -> TokenAsset {
    let account = get(accounts, &token);
    TokenAsset {
        token,
        mint: Pubkey::new_from_array(dex::key(&account.data, 0).unwrap()),
        token_program: account.owner,
    }
}

fn nonce(accounts: &mut Accounts, owner: Pubkey) -> Pubkey {
    let (address, _) = Pubkey::find_program_address(&[b"stocklana", owner.as_ref()], &SETTLE);
    let mut data = vec![0u8; 64];
    data[..8].copy_from_slice(b"SKEWSEQ1");
    data[8..40].copy_from_slice(owner.as_ref());
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

fn buyer_product(
    accounts: &mut Accounts,
    input_mint: Pubkey,
    output_mint: Pubkey,
) -> WalletProduct {
    WalletProduct {
        source: token_account(accounts, WALLET, input_mint),
        destination: token_account(accounts, WALLET, output_mint),
        nonce: nonce(accounts, WALLET),
    }
}

fn seller(
    accounts: &mut Accounts,
    discriminator: u8,
    cash_mint: Pubkey,
    stock_mint: Pubkey,
) -> SellerWallet {
    let owner = Pubkey::new_from_array([discriminator; 32]);
    put(
        accounts,
        owner,
        Account {
            lamports: 1_000_000_000_000,
            ..Account::default()
        },
    );
    SellerWallet {
        owner,
        nonce: nonce(accounts, owner),
        stock_source: token_account(accounts, owner, stock_mint),
        cash_destination: token_account(accounts, owner, cash_mint),
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
    wallet: WalletProduct,
) -> Instruction {
    let original = run(runtime, &case.jup, &case.a);
    assert!(original.program_result.is_ok());
    let direct = case.lower(&original).unwrap();
    assert_eq!(direct.len(), 1);
    let mut graph = case.graph(&direct, RESIDUAL_INPUT, 1).unwrap();
    graph.data[28..36]
        .copy_from_slice(&runtime.sysvars.clock.slot.saturating_add(100).to_le_bytes());
    graph.accounts[0] = AccountMeta::new_readonly(WALLET, true);
    graph.accounts[1] = AccountMeta::new(wallet.nonce, false);
    let assets = graph.data[1] as usize;
    let source = graph.data[36] as usize;
    let destination = graph.data[36 + (assets - 1) * 3] as usize;
    graph.accounts[source].pubkey = wallet.source;
    graph.accounts[destination].pubkey = wallet.destination;
    graph
}

fn remap_graph(graph: &mut Instruction, metas: &mut Vec<AccountMeta>) {
    let mut remap = Vec::with_capacity(graph.accounts.len());
    for meta in &graph.accounts {
        let index = if let Some(index) = metas.iter().position(|row| row.pubkey == meta.pubkey) {
            metas[index].is_writable |= meta.is_writable;
            metas[index].is_signer |= meta.is_signer;
            index
        } else {
            let index = metas.len();
            metas.push(meta.clone());
            index
        };
        remap.push(index as u8);
    }
    let assets = graph.data[1] as usize;
    for asset in 0..assets {
        for offset in 0..3 {
            let cursor = 36 + asset * 3 + offset;
            graph.data[cursor] = remap[graph.data[cursor] as usize];
        }
    }
    let mut cursor = 36 + assets * 3;
    if graph.data[0] == 9 {
        cursor += 8;
    }
    for _ in 0..graph.data[2] {
        graph.data[cursor + 12] = remap[graph.data[cursor + 12] as usize];
        let count = graph.data[cursor + 13] as usize;
        for index in cursor + 14..cursor + 14 + count {
            graph.data[index] = remap[graph.data[index] as usize];
        }
        cursor += 14 + count;
    }
    assert_eq!(cursor, graph.data.len());
}

/// Join two captured, single-leg cash->product graphs into one multi-output
/// economic candidate graph.  The candidate budgets become capacities rather
/// than a preselected split; opcode 18 v2 patches the signed minimum input to
/// the actual funding result before the on-chain optimizer runs.
fn economic_graph(graphs: &[Instruction; 2], input: u64, minimum_exposure: u64) -> Instruction {
    let mut mapped = graphs.clone();
    let mut metas = Vec::new();
    for graph in &mut mapped {
        remap_graph(graph, &mut metas);
    }
    let decoded = mapped
        .iter()
        .map(|graph| dex::graph::Graph::decode(&graph.data, metas.len()).unwrap())
        .collect::<Vec<_>>();
    assert!(decoded
        .iter()
        .all(|graph| graph.asset_count == 2 && graph.leg_count == 1));
    let key = |index: u8| metas[usize::from(index)].pubkey;
    let first_source = decoded[0].assets[0];
    let second_source = decoded[1].assets[0];
    assert_eq!(
        [
            key(first_source.token),
            key(first_source.mint),
            key(first_source.program),
        ],
        [
            key(second_source.token),
            key(second_source.mint),
            key(second_source.program),
        ]
    );
    assert_eq!(mapped[0].accounts[0].pubkey, mapped[1].accounts[0].pubkey);
    assert_eq!(mapped[0].accounts[1].pubkey, mapped[1].accounts[1].pubkey);

    let sequence = decoded[0].sequence;
    assert_eq!(sequence, decoded[1].sequence);
    let deadline = decoded[0].deadline.min(decoded[1].deadline);
    let mut data = vec![4, 3, 2, 16];
    for value in [sequence, input, minimum_exposure, deadline] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    data.extend_from_slice(&mapped[0].data[36..39]);
    data.extend_from_slice(&mapped[0].data[39..42]);
    data.extend_from_slice(&mapped[1].data[39..42]);
    for (index, graph) in mapped.iter().enumerate() {
        let mut leg = graph.data[42..].to_vec();
        leg[1] = 0;
        leg[2] = 1 + index as u8;
        leg[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
        data.extend_from_slice(&leg);
    }
    dex::graph::Graph::decode(&data, metas.len()).unwrap();
    Instruction {
        program_id: SETTLE,
        accounts: metas,
        data,
    }
}

fn meta(metas: &mut Vec<AccountMeta>, key: Pubkey, writable: bool, signer: bool) -> u8 {
    if let Some(index) = metas.iter().position(|row| row.pubkey == key) {
        metas[index].is_writable |= writable;
        metas[index].is_signer |= signer;
        index as u8
    } else {
        let index = metas.len();
        metas.push(if writable {
            AccountMeta::new(key, signer)
        } else {
            AccountMeta::new_readonly(key, signer)
        });
        index as u8
    }
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

fn exposure(raw: u64, decimals: u8) -> u64 {
    u64::try_from(
        u128::from(raw) * (1u128 << 32) * u128::from(CONSERVATIVE_BPS)
            / (10u128.pow(u32::from(decimals)) * 10_000),
    )
    .unwrap()
}

#[allow(clippy::too_many_arguments)]
fn compile(
    accounts: &Accounts,
    cash_mint: Pubkey,
    policies: [Pubkey; 2],
    output_mints: [Pubkey; 2],
    wallets: [WalletProduct; 2],
    sellers: [SellerWallet; 2],
    graphs: &mut [Instruction; 2],
    deadline: u64,
    expected_exposure: u64,
    resting: Option<(Pubkey, Pubkey, Pubkey)>,
) -> Instruction {
    let mut metas = Vec::new();
    for graph in graphs.iter_mut() {
        remap_graph(graph, &mut metas);
    }
    let cash_program = get(accounts, &cash_mint).owner;
    let buyer = meta(&mut metas, WALLET, false, true);
    let buyer_nonce = meta(&mut metas, wallets[0].nonce, true, false);
    let buyer_source = meta(&mut metas, wallets[0].source, true, false);
    let cash_mint_index = meta(&mut metas, cash_mint, false, false);
    let cash_program_index = meta(&mut metas, cash_program, false, false);
    let mut product_rows = [[0u8; 32]; 2];
    for index in 0..2 {
        let token_program = get(accounts, &output_mints[index]).owner;
        product_rows[index][0] = meta(&mut metas, policies[index], false, false);
        product_rows[index][1] = meta(&mut metas, wallets[index].destination, true, false);
        product_rows[index][2] = meta(&mut metas, output_mints[index], false, false);
        product_rows[index][3] = meta(&mut metas, token_program, false, false);
        product_rows[index][4] = 0;
        if index == 0 {
            if let Some((_, _, claim)) = resting {
                product_rows[index][5] = meta(&mut metas, claim, false, false);
            }
        }
        product_rows[index][6..8].copy_from_slice(&(CONSERVATIVE_BPS as u16).to_le_bytes());
        product_rows[index][8..16].copy_from_slice(&1u64.to_le_bytes());
        product_rows[index][16..24].copy_from_slice(&1u64.to_le_bytes());
        product_rows[index][24..32].copy_from_slice(&1u64.to_le_bytes());
    }
    let mut seller_rows = [[0u8; 40]; 2];
    for index in 0..2 {
        seller_rows[index][0] = index as u8;
        let resting_order = (index == 0).then_some(resting).flatten();
        seller_rows[index][1] = meta(
            &mut metas,
            sellers[index].owner,
            false,
            resting_order.is_none(),
        );
        seller_rows[index][2] = meta(
            &mut metas,
            resting_order.map_or(sellers[index].nonce, |value| value.0),
            true,
            false,
        );
        seller_rows[index][3] = meta(
            &mut metas,
            resting_order.map_or(sellers[index].stock_source, |value| value.1),
            true,
            false,
        );
        seller_rows[index][4] = meta(&mut metas, sellers[index].cash_destination, true, false);
        seller_rows[index][5] = u8::from(resting_order.is_some());
        seller_rows[index][8..16].copy_from_slice(&0u64.to_le_bytes());
        seller_rows[index][16..24].copy_from_slice(&INTERNAL_STOCK[index].to_le_bytes());
        seller_rows[index][24..32].copy_from_slice(&INTERNAL_CASH.to_le_bytes());
        seller_rows[index][32..40].copy_from_slice(&INTERNAL_CASH.to_le_bytes());
    }
    assert!(metas.len() <= 64);
    let buyer_input = 2 * RESIDUAL_INPUT + 2 * INTERNAL_CASH;
    let mut data = vec![14, 2, 2, 2, 0, 0, 0, 0];
    data.extend_from_slice(&deadline.to_le_bytes());
    data.extend_from_slice(&150u64.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes());
    data.extend_from_slice(&buyer_input.to_le_bytes());
    data.extend_from_slice(&expected_exposure.to_le_bytes());
    data.extend_from_slice(&[
        buyer,
        buyer_nonce,
        buyer_source,
        cash_mint_index,
        cash_program_index,
        0,
        0,
        0,
    ]);
    for row in product_rows {
        data.extend_from_slice(&row);
    }
    for row in seller_rows {
        data.extend_from_slice(&row);
    }
    for (index, graph) in graphs.iter().enumerate() {
        data.push(index as u8);
        data.extend_from_slice(&(graph.data.len() as u16).to_le_bytes());
        data.push(0);
        data.extend_from_slice(&graph.data);
    }
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
    if std::env::var_os("SKEW_SETTLE_SBF_DIR").is_none() {
        std::env::set_var(
            "SKEW_SETTLE_SBF_DIR",
            root.join("artifacts/sbf-stockmesh-v5"),
        );
    }
    let mut runtime = runtime(&root);
    runtime.compute_budget.compute_unit_limit = 1_400_000;
    let cases = [
        Case::load(&root.join("artifacts/venue-matrix/cases/NVDAx-buy-raydium_clmm")),
        Case::load(&root.join("artifacts/venue-matrix/cases/TSLAx-buy-byreal")),
    ];
    let cash_mint = pk(cases[0].quote["inputMint"].as_str().unwrap());
    assert_eq!(cash_mint, pk(cases[1].quote["inputMint"].as_str().unwrap()));
    let output_mints = [
        pk(cases[0].quote["outputMint"].as_str().unwrap()),
        pk(cases[1].quote["outputMint"].as_str().unwrap()),
    ];
    let mut bank = cases[0].a.clone();
    merge_accounts(&mut bank, &cases[1].a);
    let clock_key = pubkey!("SysvarC1ock11111111111111111111111111111111");
    let slot = dex::u64_at(&get(&bank, &clock_key).data, 0).unwrap();
    runtime.sysvars.clock.slot = slot;
    runtime.sysvars.clock.unix_timestamp =
        i64::from_le_bytes(get(&bank, &clock_key).data[32..40].try_into().unwrap());
    let wallets = [
        buyer_product(&mut bank, cash_mint, output_mints[0]),
        buyer_product(&mut bank, cash_mint, output_mints[1]),
    ];
    assert_eq!(wallets[0].source, wallets[1].source);
    assert_eq!(wallets[0].nonce, wallets[1].nonce);
    let sellers = [
        seller(&mut bank, 91, cash_mint, output_mints[0]),
        seller(&mut bank, 92, cash_mint, output_mints[1]),
    ];
    let mut graphs = [
        product_graph(&cases[0], &runtime, wallets[0]),
        product_graph(&cases[1], &runtime, wallets[1]),
    ];
    let mut external_raw = [0u64; 2];
    for index in 0..2 {
        let result = run(&runtime, &graphs[index], &bank);
        assert!(
            result.program_result.is_ok(),
            "standalone {index}: {:?}",
            result.program_result
        );
        external_raw[index] =
            amount(&result.resulting_accounts, &wallets[index].destination) - START;
        let floor = u64::try_from(u128::from(external_raw[index]) * 9_980 / 10_000).unwrap();
        graphs[index].data[20..28].copy_from_slice(&floor.to_le_bytes());
    }
    let authority = Pubkey::new_from_array([18u8; 32]);
    let instrument = wire::instrument_id("NVDA").unwrap();
    let policies = [
        policy(
            &mut bank,
            authority,
            instrument,
            wire::issuer_id("SBF-0").unwrap(),
            cash_mint,
            output_mints[0],
            slot,
        ),
        policy(
            &mut bank,
            authority,
            instrument,
            wire::issuer_id("SBF-1").unwrap(),
            cash_mint,
            output_mints[1],
            slot,
        ),
    ];
    // Both policies below are individually valid mint-bound PDAs. The second
    // one deliberately names another economic instrument, proving that opcode
    // 14 rejects cross-instrument aggregation rather than merely validating
    // each product in isolation.
    let cross_instrument_policy = policy(
        &mut bank,
        authority,
        wire::instrument_id("QQQ").unwrap(),
        wire::issuer_id("SBF-1").unwrap(),
        cash_mint,
        output_mints[1],
        slot,
    );
    put(
        &mut bank,
        authority,
        Account {
            lamports: 1_000_000_000_000,
            ..Account::default()
        },
    );
    let product_raw = [
        external_raw[0] + INTERNAL_STOCK[0],
        external_raw[1] + INTERNAL_STOCK[1],
    ];
    let product_exposure = [
        exposure(product_raw[0], get(&bank, &output_mints[0]).data[44]),
        exposure(product_raw[1], get(&bank, &output_mints[1]).data[44]),
    ];
    let expected_exposure = product_exposure[0] + product_exposure[1];
    let instruction = compile(
        &bank,
        cash_mint,
        policies,
        output_mints,
        wallets,
        sellers,
        &mut graphs,
        slot + 100,
        expected_exposure,
        None,
    );
    let result = run(&runtime, &instruction, &bank);
    assert!(
        result.program_result.is_ok(),
        "StockMesh cell: {:?}",
        result.program_result
    );
    assert_eq!(&result.return_data[..8], b"SKEWMSH1");
    assert_eq!(
        START - amount(&result.resulting_accounts, &wallets[0].source),
        2 * RESIDUAL_INPUT + 2 * INTERNAL_CASH
    );
    for index in 0..2 {
        assert_eq!(
            amount(&result.resulting_accounts, &wallets[index].destination) - START,
            product_raw[index]
        );
        assert_eq!(
            START - amount(&result.resulting_accounts, &sellers[index].stock_source),
            INTERNAL_STOCK[index]
        );
        assert_eq!(
            amount(&result.resulting_accounts, &sellers[index].cash_destination) - START,
            INTERNAL_CASH
        );
    }

    let mut faults = Vec::new();
    for name in [
        "aggregate_exposure",
        "buyer_cash_conservation",
        "product_policy_version",
        "seller_limit",
        "seller_signature",
        "residual_minimum",
        "cross_instrument_policy",
    ] {
        let mut bad = instruction.clone();
        match name {
            "aggregate_exposure" => {
                bad.data[40..48].copy_from_slice(&(expected_exposure + 1).to_le_bytes())
            }
            "buyer_cash_conservation" => {
                let value = 2 * RESIDUAL_INPUT + 2 * INTERNAL_CASH + 1;
                bad.data[32..40].copy_from_slice(&value.to_le_bytes());
            }
            "product_policy_version" => bad.data[64..72].copy_from_slice(&2u64.to_le_bytes()),
            "seller_limit" => {
                let offset = 56 + 2 * 32 + 32;
                bad.data[offset..offset + 8].copy_from_slice(&(INTERNAL_CASH + 1).to_le_bytes());
            }
            "seller_signature" => {
                let seller_owner = sellers[0].owner;
                bad.accounts
                    .iter_mut()
                    .find(|meta| meta.pubkey == seller_owner)
                    .unwrap()
                    .is_signer = false;
            }
            "residual_minimum" => {
                let graph = 56 + 2 * 32 + 2 * 40 + 4;
                bad.data[graph + 20..graph + 28]
                    .copy_from_slice(&(external_raw[0] + 1).to_le_bytes());
            }
            "cross_instrument_policy" => {
                bad.accounts
                    .iter_mut()
                    .find(|meta| meta.pubkey == policies[1])
                    .unwrap()
                    .pubkey = cross_instrument_policy;
            }
            _ => unreachable!(),
        }
        let failed = run(&runtime, &bad, &bank);
        assert!(failed.program_result.is_err(), "{name}");
        assert_eq!(failed.resulting_accounts, bank, "{name} rollback");
        faults.push(json!({
            "case":name,
            "rollback":true,
            "cu":failed.compute_units_consumed,
            "status":format!("{:?}",failed.program_result)
        }));
    }

    // One seller now leaves a real, funded order in an independent PDA. The
    // placement and fill are separate transactions: seller 0 is absent from
    // the fill signature set, while seller 1 remains a live signed intent.
    const ORDER_NONCE: u64 = 77;
    let order_nonce_bytes = ORDER_NONCE.to_le_bytes();
    let (order, _) = Pubkey::find_program_address(
        &[b"onebook", sellers[0].owner.as_ref(), &order_nonce_bytes],
        &SETTLE,
    );
    let escrow = Pubkey::new_from_array([93u8; 32]);
    let mut resting_bank = bank.clone();
    let (claim, _) = Pubkey::find_program_address(
        &[
            b"claim",
            authority.as_ref(),
            &instrument,
            output_mints[0].as_ref(),
        ],
        &SETTLE,
    );
    put(&mut resting_bank, claim, Account::default());
    let product_program = get(&resting_bank, &output_mints[0]).owner;
    let mut claim_data = vec![17, 0, 1, 0];
    claim_data.extend_from_slice(&(CONSERVATIVE_BPS as u16).to_le_bytes());
    claim_data.extend_from_slice(&[0, 0]);
    claim_data.extend_from_slice(&instrument);
    claim_data.extend_from_slice(&wire::issuer_id("SBF-0").unwrap());
    claim_data.extend_from_slice(&1u64.to_le_bytes());
    claim_data.extend_from_slice(&150u64.to_le_bytes());
    claim_data.extend_from_slice(&1u64.to_le_bytes());
    claim_data.extend_from_slice(&1u64.to_le_bytes());
    claim_data.extend_from_slice(&1u64.to_le_bytes());
    claim_data.extend_from_slice(&(slot + 500).to_le_bytes());
    assert_eq!(claim_data.len(), 120);
    let publish_claim = Instruction {
        program_id: SETTLE,
        accounts: vec![
            AccountMeta::new(authority, true),
            AccountMeta::new(claim, false),
            AccountMeta::new_readonly(policies[0], false),
            AccountMeta::new_readonly(cash_mint, false),
            AccountMeta::new_readonly(output_mints[0], false),
            AccountMeta::new_readonly(product_program, false),
            AccountMeta::new_readonly(Pubkey::default(), false),
        ],
        data: claim_data,
    };
    let claimed = run(&runtime, &publish_claim, &resting_bank);
    assert!(
        claimed.program_result.is_ok(),
        "ProductClaim publish: {:?}",
        claimed.program_result
    );
    assert_eq!(
        &get(&claimed.resulting_accounts, &claim).data[..8],
        b"SKEWCLM1"
    );
    resting_bank = claimed.resulting_accounts;
    put(&mut resting_bank, order, Account::default());
    empty_token_account(&mut resting_bank, escrow, order, output_mints[0]);
    let resting_stock = INTERNAL_STOCK[0] * 2;
    let fill_exposure = exposure(
        INTERNAL_STOCK[0],
        get(&resting_bank, &output_mints[0]).data[44],
    );
    let minimum_cash_per_share =
        u64::try_from((u128::from(INTERNAL_CASH) << 32) / u128::from(fill_exposure)).unwrap();
    let mut place_data = vec![15, 0, 0, 0];
    place_data.extend_from_slice(&(CONSERVATIVE_BPS as u16).to_le_bytes());
    place_data.extend_from_slice(&[0, 0]);
    place_data.extend_from_slice(&ORDER_NONCE.to_le_bytes());
    place_data.extend_from_slice(&resting_stock.to_le_bytes());
    place_data.extend_from_slice(&minimum_cash_per_share.to_le_bytes());
    place_data.extend_from_slice(&(slot + 500).to_le_bytes());
    place_data.extend_from_slice(&1u64.to_le_bytes());
    place_data.extend_from_slice(&150u64.to_le_bytes());
    place_data.extend_from_slice(&1u64.to_le_bytes());
    place_data.extend_from_slice(&1u64.to_le_bytes());
    place_data.extend_from_slice(&instrument);
    assert_eq!(place_data.len(), 104);
    let cash_program = get(&resting_bank, &cash_mint).owner;
    let place = Instruction {
        program_id: SETTLE,
        accounts: vec![
            AccountMeta::new(sellers[0].owner, true),
            AccountMeta::new(order, false),
            AccountMeta::new(sellers[0].stock_source, false),
            AccountMeta::new(escrow, false),
            AccountMeta::new(sellers[0].cash_destination, false),
            AccountMeta::new_readonly(cash_mint, false),
            AccountMeta::new_readonly(output_mints[0], false),
            AccountMeta::new_readonly(product_program, false),
            AccountMeta::new_readonly(cash_program, false),
            AccountMeta::new_readonly(policies[0], false),
            AccountMeta::new_readonly(claim, false),
            AccountMeta::new_readonly(Pubkey::default(), false),
        ],
        data: place_data,
    };
    let placed = run(&runtime, &place, &resting_bank);
    assert!(
        placed.program_result.is_ok(),
        "OneBook place: {:?}",
        placed.program_result
    );
    assert_eq!(
        START - amount(&placed.resulting_accounts, &sellers[0].stock_source),
        resting_stock
    );
    assert_eq!(amount(&placed.resulting_accounts, &escrow), resting_stock);
    assert_eq!(
        &get(&placed.resulting_accounts, &order).data[..8],
        b"SKEWORD1"
    );
    resting_bank = placed.resulting_accounts;

    let mut resting_graphs = [
        product_graph(&cases[0], &runtime, wallets[0]),
        product_graph(&cases[1], &runtime, wallets[1]),
    ];
    for index in 0..2 {
        let floor = u64::try_from(u128::from(external_raw[index]) * 9_980 / 10_000).unwrap();
        resting_graphs[index].data[20..28].copy_from_slice(&floor.to_le_bytes());
    }
    let resting_spec = wire::MeshFillSpec {
        buyer: WALLET,
        buyer_nonce: wallets[0].nonce,
        buyer_cash_source: wallets[0].source,
        cash_mint,
        cash_token_program: cash_program,
        buyer_sequence: 0,
        buyer_input_atoms: 2 * RESIDUAL_INPUT + 2 * INTERNAL_CASH,
        minimum_exposure_q32: expected_exposure,
        deadline_slot: slot + 100,
        maximum_policy_age: 150,
        allow_underlying_closed: false,
        products: vec![
            wire::MeshProduct {
                policy: policies[0],
                claim: Some(claim),
                destination: wallets[0].destination,
                mint: output_mints[0],
                token_program: product_program,
                model: wire::FIXED_RATIONAL,
                conservative_bps: CONSERVATIVE_BPS as u16,
                policy_version: 1,
                numerator: 1,
                denominator: 1,
            },
            wire::MeshProduct {
                policy: policies[1],
                claim: None,
                destination: wallets[1].destination,
                mint: output_mints[1],
                token_program: get(&resting_bank, &output_mints[1]).owner,
                model: wire::FIXED_RATIONAL,
                conservative_bps: CONSERVATIVE_BPS as u16,
                policy_version: 1,
                numerator: 1,
                denominator: 1,
            },
        ],
        sellers: vec![
            wire::MeshSeller {
                product_index: 0,
                owner: sellers[0].owner,
                authority: wire::MeshSellerAuthority::ClaimCell { order },
                stock_source: escrow,
                cash_destination: sellers[0].cash_destination,
                sequence_or_revision: 0,
                stock_atoms: INTERNAL_STOCK[0],
                cash_atoms: INTERNAL_CASH,
                minimum_cash_atoms: INTERNAL_CASH,
            },
            wire::MeshSeller {
                product_index: 1,
                owner: sellers[1].owner,
                authority: wire::MeshSellerAuthority::Signed {
                    nonce: sellers[1].nonce,
                },
                stock_source: sellers[1].stock_source,
                cash_destination: sellers[1].cash_destination,
                sequence_or_revision: 0,
                stock_atoms: INTERNAL_STOCK[1],
                cash_atoms: INTERNAL_CASH,
                minimum_cash_atoms: INTERNAL_CASH,
            },
        ],
        residuals: resting_graphs
            .into_iter()
            .enumerate()
            .map(|(product_index, graph)| wire::MeshResidual {
                product_index,
                graph,
            })
            .collect(),
    };
    let resting_instruction = wire::compile_mesh_fill(SETTLE, resting_spec.clone()).unwrap();
    assert!(resting_instruction
        .accounts
        .iter()
        .find(|meta| meta.pubkey == sellers[0].owner)
        .is_some_and(|meta| !meta.is_signer && !meta.is_writable));
    let lookup = solana_message::AddressLookupTableAccount {
        key: Pubkey::new_from_array([77; 32]),
        addresses: resting_instruction
            .accounts
            .iter()
            .filter(|meta| !meta.is_signer)
            .map(|meta| meta.pubkey)
            .collect(),
    };
    let v0_message = wire::compile_unsigned_v0(
        WALLET,
        std::slice::from_ref(&resting_instruction),
        std::slice::from_ref(&lookup),
        [9; 32],
    )
    .unwrap();
    let mut message_signers = vec![WALLET];
    for signer in resting_instruction
        .accounts
        .iter()
        .filter(|meta| meta.is_signer)
        .map(|meta| meta.pubkey)
    {
        if !message_signers.contains(&signer) {
            message_signers.push(signer);
        }
    }
    let eventual_signed_packet_bytes =
        1 + message_signers.len().checked_mul(64).unwrap() + v0_message.len();
    assert!(eventual_signed_packet_bytes <= 1_232);
    let resting_fill = run(&runtime, &resting_instruction, &resting_bank);
    assert!(
        resting_fill.program_result.is_ok(),
        "OneBook fill: {:?}",
        resting_fill.program_result
    );
    assert_eq!(
        amount(&resting_fill.resulting_accounts, &escrow),
        INTERNAL_STOCK[0]
    );
    let filled_order = &get(&resting_fill.resulting_accounts, &order).data;
    assert_eq!(dex::u64_at(filled_order, 248).unwrap(), INTERNAL_STOCK[0]);
    assert_eq!(dex::u64_at(filled_order, 272).unwrap(), 1);
    assert_eq!(dex::u64_at(filled_order, 280).unwrap(), INTERNAL_CASH);

    let funded = std::env::var_os("SKEW_FUNDED_PROOF").map(|_| {
        funded_probe(
            &root,
            &runtime,
            &resting_bank,
            &resting_instruction,
            wallets[0],
            &resting_spec,
        )
    });

    let mut order_faults = Vec::new();
    for name in [
        "resting_limit",
        "resting_revision",
        "resting_expiry",
        "claim_conversion",
    ] {
        let mut fault_bank = resting_bank.clone();
        let mut bad = resting_instruction.clone();
        match name {
            "resting_limit" => {
                let state = &mut fault_bank
                    .iter_mut()
                    .find(|row| row.0 == order)
                    .unwrap()
                    .1
                    .data;
                let too_high = minimum_cash_per_share * 2;
                state[256..264].copy_from_slice(&too_high.to_le_bytes());
            }
            "resting_revision" => {
                let seller_row = 56 + 2 * 32;
                bad.data[seller_row + 8..seller_row + 16].copy_from_slice(&1u64.to_le_bytes());
            }
            "resting_expiry" => {
                let state = &mut fault_bank
                    .iter_mut()
                    .find(|row| row.0 == order)
                    .unwrap()
                    .1
                    .data;
                state[264..272].copy_from_slice(&slot.saturating_sub(1).to_le_bytes());
            }
            "claim_conversion" => {
                let first_product = 56;
                bad.data[first_product + 16..first_product + 24]
                    .copy_from_slice(&2u64.to_le_bytes());
            }
            _ => unreachable!(),
        }
        let failed = run(&runtime, &bad, &fault_bank);
        assert!(failed.program_result.is_err(), "{name}");
        assert_eq!(failed.resulting_accounts, fault_bank, "{name} rollback");
        order_faults.push(json!({
            "case":name,
            "rollback":true,
            "cu":failed.compute_units_consumed,
            "status":format!("{:?}",failed.program_result)
        }));
    }

    let mut cancel_data = vec![16];
    cancel_data.extend_from_slice(&[0; 7]);
    cancel_data.extend_from_slice(&ORDER_NONCE.to_le_bytes());
    let cancel = Instruction {
        program_id: SETTLE,
        accounts: vec![
            AccountMeta::new(sellers[0].owner, true),
            AccountMeta::new(order, false),
            AccountMeta::new(escrow, false),
            AccountMeta::new(sellers[0].stock_source, false),
            AccountMeta::new_readonly(output_mints[0], false),
            AccountMeta::new_readonly(product_program, false),
        ],
        data: cancel_data,
    };
    let cancelled = run(&runtime, &cancel, &resting_fill.resulting_accounts);
    assert!(
        cancelled.program_result.is_ok(),
        "cancel: {:?}",
        cancelled.program_result
    );
    assert_eq!(amount(&cancelled.resulting_accounts, &escrow), 0);
    assert_eq!(
        START - amount(&cancelled.resulting_accounts, &sellers[0].stock_source),
        INTERNAL_STOCK[0]
    );
    assert_eq!(get(&cancelled.resulting_accounts, &order).data[328], 2);
    let cancel_replay = run(&runtime, &cancel, &cancelled.resulting_accounts);
    assert!(cancel_replay.program_result.is_err());
    assert_eq!(
        cancel_replay.resulting_accounts,
        cancelled.resulting_accounts
    );

    let evidence = json!({
        "scope":"opcode 14 product-aware internal crossing plus multi-product residual DEX settlement",
        "economic_identity":"synthetic same-instrument policies over two distinct captured product mints",
        "market_state":"archived venue states; ABI/CU/rollback proof only",
        "mainnet_submission":false,
        "funded":funded,
        "buyer_count":1,
        "seller_count":2,
        "products":2,
        "internal_transfers":4,
        "external_legs":2,
        "buyer_input_atoms":2 * RESIDUAL_INPUT + 2 * INTERNAL_CASH,
        "internal_cash_atoms":2 * INTERNAL_CASH,
        "residual_input_atoms":2 * RESIDUAL_INPUT,
        "actual_exposure_q32":expected_exposure,
        "product_raw_atoms":product_raw,
        "product_exposure_q32":product_exposure,
        "graph_compiler":"host/src/swap_wire.rs::compile_swap_graph",
        "graph_compiler_sha256":format!("{:x}",Sha256::digest(fs::read(root.join("host/src/swap_wire.rs")).unwrap())),
        "cu":result.compute_units_consumed,
        "account_count":instruction.accounts.len(),
        "instruction_data_bytes":instruction.data.len(),
        "return_data":result.return_data,
        "faults":faults,
        "onebook":{
            "model":"continuous independent escrow-backed ask PDA; no epoch or global book account",
            "place_cu":placed.compute_units_consumed,
            "product_claim_publish_cu":claimed.compute_units_consumed,
            "fill_cu":resting_fill.compute_units_consumed,
            "cancel_cu":cancelled.compute_units_consumed,
            "seller_signature_on_fill":false,
            "partial_fill":true,
            "remaining_stock_atoms":INTERNAL_STOCK[0],
            "fill_composed_with_internal_live_seller":true,
            "fill_composed_with_external_dex_legs":2,
            "v0_message_bytes":v0_message.len(),
            "eventual_signed_packet_bytes":eventual_signed_packet_bytes,
            "required_signatures":message_signers.len(),
            "lookup_table_addresses":lookup.addresses.len(),
            "faults":order_faults,
            "cancel_replay_rollback":true
        },
    });
    let output = std::env::var_os("SKEW_PROOF_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("artifacts/stockmesh-cell-v5"));
    fs::create_dir_all(&output).unwrap();
    fs::write(
        output.join("proof.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    println!("{evidence}");
}

fn funded_probe(
    root: &std::path::Path,
    runtime: &mollusk_svm::Mollusk,
    original_bank: &Accounts,
    cell: &Instruction,
    buyer: WalletProduct,
    cell_spec: &wire::MeshFillSpec,
) -> serde_json::Value {
    let case = Case::load(&root.join("artifacts/venue-matrix/cases/SOL-sell-raydium_clmm"));
    assert_eq!(
        case.quote["inputMint"],
        "So11111111111111111111111111111111111111112"
    );
    assert_eq!(case.output, buyer.source);
    let original = run(runtime, &case.jup, &case.a);
    assert!(original.program_result.is_ok());
    let imported = case.lower(&original).unwrap();
    let direct = vec![imported
        .into_iter()
        .find(|(ix, direction)| {
            let (_, _, source, destination) = case.venue.bindings(*direction);
            ix.accounts[source].pubkey == case.input
                && ix.accounts[destination].pubkey == case.output
        })
        .expect("captured direct SOL/USDC funding edge")];
    let input = 3_000_000_000u64;
    let minimum_cash = dex::u64_at(&cell.data, 32).unwrap();
    let mut funding = case.graph(&direct, input, minimum_cash).unwrap();
    funding.data[28..36].copy_from_slice(&cell.data[8..16]);
    let instruction = wire::compile_funded_mesh(SETTLE, funding.clone(), cell.clone(), 1).unwrap();
    let mut bank = original_bank.clone();
    merge_accounts(&mut bank, &case.a);
    let initial_cash = 73_000_019u64;
    bank.iter_mut()
        .find(|(key, _)| *key == buyer.source)
        .unwrap()
        .1
        .data[64..72]
        .copy_from_slice(&initial_cash.to_le_bytes());
    let funding_alone = run(runtime, &funding, &bank);
    assert!(
        funding_alone.program_result.is_ok(),
        "funding alone: {:?}",
        funding_alone.program_result
    );
    let cash = amount(&funding_alone.resulting_accounts, &buyer.source) - initial_cash;
    assert!(cash > minimum_cash, "exercise observed surplus");
    let result = run(runtime, &instruction, &bank);
    assert!(
        result.program_result.is_ok(),
        "funded mesh: {:?}",
        result.program_result
    );
    assert_eq!(
        amount(&result.resulting_accounts, &buyer.source),
        initial_cash
    );
    assert_eq!(
        amount(&bank, &case.input) - amount(&result.resulting_accounts, &case.input),
        input
    );
    assert_eq!(&result.return_data[..8], b"SKEWMSF1");
    assert_eq!(dex::u64_at(&result.return_data, 16).unwrap(), input);
    assert_eq!(dex::u64_at(&result.return_data, 48).unwrap(), cash);
    assert_eq!(
        dex::u64_at(&get(&result.resulting_accounts, &buyer.nonce).data, 48).unwrap(),
        input
    );

    let product_graphs: [Instruction; 2] = cell_spec
        .residuals
        .iter()
        .map(|residual| residual.graph.clone())
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let economic = economic_graph(
        &product_graphs,
        cell_spec
            .buyer_input_atoms
            .checked_sub(2 * INTERNAL_CASH)
            .unwrap(),
        1,
    );
    let mut v2_spec = cell_spec.clone();
    v2_spec.residuals.clear();
    let v2 = wire::compile_funded_mesh_reflow(
        SETTLE,
        funding.clone(),
        v2_spec.clone(),
        economic.clone(),
    )
    .unwrap();
    let v2_result = run(runtime, &v2, &bank);
    assert!(
        v2_result.program_result.is_ok(),
        "funded global Reflow: {:?}",
        v2_result.program_result
    );
    assert_eq!(&v2_result.return_data[..8], b"SKEWMSF2");
    assert_eq!(
        amount(&v2_result.resulting_accounts, &buyer.source),
        initial_cash
    );
    assert_eq!(
        amount(&bank, &case.input) - amount(&v2_result.resulting_accounts, &case.input),
        input
    );
    assert!(dex::u64_at(&v2_result.return_data, 24).unwrap() >= v2_spec.minimum_exposure_q32);

    let v2_funding_len = usize::from(u16::from_le_bytes([v2.data[4], v2.data[5]]));
    let v2_cell = 8 + v2_funding_len;
    let v2_residual = v2_cell
        + 56
        + usize::from(v2.data[v2_cell + 1]) * 32
        + usize::from(v2.data[v2_cell + 2]) * 40;
    let v2_graph = v2_residual + 4;
    let setup_v2 = wallet_setup_global_reflow(
        runtime,
        &bank,
        &v2,
        &v2_spec,
        case.input,
        buyer.source,
        input,
    );
    let mut v2_faults = Vec::new();
    for name in [
        "wrong_sentinel",
        "non_cash_source",
        "issuer_mint_substitution",
        "shared_funding_market",
    ] {
        let mut bad = v2.clone();
        match name {
            "wrong_sentinel" => bad.data[v2_residual] = 0,
            "non_cash_source" => {
                bad.data[v2_graph + 36] = bad.data[v2_graph + 39];
            }
            "issuer_mint_substitution" => {
                bad.data[v2_graph + 40] = bad.data[v2_graph + 43];
            }
            "shared_funding_market" => {
                let funding_leg = 8 + 36 + usize::from(bad.data[9]) * 3;
                let economic_leg = v2_graph + 36 + usize::from(bad.data[v2_graph + 1]) * 3;
                bad.data[funding_leg + 14 + 2] = bad.data[economic_leg + 14 + 2];
            }
            _ => unreachable!(),
        }
        let rejected = run(runtime, &bad, &bank);
        assert!(rejected.program_result.is_err(), "{name} must fail");
        assert_eq!(rejected.resulting_accounts, bank, "{name} rollback");
        v2_faults.push(json!({
            "case":name,
            "cu":rejected.compute_units_consumed,
            "status":format!("{:?}",rejected.program_result),
            "rollback":true
        }));
    }
    let mut zero_cash = bank.clone();
    zero_cash
        .iter_mut()
        .find(|(key, _)| *key == buyer.source)
        .unwrap()
        .1
        .data[64..72]
        .fill(0);
    let zero = run(runtime, &instruction, &zero_cash);
    assert!(
        zero.program_result.is_ok(),
        "zero cash uses funding credit only"
    );
    assert_eq!(amount(&zero.resulting_accounts, &buyer.source), 0);
    assert_eq!(zero.return_data, result.return_data);

    let mut residual_spec = cell_spec.clone();
    residual_spec.sellers.clear();
    residual_spec.buyer_input_atoms = 2 * RESIDUAL_INPUT;
    let unmatched =
        wire::compile_funded_mesh_fill(SETTLE, funding.clone(), residual_spec, 1).unwrap();
    assert_eq!(
        unmatched
            .accounts
            .iter()
            .filter(|meta| meta.is_signer)
            .count(),
        1
    );
    let unmatched_result = run(runtime, &unmatched, &zero_cash);
    assert!(
        unmatched_result.program_result.is_ok(),
        "no internal match: {:?}",
        unmatched_result.program_result
    );
    assert_eq!(
        amount(&unmatched_result.resulting_accounts, &buyer.source),
        0
    );
    assert_eq!(dex::u64_at(&unmatched_result.return_data, 40).unwrap(), 0);

    let funding_len = usize::from(u16::from_le_bytes([
        instruction.data[4],
        instruction.data[5],
    ]));
    let cell_start = 8 + funding_len;
    let product_start = cell_start + 56;
    let seller_start = product_start + 2 * 32;
    let residual_start = seller_start + 2 * 40;
    let first_len = usize::from(u16::from_le_bytes([
        instruction.data[residual_start + 1],
        instruction.data[residual_start + 2],
    ]));
    let first_graph = residual_start + 4;
    let second_graph = first_graph + first_len + 4;
    let mut faults = Vec::new();
    for name in [
        "aggregate_floor",
        "funding_floor",
        "surplus_product",
        "exact_surplus_budget",
        "shared_market",
        "seller_alias",
        "duplicate_account",
        "late_residual_cpi_failure",
        "seller_nonce",
        "delegate_source",
        "close_authority_cash",
        "five_external_legs",
    ] {
        let mut bad = instruction.clone();
        let mut fault_bank = bank.clone();
        match name {
            "aggregate_floor" => {
                bad.data[cell_start + 40..cell_start + 48].copy_from_slice(&u64::MAX.to_le_bytes())
            }
            "funding_floor" => bad.data[8 + 20..8 + 28].copy_from_slice(&(cash + 1).to_le_bytes()),
            "surplus_product" => bad.data[2] = 2,
            "exact_surplus_budget" => {
                let start = second_graph + 36 + usize::from(bad.data[second_graph + 1]) * 3;
                bad.data[start + 4..start + 12].copy_from_slice(&RESIDUAL_INPUT.to_le_bytes());
            }
            "shared_market" | "seller_alias" => {
                let fund_leg = 8 + 36 + usize::from(bad.data[9]) * 3;
                let replacement = if name == "seller_alias" {
                    bad.data[seller_start + 3]
                } else {
                    let leg = first_graph + 36 + usize::from(bad.data[first_graph + 1]) * 3;
                    bad.data[leg + 14 + 2]
                };
                bad.data[fund_leg + 14 + if name == "seller_alias" { 5 } else { 2 }] = replacement;
            }
            "duplicate_account" => bad.accounts.push(bad.accounts[2].clone()),
            "late_residual_cpi_failure" => {
                let leg = first_graph + 36 + usize::from(bad.data[first_graph + 1]) * 3;
                let pool = bad.accounts[usize::from(bad.data[leg + 14 + 2])].pubkey;
                fault_bank
                    .iter_mut()
                    .find(|(key, _)| *key == pool)
                    .unwrap()
                    .1
                    .data[0] ^= 1;
            }
            "seller_nonce" => bad.data[seller_start + 40 + 8..seller_start + 40 + 16]
                .copy_from_slice(&1u64.to_le_bytes()),
            "delegate_source" | "close_authority_cash" => {
                let address = if name == "delegate_source" {
                    case.input
                } else {
                    buyer.source
                };
                let offset = if name == "delegate_source" { 72 } else { 129 };
                fault_bank
                    .iter_mut()
                    .find(|(key, _)| *key == address)
                    .unwrap()
                    .1
                    .data[offset] = 1;
            }
            "five_external_legs" => {
                let fund = &instruction.data[8..8 + funding_len];
                let leg_start = 36 + usize::from(fund[1]) * 3;
                let mut expanded = fund.to_vec();
                expanded[2] = 3;
                expanded.extend_from_slice(&fund[leg_start..]);
                expanded.extend_from_slice(&fund[leg_start..]);
                bad.data = instruction.data[..8].to_vec();
                bad.data[4..6].copy_from_slice(&(expanded.len() as u16).to_le_bytes());
                bad.data.extend_from_slice(&expanded);
                bad.data.extend_from_slice(&instruction.data[cell_start..]);
            }
            _ => unreachable!(),
        }
        let rejected = run(runtime, &bad, &fault_bank);
        assert!(rejected.program_result.is_err(), "{name} must fail");
        assert_eq!(
            rejected.resulting_accounts, fault_bank,
            "{name}: funding and clearing rollback"
        );
        faults.push(json!({"case":name,"cu":rejected.compute_units_consumed,"status":format!("{:?}",rejected.program_result),"rollback":true}));
    }
    let replay = run(runtime, &instruction, &result.resulting_accounts);
    assert!(replay.program_result.is_err());
    assert_eq!(replay.resulting_accounts, result.resulting_accounts);
    let output = std::env::var_os("SKEW_FUNDED_PROOF_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("artifacts/stockmesh-funded-v8"));
    fs::create_dir_all(&output).unwrap();
    let mut network_spec = v2_spec.clone();
    for product in &mut network_spec.products {
        product.claim = None;
    }
    for seller in &mut network_spec.sellers {
        if matches!(
            seller.authority,
            wire::MeshSellerAuthority::ClaimCell { .. }
        ) {
            let nonce =
                Pubkey::find_program_address(&[b"stocklana", seller.owner.as_ref()], &SETTLE).0;
            let product = network_spec.products[seller.product_index];
            let stock_source = bank
                .iter()
                .find(|(_, account)| {
                    account.owner == product.token_program
                        && account.data.len() >= 165
                        && account.data[..32] == product.mint.to_bytes()
                        && account.data[32..64] == seller.owner.to_bytes()
                })
                .map(|(key, _)| *key)
                .unwrap();
            seller.authority = wire::MeshSellerAuthority::Signed { nonce };
            seller.stock_source = stock_source;
            seller.sequence_or_revision = 0;
        }
    }
    let network_v2 =
        wire::compile_funded_mesh_reflow(SETTLE, funding.clone(), network_spec, economic.clone())
            .unwrap();
    assert_eq!(
        network_v2
            .accounts
            .iter()
            .filter(|meta| meta.is_signer)
            .count(),
        3
    );
    let network_result = run(runtime, &network_v2, &bank);
    assert!(
        network_result.program_result.is_ok(),
        "residual-only global reflow: {:?}",
        network_result.program_result
    );
    let network_fixture = write_network_fixture(
        &output,
        runtime.sysvars.clock.slot,
        &network_v2,
        &bank,
        &network_result,
    );
    let envelope = funded_vector(
        &output,
        runtime.sysvars.clock.slot,
        &instruction,
        &bank,
        &result,
    );
    let no_match = funded_vector(
        &output.join("no-match"),
        runtime.sysvars.clock.slot,
        &unmatched,
        &zero_cash,
        &unmatched_result,
    );
    let no_match_proof = json!({"hostVectorSha256":no_match["hostVectorSha256"],"signedPacketBytes":no_match["signedPacketBytes"],
        "actualExposureQ32":dex::u64_at(&unmatched_result.return_data,24).unwrap(),"sellerCount":0,"productCount":2,
        "preexistingCashAtoms":0,"cu":unmatched_result.compute_units_consumed,"mainnet":false});
    fs::write(
        output.join("no-match/proof.json"),
        serde_json::to_vec_pretty(&no_match_proof).unwrap(),
    )
    .unwrap();
    let v2_vector = funded_vector(
        &output.join("global-reflow"),
        runtime.sysvars.clock.slot,
        &v2,
        &bank,
        &v2_result,
    );
    let v2_proof = json!({
        "hostVectorSha256":v2_vector["hostVectorSha256"],
        "sellerCount":v2_spec.sellers.len(),
        "preexistingCashAtoms":initial_cash,
        "actualExposureQ32":dex::u64_at(&v2_result.return_data,24).unwrap(),
        "signedPacketBytes":v2_vector["signedPacketBytes"],
        "cu":v2_result.compute_units_consumed,
        "returnTag":"SKEWMSF2",
        "mainnet":false
    });
    fs::write(
        output.join("global-reflow/proof.json"),
        serde_json::to_vec_pretty(&v2_proof).unwrap(),
    )
    .unwrap();
    let report = json!({"opcode":18,"cu":result.compute_units_consumed,"fundingAloneCu":funding_alone.compute_units_consumed,
        "sourceInputAtoms":input,"fundedCashAtoms":cash,"signedMinimumCashAtoms":minimum_cash,"observedSurplusAtoms":cash-minimum_cash,
        "preexistingCashAtoms":initial_cash,"finalCashAtoms":amount(&result.resulting_accounts,&buyer.source),"zeroInitialCashPassed":true,"noInternalMatchPassed":true,"noInternalMatchCu":unmatched_result.compute_units_consumed,
        "productCount":2,"sellerCount":2,"fundingLegs":1,"residualLegs":2,"claimCellAndLiveSeller":true,
        "requiredSignatures":envelope["requiredSignatures"],"signedPacketBytes":envelope["signedPacketBytes"],"messageBytes":envelope["messageBytes"],"accounts":instruction.accounts.len(),
        "actualExposureQ32":dex::u64_at(&result.return_data,24).unwrap(),"nonceReplayRollback":true,"faults":faults,
        "hostVectorSha256":envelope["hostVectorSha256"],"surplusPolicy":"signed final residual product; not global reoptimization",
        "globalReflowV2":{"cu":v2_result.compute_units_consumed,"returnTag":"SKEWMSF2","actualExposureQ32":dex::u64_at(&v2_result.return_data,24).unwrap(),
            "fundingLegs":1,"candidateVenues":["Raydium CLMM","Byreal CLMM"],"candidateProducts":2,"maximumProductExecutions":3,
            "preexistingCashPreserved":true,"sourceInputAtoms":input,"fundedCashAtoms":cash,"faults":v2_faults,"vector":v2_vector,
            "walletSetup":setup_v2,"isolatedValidatorFixture":network_fixture},
        "noMatchVector":no_match,"mainnet":false,"scope":"captured program/account SBF with synthetic wallet balances and same-instrument policies"});
    fs::write(
        output.join("proof.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    report
}

fn write_network_fixture(
    output: &std::path::Path,
    slot: u64,
    instruction: &Instruction,
    bank: &Accounts,
    result: &mollusk_svm::result::types::TransactionResult,
) -> serde_json::Value {
    let lookup = Pubkey::new_from_array([75; 32]);
    let fixture = json!({
        "schema":"skew.stockmesh-v2-network-fixture/v1",
        "scope":"captured programs/state with synthetic product policies and wallet funds; isolated validator only",
        "slot":slot,
        "wallets":instruction.accounts.iter().filter(|meta|meta.is_signer)
            .map(|meta|meta.pubkey.to_string()).collect::<Vec<_>>(),
        "instruction":{
            "programId":instruction.program_id.to_string(),
            "accounts":instruction.accounts.iter().map(|meta|json!({
                "pubkey":meta.pubkey.to_string(),
                "isSigner":meta.is_signer,
                "isWritable":meta.is_writable
            })).collect::<Vec<_>>(),
            "data":STANDARD.encode(&instruction.data)
        },
        "lookupTable":{
            "key":lookup.to_string(),
            "addresses":instruction.accounts.iter().filter(|meta|!meta.is_signer)
                .map(|meta|meta.pubkey.to_string()).collect::<Vec<_>>()
        },
        "accounts":bank.iter().map(|(key,account)|json!({
            "pubkey":key.to_string(),
            "owner":account.owner.to_string(),
            "lamports":account.lamports,
            "executable":account.executable,
            "rent_epoch":account.rent_epoch,
            "data":STANDARD.encode(&account.data)
        })).collect::<Vec<_>>(),
        "expected":{
            "cu":result.compute_units_consumed,
            "returnData":STANDARD.encode(&result.return_data)
        }
    });
    let bytes = serde_json::to_vec_pretty(&fixture).unwrap();
    fs::write(output.join("global-reflow-network-fixture.json"), &bytes).unwrap();
    json!({
        "path":"global-reflow-network-fixture.json",
        "sha256":format!("{:x}",Sha256::digest(&bytes)),
        "molluskCu":result.compute_units_consumed,
        "sellerCount":2,
        "requiredSignatures":instruction.accounts.iter().filter(|meta|meta.is_signer).count()
    })
}

fn wallet_setup_global_reflow(
    runtime: &mollusk_svm::Mollusk,
    original: &Accounts,
    v2: &Instruction,
    spec: &wire::MeshFillSpec,
    input_token: Pubkey,
    cash_token: Pubkey,
    input_atoms: u64,
) -> serde_json::Value {
    let input = token_asset(original, input_token);
    let cash = token_asset(original, cash_token);
    let mut assets = vec![input, cash];
    assets.extend(spec.products.iter().map(|product| TokenAsset {
        token: product.destination,
        mint: product.mint,
        token_program: product.token_program,
    }));
    assert_eq!(assets.len(), 4);

    // Exercise both kinds of wallet initialization in the same atomic wire:
    // the native input ATA and one product ATA start absent, as does the nonce.
    let mut bank = original.clone();
    for address in [input.token, assets[2].token, spec.buyer_nonce] {
        put(&mut bank, address, Account::default());
    }
    let ata = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
    put(
        &mut bank,
        ata,
        mollusk_svm::program::create_program_account_loader_v3(&ata),
    );
    let setup = WalletSetup::plan(WALLET, &assets, Some(input_atoms), |key| {
        account_view(&bank, key)
    })
    .unwrap()
    .with_nonce(SETTLE, 0, |key| account_view(&bank, key))
    .unwrap();
    assert_eq!(setup.created_assets().count(), 2);
    assert_eq!(setup.created_nonce(), Some(spec.buyer_nonce));

    let initialized = runtime.process_transaction_instructions(setup.instructions(), &bank);
    assert!(
        initialized.program_result.is_ok(),
        "v2 setup-only: {:?}",
        initialized.program_result
    );
    setup
        .verify_initialized(|key| account_view(&initialized.resulting_accounts, key))
        .unwrap();

    let mut full_wire = setup.instructions().to_vec();
    full_wire.push(v2.clone());
    let executed = runtime.process_transaction_instructions(&full_wire, &bank);
    assert!(
        executed.program_result.is_ok(),
        "wallet setup plus v2: {:?}",
        executed.program_result
    );
    assert_eq!(executed.return_data, run(runtime, v2, original).return_data);
    assert_eq!(amount(&executed.resulting_accounts, &input.token), 0);
    assert_eq!(
        amount(&executed.resulting_accounts, &cash.token),
        amount(original, &cash.token)
    );
    assert!(executed.compute_units_consumed <= 1_400_000);

    let nonce_rent = get(&executed.resulting_accounts, &spec.buyer_nonce).lamports;
    let rents = setup.created_assets().fold(nonce_rent, |sum, asset| {
        let account = get(&executed.resulting_accounts, &asset.token);
        let rent = if asset.mint == pubkey!("So11111111111111111111111111111111111111112") {
            account
                .lamports
                .checked_sub(amount(&executed.resulting_accounts, &asset.token))
                .unwrap()
        } else {
            account.lamports
        };
        sum.checked_add(rent).unwrap()
    });
    let payer_debit = get(&bank, &WALLET)
        .lamports
        .checked_sub(get(&executed.resulting_accounts, &WALLET).lamports)
        .unwrap();
    assert_eq!(payer_debit, rents + input_atoms);

    let funding_len = usize::from(u16::from_le_bytes([v2.data[4], v2.data[5]]));
    let cell = 8 + funding_len;
    let mut failing_v2 = v2.clone();
    failing_v2.data[cell + 40..cell + 48].copy_from_slice(&u64::MAX.to_le_bytes());
    let mut failing_wire = setup.instructions().to_vec();
    failing_wire.push(failing_v2);
    let rejected = runtime.process_transaction_instructions(&failing_wire, &bank);
    assert!(rejected.program_result.is_err());
    assert_eq!(rejected.resulting_accounts, bank);

    let mut compute_data = vec![2];
    compute_data.extend_from_slice(&1_400_000u32.to_le_bytes());
    let mut envelope = vec![Instruction {
        program_id: pubkey!("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data: compute_data,
    }];
    envelope.extend(full_wire);
    assert!(envelope.len() <= 8);
    let lookup = solana_message::AddressLookupTableAccount {
        key: Pubkey::new_from_array([77; 32]),
        addresses: envelope
            .iter()
            .flat_map(|instruction| &instruction.accounts)
            .filter(|meta| !meta.is_signer)
            .map(|meta| meta.pubkey)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    };
    let message = wire::compile_unsigned_v0(WALLET, &envelope, &[lookup], [76; 32]).unwrap();
    let signatures = envelope
        .iter()
        .flat_map(|instruction| &instruction.accounts)
        .filter(|meta| meta.is_signer)
        .map(|meta| meta.pubkey)
        .chain(std::iter::once(WALLET))
        .collect::<BTreeSet<_>>()
        .len();
    let packet = 1 + signatures * 64 + message.len();
    assert!(packet <= 1_232);

    json!({
        "combinedSbfTransaction":true,
        "nativeSolWrapped":true,
        "createdAtas":setup.created_assets().count(),
        "createdNonce":true,
        "setupOnlyCu":initialized.compute_units_consumed,
        "fullWireCu":executed.compute_units_consumed,
        "instructionsIncludingComputeBudget":envelope.len(),
        "messageBytes":message.len(),
        "requiredSignatures":signatures,
        "eventualSignedPacketBytes":packet,
        "payerLamportsDebited":payer_debit,
        "rentLamports":rents,
        "wrapLamports":input_atoms,
        "lateExposureFloorRollsBackSetup":true,
        "syntheticFrozenAltForPacketSizing":true,
        "mainnet":false
    })
}

fn funded_vector(
    output: &std::path::Path,
    slot: u64,
    instruction: &Instruction,
    bank: &Accounts,
    result: &mollusk_svm::result::types::TransactionResult,
) -> serde_json::Value {
    let lookup = solana_message::AddressLookupTableAccount {
        key: Pubkey::new_from_array([78; 32]),
        addresses: instruction
            .accounts
            .iter()
            .filter(|meta| !meta.is_signer)
            .map(|meta| meta.pubkey)
            .collect(),
    };
    let mut cu_data = vec![2];
    cu_data.extend_from_slice(&1_400_000u32.to_le_bytes());
    let compute = Instruction {
        program_id: pubkey!("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data: cu_data,
    };
    let message = wire::compile_unsigned_v0(
        WALLET,
        &[compute, instruction.clone()],
        std::slice::from_ref(&lookup),
        [9; 32],
    )
    .unwrap();
    let signers = instruction
        .accounts
        .iter()
        .filter(|meta| meta.is_signer)
        .count();
    let packet_bytes = 1 + signers * 64 + message.len();
    assert!(packet_bytes <= 1232 && instruction.accounts.len() <= 64);
    fs::create_dir_all(output).unwrap();
    let vector = json!({"schema":"skew.funded-mesh-sbf-vector/v1","scope":"captured SBF; synthetic same-instrument product policies; not mainnet",
        "program":SETTLE.to_string(),"slot":slot,"cu":result.compute_units_consumed,
        "messageBase64":STANDARD.encode(&message),"returnData":STANDARD.encode(&result.return_data),
        "lookup":{"key":lookup.key.to_string(),"addresses":lookup.addresses.iter().map(|key|key.to_string()).collect::<Vec<_>>()},
        "before":bank.iter().map(|(k,a)|json!({"key":k.to_string(),"owner":a.owner.to_string(),"lamports":a.lamports,"executable":a.executable,"data":[STANDARD.encode(&a.data),"base64"]})).collect::<Vec<_>>(),
        "after":result.resulting_accounts.iter().map(|(k,a)|json!({"key":k.to_string(),"owner":a.owner.to_string(),"lamports":a.lamports,"executable":a.executable,"data":[STANDARD.encode(&a.data),"base64"]})).collect::<Vec<_>>()});
    let vector_bytes = serde_json::to_vec_pretty(&vector).unwrap();
    fs::write(output.join("host-vector.json"), &vector_bytes).unwrap();
    json!({"hostVectorSha256":format!("{:x}",Sha256::digest(&vector_bytes)),
        "requiredSignatures":signers,"signedPacketBytes":packet_bytes,"messageBytes":message.len()})
}
