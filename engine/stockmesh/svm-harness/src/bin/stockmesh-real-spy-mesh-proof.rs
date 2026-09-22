//! Coherent-bank StockMesh proof for two actual SPY product mints.
//!
//! The buyer receives Backed SPYx from an internal seller and Ondo SPYon from
//! a two-leg USDC -> SPYx -> SPYon residual graph in the same opcode-14 SBF
//! instruction. Market, mint and deployed-program accounts come from one final
//! public-RPC response. Wallet balances and mint-bound product policies are
//! explicit fixtures. This proves execution composition, not legal equivalence,
//! issuer eligibility, signing, submission or a mainnet fill.
#[allow(dead_code)]
#[path = "../matrix.rs"]
mod matrix;

use base64::{engine::general_purpose::STANDARD, Engine};
use matrix::{amount, get, put, token, Accounts, SETTLE, START, WALLET};
use mollusk_svm::Mollusk;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use skew_execution_host::{
    feed::{Account as HostAccount, Snapshot},
    market::{MarketConfig, Venue},
    native_wire, onebook_wire as wire,
    rpc::Rpc,
    swap_wire::{self, AccountView, SwapGraph},
    world::NativeSwapProposal,
};
use skew_native::{dlmm::DlmmCurve, ScaledUiAmount, TransferFee};
use solana_account::Account;
use solana_instruction::Instruction;
use solana_pubkey::{pubkey, Pubkey};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
const CLOCK: Pubkey = pubkey!("SysvarC1ock11111111111111111111111111111111");
const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN22: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const MEMO: Pubkey = pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
const UPGRADEABLE: Pubkey = pubkey!("BPFLoaderUpgradeab1e11111111111111111111111");
const USDC: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const SPYX: Pubkey = pubkey!("XsoCS1TfEyfFhfvj8EtZ528L3CaKBDBRqRapnBbDF2W");
const SPYON: Pubkey = pubkey!("k18WJUULWheRkSpSquYGdNNmtuE2Vbw1hpuUi92ondo");
const RESIDUAL_USDC: u64 = 10_000_000;
const INTERNAL_USDC: u64 = 100_000;
const INTERNAL_SPYX: u64 = 100_000;
const CONSERVATIVE_BPS: u16 = 9_980;
const POLICY_LEN: usize = 225;

fn pk(value: &str) -> Pubkey {
    value
        .parse()
        .unwrap_or_else(|error| panic!("invalid public key {value}: {error}"))
}

fn read(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn fetch(rpc: &Rpc, keys: &[String], minimum_slot: Option<u64>) -> Value {
    assert!(!keys.is_empty() && keys.len() <= 100);
    let mut config = json!({"encoding":"base64","commitment":"confirmed"});
    if let Some(slot) = minimum_slot {
        config["minContextSlot"] = json!(slot);
    }
    rpc.call("getMultipleAccounts", json!([keys, config]))
        .unwrap()
}

fn snapshot(keys: &[String], bank: &Value) -> Snapshot {
    let rows = bank["value"].as_array().unwrap();
    assert_eq!(keys.len(), rows.len());
    Snapshot {
        slot: bank["context"]["slot"].as_u64().unwrap(),
        generation: 1,
        revision: 1,
        hash: [0; 32],
        observed: Instant::now(),
        slot_advanced: Instant::now(),
        accounts: keys
            .iter()
            .zip(rows)
            .map(|(key, row)| {
                if row.is_null() {
                    HostAccount {
                        key: key.clone(),
                        owner: Pubkey::default().to_string(),
                        executable: false,
                        lamports: 0,
                        data: vec![],
                    }
                } else {
                    HostAccount {
                        key: key.clone(),
                        owner: row["owner"].as_str().unwrap().into(),
                        executable: row["executable"].as_bool().unwrap(),
                        lamports: row["lamports"].as_u64().unwrap(),
                        data: STANDARD.decode(row["data"][0].as_str().unwrap()).unwrap(),
                    }
                }
            })
            .collect(),
    }
}

fn accounts(keys: &[String], bank: &Value, allowed_absent: &BTreeSet<Pubkey>) -> Accounts {
    keys.iter()
        .zip(bank["value"].as_array().unwrap())
        .map(|(key, row)| {
            let key = pk(key);
            let account = if row.is_null() {
                assert!(
                    allowed_absent.contains(&key),
                    "unexpected absent account {key}"
                );
                Account::default()
            } else {
                Account {
                    lamports: row["lamports"].as_u64().unwrap(),
                    owner: pk(row["owner"].as_str().unwrap()),
                    executable: row["executable"].as_bool().unwrap(),
                    data: STANDARD.decode(row["data"][0].as_str().unwrap()).unwrap(),
                    rent_epoch: 0,
                }
            };
            (key, account)
        })
        .collect()
}

fn snapshot_from_accounts(accounts: &Accounts, slot: u64) -> Snapshot {
    Snapshot {
        slot,
        generation: 1,
        revision: 1,
        hash: [0; 32],
        observed: Instant::now(),
        slot_advanced: Instant::now(),
        accounts: accounts
            .iter()
            .map(|(key, account)| HostAccount {
                key: key.to_string(),
                owner: account.owner.to_string(),
                executable: account.executable,
                lamports: account.lamports,
                data: account.data.clone(),
            })
            .collect(),
    }
}

fn token_account(accounts: &mut Accounts, authority: Pubkey, mint: Pubkey) -> Pubkey {
    let token_program = get(accounts, &mint).owner;
    let address = Pubkey::find_program_address(
        &[authority.as_ref(), token_program.as_ref(), mint.as_ref()],
        &pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
    )
    .0;
    let mut account = token(mint, token_program, &get(accounts, &mint).data);
    account.data[32..64].copy_from_slice(authority.as_ref());
    put(accounts, address, account);
    address
}

fn nonce(accounts: &mut Accounts, owner: Pubkey) -> Pubkey {
    let address = Pubkey::find_program_address(&[b"stocklana", owner.as_ref()], &SETTLE).0;
    let mut data = vec![0; 64];
    data[..8].copy_from_slice(b"SKEWSEQ1");
    data[8..40].copy_from_slice(owner.as_ref());
    put(
        accounts,
        address,
        Account {
            lamports: 10_000_000,
            data,
            owner: SETTLE,
            ..Account::default()
        },
    );
    address
}

fn merge_accounts(left: &mut Accounts, right: &Accounts) {
    for (key, value) in right {
        if let Some(existing) = left.iter_mut().find(|row| row.0 == *key) {
            if *key == CLOCK {
                if stocklana_adapters::u64_at(&value.data, 0).unwrap_or(0)
                    > stocklana_adapters::u64_at(&existing.1.data, 0).unwrap_or(0)
                {
                    existing.1 = value.clone();
                }
            } else if existing.1 != *value {
                assert!(
                    [TOKEN, TOKEN22].contains(&existing.1.owner),
                    "conflicting non-token account {key}"
                );
            }
        } else {
            left.push((*key, value.clone()));
        }
    }
}

fn policy(
    accounts: &mut Accounts,
    authority: Pubkey,
    instrument: [u8; 32],
    issuer: [u8; 32],
    output: Pubkey,
    rights: [u8; 32],
    slot: u64,
) -> Pubkey {
    let address = Pubkey::find_program_address(
        &[
            b"stock2",
            authority.as_ref(),
            &instrument,
            USDC.as_ref(),
            output.as_ref(),
            &rights,
        ],
        &SETTLE,
    )
    .0;
    let mut data = vec![0; POLICY_LEN];
    data[..8].copy_from_slice(b"SKEWSTK2");
    data[8..40].copy_from_slice(authority.as_ref());
    data[40..72].copy_from_slice(&instrument);
    data[72..104].copy_from_slice(&issuer);
    data[104..136].copy_from_slice(USDC.as_ref());
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
            ..Account::default()
        },
    );
    address
}

