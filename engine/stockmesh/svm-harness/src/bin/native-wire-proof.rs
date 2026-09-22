//! Derive CPI recipes from pool/mint/tick state rather than importing CPI metas.
//! The captured CPI is only an independent execution/price reference. No send.
#[allow(dead_code)]
#[path = "../matrix.rs"]
mod matrix;
#[path = "../native_bridge.rs"]
mod native_bridge;
use matrix::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use skew_execution_host::{
    exposure_wire::{self, DirectExposureSpec, DirectProduct},
    feed::{Account as HostAccount, Snapshot},
    market::{MarketConfig, Venue as NativeVenue},
    native_wire,
    onebook_wire::MeshProduct,
    swap_wire::{self, AccountView, Budget, SwapGraph, TokenAsset},
    world::NativeSwapProposal,
};
use solana_pubkey::Pubkey;
use std::{collections::BTreeSet, fs, path::PathBuf, time::Instant};
use stocklana_adapters::{self as dex, graph::Venue};

fn snapshot(c: &Case) -> Snapshot {
    Snapshot {
        slot: c.slot,
        generation: 1,
        hash: [0; 32],
        revision: 1,
        observed: Instant::now(),
        slot_advanced: Instant::now(),
        accounts: c
            .a
            .iter()
            .map(|(k, a)| HostAccount {
                key: k.to_string(),
                owner: a.owner.to_string(),
                executable: a.executable,
                lamports: a.lamports,
                data: a.data.clone(),
            })
            .collect(),
    }
}
fn asset(c: &Case, token: Pubkey) -> TokenAsset {
    let a = get(&c.a, &token);
    TokenAsset {
        token,
        mint: Pubkey::from(dex::key(&a.data, 0).unwrap()),
        token_program: a.owner,
    }
}

