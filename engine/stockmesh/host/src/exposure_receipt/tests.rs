use super::*;
use crate::{
    feed::{Account, Feed},
    onebook_wire::*,
    swap_wire::{AccountView, TokenAsset},
    wallet_wire::{frozen_lookup_table, WalletSetup},
};
use ed25519_dalek::{Signer, SigningKey};
use solana_instruction::Instruction;
use std::time::Duration;

fn k(byte: u8) -> Pubkey {
    Pubkey::new_from_array([byte; 32])
}
fn row(address: Pubkey, owner: Pubkey, data: Vec<u8>) -> Account {
    Account {
        key: address.to_string(),
        owner: owner.to_string(),
        executable: false,
        lamports: 10_000_000,
        data,
    }
}
fn mint(address: Pubkey) -> Account {
    let mut d = vec![0; 82];
    d[45] = 1;
    row(address, key(TOKEN).unwrap(), d)
}
fn balance(address: Pubkey, mint: Pubkey, owner: Pubkey, amount: u64) -> Account {
    let mut d = vec![0; 165];
    d[..32].copy_from_slice(mint.as_ref());
    d[32..64].copy_from_slice(owner.as_ref());
    d[64..72].copy_from_slice(&amount.to_le_bytes());
    d[108] = 1;
    row(address, key(TOKEN).unwrap(), d)
}

pub(crate) struct Fixture {
    pub feed: std::sync::Arc<Feed>,
    pub snapshot: std::sync::Arc<Snapshot>,
    pub intent: FrozenIntent,
    pub admission: Admission,
    pub message: Vec<u8>,
    pub entry: Entry,
    pub expected: ExpectedExposure,
    pub value: Value,
}