fn fixture_rights(instrument: [u8; 32], issuer: [u8; 32], output: Pubkey) -> [u8; 32] {
    Sha256::digest(
        [
            b"SKEW_FIXTURE_RIGHTS_V2".as_slice(),
            &instrument,
            &issuer,
            USDC.as_ref(),
            output.as_ref(),
        ]
        .concat(),
    )
    .into()
}

fn hash32(value: &str) -> [u8; 32] {
    assert_eq!(value.len(), 64);
    let mut output = [0; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            _ => panic!("non-canonical rights hash"),
        };
        output[index] = digit(pair[0]) * 16 + digit(pair[1]);
    }
    output
}

fn manifest_rights(manifest: &Value, mint: Pubkey) -> [u8; 32] {
    manifest["banks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|bank| bank["lanes"].as_array().unwrap())
        .find(|lane| lane["output_mint"] == mint.to_string())
        .and_then(|lane| lane["rights_hash"].as_str())
        .map(hash32)
        .unwrap_or_else(|| panic!("rights manifest lacks {mint}"))
}

fn manifest_product_identifier_verified(manifest: &Value, mint: Pubkey, isin: &str) -> bool {
    manifest["banks"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|bank| bank["lanes"].as_array().into_iter().flatten())
        .find(|lane| lane["output_mint"] == mint.to_string())
        .is_some_and(|lane| {
            lane["rights_document"]["productIdentifierScheme"] == "ISIN"
                && lane["rights_document"]["productIdentifier"] == isin
                && lane["rights_document"]["productIdentifierStatus"] == "VERIFIED_ISSUER_SOURCE"
        })
}

fn hex32(value: [u8; 32]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn add_program(runtime: &mut Mollusk, accounts: &Accounts, program: Pubkey) -> Value {
    let account = get(accounts, &program);
    assert!(account.executable);
    let elf = if account.owner == UPGRADEABLE {
        assert_eq!(&account.data[..4], &[2, 0, 0, 0]);
        let program_data = Pubkey::new_from_array(account.data[4..36].try_into().unwrap());
        let account = get(accounts, &program_data);
        assert_eq!(account.owner, UPGRADEABLE);
        assert_eq!(&account.data[..4], &[3, 0, 0, 0]);
        &account.data[45..]
    } else {
        &account.data
    };
    assert_eq!(&elf[..4], b"\x7fELF");
    runtime.add_program_with_loader_and_elf(&program, &account.owner, elf);
    json!({"program":program.to_string(),"elfSha256":format!("{:x}",Sha256::digest(elf))})
}

fn dlmm_quote(config: &MarketConfig, bank: &Snapshot, input: u64) -> u64 {
    let account = |key: &str| bank.accounts.iter().find(|row| row.key == key).unwrap();
    let pool = account(&config.pool);
    let mints = [
        Pubkey::new_from_array(stocklana_adapters::key(&pool.data, 88).unwrap()),
        Pubkey::new_from_array(stocklana_adapters::key(&pool.data, 120).unwrap()),
    ];
    assert_eq!(pk(&config.input_mint), mints[0]);
    assert_eq!(pk(&config.output_mint), mints[1]);
    let clock = account(&config.clock);
    let epoch = stocklana_adapters::u64_at(&clock.data, 16).unwrap();
    let time = stocklana_adapters::u64_at(&clock.data, 32).unwrap();
    let fees =
        mints.map(|mint| TransferFee::decode(&account(&mint.to_string()).data, epoch).unwrap());
    let arrays = config
        .tick_arrays
        .iter()
        .map(|key| account(key).data.as_slice())
        .collect::<Vec<_>>();
    DlmmCurve::decode(
        pk(&config.pool).to_bytes(),
        &pool.data,
        &arrays,
        fees,
        bank.slot,
        time,
    )
    .unwrap()
    .quote(input, true)
    .unwrap()
}

fn assert_rollback(runtime: &Mollusk, instruction: &Instruction, bank: &Accounts, name: &str) {
    let result = matrix::run(runtime, instruction, bank);
    assert!(
        result.program_result.is_err(),
        "fault {name} unexpectedly passed"
    );
    assert_eq!(
        result.resulting_accounts, *bank,
        "fault {name} mutated state"
    );
}

fn main() {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "/srv/skew/stocklana-engine-20260912".into()),
    );
    assert_eq!(std::env::current_dir().unwrap(), root);
    let policy_manifest = std::env::var_os("SKEW_PRODUCT_POLICY_MANIFEST")
        .map(PathBuf::from)
        .map(|path| read(&path));
    let policy_rights_source = if policy_manifest.is_some() {
        "OFFICIAL_SOURCE_BOUND_MANIFEST"
    } else {
        "SYNTHETIC_MINT_BOUND_FIXTURE"
    };
    let product_specific_offering_verified = policy_manifest.as_ref().is_some_and(|manifest| {
        manifest_product_identifier_verified(manifest, SPYON, "VGG7001AAA21")
    });
    let evidence = root.join("artifacts/issuer-discovery-v11-20260914");
    let world = read(&root.join("artifacts/cherry-chicago-rc5/config/spy-usdc.json"));
    let stock_markets = world["markets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| serde_json::from_value::<MarketConfig>(row.clone()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(stock_markets.len(), 3);

    let direct = read(&evidence.join("direct-secondary-proof.json"));
    let passed = direct["products"]
        .as_array()
        .unwrap()
        .iter()
        .find(|product| product["symbol"] == "SPYon")
        .unwrap()["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| {
            row["status"] == "PASSED_CAPTURED_SBF"
                && row["inputMint"] == SPYX.to_string()
                && row["outputMint"] == SPYON.to_string()
        })
        .unwrap();
    let discovery = read(&evidence.join("SPYon-secondary-bank.json"));
    let pool = discovery["pools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pool| pool["pool"] == passed["pool"])
        .unwrap();
    let dlmm = MarketConfig {
        venue: Venue::MeteoraDlmm,
        program: Venue::MeteoraDlmm.program().into(),
        pool: pool["pool"].as_str().unwrap().into(),
        config: String::new(),
        input_mint: SPYX.to_string(),
        output_mint: SPYON.to_string(),
        tick_arrays: pool["tick_arrays"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row.as_str().unwrap().into())
            .collect(),
        array_capacity: None,
        clock: CLOCK.to_string(),
    };

    let mut discovery_programs = stock_markets
        .iter()
        .map(|market| pk(&market.program))
        .collect::<BTreeSet<_>>();
    discovery_programs.extend([pk(&dlmm.program), TOKEN, TOKEN22, MEMO]);
    let mut discovery_keys = BTreeSet::new();
    for market in &stock_markets {
        discovery_keys.extend(market.keys());
    }
    discovery_keys.extend(dlmm.keys());
    discovery_keys.extend(discovery_programs.iter().map(ToString::to_string));
    let discovery_keys = discovery_keys.into_iter().collect::<Vec<_>>();
    let rpc = Rpc::pinned_with_response_budget(
        "https://api.mainnet-beta.solana.com".into(),
        MAINNET_GENESIS.into(),
        32 * 1024 * 1024,
    )
    .unwrap();
    let discovery_bank = fetch(&rpc, &discovery_keys, None);
    let discovery_slot = discovery_bank["context"]["slot"].as_u64().unwrap();
    let discovery_snapshot = snapshot(&discovery_keys, &discovery_bank);
    let mut stock_quotes = Vec::new();
    let mut admitted_stock_markets = Vec::new();
    let mut selected = None;
    let forced_stock_pool = std::env::var("SKEW_FORCE_STOCK_POOL").ok();
    if let Some(pool) = &forced_stock_pool {
        assert!(
            stock_markets.iter().any(|market| &market.pool == pool),
            "forced stock pool is not in the pinned world"
        );
    }
    for market in &stock_markets {
        let quote = market
            .compile(&discovery_snapshot)
            .and_then(|market| market.quote(RESIDUAL_USDC));
        match quote {
            Ok(output) => {
                stock_quotes.push(json!({"venue":format!("{:?}",market.venue),"pool":market.pool,"status":"DISCOVERY_NATIVE_ADMITTED","outputAtoms":output.to_string()}));
                admitted_stock_markets.push(market.clone());
                if forced_stock_pool
                    .as_ref()
                    .is_none_or(|pool| pool == &market.pool)
                    && selected
                        .as_ref()
                        .is_none_or(|(_, prior): &(&MarketConfig, u64)| output > *prior)
                {
                    selected = Some((market, output));
                }
            }
            Err(error) => stock_quotes.push(json!({"venue":format!("{:?}",market.venue),"pool":market.pool,"status":"DISCOVERY_NATIVE_REJECTED","reason":error})),
        }
    }
    let (stock_market, discovery_spyx_out) =
        selected.expect("no discovery USDC to SPYx market admitted");
    let mut execution_programs = admitted_stock_markets
        .iter()
        .map(|market| pk(&market.program))
        .collect::<BTreeSet<_>>();
    execution_programs.extend([pk(&dlmm.program), TOKEN, TOKEN22, MEMO]);
    let programs = BTreeSet::from([
        pk(&stock_market.program),
        pk(&dlmm.program),
        TOKEN,
        TOKEN22,
        MEMO,
    ]);

    let mut final_keys = BTreeSet::new();
    for market in &admitted_stock_markets {
        final_keys.extend(
            native_wire::execution_dependencies(market, &discovery_snapshot)
                .unwrap()
                .into_iter()
                .map(|key| key.to_string()),
        );
    }
    final_keys.extend(
        native_wire::execution_dependencies(&dlmm, &discovery_snapshot)
            .unwrap()
            .into_iter()
            .map(|key| key.to_string()),
    );
    for program in &execution_programs {
        final_keys.insert(program.to_string());
        let row = discovery_snapshot
            .accounts
            .iter()
            .find(|row| row.key == program.to_string())
            .unwrap();
        if row.owner == UPGRADEABLE.to_string() {
            assert_eq!(&row.data[..4], &[2, 0, 0, 0]);
            final_keys
                .insert(Pubkey::new_from_array(row.data[4..36].try_into().unwrap()).to_string());
        }
    }
    let event = Pubkey::find_program_address(&[b"__event_authority"], &pk(&dlmm.program)).0;
    final_keys.insert(event.to_string());
    let final_keys = final_keys.into_iter().collect::<Vec<_>>();
    assert!(final_keys.len() <= 100);
    let final_bank = fetch(&rpc, &final_keys, Some(discovery_slot));
    let slot = final_bank["context"]["slot"].as_u64().unwrap();
    let bank_bytes =
        serde_json::to_vec_pretty(&json!({"keys":final_keys,"bank":final_bank})).unwrap();
    let bank_sha = format!("{:x}", Sha256::digest(&bank_bytes));
    let output = std::env::var_os("SKEW_REAL_SPY_PROOF_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("artifacts/stockmesh-real-spy-v1"));
    fs::create_dir_all(&output).unwrap();
    fs::write(output.join("execution-bank.json"), &bank_bytes).unwrap();
    let saved = read(&output.join("execution-bank.json"));
    let final_keys: Vec<String> = serde_json::from_value(saved["keys"].clone()).unwrap();
    let mut allowed_absent = BTreeSet::from([event]);
    for market in &admitted_stock_markets {
        if market.venue == Venue::OrcaWhirlpool {
            allowed_absent.insert(pk(&market.config));
        }
    }
    let mut bank = accounts(&final_keys, &saved["bank"], &allowed_absent);
    let clock = &get(&bank, &CLOCK).data;
    assert_eq!(stocklana_adapters::u64_at(clock, 0).unwrap(), slot);
    let epoch = stocklana_adapters::u64_at(clock, 16).unwrap();
    let time = stocklana_adapters::u64_at(clock, 32).unwrap();

    put(
        &mut bank,
        WALLET,
        Account {
            lamports: 1_000_000_000_000,
            ..Account::default()
        },
    );
    put(
        &mut bank,
        SETTLE,
        mollusk_svm::program::create_program_account_loader_v3(&SETTLE),
    );
    let buyer_nonce = nonce(&mut bank, WALLET);
    let buyer_cash = token_account(&mut bank, WALLET, USDC);
    let buyer_spyx = token_account(&mut bank, WALLET, SPYX);
    let buyer_spyon = token_account(&mut bank, WALLET, SPYON);
    let seller_owner = Pubkey::new_from_array([91; 32]);
    put(
        &mut bank,
        seller_owner,
        Account {
            lamports: 1_000_000_000_000,
            ..Account::default()
        },
    );
    let seller_nonce = nonce(&mut bank, seller_owner);
    let seller_spyx = token_account(&mut bank, seller_owner, SPYX);
    let seller_cash = token_account(&mut bank, seller_owner, USDC);

    let view = snapshot_from_accounts(&bank, slot);
    let final_stock_candidates = admitted_stock_markets
        .iter()
        .filter_map(|market| {
            market
                .compile(&view)
                .and_then(|curve| curve.quote(RESIDUAL_USDC))
                .ok()
                .map(|output| (market, output))
        })
        .collect::<Vec<_>>();
    assert!(
        final_stock_candidates.len() >= 2,
        "resource admission needs interchangeable live candidates"
    );
    let spyx_out = stock_market
        .compile(&view)
        .and_then(|market| market.quote(RESIDUAL_USDC))
        .expect("selected stock venue rejected by final bank");
    let spyon_out = dlmm_quote(&dlmm, &view, spyx_out);
    let input = native_wire::wallet_asset(WALLET, USDC, &view).unwrap();
    let intermediate = native_wire::wallet_asset(WALLET, SPYX, &view).unwrap();
    let product = native_wire::wallet_asset(WALLET, SPYON, &view).unwrap();
    assert_eq!(input.token, buyer_cash);
    assert_eq!(intermediate.token, buyer_spyx);
    assert_eq!(product.token, buyer_spyon);
    let deadline = slot + 100;
    let graph = native_wire::lower_native_graph(
        SwapGraph {
            owner: WALLET,
            sequence: 0,
            input_atoms: RESIDUAL_USDC,
            minimum_output_atoms: spyon_out,
            deadline_slot: deadline,
            input,
            output: product,
            intermediates: vec![intermediate],
            legs: vec![],
        },
        &[
            NativeSwapProposal {
                market: stock_market.clone(),
                stage: 2,
                product_id: Some("SPY:BACKED".into()),
                input_atoms: RESIDUAL_USDC,
                expected_output_atoms: spyx_out,
            },
            NativeSwapProposal {
                market: dlmm.clone(),
                stage: 2,
                product_id: Some("SPY:ONDO".into()),
                input_atoms: spyx_out,
                expected_output_atoms: spyon_out,
            },
        ],
        &view,
    )
    .unwrap();
    let residual = swap_wire::compile_swap_graph(SETTLE, &graph, |key| {
        let row = view
            .accounts
            .iter()
            .find(|row| row.key == key.to_string())
            .ok_or("missing graph account")?;
        Ok(AccountView {
            owner: pk(&row.owner),
            executable: row.executable,
            data: &row.data,
        })
    })
    .unwrap();

    let settlement_sbf = std::env::var_os("SKEW_SETTLE_SBF_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("artifacts/sbf-stockmesh-v26-resource-admission"));
    std::env::set_var("SBF_OUT_DIR", settlement_sbf);
    // Load the captured venue program set first so the independent SOL funding
    // market is available, then overwrite the two selected SPY venues with the
    // exact program bytes captured in this coherent bank.
    let mut runtime = matrix::runtime(&root);
    runtime.compute_budget.compute_unit_limit = 1_400_000;
    runtime.compute_budget.heap_size = 256 * 1024;
    runtime.sysvars.clock.slot = slot;
    runtime.sysvars.clock.epoch = epoch;
    runtime.sysvars.clock.unix_timestamp = time as i64;
    let mut program_evidence = programs
        .iter()
        .map(|program| add_program(&mut runtime, &bank, *program))
        .collect::<Vec<_>>();
    let instrument = wire::instrument_id("SPY").unwrap();
    let authority = Pubkey::new_from_array([18; 32]);
    let backed_issuer = wire::issuer_id("BACKED").unwrap();
    let ondo_issuer = wire::issuer_id("ONDO").unwrap();
    let spyx_rights = policy_manifest.as_ref().map_or_else(
        || fixture_rights(instrument, backed_issuer, SPYX),
        |manifest| manifest_rights(manifest, SPYX),
    );
    let spyon_rights = policy_manifest.as_ref().map_or_else(
        || fixture_rights(instrument, ondo_issuer, SPYON),
        |manifest| manifest_rights(manifest, SPYON),
    );
    let policies = [
        policy(
            &mut bank,
            authority,
            instrument,
            backed_issuer,
            SPYX,
            spyx_rights,
            slot,
        ),
        policy(
            &mut bank,
            authority,
            instrument,
            ondo_issuer,
            SPYON,
            spyon_rights,
            slot,
        ),
    ];
    let cross_instrument_policy = policy(
        &mut bank,
        authority,
        wire::instrument_id("QQQ").unwrap(),
        ondo_issuer,
        SPYON,
        spyon_rights,
        slot,
    );
    let spyx_scaled = ScaledUiAmount::decode(&get(&bank, &SPYX).data, time as i64).unwrap();
    let spyon_scaled = ScaledUiAmount::decode(&get(&bank, &SPYON).data, time as i64).unwrap();
    if std::env::var("SKEW_RESOURCE_PROBE_ONLY").as_deref() == Ok("1") {
        for program in execution_programs.difference(&programs) {
            add_program(&mut runtime, &bank, *program);
        }
        let preferred_stock = final_stock_candidates
            .iter()
            .min_by_key(|(market, _)| match market.venue {
                Venue::ByrealClmm => 0,
                Venue::RaydiumClmm => 1,
                Venue::MeteoraDlmm => 2,
                Venue::OrcaWhirlpool => 3,
            })
            .unwrap();
        let preferred_spyon = dlmm_quote(&dlmm, &view, preferred_stock.1);
        let resource_floor = u64::try_from(
            u128::from(
                spyon_scaled
                    .exposure_q32(preferred_spyon, 1, 1, CONSERVATIVE_BPS)
                    .unwrap(),
            ) * 9_950
                / 10_000,
        )
        .unwrap();
        let resource_products = vec![
            wire::MeshProduct {
                policy: policies[0],
                claim: None,
                destination: buyer_spyx,
                mint: SPYX,
                token_program: TOKEN22,
                model: wire::TOKEN_2022_SCALED_UI,
                conservative_bps: 8_000,
                policy_version: 1,
                numerator: 1,
                denominator: 1,
            },
            wire::MeshProduct {
                policy: policies[1],
                claim: None,
                destination: buyer_spyon,
                mint: SPYON,
                token_program: TOKEN22,
                model: wire::TOKEN_2022_SCALED_UI,
                conservative_bps: CONSERVATIVE_BPS,
                policy_version: 1,
                numerator: 1,
                denominator: 1,
            },
        ];
        let compile_resource_candidate = |candidates: &[(&MarketConfig, u64)]| {
            let mut proposals = candidates
                .iter()
                .map(|(market, output)| NativeSwapProposal {
                    market: (*market).clone(),
                    stage: 2,
                    product_id: Some("SPY:BACKED".into()),
                    input_atoms: RESIDUAL_USDC,
                    expected_output_atoms: *output,
                })
                .collect::<Vec<_>>();
            let intermediate_seed = candidates.iter().map(|(_, output)| *output).max().unwrap();
            proposals.push(NativeSwapProposal {
                market: dlmm.clone(),
                stage: 2,
                product_id: Some("SPY:ONDO".into()),
                input_atoms: intermediate_seed,
                expected_output_atoms: dlmm_quote(&dlmm, &view, intermediate_seed),
            });
            let graph = native_wire::lower_economic_reflow_graph(
                SwapGraph {
                    owner: WALLET,
                    sequence: 0,
                    input_atoms: RESIDUAL_USDC,
                    minimum_output_atoms: 1,
                    deadline_slot: deadline,
                    input,
                    output: product,
                    intermediates: vec![intermediate],
                    legs: vec![],
                },
                &proposals,
                &view,
            )
            .unwrap();
            let graph = swap_wire::compile_economic_reflow_graph(SETTLE, &graph, 16, |key| {
                let row = view
                    .accounts
                    .iter()
                    .find(|row| row.key == key.to_string())
                    .ok_or("missing resource graph account")?;
                Ok(AccountView {
                    owner: pk(&row.owner),
                    executable: row.executable,
                    data: &row.data,
                })
            })
            .unwrap();
            wire::compile_direct_mesh_reflow(
                SETTLE,
                wire::MeshFillSpec {
                    buyer: WALLET,
                    buyer_nonce,
                    buyer_cash_source: buyer_cash,
                    cash_mint: USDC,
                    cash_token_program: TOKEN,
                    buyer_sequence: 0,
                    buyer_input_atoms: RESIDUAL_USDC,
                    minimum_exposure_q32: resource_floor,
                    deadline_slot: deadline,
                    maximum_policy_age: 150,
                    allow_underlying_closed: false,
                    products: resource_products.clone(),
                    sellers: vec![],
                    residuals: vec![],
                },
                graph,
            )
            .unwrap()
        };
        let full_instruction = compile_resource_candidate(&final_stock_candidates);
        let full_result = matrix::run(&runtime, &full_instruction, &bank);
        let fallback_candidates = vec![(preferred_stock.0, preferred_stock.1)];
        let fallback_instruction = compile_resource_candidate(&fallback_candidates);
        let fallback_result = matrix::run(&runtime, &fallback_instruction, &bank);
        assert!(
            fallback_result.program_result.is_ok(),
            "resource fallback: {:?}",
            fallback_result.program_result
        );
        let fallback_spyon = amount(&fallback_result.resulting_accounts, &buyer_spyon) - START;
        assert!(fallback_spyon > 0);
        let resource_admission = json!({
            "schema":"skew.stockmesh.resource-admission-sbf/v1",
            "scope":"same captured account bank; fixture-owned ProductPolicy PDAs with recorded rights source; unsigned and not submitted",
            "productPolicyRightsSource":policy_rights_source,
            "stateSlot":slot,
            "executionBankSha256":bank_sha,
            "sameEconomicExposureFloorQ32":resource_floor.to_string(),
            "fullCandidateCount":final_stock_candidates.len() + 1,
            "fullCandidateVenues":final_stock_candidates.iter().map(|(market, _)| format!("{:?}", market.venue)).collect::<Vec<_>>(),
            "fullResult":if full_result.program_result.is_ok(){"ADMITTED"}else{"REJECTED"},
            "fullComputeUnits":full_result.compute_units_consumed,
            "fullProgramResult":format!("{:?}",full_result.program_result),
            "fallbackCandidateCount":2,
            "fallbackVenue":format!("{:?}",preferred_stock.0.venue),
            "fallbackResult":"ADMITTED",
            "fallbackComputeUnits":fallback_result.compute_units_consumed,
            "fallbackOutputSpyonAtoms":fallback_spyon.to_string(),
            "submitted":false
        });
        fs::write(
            output.join("resource-admission.json"),
            serde_json::to_vec_pretty(&resource_admission).unwrap(),
        )
        .unwrap();
        println!(
            "{}",
            serde_json::to_string_pretty(&resource_admission).unwrap()
        );
        return;
    }
    let exposure = [
        spyx_scaled
            .exposure_q32(INTERNAL_SPYX, 1, 1, CONSERVATIVE_BPS)
            .unwrap(),
        spyon_scaled
            .exposure_q32(spyon_out, 1, 1, CONSERVATIVE_BPS)
            .unwrap(),
    ];
    let minimum_exposure = exposure[0].checked_add(exposure[1]).unwrap();
    let instruction = wire::compile_mesh_fill(
        SETTLE,
        wire::MeshFillSpec {
            buyer: WALLET,
            buyer_nonce,
            buyer_cash_source: buyer_cash,
            cash_mint: USDC,
            cash_token_program: TOKEN,
            buyer_sequence: 0,
            buyer_input_atoms: RESIDUAL_USDC + INTERNAL_USDC,
            minimum_exposure_q32: minimum_exposure,
            deadline_slot: deadline,
            maximum_policy_age: 150,
            allow_underlying_closed: false,
            products: vec![
                wire::MeshProduct {
                    policy: policies[0],
                    claim: None,
                    destination: buyer_spyx,
                    mint: SPYX,
                    token_program: TOKEN22,
                    model: wire::TOKEN_2022_SCALED_UI,
                    conservative_bps: CONSERVATIVE_BPS,
                    policy_version: 1,
                    numerator: 1,
                    denominator: 1,
                },
                wire::MeshProduct {
                    policy: policies[1],
                    claim: None,
                    destination: buyer_spyon,
                    mint: SPYON,
                    token_program: TOKEN22,
                    model: wire::TOKEN_2022_SCALED_UI,
                    conservative_bps: CONSERVATIVE_BPS,
                    policy_version: 1,
                    numerator: 1,
                    denominator: 1,
                },
            ],
            sellers: vec![wire::MeshSeller {
                product_index: 0,
                owner: seller_owner,
                authority: wire::MeshSellerAuthority::Signed {
                    nonce: seller_nonce,
                },
                stock_source: seller_spyx,
                cash_destination: seller_cash,
                sequence_or_revision: 0,
                stock_atoms: INTERNAL_SPYX,
                cash_atoms: INTERNAL_USDC,
                minimum_cash_atoms: INTERNAL_USDC,
            }],
            residuals: vec![wire::MeshResidual {
                product_index: 1,
                graph: residual,
            }],
        },
    )
    .unwrap();
    let result = matrix::run(&runtime, &instruction, &bank);
    assert!(
        result.program_result.is_ok(),
        "mesh: {:?}",
        result.program_result
    );
    assert_eq!(&result.return_data[..8], b"SKEWMSH1");
    assert_eq!(
        START - amount(&result.resulting_accounts, &buyer_cash),
        RESIDUAL_USDC + INTERNAL_USDC
    );
    assert_eq!(
        amount(&result.resulting_accounts, &buyer_spyx) - START,
        INTERNAL_SPYX
    );
    assert_eq!(
        amount(&result.resulting_accounts, &buyer_spyon) - START,
        spyon_out
    );
    assert_eq!(
        START - amount(&result.resulting_accounts, &seller_spyx),
        INTERNAL_SPYX
    );
    assert_eq!(
        amount(&result.resulting_accounts, &seller_cash) - START,
        INTERNAL_USDC
    );
    program_evidence.extend(
        execution_programs
            .difference(&programs)
            .map(|program| add_program(&mut runtime, &bank, *program)),
    );

    // Execute the same two issuer products through opcode 18 v2. A live
    // SOL->USDC funding leg runs first, one signed seller clears internally,
    // then the onchain optimizer compares direct SPYx with the composed
    // USDC->SPYx->SPYon path. The SPYx leg is shared by both logical paths; if
    // SPYon wins, only the new SPYx delta may enter the second CPI.
    let funding_case =
        matrix::Case::load(&root.join("artifacts/venue-matrix/cases/SOL-sell-raydium_clmm"));
    assert_eq!(funding_case.output, buyer_cash);
    let funding_original = matrix::run(&runtime, &funding_case.jup, &funding_case.a);
    assert!(funding_original.program_result.is_ok());
    let funding_edges = funding_case.lower(&funding_original).unwrap();
    let funding_edge = funding_edges
        .into_iter()
        .find(|(instruction, direction)| {
            let (_, _, source, destination) = funding_case.venue.bindings(*direction);
            instruction.accounts[source].pubkey == funding_case.input
                && instruction.accounts[destination].pubkey == funding_case.output
        })
        .expect("captured SOL/USDC funding edge");
    let funding_input = 120_000_000u64;
    let signed_cash = RESIDUAL_USDC + INTERNAL_USDC;
    let mut funding = funding_case
        .graph(&[funding_edge], funding_input, signed_cash)
        .unwrap();
    funding.data[28..36].copy_from_slice(&deadline.to_le_bytes());
    let mut composed_bank = bank.clone();
    merge_accounts(&mut composed_bank, &funding_case.a);
    let composed_view = snapshot_from_accounts(&composed_bank, slot);
    let composed_graph = native_wire::lower_economic_reflow_graph(
        SwapGraph {
            owner: WALLET,
            sequence: 0,
            input_atoms: RESIDUAL_USDC,
            minimum_output_atoms: 1,
            deadline_slot: deadline,
            input,
            output: product,
            intermediates: vec![intermediate],
            legs: vec![],
        },
        &[
            NativeSwapProposal {
                market: stock_market.clone(),
                stage: 2,
                product_id: Some("SPY:BACKED".into()),
                input_atoms: RESIDUAL_USDC,
                expected_output_atoms: spyx_out,
            },
            NativeSwapProposal {
                market: dlmm.clone(),
                stage: 2,
                product_id: Some("SPY:ONDO".into()),
                input_atoms: spyx_out,
                expected_output_atoms: spyon_out,
            },
        ],
        &composed_view,
    )
    .unwrap();
    let composed_graph =
        swap_wire::compile_economic_reflow_graph(SETTLE, &composed_graph, 16, |key| {
            let row = composed_view
                .accounts
                .iter()
                .find(|row| row.key == key.to_string())
                .ok_or("missing composed graph account")?;
            Ok(AccountView {
                owner: pk(&row.owner),
                executable: row.executable,
                data: &row.data,
            })
        })
        .unwrap();
    let composed_spec = wire::MeshFillSpec {
        buyer: WALLET,
        buyer_nonce,
        buyer_cash_source: buyer_cash,
        cash_mint: USDC,
        cash_token_program: TOKEN,
        buyer_sequence: 0,
        buyer_input_atoms: signed_cash,
        minimum_exposure_q32: 1,
        deadline_slot: deadline,
        maximum_policy_age: 150,
        allow_underlying_closed: false,
        products: vec![
            wire::MeshProduct {
                policy: policies[0],
                claim: None,
                destination: buyer_spyx,
                mint: SPYX,
                token_program: TOKEN22,
                model: wire::TOKEN_2022_SCALED_UI,
                // Synthetic preference used only to force the composed path
                // through the proof. It is not an issuer-rights assertion.
                conservative_bps: 8_000,
                policy_version: 1,
                numerator: 1,
                denominator: 1,
            },
            wire::MeshProduct {
                policy: policies[1],
                claim: None,
                destination: buyer_spyon,
                mint: SPYON,
                token_program: TOKEN22,
                model: wire::TOKEN_2022_SCALED_UI,
                conservative_bps: CONSERVATIVE_BPS,
                policy_version: 1,
                numerator: 1,
                denominator: 1,
            },
        ],
        sellers: vec![wire::MeshSeller {
            product_index: 0,
            owner: seller_owner,
            authority: wire::MeshSellerAuthority::Signed {
                nonce: seller_nonce,
            },
            stock_source: seller_spyx,
            cash_destination: seller_cash,
            sequence_or_revision: 0,
            stock_atoms: INTERNAL_SPYX,
            cash_atoms: INTERNAL_USDC,
            minimum_cash_atoms: INTERNAL_USDC,
        }],
        residuals: vec![],
    };
    // Resource admission proof: the full interchangeable candidate surface and
    // the lower-heap physical variant are compiled from one account bank and
    // execute against the same economic exposure floor. The full result is
    // observed rather than asserted so a CU/heap rejection remains evidence;
    // the fallback must succeed or this proof fails.
    let preferred_stock = final_stock_candidates
        .iter()
        .min_by_key(|(market, _)| match market.venue {
            Venue::ByrealClmm => 0,
            Venue::RaydiumClmm => 1,
            Venue::MeteoraDlmm => 2,
            Venue::OrcaWhirlpool => 3,
        })
        .unwrap();
    let preferred_spyon = dlmm_quote(&dlmm, &view, preferred_stock.1);
    let resource_floor = u64::try_from(
        u128::from(
            spyon_scaled
                .exposure_q32(preferred_spyon, 1, 1, CONSERVATIVE_BPS)
                .unwrap(),
        ) * 9_950
            / 10_000,
    )
    .unwrap();
    let compile_resource_candidate = |candidates: &[(&MarketConfig, u64)]| {
        let mut proposals = candidates
            .iter()
            .map(|(market, output)| NativeSwapProposal {
                market: (*market).clone(),
                stage: 2,
                product_id: Some("SPY:BACKED".into()),
                input_atoms: RESIDUAL_USDC,
                expected_output_atoms: *output,
            })
            .collect::<Vec<_>>();
        let intermediate_seed = candidates.iter().map(|(_, output)| *output).max().unwrap();
        proposals.push(NativeSwapProposal {
            market: dlmm.clone(),
            stage: 2,
            product_id: Some("SPY:ONDO".into()),
            input_atoms: intermediate_seed,
            expected_output_atoms: dlmm_quote(&dlmm, &view, intermediate_seed),
        });
        let graph = native_wire::lower_economic_reflow_graph(
            SwapGraph {
                owner: WALLET,
                sequence: 0,
                input_atoms: RESIDUAL_USDC,
                minimum_output_atoms: 1,
                deadline_slot: deadline,
                input,
                output: product,
                intermediates: vec![intermediate],
                legs: vec![],
            },
            &proposals,
            &view,
        )
        .unwrap();
        let graph = swap_wire::compile_economic_reflow_graph(SETTLE, &graph, 16, |key| {
            let row = view
                .accounts
                .iter()
                .find(|row| row.key == key.to_string())
                .ok_or("missing resource graph account")?;
            Ok(AccountView {
                owner: pk(&row.owner),
                executable: row.executable,
                data: &row.data,
            })
        })
        .unwrap();
        wire::compile_direct_mesh_reflow(
            SETTLE,
            wire::MeshFillSpec {
                buyer: WALLET,
                buyer_nonce,
                buyer_cash_source: buyer_cash,
                cash_mint: USDC,
                cash_token_program: TOKEN,
                buyer_sequence: 0,
                buyer_input_atoms: RESIDUAL_USDC,
                minimum_exposure_q32: resource_floor,
                deadline_slot: deadline,
                maximum_policy_age: 150,
                allow_underlying_closed: false,
                products: composed_spec.products.clone(),
                sellers: vec![],
                residuals: vec![],
            },
            graph,
        )
        .unwrap()
    };
    let full_resource_instruction = compile_resource_candidate(&final_stock_candidates);
    let full_resource_result = matrix::run(&runtime, &full_resource_instruction, &bank);
    let preferred = vec![(preferred_stock.0, preferred_stock.1)];
    let fallback_resource_instruction = compile_resource_candidate(&preferred);
    let fallback_resource_result = matrix::run(&runtime, &fallback_resource_instruction, &bank);
    assert!(
        fallback_resource_result.program_result.is_ok(),
        "resource fallback: {:?}",
        fallback_resource_result.program_result
    );
    let fallback_spyon = amount(&fallback_resource_result.resulting_accounts, &buyer_spyon) - START;
    assert!(fallback_spyon > 0);
    let resource_admission = json!({
        "schema":"skew.stockmesh.resource-admission-sbf/v1",
        "scope":"same captured account bank; fixture-owned ProductPolicy PDAs with recorded rights source; unsigned and not submitted",
        "productPolicyRightsSource":policy_rights_source,
        "stateSlot":slot,
        "executionBankSha256":bank_sha,
        "sameEconomicExposureFloorQ32":resource_floor.to_string(),
        "fullCandidateCount":final_stock_candidates.len() + 1,
        "fullCandidateVenues":final_stock_candidates.iter().map(|(market, _)| format!("{:?}", market.venue)).collect::<Vec<_>>(),
        "fullResult":if full_resource_result.program_result.is_ok(){"ADMITTED"}else{"REJECTED"},
        "fullComputeUnits":full_resource_result.compute_units_consumed,
        "fullProgramResult":format!("{:?}",full_resource_result.program_result),
        "fallbackCandidateCount":2,
        "fallbackVenue":format!("{:?}",preferred_stock.0.venue),
        "fallbackResult":"ADMITTED",
        "fallbackComputeUnits":fallback_resource_result.compute_units_consumed,
        "fallbackOutputSpyonAtoms":fallback_spyon.to_string(),
        "submitted":false
    });
    fs::write(
        output.join("resource-admission.json"),
        serde_json::to_vec_pretty(&resource_admission).unwrap(),
    )
    .unwrap();
    let direct_global = wire::compile_direct_mesh_reflow(
        SETTLE,
        wire::MeshFillSpec {
            buyer: WALLET,
            buyer_nonce,
            buyer_cash_source: buyer_cash,
            cash_mint: USDC,
            cash_token_program: TOKEN,
            buyer_sequence: 0,
            buyer_input_atoms: RESIDUAL_USDC,
            minimum_exposure_q32: 1,
            deadline_slot: deadline,
            maximum_policy_age: 150,
            allow_underlying_closed: false,
            products: composed_spec.products.clone(),
            sellers: vec![],
            residuals: vec![],
        },
        composed_graph.clone(),
    )
    .unwrap();
    let direct_global_result = matrix::run(&runtime, &direct_global, &bank);
    assert!(
        direct_global_result.program_result.is_ok(),
        "direct global Reflow: {:?}",
        direct_global_result.program_result
    );
    assert_eq!(&direct_global_result.return_data[..8], b"SKEWMSR1");
    assert_eq!(
        START - amount(&direct_global_result.resulting_accounts, &buyer_cash),
        RESIDUAL_USDC
    );
    assert_eq!(
        amount(&direct_global_result.resulting_accounts, &buyer_spyx),
        START,
        "a composed path cannot strand temporary SPYx"
    );
    let direct_global_spyon =
        amount(&direct_global_result.resulting_accounts, &buyer_spyon) - START;
    assert!(direct_global_spyon > 0);

    // Persist the exact direct Economic Reflow message and its SBF account
    // deltas for the host's prepared-transaction/receipt contract. This is a
    // zero-signature wallet envelope over one coherent captured bank. It is
    // never submitted. ProductPolicy PDAs remain fixture-owned; their rights
    // hashes may be sourced from the explicit authority-bound manifest.
    let prepared_lookup = solana_message::AddressLookupTableAccount {
        key: Pubkey::new_from_array([79; 32]),
        addresses: direct_global
            .accounts
            .iter()
            .filter(|meta| !meta.is_signer)
            .map(|meta| meta.pubkey)
            .collect(),
    };
    let mut compute_data = vec![2];
    compute_data.extend_from_slice(&1_400_000u32.to_le_bytes());
    let compute = Instruction {
        program_id: pubkey!("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data: compute_data,
    };
    let mut heap_data = vec![1];
    heap_data.extend_from_slice(&(256u32 * 1024).to_le_bytes());
    let heap = Instruction {
        program_id: pubkey!("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data: heap_data,
    };
    let prepared_message = wire::compile_unsigned_v0(
        WALLET,
        &[compute, heap, direct_global.clone()],
        std::slice::from_ref(&prepared_lookup),
        [9; 32],
    )
    .unwrap();
    let mut prepared_alt = vec![0u8; 56];
    prepared_alt[..4].copy_from_slice(&1u32.to_le_bytes());
    prepared_alt[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
    for address in &prepared_lookup.addresses {
        prepared_alt.extend_from_slice(address.as_ref());
    }
    let prepared_alt_account = Account {
        lamports: 10_000_000,
        data: prepared_alt,
        owner: pubkey!("AddressLookupTab1e1111111111111111111111111"),
        executable: false,
        rent_epoch: 0,
    };
    let mut prepared_keys = direct_global
        .accounts
        .iter()
        .map(|meta| meta.pubkey)
        .collect::<BTreeSet<_>>();
    prepared_keys.extend([WALLET, prepared_lookup.key]);
    let mut prepared_before = bank
        .iter()
        .filter(|(key, _)| prepared_keys.contains(key))
        .cloned()
        .collect::<Accounts>();
    put(
        &mut prepared_before,
        prepared_lookup.key,
        prepared_alt_account.clone(),
    );
    let mut prepared_after = direct_global_result
        .resulting_accounts
        .iter()
        .filter(|(key, _)| prepared_keys.contains(key))
        .cloned()
        .collect::<Accounts>();
    put(
        &mut prepared_after,
        prepared_lookup.key,
        prepared_alt_account,
    );
    let prepared_packet_bytes = 65 + prepared_message.len();
    assert!(prepared_packet_bytes <= 1_232);
    let prepared_vector = json!({
        "schema":"skew.stockmesh.direct-reflow-sbf-vector/v1",
        "scope":"captured coherent SBF direct Economic Reflow; actual SPYx/SPYon mints and venue state; fixture-owned ProductPolicy PDAs with recorded rights source; unsigned and not submitted",
        "productPolicyRightsSource":policy_rights_source,
        "productRightsHashes":{"SPYx":hex32(spyx_rights),"SPYon":hex32(spyon_rights)},
        "program":SETTLE.to_string(),
        "slot":slot,
        "cu":direct_global_result.compute_units_consumed,
        "heapFrameBytes":256 * 1024,
        "owner":WALLET.to_string(),
        "ownerSequence":0,
        "instrument":"SPY",
        "inputMint":USDC.to_string(),
        "inputAtoms":RESIDUAL_USDC,
        "minimumExposureQ32":1,
        "actualExposureQ32":spyon_scaled.exposure_q32(direct_global_spyon, 1, 1, CONSERVATIVE_BPS).unwrap(),
        "deadlineSlot":deadline,
        "productPolicyHash":vec![91u8;32],
        "products":[
            {
                "issuer":"BACKED",
                "mint":SPYX.to_string(),
                "tokenProgram":TOKEN22.to_string(),
                "rawDecimals":get(&bank, &SPYX).data[44],
                "policy":policies[0].to_string(),
                "model":wire::TOKEN_2022_SCALED_UI,
                "numerator":1,
                "denominator":1,
                "conservativeBps":8_000
            },
            {
                "issuer":"ONDO",
                "mint":SPYON.to_string(),
                "tokenProgram":TOKEN22.to_string(),
                "rawDecimals":get(&bank, &SPYON).data[44],
                "policy":policies[1].to_string(),
                "model":wire::TOKEN_2022_SCALED_UI,
                "numerator":1,
                "denominator":1,
                "conservativeBps":CONSERVATIVE_BPS
            }
        ],
        "messageBase64":STANDARD.encode(&prepared_message),
        "returnData":STANDARD.encode(&direct_global_result.return_data),
        "lookup":{
            "key":prepared_lookup.key.to_string(),
            "addresses":prepared_lookup.addresses.iter().map(ToString::to_string).collect::<Vec<_>>()
        },
        "eventualSignedPacketBytes":prepared_packet_bytes,
        "before":prepared_before.iter().map(|(key, account)| json!({
            "key":key.to_string(),"owner":account.owner.to_string(),"lamports":account.lamports,
            "executable":account.executable,"data":[STANDARD.encode(&account.data),"base64"]
        })).collect::<Vec<_>>(),
        "after":prepared_after.iter().map(|(key, account)| json!({
            "key":key.to_string(),"owner":account.owner.to_string(),"lamports":account.lamports,
            "executable":account.executable,"data":[STANDARD.encode(&account.data),"base64"]
        })).collect::<Vec<_>>(),
        "evidenceBoundary":{
            "issuerRightsVerified":false,"legalEquivalenceProven":false,
            "ownerSigned":false,"submitted":false,"mainnetFilled":false
        }
    });
    let prepared_vector_bytes = serde_json::to_vec_pretty(&prepared_vector).unwrap();
    fs::write(
        output.join("direct-reflow-host-vector.json"),
        &prepared_vector_bytes,
    )
    .unwrap();
    let prepared_vector_sha = format!("{:x}", Sha256::digest(&prepared_vector_bytes));
    let composed =
        wire::compile_funded_mesh_reflow(SETTLE, funding, composed_spec, composed_graph).unwrap();
    let composed_result = matrix::run(&runtime, &composed, &composed_bank);
    assert!(
        composed_result.program_result.is_ok(),
        "composed global Reflow: {:?}",
        composed_result.program_result
    );
    assert_eq!(&composed_result.return_data[..8], b"SKEWMSF2");
    assert_eq!(
        amount(&composed_result.resulting_accounts, &buyer_spyx) - START,
        INTERNAL_SPYX,
        "temporary SPYx delta must be fully consumed by SPYon path"
    );
    let composed_spyon = amount(&composed_result.resulting_accounts, &buyer_spyon) - START;
    assert!(composed_spyon > 0);
    assert_eq!(
        amount(&composed_result.resulting_accounts, &seller_cash) - START,
        INTERNAL_USDC
    );
    let mut late_failure_bank = composed_bank.clone();
    let dlmm_pool = pk(&dlmm.pool);
    get(&late_failure_bank, &dlmm_pool);
    late_failure_bank
        .iter_mut()
        .find(|row| row.0 == dlmm_pool)
        .unwrap()
        .1
        .data[0] ^= 1;
    assert_rollback(
        &runtime,
        &composed,
        &late_failure_bank,
        "composed_second_hop_failure",
    );
    assert_rollback(
        &runtime,
        &direct_global,
        &late_failure_bank,
        "direct_composed_second_hop_failure",
    );

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
                bad.data[40..48].copy_from_slice(&minimum_exposure.saturating_add(1).to_le_bytes())
            }
            "buyer_cash_conservation" => {
                bad.data[32..40].copy_from_slice(&(RESIDUAL_USDC + INTERNAL_USDC + 1).to_le_bytes())
            }
            "product_policy_version" => bad.data[64..72].copy_from_slice(&2u64.to_le_bytes()),
            "seller_limit" => {
                bad.data[152..160].copy_from_slice(&(INTERNAL_USDC + 1).to_le_bytes())
            }
            "seller_signature" => {
                bad.accounts
                    .iter_mut()
                    .find(|meta| meta.pubkey == seller_owner)
                    .unwrap()
                    .is_signer = false;
            }
            "residual_minimum" => {
                let graph_start = 56 + 2 * 32 + 40 + 4;
                bad.data[graph_start + 20..graph_start + 28]
                    .copy_from_slice(&(spyon_out + 1).to_le_bytes());
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
        assert_rollback(&runtime, &bad, &bank, name);
        faults.push(json!({"fault":name,"rejected":true,"fullRollback":true}));
    }
    let mut wrong_rights_bank = bank.clone();
    let mut wrong_policy = get(&wrong_rights_bank, &policies[1]).clone();
    wrong_policy.data[168..200].copy_from_slice(&[9; 32]);
    put(&mut wrong_rights_bank, policies[1], wrong_policy);
    assert_rollback(
        &runtime,
        &instruction,
        &wrong_rights_bank,
        "rights_commitment",
    );
    faults.push(json!({"fault":"rights_commitment","rejected":true,"fullRollback":true}));

    let lookup = solana_message::AddressLookupTableAccount {
        key: Pubkey::new_from_array([77; 32]),
        addresses: instruction
            .accounts
            .iter()
            .filter(|meta| !meta.is_signer)
            .map(|meta| meta.pubkey)
            .collect(),
    };
    let message = wire::compile_unsigned_v0(
        WALLET,
        std::slice::from_ref(&instruction),
        std::slice::from_ref(&lookup),
        [9; 32],
    )
    .unwrap();
    let signatures = instruction
        .accounts
        .iter()
        .filter(|meta| meta.is_signer)
        .map(|meta| meta.pubkey)
        .chain(std::iter::once(WALLET))
        .collect::<BTreeSet<_>>()
        .len();
    let packet_bytes = 1 + signatures * 64 + message.len();
    assert!(packet_bytes <= 1_232);

    let report = json!({
        "schema":"skew.stockmesh.real-spy-mesh-sbf/v1",
        "status":"CAPTURED_COHERENT_SBF_PROVEN",
        "scope":"Actual SPYx/SPYon mints, actual stock-venue and Meteora DLMM state and deployed program bytes from one final RPC bank; synthetic wallet balances and fixture-owned ProductPolicy PDAs; no signing or submission",
        "bank":{"slot":slot,"keyCount":final_keys.len(),"sha256":bank_sha,"singleRpcResponse":true},
        "instrument":"SPY",
        "productPolicyRightsSource":policy_rights_source,
        "productRightsHashes":{"SPYx":hex32(spyx_rights),"SPYon":hex32(spyon_rights)},
        "products":[
            {"issuer":"BACKED","mint":SPYX.to_string(),"rawAtoms":INTERNAL_SPYX.to_string(),"exposureQ32":exposure[0].to_string(),"source":"INTERNAL_SIGNED_SELLER"},
            {"issuer":"ONDO","mint":SPYON.to_string(),"rawAtoms":spyon_out.to_string(),"exposureQ32":exposure[1].to_string(),"source":"USDC_TO_SPYX_TO_SPYON_RESIDUAL"}
        ],
        "residual":{"inputUsdcAtoms":RESIDUAL_USDC.to_string(),"discoveryIntermediateSpyxAtoms":discovery_spyx_out.to_string(),"finalIntermediateSpyxAtoms":spyx_out.to_string(),"outputSpyonAtoms":spyon_out.to_string(),"legs":2,"selectionMode":if forced_stock_pool.is_some(){"PINNED_CU_ADMITTED_FIXTURE"}else{"BEST_DISCOVERY_OUTPUT"},"selectedStockVenue":format!("{:?}",stock_market.venue),"stockPool":stock_market.pool,"stockVenueCandidates":stock_quotes,"selectedVenueRequotedOnFinalBank":true,"dlmmPool":dlmm.pool,"standalonePublicOpcode2":"NOT_APPLICABLE_MULTI_HOP_REQUIRES_ECONOMIC_ENVELOPE"},
        "settlement":{"opcode":14,"computeUnits":result.compute_units_consumed,"ceiling":1_400_000,"accountCount":instruction.accounts.len(),"instructionDataBytes":instruction.data.len(),"eventualSignedPacketBytes":packet_bytes,"aggregateExposureQ32":minimum_exposure.to_string(),"returnData":result.return_data},
        "directPathReflow":{"opcode":14,"mode":"DIRECT_ECONOMIC_REFLOW","computeUnits":direct_global_result.compute_units_consumed,"ceiling":1_400_000,"heapFrameBytes":256 * 1024,"economicCandidatePaths":["USDC_TO_SPYX","USDC_TO_SPYX_TO_SPYON"],"selectedTerminalMint":SPYON.to_string(),"outputSpyonAtoms":direct_global_spyon.to_string(),"zeroOutputIssuerAccepted":true,"temporarySpyxDeltaFullyConsumed":true,"lateSecondHopFailureRollsBackFirstHop":true,"preparedHostVectorSha256":prepared_vector_sha,"eventualSignedPacketBytes":prepared_packet_bytes,"returnData":direct_global_result.return_data},
        "pathReflow":{"opcode":18,"version":2,"computeUnits":composed_result.compute_units_consumed,"ceiling":1_400_000,"heapFrameBytes":256 * 1024,"fundingLegs":1,"economicCandidatePaths":["USDC_TO_SPYX","USDC_TO_SPYX_TO_SPYON"],"selectedTerminalMint":SPYON.to_string(),"outputSpyonAtoms":composed_spyon.to_string(),"temporarySpyxDeltaFullyConsumed":true,"internalSpyxPreserved":true,"lateSecondHopFailureRollsBackFundingClearingAndFirstHop":true,"returnData":composed_result.return_data},
        "programs":program_evidence,
        "faults":faults,
        "resourceAdmission":resource_admission,
        "invariants":{"rightsHashCommittedByPolicyPda":true,"sameEconomicInstrumentEnforcedOnchain":true,"integerCashConservation":true,"intermediatePrebalancePreserved":true,"aggregateExposurePostcondition":true,"nonceReplayProtection":true},
        "evidenceBoundary":{"officialIssuerGeneralRightsHashesBound":policy_manifest.is_some(),"productSpecificOfferingDocumentVerified":product_specific_offering_verified,"primaryEligibilityIntegrated":false,"legalEquivalenceProven":false,"productPolicyPda":"FIXTURE_OWNED_NOT_DEPLOYED","mainnetTransactionSigned":false,"mainnetSubmitted":false,"mainnetFilled":false}
    });
    fs::write(
        output.join("proof.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    println!("{report}");
}