fn exposure_policy(accounts: &mut Accounts, input: Pubkey, output: Pubkey, slot: u64) -> Pubkey {
    let authority = Pubkey::new_from_array([214; 32]);
    let instrument = [215; 32];
    let issuer = [216; 32];
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
    let address = Pubkey::find_program_address(
        &[
            b"stock2",
            authority.as_ref(),
            &instrument,
            input.as_ref(),
            output.as_ref(),
            &rights,
        ],
        &SETTLE,
    )
    .0;
    let mut data = vec![0; 225];
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
        solana_account::Account {
            lamports: 10_000_000,
            data,
            owner: SETTLE,
            executable: false,
            rent_epoch: 0,
        },
    );
    address
}
fn config(c: &Case, pool: Pubkey, source: TokenAsset, destination: TokenAsset) -> MarketConfig {
    let data = &get(&c.a, &pool).data;
    let (venue, config) = match c.venue {
        Venue::RaydiumClmm => (
            NativeVenue::RaydiumClmm,
            Pubkey::from(dex::key(data, 9).unwrap()).to_string(),
        ),
        Venue::ByrealClmm => (
            NativeVenue::ByrealClmm,
            Pubkey::from(dex::key(data, 9).unwrap()).to_string(),
        ),
        Venue::OrcaWhirlpool => (
            NativeVenue::OrcaWhirlpool,
            Pubkey::find_program_address(&[b"oracle", pool.as_ref()], &pk(c.venue.program()))
                .0
                .to_string(),
        ),
        Venue::MeteoraDlmm => (NativeVenue::MeteoraDlmm, String::new()),
        _ => unreachable!(),
    };
    let tick_arrays =
        c.a.iter()
            .filter(|(_, a)| {
                if a.owner != pk(c.venue.program()) {
                    return false;
                }
                let offset = match c.venue {
                    Venue::OrcaWhirlpool if a.data.len() == 9988 => 9956,
                    Venue::OrcaWhirlpool
                        if a.data.get(..8) == Some(&[17, 216, 246, 142, 225, 199, 218, 56]) =>
                    {
                        12
                    }
                    Venue::MeteoraDlmm if a.data.len() == 10136 => 24,
                    Venue::RaydiumClmm | Venue::ByrealClmm
                        if a.data.get(..8) == Some(&[192, 155, 85, 205, 49, 249, 129, 42])
                            || a.data.get(..8) == Some(&[106, 139, 152, 36, 117, 153, 184, 56]) =>
                    {
                        8
                    }
                    _ => return false,
                };
                dex::key(&a.data, offset).ok() == Some(pool.to_bytes())
            })
            .map(|(key, _)| key.to_string())
            .collect();
    MarketConfig {
        venue,
        program: c.venue.program().into(),
        pool: pool.to_string(),
        config,
        input_mint: source.mint.to_string(),
        output_mint: destination.mint.to_string(),
        tick_arrays,
        array_capacity: None,
        clock: "SysvarC1ock11111111111111111111111111111111".into(),
    }
}
fn main() {
    let root = PathBuf::from("/srv/skew/stocklana-engine-20260912");
    let output = root.join("artifacts/stockmesh-native-wire-v11");
    fs::create_dir_all(&output).unwrap();
    let mut runtime = runtime(&root);
    runtime.compute_budget.compute_unit_limit = 1_400_000;
    let mut paths = fs::read_dir(root.join("artifacts/venue-matrix/cases"))
        .unwrap()
        .map(|p| p.unwrap().path())
        .collect::<Vec<_>>();
    paths.sort();
    let mut rows = Vec::new();
    let mut joint_rows = Vec::new();
    let mut wallet_rows = Vec::new();
    for path in paths {
        if !path.join("manifest.json").exists() {
            continue;
        }
        let c = Case::load(&path);
        if ![
            Venue::RaydiumClmm,
            Venue::ByrealClmm,
            Venue::OrcaWhirlpool,
            Venue::MeteoraDlmm,
        ]
        .contains(&c.venue)
        {
            continue;
        }
        runtime.sysvars.clock.slot = c.slot;
        runtime.sysvars.clock.unix_timestamp = c.time as i64;
        runtime.sysvars.clock.epoch = dex::u64_at(
            &get(&c.a, &pk("SysvarC1ock11111111111111111111111111111111")).data,
            16,
        )
        .unwrap();
        let reference = run(&runtime, &c.jup, &c.a);
        if reference.program_result.is_err() {
            continue;
        }
        let Ok(legs) = c.lower(&reference) else {
            continue;
        };
        let mut native_plan = Vec::new();
        for (index, (reference, direction)) in legs.iter().enumerate() {
            let (p, _, s, d) = c.venue.bindings(*direction);
            let source = asset(&c, reference.accounts[s].pubkey);
            let destination = asset(&c, reference.accounts[d].pubkey);
            let config = config(&c, reference.accounts[p].pubkey, source, destination);
            let quantity = dex::u64_at(&reference.data, 8).unwrap();
            for input in [quantity / 4, quantity, quantity.saturating_mul(4)] {
                if input == 0 || input > dex::MAX_INPUT {
                    continue;
                }
                let result = (|| -> Result<serde_json::Value, String> {
                    let curve = native_bridge::Curve::decode(
                        &c.a,
                        c.venue,
                        reference,
                        *direction,
                        c.slot,
                        c.time,
                        runtime.sysvars.clock.epoch,
                    )
                    .map_err(|e| format!("reference native admission: {e:?}"))?;
                    let expected = curve
                        .quote(input)
                        .map_err(|e| format!("reference native capacity: {e:?}"))?;
                    let mut bank = snapshot(&c);
                    let deps = native_wire::execution_dependencies(&config, &bank)?;
                    let proposal = NativeSwapProposal {
                        market: config.clone(),
                        stage: if source.token == c.input { 1 } else { 2 },
                        product_id: None,
                        input_atoms: input,
                        expected_output_atoms: expected,
                    };
                    let lower = |p: &NativeSwapProposal, b: &Snapshot, budget| {
                        native_wire::lower_native_leg(p, b, WALLET, source, destination, budget)
                    };
                    let leg = lower(&proposal, &bank, Budget::Remaining)?;
                    if input == quantity {
                        native_plan.push(proposal.clone());
                    }
                    // Price/allocation/wallet/dependency mutations must never become executable.
                    let mut attacks = 0;
                    let mut bad = proposal.clone();
                    bad.expected_output_atoms += 1;
                    if lower(&bad, &bank, Budget::Remaining).is_ok() {
                        return Err("accepted changed expected output".into());
                    }
                    attacks += 1;
                    if lower(&proposal, &bank, Budget::Exact(input + 1)).is_ok() {
                        return Err("accepted changed allocation".into());
                    }
                    attacks += 1;
                    if native_wire::lower_native_leg(
                        &proposal,
                        &bank,
                        Pubkey::new_from_array([19; 32]),
                        source,
                        destination,
                        Budget::Remaining,
                    )
                    .is_ok()
                    {
                        return Err("accepted changed wallet".into());
                    }
                    attacks += 1;
                    let saved = bank
                        .accounts
                        .iter()
                        .position(|a| a.key == config.pool)
                        .unwrap();
                    let pool = bank.accounts.remove(saved);
                    if lower(&proposal, &bank, Budget::Remaining).is_ok() {
                        return Err("accepted missing pool".into());
                    }
                    attacks += 1;
                    bank.accounts.insert(saved, pool.clone());
                    bank.accounts.push(pool);
                    if lower(&proposal, &bank, Budget::Remaining).is_ok() {
                        return Err("accepted ambiguous pool".into());
                    }
                    attacks += 1;
                    bank.accounts.pop();
                    bad = proposal.clone();
                    bad.market.tick_arrays[0] = Pubkey::new_from_array([18; 32]).to_string();
                    if lower(&bad, &bank, Budget::Remaining).is_ok() {
                        return Err("accepted substituted tick address".into());
                    }
                    attacks += 1;
                    let spec = SwapGraph {
                        owner: WALLET,
                        sequence: 0,
                        input_atoms: input,
                        minimum_output_atoms: expected,
                        deadline_slot: c.slot + 1000,
                        input: source,
                        output: destination,
                        intermediates: vec![],
                        legs: vec![leg],
                    };
                    let instruction = swap_wire::compile_swap_graph(SETTLE, &spec, |key| {
                        let a =
                            c.a.iter()
                                .find(|(k, _)| k == key)
                                .ok_or("compiler dependency missing")?;
                        Ok(AccountView {
                            owner: a.1.owner,
                            executable: a.1.executable,
                            data: &a.1.data,
                        })
                    })?;
                    let execution = run(&runtime, &instruction, &c.a);
                    if execution.program_result.is_err() {
                        return Err(format!(
                            "native-derived SBF: {:?} CU {}",
                            execution.program_result, execution.compute_units_consumed
                        ));
                    }
                    if amount(&c.a, &source.token)
                        - amount(&execution.resulting_accounts, &source.token)
                        != input
                        || amount(&execution.resulting_accounts, &destination.token)
                            - amount(&c.a, &destination.token)
                            != expected
                    {
                        return Err("native-derived execution raw delta mismatch".into());
                    }
                    let mut fail = instruction.clone();
                    fail.data[20..28].copy_from_slice(&(expected + 1).to_le_bytes());
                    let rejected = run(&runtime, &fail, &c.a);
                    if rejected.program_result.is_ok() || rejected.resulting_accounts != c.a {
                        return Err("floor rollback failed".into());
                    }
                    let replay = run(&runtime, &instruction, &execution.resulting_accounts);
                    if replay.program_result.is_ok()
                        || replay.resulting_accounts != execution.resulting_accounts
                    {
                        return Err("nonce rollback failed".into());
                    }
                    Ok(
                        json!({"expectedOutputAtoms":expected.to_string(),"computeUnits":execution.compute_units_consumed,
                        "accounts":instruction.accounts.len(),"discoveredDependencies":deps.len(),"hostAttacksRejected":attacks,
                        "floorRollback":true,"nonceRollback":true,"passed":true}),
                    )
                })();
                let mut row = result.unwrap_or_else(|e| {
                    let status = if e.starts_with("reference native admission:")
                        || e.starts_with("reference native capacity:")
                    {
                        "NOT_NATIVE_ADMITTED"
                    } else {
                        "LOWERING_FAILURE"
                    };
                    json!({"passed":false,"status":status,"error":e})
                });
                row["case"] = json!(c.name);
                row["leg"] = json!(index);
                row["inputAtoms"] = json!(input.to_string());
                row["venue"] = json!(c.venue as u8);
                row["slot"] = json!(c.slot);
                println!("{row}");
                rows.push(row);
            }
        }
        if native_plan.len() == legs.len() {
            let result = prove_joint(&runtime, &c, &native_plan, &legs);
            let mut row = result.unwrap_or_else(|e| json!({"passed":false,"error":e}));
            row["case"] = json!(c.name);
            joint_rows.push(row);
            for existing_native in [false, true] {
                if existing_native
                    && asset(&c, c.input).mint != pk("So11111111111111111111111111111111111111112")
                {
                    continue;
                }
                let result = prove_wallet(&runtime, &c, &native_plan, &legs, existing_native);
                let mut row = result.unwrap_or_else(|error| json!({"passed":false,"error":error}));
                row["case"] = json!(c.name);
                row["existingNativeAccount"] = json!(existing_native);
                wallet_rows.push(row);
            }
        }
    }
    let passed = rows.iter().filter(|r| r["passed"] == true).count();
    let excluded = rows
        .iter()
        .filter(|r| r["status"] == "NOT_NATIVE_ADMITTED")
        .count();
    let lowering_failures = rows.len() - passed - excluded;
    let joint_passed = joint_rows.iter().filter(|r| r["passed"] == true).count();
    let wallet_passed = wallet_rows.iter().filter(|r| r["passed"] == true).count();
    let economic_receipts = wallet_rows
        .iter()
        .filter(|row| row["economicReceipt"] == true)
        .count();
    let venues = rows
        .iter()
        .filter(|r| r["passed"] == true)
        .map(|r| r["venue"].as_u64().unwrap())
        .collect::<BTreeSet<_>>();
    let report = json!({"schema":"skew.native-cpi-lowering-proof/v2","environment":"AWS captured DEX ELFs/state; synthetic wallet balances; no submission",
        "sourceSha256":format!("{:x}",Sha256::digest(fs::read(root.join("host/src/native_wire.rs")).unwrap())),
        "exposureWireSha256":format!("{:x}",Sha256::digest(fs::read(root.join("host/src/exposure_wire.rs")).unwrap())),
        "attempted":rows.len(),"passed":passed,"notNativeAdmitted":excluded,
        "loweringFailures":lowering_failures,"venues":venues,"rows":rows,
        "jointAttempted":joint_rows.len(),"jointPassed":joint_passed,"jointRows":joint_rows,
        "walletAttempted":wallet_rows.len(),"walletPassed":wallet_passed,
        "economicReceipts":economic_receipts,"walletRows":wallet_rows});
    fs::write(
        output.join("proof.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    assert!(
        passed >= 232
            && venues.len() == 4
            && lowering_failures == 0
            && rows.len() == 297
            && joint_passed == joint_rows.len()
            && joint_passed > 0,
        "every native-admitted case must lower and execute exactly across all four venues"
    );
    assert!(
        wallet_passed == wallet_rows.len() && wallet_passed >= 34 && economic_receipts > 0,
        "all admitted native paths must initialize wallet assets atomically"
    );
}

fn prove_wallet(
    runtime: &mollusk_svm::Mollusk,
    c: &Case,
    proposals: &[NativeSwapProposal],
    reference: &[(solana_instruction::Instruction, bool)],
    existing_native: bool,
) -> Result<serde_json::Value, String> {
    use skew_execution_host::wallet_wire::WalletSetup;
    let baseline = runtime.process_transaction_instructions(
        &reference
            .iter()
            .map(|(ix, _)| ix.clone())
            .collect::<Vec<_>>(),
        &c.a,
    );
    if baseline.program_result.is_err() {
        return Err("wallet reference failed".into());
    }
    let input = amount(&c.a, &c.input) - amount(&baseline.resulting_accounts, &c.input);
    let output = amount(&baseline.resulting_accounts, &c.output) - amount(&c.a, &c.output);
    let source = asset(c, c.input);
    let destination = asset(c, c.output);
    let native = source.mint == pk("So11111111111111111111111111111111111111112");
    let bank = snapshot(c);
    let mut assets = vec![source];
    for p in proposals {
        for mint in [&p.market.input_mint, &p.market.output_mint] {
            let mint = pk(mint);
            if mint != destination.mint && !assets.iter().any(|a| a.mint == mint) {
                assets.push(native_wire::wallet_asset(WALLET, mint, &bank)?);
            }
        }
    }
    assets.push(destination);
    let ata = pk("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
    let mut before = c.a.clone();
    let policy = exposure_policy(&mut before, source.mint, destination.mint, c.slot);
    let nonce = Pubkey::find_program_address(&[b"stocklana", WALLET.as_ref()], &SETTLE).0;
    if !existing_native {
        put(&mut before, nonce, solana_account::Account::default());
    }
    put(
        &mut before,
        ata,
        mollusk_svm::program::create_program_account_loader_v3(&ata),
    );
    for asset in &assets {
        if asset.token != source.token || (native && !existing_native) {
            put(&mut before, asset.token, solana_account::Account::default());
        }
    }
    fn read<'a>(a: &'a Accounts, key: &Pubkey) -> Result<AccountView<'a>, String> {
        let (_, value) = a
            .iter()
            .find(|(k, _)| k == key)
            .ok_or("wallet bank missing")?;
        Ok(AccountView {
            owner: value.owner,
            executable: value.executable,
            data: &value.data,
        })
    }
    let setup = WalletSetup::plan(WALLET, &assets, native.then_some(input), |k| {
        read(&before, k)
    })?
    .with_nonce(SETTLE, 0, |k| read(&before, k))?;
    let setup_table = solana_message::AddressLookupTableAccount {
        key: Pubkey::new_from_array([218; 32]),
        addresses: setup
            .instructions()
            .iter()
            .flat_map(|instruction| &instruction.accounts)
            .filter(|account| !account.is_signer)
            .map(|account| account.pubkey)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    };
    let setup_message = setup
        .compile_setup_only(&[setup_table], [219; 32], 400_000)?
        .ok_or("wallet setup-only message missing")?;
    let initialized = runtime.process_transaction_instructions(setup.instructions(), &before);
    if initialized.program_result.is_err() {
        return Err(format!("wallet setup SBF {:?}", initialized.program_result));
    }
    setup.verify_initialized(|k| read(&initialized.resulting_accounts, k))?;
    let host_account = |(k, a): &(Pubkey, solana_account::Account)| HostAccount {
        key: k.to_string(),
        owner: a.owner.to_string(),
        executable: a.executable,
        lamports: a.lamports,
        data: a.data.clone(),
    };
    let mut original = snapshot(c);
    original.accounts = before.iter().map(host_account).collect();
    let setup_returned = setup
        .setup_only_addresses()
        .iter()
        .map(|address| {
            initialized
                .resulting_accounts
                .iter()
                .find(|(key, _)| key == address)
                .map(host_account)
                .ok_or("wallet setup returned account")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let planned = setup.lowering_projection(&original, &setup_returned)?;
    let mut changed = setup_returned.clone();
    changed[0].lamports = changed[0].lamports.saturating_add(1);
    if setup.lowering_projection(&original, &changed).is_ok() {
        return Err("wallet setup payer mutation accepted".into());
    }
    if let Some(address) = setup.created_nonce() {
        changed = setup_returned.clone();
        let nonce = changed
            .iter_mut()
            .find(|account| account.key == address.to_string())
            .ok_or("wallet setup returned nonce")?;
        nonce.data[40..48].copy_from_slice(&1u64.to_le_bytes());
        if setup.lowering_projection(&original, &changed).is_ok() {
            return Err("wallet setup nonce mutation accepted".into());
        }
    }
    let header = SwapGraph {
        owner: WALLET,
        sequence: 0,
        input_atoms: input,
        minimum_output_atoms: output,
        deadline_slot: c.slot + 1000,
        input: source,
        output: destination,
        intermediates: assets[1..assets.len() - 1].to_vec(),
        legs: vec![],
    };
    let graph = native_wire::lower_native_graph(header, proposals, &planned)?;
    let ix = swap_wire::compile_swap_graph(SETTLE, &graph, |k| {
        read(&initialized.resulting_accounts, k)
    })?;
    let mut wire = setup.instructions().to_vec();
    wire.push(ix);
    let executed = runtime.process_transaction_instructions(&wire, &before);
    if executed.program_result.is_err() {
        return Err(format!(
            "wallet full wire SBF {:?}",
            executed.program_result
        ));
    }
    let original_input = if native && !existing_native {
        0
    } else {
        amount(&before, &source.token)
    };
    let economic = if graph.intermediates.is_empty() {
        let mut allocated = proposals.to_vec();
        for proposal in &mut allocated {
            proposal.stage = 1;
            proposal.product_id = Some("captured-sbf-product".into());
        }
        let decimals = get(&before, &destination.mint).data[44];
        let exposure = u64::try_from(
            u128::from(output)
                .checked_mul(1u128 << 32)
                .ok_or("economic exposure overflow")?
                / 10u128.pow(u32::from(decimals)),
        )
        .map_err(|_| "economic exposure range")?;
        if exposure == 0 {
            return Err("economic exposure floor".into());
        }
        let direct_product = DirectProduct {
            product_id: "captured-sbf-product".into(),
            product: MeshProduct {
                policy,
                claim: None,
                destination: destination.token,
                mint: destination.mint,
                token_program: destination.token_program,
                model: 0,
                conservative_bps: 10_000,
                policy_version: 1,
                numerator: 1,
                denominator: 1,
            },
            minimum_output_atoms: output,
        };
        let bank_plan = exposure_wire::plan_direct_execution_bank(
            SETTLE,
            Pubkey::new_from_array([217; 32]),
            WALLET,
            source.mint,
            std::slice::from_ref(&direct_product),
            &allocated,
            &bank,
        )?;
        if bank_plan.assets != [source, destination]
            || bank_plan.nonce != nonce
            || !bank_plan.keys.contains(&policy.to_string())
            || !bank_plan
                .optional_wallet_accounts
                .contains(&destination.token.to_string())
        {
            return Err("economic execution bank plan".into());
        }
        let ix = exposure_wire::compile_direct_exposure(
            SETTLE,
            DirectExposureSpec {
                buyer: WALLET,
                buyer_nonce: nonce,
                input: source,
                buyer_sequence: 0,
                input_atoms: input,
                minimum_exposure_q32: exposure,
                deadline_slot: c.slot + 1_000,
                maximum_policy_age: 150,
                allow_underlying_closed: false,
                products: vec![direct_product.clone()],
            },
            &allocated,
            &planned,
        )?;
        let mut economic_wire = setup.instructions().to_vec();
        economic_wire.push(ix);
        let result = runtime.process_transaction_instructions(&economic_wire, &before);
        if result.program_result.is_err()
            || amount(&result.resulting_accounts, &destination.token) != output
            || amount(&result.resulting_accounts, &source.token)
                != if native {
                    original_input
                } else {
                    original_input - input
                }
            || result.return_data.get(..8) != Some(b"SKEWEXP1")
        {
            return Err(format!("economic wallet wire {:?}", result.program_result));
        }
        let mut changed = allocated.clone();
        changed[0].product_id = Some("substituted-product".into());
        if exposure_wire::compile_direct_exposure(
            SETTLE,
            DirectExposureSpec {
                buyer: WALLET,
                buyer_nonce: nonce,
                input: source,
                buyer_sequence: 0,
                input_atoms: input,
                minimum_exposure_q32: exposure,
                deadline_slot: c.slot + 1_000,
                maximum_policy_age: 150,
                allow_underlying_closed: false,
                products: vec![direct_product],
            },
            &changed,
            &planned,
        )
        .is_ok()
        {
            return Err("economic product substitution accepted".into());
        }
        Some((result.compute_units_consumed, bank_plan.keys.len()))
    } else {
        None
    };
    if amount(&executed.resulting_accounts, &destination.token) != output
        || amount(&executed.resulting_accounts, &source.token)
            != if native {
                original_input
            } else {
                original_input - input
            }
    {
        return Err("wallet full wire balance delta".into());
    }
    for asset in &graph.intermediates {
        if amount(&executed.resulting_accounts, &asset.token) != 0 {
            return Err("wallet intermediate residue".into());
        }
    }
    let nonce_rent = setup
        .created_nonce()
        .map_or(0, |key| get(&executed.resulting_accounts, &key).lamports);
    let rents = setup.created_assets().try_fold(nonce_rent, |sum, asset| {
        let lamports = get(&executed.resulting_accounts, &asset.token).lamports;
        let reserve = if asset.mint == pk("So11111111111111111111111111111111111111112") {
            lamports
                .checked_sub(amount(&executed.resulting_accounts, &asset.token))
                .ok_or("wallet native reserve")?
        } else {
            lamports
        };
        sum.checked_add(reserve).ok_or("wallet rent overflow")
    })?;
    let actual_debit =
        get(&before, &WALLET).lamports - get(&executed.resulting_accounts, &WALLET).lamports;
    if actual_debit != rents + setup.wrap_lamports() {
        return Err(format!(
            "wallet lamport debit {actual_debit} rents {rents} wrap {}",
            setup.wrap_lamports()
        ));
    }
    let table = solana_message::AddressLookupTableAccount {
        key: Pubkey::new_from_array([213; 32]),
        addresses: wire
            .iter()
            .flat_map(|ix| ix.accounts.iter())
            .filter(|m| !m.is_signer)
            .map(|m| m.pubkey)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    };
    let mut cb = vec![2];
    cb.extend_from_slice(&1_400_000u32.to_le_bytes());
    let mut envelope = vec![solana_instruction::Instruction {
        program_id: pk("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data: cb,
    }];
    envelope.extend(wire.clone());
    let message = skew_execution_host::onebook_wire::compile_unsigned_v0(
        WALLET,
        &envelope,
        &[table],
        [211; 32],
    )?;
    wire.last_mut().unwrap().data[20..28].copy_from_slice(&(output + 1).to_le_bytes());
    let rejected = runtime.process_transaction_instructions(&wire, &before);
    if rejected.program_result.is_ok() || rejected.resulting_accounts != before {
        return Err("wallet creation/funding atomic rollback".into());
    }
    Ok(
        json!({"passed":true,"nativeWrapLamports":setup.wrap_lamports(),"createdAtas":setup.created_assets().count(),
        "createdNonce":setup.created_nonce().is_some(),
        "setupCU":initialized.compute_units_consumed,"setupOnlyPacketBytes":setup_message.len()+65,
        "setupProjectionAttacksRejected":1+usize::from(setup.created_nonce().is_some()),
        "fullWireCU":executed.compute_units_consumed,
        "walletLamportsDebited":actual_debit,"rentLamports":rents,"signedPacketBytes":message.len()+65,
        "syntheticFrozenAltForPacketSizing":true,"instructions":envelope.len(),"creationAndFundingRollback":true,
        "priorNativeBalancePreserved":native&&existing_native,
        "economicExposureCU":economic.map(|value|value.0),
        "executionBankAccounts":economic.map(|value|value.1),
        "economicReceipt":economic.is_some()}),
    )
}

fn prove_joint(
    runtime: &mollusk_svm::Mollusk,
    c: &Case,
    proposals: &[NativeSwapProposal],
    reference: &[(solana_instruction::Instruction, bool)],
) -> Result<serde_json::Value, String> {
    let reference = runtime.process_transaction_instructions(
        &reference
            .iter()
            .map(|(ix, _)| ix.clone())
            .collect::<Vec<_>>(),
        &c.a,
    );
    if reference.program_result.is_err() {
        return Err("joint direct reference failed".into());
    }
    let input = amount(&c.a, &c.input) - amount(&reference.resulting_accounts, &c.input);
    let output = amount(&reference.resulting_accounts, &c.output) - amount(&c.a, &c.output);
    let bank = snapshot(c);
    let source = asset(c, c.input);
    let destination = asset(c, c.output);
    let mut intermediate_mints = Vec::new();
    for p in proposals {
        for mint in [&p.market.input_mint, &p.market.output_mint] {
            let mint = pk(mint);
            if mint != source.mint
                && mint != destination.mint
                && !intermediate_mints.contains(&mint)
            {
                intermediate_mints.push(mint);
            }
        }
    }
    let intermediates = intermediate_mints
        .into_iter()
        .map(|mint| native_wire::wallet_asset(WALLET, mint, &bank))
        .collect::<Result<Vec<_>, _>>()?;
    let header = SwapGraph {
        owner: WALLET,
        sequence: 0,
        input_atoms: input,
        minimum_output_atoms: output,
        deadline_slot: c.slot + 1000,
        input: source,
        output: destination,
        intermediates,
        legs: vec![],
    };
    let graph = native_wire::lower_native_graph(header.clone(), proposals, &bank)?;
    let mut changed = proposals.to_vec();
    changed[0].input_atoms += 1;
    if native_wire::lower_native_graph(header.clone(), &changed, &bank).is_ok() {
        return Err("joint changed allocation accepted".into());
    }
    changed = proposals.to_vec();
    changed[0].stage = 3;
    if native_wire::lower_native_graph(header, &changed, &bank).is_ok() {
        return Err("joint changed stage accepted".into());
    }
    let instruction = swap_wire::compile_swap_graph(SETTLE, &graph, |key| {
        let a =
            c.a.iter()
                .find(|(k, _)| k == key)
                .ok_or("joint missing account")?;
        Ok(AccountView {
            owner: a.1.owner,
            executable: a.1.executable,
            data: &a.1.data,
        })
    })?;
    let execution = run(runtime, &instruction, &c.a);
    if execution.program_result.is_err() {
        return Err(format!("joint SBF {:?}", execution.program_result));
    }
    if amount(&c.a, &c.input) - amount(&execution.resulting_accounts, &c.input) != input
        || amount(&execution.resulting_accounts, &c.output) - amount(&c.a, &c.output) != output
    {
        return Err("joint exact raw deltas".into());
    }
    for asset in &graph.intermediates {
        if amount(&c.a, &asset.token) != amount(&execution.resulting_accounts, &asset.token) {
            return Err("joint old intermediate balance changed".into());
        }
    }
    let mut floor = instruction.clone();
    floor.data[20..28].copy_from_slice(&(output + 1).to_le_bytes());
    let failed = run(runtime, &floor, &c.a);
    if failed.program_result.is_ok() || failed.resulting_accounts != c.a {
        return Err("joint rollback failed".into());
    }
    Ok(
        json!({"passed":true,"inputAtoms":input.to_string(),"outputAtoms":output.to_string(),
        "computeUnits":execution.compute_units_consumed,"legs":graph.legs.len(),"intermediates":graph.intermediates.len(),
        "oldIntermediateBalancesPreserved":true,"floorRollback":true}),
    )
}