pub(crate) fn fixture() -> Fixture {
    let buyer = SigningKey::from_bytes(&[201; 32]);
    let seller = SigningKey::from_bytes(&[202; 32]);
    let owner = Pubkey::new_from_array(buyer.verifying_key().to_bytes());
    let live = Pubkey::new_from_array(seller.verifying_key().to_bytes());
    let program = k(90);
    let cash = key(crate::market::USDC_MINT).unwrap();
    let cash_source = k(4);
    let token_program = key(TOKEN).unwrap();
    let nonce = Pubkey::find_program_address(&[b"stocklana", owner.as_ref()], &program).0;
    let mut nonce_data = vec![0; 64];
    nonce_data[..8].copy_from_slice(b"SKEWSEQ1");
    nonce_data[8..40].copy_from_slice(owner.as_ref());
    let mut bank = vec![
        mint(cash),
        balance(cash_source, cash, owner, 10000),
        row(nonce, program, nonce_data),
    ];
    let mut admissions = Vec::new();
    let mut products = Vec::new();
    let mut sellers = Vec::new();
    for i in 0..2u8 {
        let mint_key = k(10 + i);
        let policy_authority = k(20 + i);
        let policy_instrument = instrument_id("NVDA").unwrap();
        let issuer_name = format!("FIXTURE-{i}");
        let policy_issuer = issuer_id(&issuer_name).unwrap();
        let rights_hash = [i + 1; 32];
        let policy = stock_policy_v2_address(
            &program,
            &policy_authority,
            &policy_instrument,
            &cash,
            &mint_key,
            &rights_hash,
        );
        let destination = Pubkey::find_program_address(
            &[owner.as_ref(), token_program.as_ref(), mint_key.as_ref()],
            &key(ASSOCIATED_TOKEN).unwrap(),
        )
        .0;
        let claim = k(40 + i);
        let stock = k(50 + i);
        let seller_cash = k(60 + i);
        let order = k(70 + i);
        let product_owner = if i == 0 { k(100) } else { live };
        let stock_owner = if i == 0 { order } else { live };
        let mut policy_data = vec![0; STOCK_POLICY_V2_LEN];
        policy_data[..8].copy_from_slice(STOCK_POLICY_V2_TAG);
        policy_data[8..40].copy_from_slice(policy_authority.as_ref());
        policy_data[40..72].copy_from_slice(&policy_instrument);
        policy_data[72..104].copy_from_slice(&policy_issuer);
        policy_data[104..136].copy_from_slice(cash.as_ref());
        policy_data[136..168].copy_from_slice(mint_key.as_ref());
        policy_data[168..200].copy_from_slice(&rights_hash);
        policy_data[200..208].copy_from_slice(&1u64.to_le_bytes());
        policy_data[208..216].copy_from_slice(&100u64.to_le_bytes());
        policy_data[216..224].copy_from_slice(&300u64.to_le_bytes());
        policy_data[224] = 17;
        let mut claim_data = vec![0; 288];
        claim_data[..8].copy_from_slice(b"SKEWCLM1");
        let identity = ProductIdentity {
            instrument: "NVDA".into(),
            issuer: issuer_name,
            mint: mint_key.to_string(),
            token_program: TOKEN.into(),
            rights_hash,
            raw_decimals: 0,
        };
        admissions.push(ProductAdmission {
            identity,
            policy: policy.to_string(),
            policy_data_hash: Sha256::digest(&policy_data).into(),
            claim: Some(claim.to_string()),
            claim_data_hash: Some(Sha256::digest(&claim_data).into()),
            policy_version: 1,
            model: 0,
            numerator: u64::from(i) + 1,
            denominator: 1,
            conservative_bps: 10000,
        });
        bank.extend([
            mint(mint_key),
            row(policy, program, policy_data),
            row(claim, program, claim_data),
            balance(destination, mint_key, owner, 0),
            balance(stock, mint_key, stock_owner, 100),
            balance(seller_cash, cash, product_owner, 0),
        ]);
        products.push(MeshProduct {
            policy,
            claim: if i == 0 { Some(claim) } else { None },
            destination,
            mint: mint_key,
            token_program,
            model: 0,
            conservative_bps: 10000,
            policy_version: 1,
            numerator: u64::from(i) + 1,
            denominator: 1,
        });
        sellers.push(MeshSeller {
            product_index: usize::from(i),
            owner: product_owner,
            authority: if i == 0 {
                MeshSellerAuthority::ClaimCell { order }
            } else {
                MeshSellerAuthority::Signed { nonce: order }
            },
            stock_source: stock,
            cash_destination: seller_cash,
            sequence_or_revision: 0,
            stock_atoms: 20,
            cash_atoms: 500,
            minimum_cash_atoms: 500,
        });
    }
    let fill = compile_mesh_fill(
        program,
        MeshFillSpec {
            buyer: owner,
            buyer_nonce: nonce,
            buyer_cash_source: cash_source,
            cash_mint: cash,
            cash_token_program: token_program,
            buyer_sequence: 0,
            buyer_input_atoms: 1000,
            minimum_exposure_q32: 60u64 << 32,
            deadline_slot: 200,
            maximum_policy_age: 150,
            allow_underlying_closed: false,
            products,
            sellers,
            residuals: vec![],
        },
    )
    .unwrap();
    let table = solana_message::AddressLookupTableAccount {
        key: k(80),
        addresses: fill
            .accounts
            .iter()
            .filter(|a| !a.is_signer)
            .map(|a| a.pubkey)
            .collect(),
    };
    let mut table_data = vec![0; 56];
    table_data[..4].copy_from_slice(&1u32.to_le_bytes());
    table_data[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
    for address in &table.addresses {
        table_data.extend_from_slice(address.as_ref());
    }
    bank.push(row(
        table.key,
        key("AddressLookupTab1e1111111111111111111111111").unwrap(),
        table_data,
    ));
    let budget = Instruction {
        program_id: key(COMPUTE).unwrap(),
        accounts: vec![],
        data: [vec![2], 300_000u32.to_le_bytes().to_vec()].concat(),
    };
    let message = compile_unsigned_v0(
        owner,
        &[budget, fill],
        std::slice::from_ref(&table),
        [9; 32],
    )
    .unwrap();
    let feed = Feed::new(
        bank.iter().map(|a| a.key.clone()).collect(),
        Duration::from_secs(60),
        1_000_000,
    )
    .unwrap();
    let response = json!({"context":{"slot":100},"value":bank.iter().map(|a|json!({"owner":a.owner,"executable":a.executable,"lamports":a.lamports,"data":[STANDARD.encode(&a.data),"base64"]})).collect::<Vec<_>>()});
    let snapshot = feed.publish(&response).unwrap();
    let admission = Admission {
        settlement_program: program.to_string(),
        product_policy_hash: [44; 32],
        maximum_cu: 300000,
        maximum_heap_frame_bytes: 256 * 1024,
        maximum_compute_price: 0,
        allow_underlying_closed: false,
        products: admissions,
    };
    let intent = FrozenIntent {
        intent_id: [33; 32],
        owner: owner.to_string(),
        owner_nonce: 0,
        instrument: "NVDA".into(),
        input_mint: cash.to_string(),
        input_atoms: 1000,
        minimum_exposure_q32: 60u128 << 32,
        admitted_product_ids: admission
            .products
            .iter()
            .map(|p| p.identity.id().unwrap())
            .collect(),
        product_policy_hash: admission.product_policy_hash,
        world_generation_hash: snapshot.hash,
        deadline_slot: 200,
    };
    let expected = ExpectedExposure::bind(&snapshot, &intent, &admission, &message).unwrap();
    let mut wire = vec![2];
    wire.extend_from_slice(&buyer.sign(&message).to_bytes());
    wire.extend_from_slice(&seller.sign(&message).to_bytes());
    wire.extend_from_slice(&message);
    let mut entry = crate::sender::authorize(
        &wire,
        crate::sender::Authorization {
            intent_id: "economic-fixture".into(),
            message_hash: expected.message_hash,
            last_valid_height: 999,
            resources: expected.resources.clone(),
        },
    )
    .unwrap();
    entry.phase = Phase::Finalized;
    let msg = pipeline::decode(&message).unwrap();
    let solana_message::VersionedMessage::V0(msg) = msg else {
        panic!("v0 required")
    };
    let lookup = &msg.address_table_lookups[0];
    let loaded = |indices: &Vec<u8>| {
        indices
            .iter()
            .map(|i| table.addresses[usize::from(*i)].to_string())
            .collect::<Vec<_>>()
    };
    let before = [10000, 0, 0, 100, 0, 100, 0];
    let after = [9000, 20, 20, 80, 500, 80, 500];
    let token_rows = |amounts: &[u64]| {
        expected.balance_bindings().iter().zip(amounts).map(|(b,a)|json!({"accountIndex":expected.keys.iter().position(|k|k==&b.account).unwrap(),"mint":b.mint,"owner":b.owner,"programId":b.token_program,"uiTokenAmount":{"amount":a.to_string(),"decimals":b.decimals,"uiAmount":999999999999.9}})).collect::<Vec<_>>()
    };
    let mut data = b"SKEWMSH1".to_vec();
    for value in [
        0u64,
        1000,
        60u64 << 32,
        2,
        2,
        20,
        20u64 << 32,
        20,
        40u64 << 32,
        0,
        20,
        500,
        0,
        20,
        500,
    ] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    let value = json!({"slot":101,"transaction":[STANDARD.encode(wire),"base64"],"meta":{"err":null,"fee":10000,"computeUnitsConsumed":70000,"loadedAddresses":{"writable":loaded(&lookup.writable_indexes),"readonly":loaded(&lookup.readonly_indexes)},"preTokenBalances":token_rows(&before),"postTokenBalances":token_rows(&after),"returnData":{"programId":program.to_string(),"data":[STANDARD.encode(data),"base64"]}}});
    Fixture {
        feed: std::sync::Arc::new(feed),
        snapshot,
        intent,
        admission,
        message,
        entry,
        expected,
        value,
    }
}

#[test]
fn multiple_claims_and_internal_sellers_have_one_economic_receipt() {
    let f = fixture();
    let receipt = f.expected.verify(&f.entry, &f.value).unwrap().summary();
    assert_eq!(receipt["actualExposureQ32"], (60u64 << 32).to_string());
    assert_eq!(receipt["products"].as_array().unwrap().len(), 2);
    let restored: ExpectedExposure =
        serde_json::from_slice(&serde_json::to_vec(&f.expected).unwrap()).unwrap();
    assert_eq!(
        restored.verify(&f.entry, &f.value).unwrap().summary(),
        receipt
    );
    f.feed.validate_fence(&f.snapshot).unwrap();
}

#[test]
fn market_commitment_is_a_projection_of_the_same_execution_bank() {
    let f = fixture();
    let keys = f
        .admission
        .products
        .iter()
        .map(|p| p.identity.mint.clone())
        .collect::<Vec<_>>();
    let market = f.snapshot.project(&keys).unwrap();
    assert_ne!(market.hash, f.snapshot.hash);
    assert_eq!(market.observed, f.snapshot.observed);
    assert_eq!(market.slot_advanced, f.snapshot.slot_advanced);
    let mut intent = f.intent.clone();
    intent.world_generation_hash = market.hash;
    let expected =
        ExpectedExposure::bind_market(&f.snapshot, &keys, &intent, &f.admission, &f.message)
            .unwrap();
    assert!(expected.verify(&f.entry, &f.value).is_ok());
    assert!(ExpectedExposure::bind(&f.snapshot, &intent, &f.admission, &f.message).is_err());
    for bad_keys in [
        vec![keys[0].clone()],
        vec![keys[1].clone(), keys[0].clone()],
        vec![keys[0].clone(), keys[0].clone()],
        vec![k(124).to_string()],
    ] {
        assert!(ExpectedExposure::bind_market(
            &f.snapshot,
            &bad_keys,
            &intent,
            &f.admission,
            &f.message
        )
        .is_err());
    }
    // Adding wallet state cannot hide a changed market or weaken token identity.
    let all = f
        .snapshot
        .accounts
        .iter()
        .map(|a| a.key.clone())
        .collect::<Vec<_>>();
    let mut mutated = f.snapshot.project(&all).unwrap();
    mutated
        .accounts
        .iter_mut()
        .find(|a| a.key == keys[0])
        .unwrap()
        .data[44] = 1;
    assert!(
        ExpectedExposure::bind_market(&mutated, &keys, &intent, &f.admission, &f.message).is_err()
    );
    let mut mutated = f.snapshot.project(&all).unwrap();
    mutated
        .accounts
        .iter_mut()
        .find(|a| a.key == expected.input_token.account)
        .unwrap()
        .data[32..64]
        .fill(9);
    assert!(
        ExpectedExposure::bind_market(&mutated, &keys, &intent, &f.admission, &f.message).is_err()
    );
}

#[test]
fn frozen_intent_and_actual_wire_cannot_disagree() {
    let f = fixture();
    for attack in [
        "floor",
        "nonce",
        "issuer",
        "rights",
        "policy",
        "policy_hash",
        "conversion",
        "world",
        "compute",
        "extra_action",
    ] {
        let mut intent = f.intent.clone();
        let mut admission = f.admission.clone();
        let mut message = pipeline::decode(&f.message).unwrap();
        match attack {
            "floor" => intent.minimum_exposure_q32 += 1,
            "nonce" => intent.owner_nonce += 1,
            "issuer" => admission.products[0].identity.issuer = "OTHER".into(),
            "rights" => admission.products[0].identity.rights_hash = [9; 32],
            "policy" => admission.products[0].policy_data_hash = [1; 32],
            "policy_hash" => admission.product_policy_hash = [2; 32],
            "conversion" => admission.products[0].numerator = 2,
            "world" => intent.world_generation_hash = [3; 32],
            "compute" => admission.maximum_cu = 299999,
            "extra_action" => {
                let solana_message::VersionedMessage::V0(m) = &mut message else {
                    unreachable!()
                };
                m.instructions.push(m.instructions[0].clone());
            }
            _ => unreachable!(),
        }
        assert!(
            ExpectedExposure::bind(&f.snapshot, &intent, &admission, &message.serialize()).is_err(),
            "{attack}"
        );
    }
}

#[test]
fn heap_frame_is_part_of_the_exact_admitted_wallet_message() {
    let f = fixture();
    let message_with_heap = |bytes: u32, duplicate: bool| {
        let mut message = pipeline::decode(&f.message).unwrap();
        let instructions = match &mut message {
            solana_message::VersionedMessage::Legacy(message) => &mut message.instructions,
            solana_message::VersionedMessage::V0(message) => &mut message.instructions,
        };
        let mut heap = instructions[0].clone();
        heap.data = [vec![1], bytes.to_le_bytes().to_vec()].concat();
        instructions.insert(1, heap.clone());
        if duplicate {
            instructions.insert(2, heap);
        }
        message.serialize()
    };

    let message = message_with_heap(128 * 1024, false);
    let expected = ExpectedExposure::bind(&f.snapshot, &f.intent, &f.admission, &message).unwrap();
    assert_eq!(expected.heap_frame_bytes(), 128 * 1024);

    for bytes in [31 * 1024, 128 * 1024 + 1, 257 * 1024] {
        assert!(ExpectedExposure::bind(
            &f.snapshot,
            &f.intent,
            &f.admission,
            &message_with_heap(bytes, false)
        )
        .is_err());
    }
    assert!(ExpectedExposure::bind(
        &f.snapshot,
        &f.intent,
        &f.admission,
        &message_with_heap(128 * 1024, true)
    )
    .is_err());
    let mut smaller_policy = f.admission.clone();
    smaller_policy.maximum_heap_frame_bytes = 64 * 1024;
    assert!(ExpectedExposure::bind(&f.snapshot, &f.intent, &smaller_policy, &message).is_err());
}

#[test]
fn token_balance_deltas_cannot_hide_persistent_authority_changes() {
    let binding = TokenBinding {
        account: k(60).to_string(),
        mint: k(61).to_string(),
        owner: k(62).to_string(),
        token_program: TOKEN.into(),
        decimals: 0,
    };
    let original = balance(k(60), k(61), k(62), 123);
    assert_eq!(raw_amount(&original, &binding).unwrap(), 123);
    for offset in [72, 121, 129] {
        let mut changed = original.clone();
        changed.data[offset] = 1;
        assert!(
            raw_amount(&changed, &binding).is_err(),
            "authority field {offset}"
        );
    }
}

#[test]
fn economic_receipt_attacks_do_not_finalize() {
    let f = fixture();
    for attack in [
        "unfinalized",
        "signature",
        "wire",
        "error",
        "alt",
        "owner",
        "program",
        "mint",
        "duplicate",
        "input",
        "seller",
        "return_program",
        "return_sum",
        "return_product",
        "return_seller",
        "return_trailing",
        "cu",
        "slot",
    ] {
        let mut entry = f.entry.clone();
        let mut value = f.value.clone();
        match attack {
            "unfinalized" => entry.phase = Phase::Submitted,
            "signature" => entry.signature = "other".into(),
            "wire" => value["transaction"][0] = json!("AAAA"),
            "error" => value["meta"]["err"] = json!("failed"),
            "alt" => value["meta"]["loadedAddresses"]["writable"][0] = json!(k(123).to_string()),
            "owner" => value["meta"]["postTokenBalances"][1]["owner"] = json!("other"),
            "program" => value["meta"]["postTokenBalances"][1]["programId"] = json!(TOKEN_2022),
            "mint" => value["meta"]["postTokenBalances"][1]["mint"] = json!(k(99).to_string()),
            "duplicate" => {
                let x = value["meta"]["postTokenBalances"][1].clone();
                value["meta"]["postTokenBalances"]
                    .as_array_mut()
                    .unwrap()
                    .push(x);
            }
            "input" => {
                value["meta"]["postTokenBalances"][0]["uiTokenAmount"]["amount"] = json!("8999")
            }
            "seller" => {
                value["meta"]["postTokenBalances"][4]["uiTokenAmount"]["amount"] = json!("499")
            }
            "return_program" => value["meta"]["returnData"]["programId"] = json!(k(89).to_string()),
            "return_sum" | "return_product" | "return_seller" | "return_trailing" => {
                let mut data = STANDARD
                    .decode(value["meta"]["returnData"]["data"][0].as_str().unwrap())
                    .unwrap();
                let offset = match attack {
                    "return_sum" => 24,
                    "return_product" => 56,
                    "return_seller" => 96,
                    _ => 0,
                };
                if offset == 0 {
                    data.push(0);
                } else {
                    data[offset] ^= 1;
                }
                value["meta"]["returnData"]["data"][0] = json!(STANDARD.encode(data));
            }
            "cu" => value["meta"]["computeUnitsConsumed"] = json!(300001),
            "slot" => value["slot"] = json!(201),
            _ => unreachable!(),
        }
        assert!(f.expected.verify(&entry, &value).is_err(), "{attack}");
    }
}

#[test]
fn exact_wallet_setup_prefix_initializes_missing_product_atas_and_nonce() {
    let f = fixture();
    let owner = key(&f.intent.owner).unwrap();
    let program = key(&f.admission.settlement_program).unwrap();
    let token_program = key(TOKEN).unwrap();
    let ata_program = key(ASSOCIATED_TOKEN).unwrap();
    let system = key(SYSTEM).unwrap();
    let nonce = Pubkey::find_program_address(&[b"stocklana", owner.as_ref()], &program).0;
    let mut accounts = f.snapshot.accounts.clone();
    for product in &f.expected.products {
        let destination = accounts
            .iter_mut()
            .find(|account| account.key == product.token.account)
            .unwrap();
        destination.owner = SYSTEM.into();
        destination.executable = false;
        destination.lamports = 0;
        destination.data.clear();
    }
    let nonce_account = accounts
        .iter_mut()
        .find(|account| account.key == nonce.to_string())
        .unwrap();
    nonce_account.owner = SYSTEM.into();
    nonce_account.lamports = 0;
    nonce_account.data.clear();
    accounts.push(Account {
        key: owner.to_string(),
        owner: SYSTEM.into(),
        executable: false,
        lamports: 100_000_000,
        data: vec![],
    });
    for address in [program, token_program, ata_program] {
        accounts.push(Account {
            key: address.to_string(),
            owner: system.to_string(),
            executable: true,
            lamports: 1,
            data: vec![],
        });
    }
    let preliminary = Snapshot {
        slot: f.snapshot.slot,
        generation: f.snapshot.generation,
        hash: f.snapshot.hash,
        accounts,
        observed: f.snapshot.observed,
        slot_advanced: f.snapshot.slot_advanced,
        revision: f.snapshot.revision,
    };
    let read = |address: &Pubkey| -> Result<AccountView<'_>> {
        let value = preliminary
            .accounts
            .iter()
            .find(|account| account.key == address.to_string())
            .ok_or("test setup account")?;
        Ok(AccountView {
            owner: key(&value.owner)?,
            executable: value.executable,
            data: &value.data,
        })
    };
    let assets = f
        .expected
        .products
        .iter()
        .map(|product| TokenAsset {
            token: key(&product.token.account).unwrap(),
            mint: key(&product.token.mint).unwrap(),
            token_program,
        })
        .collect::<Vec<_>>();
    let (setup, observed_sequence) = WalletSetup::plan(owner, &assets, None, read)
        .unwrap()
        .with_observed_nonce(program, read)
        .unwrap();
    assert_eq!(observed_sequence, 0);

    let old = pipeline::decode(&f.message).unwrap();
    let old_keys = pipeline::resolved(&old, &f.snapshot).unwrap();
    let compiled = old.instructions().last().unwrap();
    let settlement = Instruction {
        program_id: key(&old_keys[usize::from(compiled.program_id_index)]).unwrap(),
        accounts: compiled
            .accounts
            .iter()
            .map(|index| solana_instruction::AccountMeta {
                pubkey: key(&old_keys[usize::from(*index)]).unwrap(),
                is_signer: usize::from(*index) < usize::from(old.header().num_required_signatures),
                is_writable: old.is_maybe_writable(usize::from(*index), None),
            })
            .collect(),
        data: compiled.data.clone(),
    };
    let budget = Instruction {
        program_id: key(COMPUTE).unwrap(),
        accounts: vec![],
        data: [vec![2], 300_000u32.to_le_bytes().to_vec()].concat(),
    };
    let mut instructions = vec![budget];
    instructions.extend_from_slice(setup.instructions());
    instructions.push(settlement);
    let table = solana_message::AddressLookupTableAccount {
        key: k(80),
        addresses: instructions
            .iter()
            .flat_map(|instruction| &instruction.accounts)
            .filter(|meta| !meta.is_signer)
            .map(|meta| meta.pubkey)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    };
    let mut table_data = vec![0; 56];
    table_data[..4].copy_from_slice(&1u32.to_le_bytes());
    table_data[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
    for address in &table.addresses {
        table_data.extend_from_slice(address.as_ref());
    }
    let mut complete = preliminary.accounts;
    let table_account = complete
        .iter_mut()
        .find(|account| account.key == table.key.to_string())
        .unwrap();
    table_account.data = table_data;
    let feed = Feed::new(
        complete.iter().map(|account| account.key.clone()).collect(),
        Duration::from_secs(60),
        1_000_000,
    )
    .unwrap();
    let response = json!({"context":{"slot":100},"value":complete.iter().map(|account|json!({
        "owner":account.owner,"executable":account.executable,"lamports":account.lamports,
        "data":[STANDARD.encode(&account.data),"base64"]})).collect::<Vec<_>>()});
    let snapshot = feed.publish(&response).unwrap();
    let decoded_table = frozen_lookup_table(&snapshot, table.key).unwrap();
    assert_eq!(decoded_table.addresses, table.addresses);
    for attack in ["authority", "future", "duplicate"] {
        let mut changed = (*snapshot).clone();
        let account = changed
            .accounts
            .iter_mut()
            .find(|account| account.key == table.key.to_string())
            .unwrap();
        match attack {
            "authority" => account.data[21] = 1,
            "future" => account.data[12..20].copy_from_slice(&snapshot.slot.to_le_bytes()),
            "duplicate" => {
                let first = account.data[56..88].to_vec();
                account.data.extend_from_slice(&first);
            }
            _ => unreachable!(),
        }
        assert!(
            frozen_lookup_table(&changed, table.key).is_err(),
            "{attack}"
        );
    }
    let message = compile_unsigned_v0(owner, &instructions, &[table], [19; 32]).unwrap();
    let mut intent = f.intent.clone();
    intent.world_generation_hash = snapshot.hash;
    let expected = ExpectedExposure::bind(&snapshot, &intent, &f.admission, &message).unwrap();
    assert_eq!(expected.setup.created_tokens.len(), 2);
    let nonce_text = nonce.to_string();
    assert_eq!(
        expected.setup.created_nonce.as_deref(),
        Some(nonce_text.as_str())
    );
    let setup_message = expected.setup_message(&message).unwrap().unwrap();
    let setup_decoded = pipeline::decode(&setup_message).unwrap();
    let setup_keys = pipeline::resolved(&setup_decoded, &snapshot).unwrap();
    assert_eq!(setup_decoded.instructions().len(), 4);
    assert!(setup_decoded.instructions().iter().all(|instruction| {
        let program = &setup_keys[usize::from(instruction.program_id_index)];
        program == COMPUTE
            || program == ASSOCIATED_TOKEN
            || (program == &f.admission.settlement_program && instruction.data == [0])
    }));
    let mut setup_returned = Vec::new();
    for address in expected.setup_only_addresses() {
        if address == owner.to_string() {
            setup_returned.push(Account {
                key: address,
                owner: SYSTEM.into(),
                executable: false,
                lamports: 70_000_000,
                data: vec![],
            });
        } else if address == nonce.to_string() {
            let mut data = vec![0; 64];
            data[..8].copy_from_slice(b"SKEWSEQ1");
            data[8..40].copy_from_slice(owner.as_ref());
            setup_returned.push(Account {
                key: address,
                owner: program.to_string(),
                executable: false,
                lamports: 10_000_000,
                data,
            });
        } else {
            let binding = expected
                .setup
                .created_tokens
                .iter()
                .find(|binding| binding.account == address)
                .unwrap();
            let mut value = balance(
                key(&binding.account).unwrap(),
                key(&binding.mint).unwrap(),
                owner,
                0,
            );
            value.owner = binding.token_program.clone();
            value.lamports = 10_000_000;
            setup_returned.push(value);
        }
    }
    expected
        .verify_setup_only(&snapshot, &setup_returned)
        .unwrap();
    for attack in ["payer", "nonce", "token", "message"] {
        if attack == "message" {
            let mut changed = message.clone();
            *changed.last_mut().unwrap() ^= 1;
            assert!(expected.setup_message(&changed).is_err());
            continue;
        }
        let mut changed = setup_returned.clone();
        match attack {
            "payer" => changed[0].lamports += 1,
            "nonce" => {
                let value = changed
                    .iter_mut()
                    .find(|account| account.key == nonce.to_string())
                    .unwrap();
                value.data[40..48].copy_from_slice(&1u64.to_le_bytes());
            }
            "token" => {
                let value = changed
                    .iter_mut()
                    .find(|account| {
                        expected
                            .setup
                            .created_tokens
                            .iter()
                            .any(|binding| binding.account == account.key)
                    })
                    .unwrap();
                value.data[64..72].copy_from_slice(&1u64.to_le_bytes());
            }
            _ => unreachable!(),
        }
        assert!(expected.verify_setup_only(&snapshot, &changed).is_err());
    }

    let before = [10_000, 0, 0, 100, 0, 100, 0];
    let after = [9_000, 20, 20, 80, 500, 80, 500];
    assert!(expected
        .outcome(&f.value["meta"]["returnData"], &before, &after, 70_000)
        .is_ok());
    let mut returned = expected
        .observation_bindings()
        .iter()
        .zip(after)
        .map(|(binding, amount)| {
            let mut account = balance(
                key(&binding.account).unwrap(),
                key(&binding.mint).unwrap(),
                owner,
                amount,
            );
            account.owner = binding.token_program.clone();
            account
        })
        .collect::<Vec<_>>();
    returned.push(Account {
        key: owner.to_string(),
        owner: SYSTEM.into(),
        executable: false,
        lamports: 70_000_000,
        data: vec![],
    });
    let mut nonce_data = vec![0; 64];
    nonce_data[..8].copy_from_slice(b"SKEWSEQ1");
    nonce_data[8..40].copy_from_slice(owner.as_ref());
    nonce_data[40..48].copy_from_slice(&1u64.to_le_bytes());
    returned.push(Account {
        key: nonce.to_string(),
        owner: program.to_string(),
        executable: false,
        lamports: 10_000_000,
        data: nonce_data,
    });
    expected
        .verify_simulated_setup(&snapshot, &after, &returned)
        .unwrap();
    let restored: ExpectedExposure =
        serde_json::from_slice(&serde_json::to_vec(&expected).unwrap()).unwrap();
    restored
        .verify_simulated_setup(&snapshot, &after, &returned)
        .unwrap();
    let mut pre_lamports = expected
        .keys
        .iter()
        .map(|address| {
            snapshot
                .accounts
                .iter()
                .find(|account| &account.key == address)
                .map_or(0, |account| account.lamports)
        })
        .collect::<Vec<_>>();
    let mut post_lamports = pre_lamports.clone();
    let position = |address: &str| expected.keys.iter().position(|key| key == address).unwrap();
    pre_lamports[position(&owner.to_string())] = 100_000_000;
    post_lamports[position(&owner.to_string())] = 69_995_000;
    for binding in &expected.setup.created_tokens {
        pre_lamports[position(&binding.account)] = 0;
        post_lamports[position(&binding.account)] = 10_000_000;
    }
    pre_lamports[position(&nonce.to_string())] = 0;
    post_lamports[position(&nonce.to_string())] = 10_000_000;
    let landed = json!({"fee":5000,"preBalances":pre_lamports,"postBalances":post_lamports});
    expected.verify_landed_setup(&landed, &after).unwrap();
    let mut bad_landed = landed.clone();
    bad_landed["postBalances"][position(&owner.to_string())] = json!(69_995_001u64);
    assert!(expected.verify_landed_setup(&bad_landed, &after).is_err());

    let mut bad = pipeline::decode(&message).unwrap();
    let instructions = match &mut bad {
        solana_message::VersionedMessage::Legacy(message) => &mut message.instructions,
        solana_message::VersionedMessage::V0(message) => &mut message.instructions,
    };
    instructions[1].data[0] = 1;
    assert!(ExpectedExposure::bind(&snapshot, &intent, &f.admission, &bad.serialize()).is_err());
}

#[test]
fn six_asset_first_use_setup_has_a_typed_thirteen_instruction_ceiling() {
    // Parser/compiler envelope regression, not an SBF or issuer authorization.
    let f=fixture();
    let owner=key(&f.intent.owner).unwrap();
    let program=key(&f.admission.settlement_program).unwrap();
    let token=key(TOKEN).unwrap();let ata=key(ASSOCIATED_TOKEN).unwrap();let system=key(SYSTEM).unwrap();
    let nonce=Pubkey::find_program_address(&[b"stocklana",owner.as_ref()],&program).0;
    let mints=[key(WSOL).unwrap(),key(crate::market::USDC_MINT).unwrap(),k(100),k(101),k(102),k(103)];
    let assets=mints.iter().map(|mint|TokenAsset{mint:*mint,token_program:token,
        token:Pubkey::find_program_address(&[owner.as_ref(),token.as_ref(),mint.as_ref()],&ata).0}).collect::<Vec<_>>();
    let mut snapshot=(*f.snapshot).clone();snapshot.accounts.clear();
    snapshot.accounts.push(row(owner,system,vec![]));
    snapshot.accounts.push(row(nonce,system,vec![]));
    for address in [program,token,ata] {let mut a=row(address,system,vec![]);a.executable=true;snapshot.accounts.push(a);}
    for asset in &assets {snapshot.accounts.push(mint(asset.mint));snapshot.accounts.push(row(asset.token,system,vec![]));}
    let read=|address:&Pubkey| {let a=snapshot.accounts.iter().find(|a|a.key==address.to_string()).ok_or("test setup account")?;
        Ok(AccountView{owner:key(&a.owner)?,executable:a.executable,data:&a.data})};
    let setup=WalletSetup::plan(owner,&assets,Some(100),read).unwrap().with_nonce(program,0,read).unwrap();
    assert_eq!(setup.instructions().len(),9);
    let mut seven=assets.clone();seven.push(assets[0]);
    assert!(WalletSetup::plan(owner,&seven,Some(100),read).is_err());
    let compute=|tag,n:u32|Instruction{program_id:key(COMPUTE).unwrap(),accounts:vec![],data:[vec![tag],n.to_le_bytes().to_vec()].concat()};
    let mut instructions=vec![compute(2,1_400_000),compute(1,262_144),Instruction{
        program_id:key(COMPUTE).unwrap(),accounts:vec![],data:[vec![3],0u64.to_le_bytes().to_vec()].concat()}];
    instructions.extend_from_slice(setup.instructions());
    instructions.push(Instruction{program_id:program,accounts:vec![],data:vec![13]});
    assert_eq!(instructions.len(),crate::wallet_wire::MAX_STOCK_TRANSACTION_INSTRUCTIONS);
    let table=solana_message::AddressLookupTableAccount{key:k(81),addresses:instructions.iter().flat_map(|ix|&ix.accounts)
        .filter(|a|!a.is_signer).map(|a|a.pubkey).collect::<BTreeSet<_>>().into_iter().collect()};
    let mut data=vec![0;56];data[..4].copy_from_slice(&1u32.to_le_bytes());data[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
    for address in &table.addresses{data.extend_from_slice(address.as_ref());}
    snapshot.accounts.push(row(table.key,key("AddressLookupTab1e1111111111111111111111111").unwrap(),data));
    let compile=|instructions:&[Instruction]|compile_unsigned_v0(owner,instructions,std::slice::from_ref(&table),[19;32]);
    let message=compile(&instructions).unwrap();assert!(message.len()+65<=1232);
    let decoded=pipeline::decode(&message).unwrap();let keys=pipeline::resolved(&decoded,&snapshot).unwrap();
    let (parsed,positions)=parse_setup(&snapshot,&decoded,&keys,&f.intent,&f.admission).unwrap();
    assert_eq!(parsed.created_tokens.len(),6);assert_eq!(positions.len(),9);assert_eq!(parsed.wrap_lamports,100);
    for attack in ["duplicate ATA","missing sync","zero transfer","wrong nonce"] {
        let mut bad=instructions.clone();
        match attack {
            "duplicate ATA"=>bad[5]=bad[4].clone(),
            "missing sync"=>bad[11].data=vec![16],
            "zero transfer"=>bad[10].data[4..12].fill(0),
            "wrong nonce"=>bad[3].accounts[1].pubkey=assets[0].token,
            _=>unreachable!(),
        }
        let decoded=pipeline::decode(&compile(&bad).unwrap()).unwrap();let keys=pipeline::resolved(&decoded,&snapshot).unwrap();
        assert!(parse_setup(&snapshot,&decoded,&keys,&f.intent,&f.admission).is_err(),"{attack}");
    }
    instructions.insert(0,compute(2,1_400_000));
    assert_eq!(compile(&instructions).unwrap_err(),"v0 message bounds");
}
