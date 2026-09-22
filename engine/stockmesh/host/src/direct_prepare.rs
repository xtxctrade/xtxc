//! Keyless secondary-liquidity preparation.
//!
//! No aggregator or issuer API supplies a quote, account meta or transaction.
//! A deployment manifest contributes only immutable settlement trust roots; all
//! DEX dependencies and wallet accounts are derived and refetched in one bank.
//! Primary issuer mint/redeem authority is deliberately outside this path.
use crate::{
    exposure_pipeline::{simulate_wallet_setup, PreparedExposure},
    exposure_receipt::{Admission, ProductAdmission},
    exposure_wire::{
        compile_direct_exposure, compile_direct_exposure_reflow, compile_funded_exposure_reflow,
        plan_direct_execution_bank, plan_funded_execution_bank, DirectExecutionBank,
        DirectExposureSpec, DirectProduct, FundedExposureSpec,
    },
    feed::Snapshot,
    lookup_registry::LookupRegistry,
    mesh::{ExecutorBid, FrozenIntent, ProductIdentity},
    native_wire,
    onebook_wire::MeshProduct,
    prepared_quotes::Candidate,
    receipt::Expected,
    rpc::Rpc,
    swap_wire::AccountView,
    wallet_wire::{frozen_lookup_table, WalletSetup},
    world::NativeSwapProposal,
    Result,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use solana_instruction::Instruction;
use solana_pubkey::{pubkey, Pubkey};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
const COMPUTE: Pubkey = pubkey!("ComputeBudget111111111111111111111111111111");
const UPGRADEABLE_LOADER: Pubkey = pubkey!("BPFLoaderUpgradeab1e11111111111111111111111");
const DEFAULT_HEAP_FRAME_BYTES: u32 = 32 * 1024;
const MAXIMUM_HEAP_FRAME_BYTES: u32 = 256 * 1024;
const USDC: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
mod basket;
pub use basket::{BasketPrepareRequest,BasketStockQuote,BasketCandidate};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DeploymentFile {
    schema: String,
    network: String,
    settlement_program: String,
    policy_authority: String,
    lookup_table: String,
    #[serde(default)]
    instrument_lookup_tables: BTreeMap<String, String>,
    executor: String,
    deadline_slots: u64,
    maximum_policy_age_slots: u64,
    compute_unit_limit: u32,
    maximum_heap_frame_bytes: u32,
    compute_unit_price_micro_lamports: u64,
    allow_underlying_closed: bool,
    products: Vec<DeploymentProductFile>,
}

const MAX_SELL_ASSETS: usize = 4;

/// SELL is one economic group (stage=1); its ordered mint path, not BUY's
/// funding-stage label, determines the hops. Preserve parallel splits and
/// prove exact intermediate conservation before requesting a final bank.
fn sell_asset_path(
    product_mint: Pubkey,
    output_mint: Pubkey,
    input_atoms: u64,
    proposals: &[NativeSwapProposal],
) -> Result<Vec<Pubkey>> {
    if product_mint == Pubkey::default() || output_mint == Pubkey::default()
        || product_mint == output_mint || input_atoms == 0
        || input_atoms > stocklana_adapters::MAX_INPUT
        || proposals.is_empty() || proposals.len() > 4 {
        return Err("sell path bounds".into());
    }
    let product = proposals[0].product_id.as_deref().filter(|id| !id.is_empty())
        .ok_or("sell path product identity")?;
    let mut path = vec![product_mint];
    let mut current = product_mint;
    let mut target = None;
    let mut required = input_atoms;
    let mut spent = 0u64;
    let mut produced = 0u64;
    let mut pools = BTreeSet::new();
    for proposal in proposals {
        let source = parse_key(&proposal.market.input_mint)?;
        let destination = parse_key(&proposal.market.output_mint)?;
        if source != current {
            let next = target.ok_or("sell path missing producer")?;
            if source != next || spent != required || produced == 0
                || next == output_mint || path.contains(&next) {
                return Err("sell path continuity or intermediate conservation".into());
            }
            path.push(next);
            if path.len() >= MAX_SELL_ASSETS { return Err("sell path depth".into()); }
            current = next;
            required = produced;
            target = None;
            spent = 0;
            produced = 0;
        }
        if proposal.stage != 1 || proposal.product_id.as_deref() != Some(product)
            || destination == Pubkey::default() || path.contains(&destination)
            || !pools.insert(parse_key(&proposal.market.pool)?)
            || target.is_some_and(|next| next != destination)
            || proposal.input_atoms == 0 || proposal.expected_output_atoms == 0 {
            return Err("sell path topology or product identity".into());
        }
        target = Some(destination);
        spent = spent.checked_add(proposal.input_atoms).ok_or("sell path input overflow")?;
        produced = produced.checked_add(proposal.expected_output_atoms).ok_or("sell path output overflow")?;
        if spent > required || produced > stocklana_adapters::MAX_INPUT {
            return Err("sell path allocation bound".into());
        }
    }
    if target != Some(output_mint) || spent != required || produced == 0 {
        return Err("sell path terminal conservation".into());
    }
    path.push(output_mint);
    Ok(path)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_sell_execution_bank(
    settlement_program: Pubkey,
    lookup_table: Pubkey,
    owner: Pubkey,
    policy: Pubkey,
    product_mint: Pubkey,
    output_mint: Pubkey,
    input_atoms: u64,
    proposals: &[NativeSwapProposal],
    discovery: &Snapshot,
    payout: Option<&crate::native_payout::NativePayout>,
) -> Result<DirectExecutionBank> {
    let system = Pubkey::default();
    let ata = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
    if [
        settlement_program,
        lookup_table,
        owner,
        policy,
        product_mint,
        output_mint,
    ]
    .contains(&system)
        || product_mint == output_mint
        || proposals.is_empty()
        || proposals.len() > 4
    {
        return Err("sell execution bank bounds".into());
    }
    let mut keys = BTreeSet::from([settlement_program, lookup_table, owner, policy, system, ata]);
    let mut optional = BTreeSet::new();
    let mint_order = sell_asset_path(product_mint, output_mint, input_atoms, proposals)?;
    for proposal in proposals {
        keys.extend(native_wire::execution_dependencies(
            &proposal.market,
            discovery,
        )?);
    }
    let mut assets = mint_order
        .iter()
        .map(|mint| native_wire::wallet_asset(owner, *mint, discovery))
        .collect::<Result<Vec<_>>>()?;
    if let Some(payout)=payout {
        if output_mint!=WSOL || payout.owner()?!=owner {return Err("sell native payout binding".into());}
        *assets.last_mut().ok_or("sell payout asset")?=payout.asset()?;
    }
    for asset in &assets {
        keys.extend([asset.token, asset.mint, asset.token_program]);
        optional.insert(asset.token);
    }
    let nonce =
        Pubkey::find_program_address(&[b"stocklana", owner.as_ref()], &settlement_program).0;
    keys.insert(nonce);
    optional.insert(nonce);
    if keys.len() > 100 || optional.len() > MAX_SELL_ASSETS + 1 {
        return Err("sell execution bank account bound".into());
    }
    Ok(DirectExecutionBank {
        keys: keys.into_iter().map(|key| key.to_string()).collect(),
        optional_wallet_accounts: optional.into_iter().map(|key| key.to_string()).collect(),
        assets,
        nonce,
    })
}

fn token_amount(snapshot: &Snapshot, account: &str, mint: &str, owner: &str) -> Result<u64> {
    let row = snapshot
        .accounts
        .iter()
        .find(|row| row.key == account)
        .ok_or("sell simulation token account")?;
    if row.owner == Pubkey::default().to_string() && row.data.is_empty() {
        return Ok(0);
    }
    if row.data.len() < 165
        || row.data[..32] != parse_key(mint)?.to_bytes()
        || row.data[32..64] != parse_key(owner)?.to_bytes()
        || row.data[108] != 1
    {
        return Err("sell simulation token identity".into());
    }
    Ok(u64::from_le_bytes(
        row.data[64..72]
            .try_into()
            .map_err(|_| "sell simulation token amount")?,
    ))
}

fn simulate_sell_candidate(
    feed: &Arc<crate::feed::Feed>,
    snapshot: &Arc<Snapshot>,
    rpc: &Rpc,
    message: &[u8],
    expected: Expected,
    last_valid_block_height: u64,
) -> Result<PreparedSellCandidate> {
    feed.validate_fence(snapshot)?;
    let decoded = crate::pipeline::decode(message)?;
    if decoded.header().num_required_signatures != 1 || message.len() + 65 > 1_232 {
        return Err("sell simulation message bounds".into());
    }
    let keys = crate::pipeline::resolved(&decoded, snapshot)?;
    let resources = keys
        .iter()
        .enumerate()
        .filter(|(index, _)| decoded.is_maybe_writable(*index, None))
        .map(|(_, key)| parse_key(key).map(|key| key.to_bytes()))
        .collect::<Result<Vec<_>>>()?;
    let before_input = token_amount(
        snapshot,
        &expected.input_account,
        &expected.input_mint,
        &expected.owner,
    )?;
    let before_output = if expected.native_output.is_none() {
        token_amount(snapshot,&expected.output_account,&expected.output_mint,&expected.owner)?
    } else {0};
    let mut addresses=vec![expected.input_account.clone(),expected.output_account.clone()];
    if let Some(native)=&expected.native_output {
        native.validate_wire(&decoded,&keys)?;
        addresses.push(expected.owner.clone());addresses.extend(native.created_accounts.clone());
        if addresses.iter().collect::<BTreeSet<_>>().len()!=addresses.len() || addresses.len()>7 {
            return Err("sell native simulation accounts".into());
        }
    }
    let mut wire = vec![1];
    wire.extend([0; 64]);
    wire.extend_from_slice(message);
    rpc.check_genesis()?;
    let result = rpc.call(
        "simulateTransaction",
        json!([STANDARD.encode(&wire),{"encoding":"base64","sigVerify":false,
            "replaceRecentBlockhash":false,"commitment":"confirmed","minContextSlot":snapshot.slot,
            "accounts":{"encoding":"base64","addresses":addresses}}]),
    )?;
    if result["context"]["slot"].as_u64() != Some(snapshot.slot)
        || !result["value"].get("err").is_some_and(Value::is_null)
    {
        return Err("sell simulation failed or bank changed; rebuild".into());
    }
    let returned = result["value"]["accounts"]
        .as_array()
        .filter(|rows| rows.len() == addresses.len())
        .ok_or("sell simulation balances")?;
    let decode_amount = |row: &serde_json::Value, mint: &str| -> Result<u64> {
        let data = STANDARD
            .decode(row["data"][0].as_str().ok_or("sell simulation account")?)
            .map_err(|error| error.to_string())?;
        if data.len() < 165
            || data[..32] != parse_key(mint)?.to_bytes()
            || data[32..64] != parse_key(&expected.owner)?.to_bytes()
        {
            return Err("sell simulation returned token identity".into());
        }
        Ok(u64::from_le_bytes(
            data[64..72]
                .try_into()
                .map_err(|_| "sell simulation returned amount")?,
        ))
    };
    let after_input = decode_amount(&returned[0], &expected.input_mint)?;
    let output = if let Some(native)=&expected.native_output {
        let accounts=returned.iter().zip(&addresses).map(|(value,address)| {
            if value.is_null() && address==&expected.output_account {
                return Ok(crate::feed::Account{key:address.clone(),owner:Pubkey::default().to_string(),lamports:0,data:vec![],executable:false});
            }
            if value["data"][1].as_str()!=Some("base64") {return Err("sell native returned encoding".to_string());}
            Ok(crate::feed::Account{key:address.clone(),owner:value["owner"].as_str().ok_or("sell native returned owner")?.into(),
                lamports:value["lamports"].as_u64().ok_or("sell native returned lamports")?,
                executable:value["executable"].as_bool().ok_or("sell native returned executable")?,
                data:STANDARD.decode(value["data"][0].as_str().ok_or("sell native returned data")?).map_err(|_|"sell native returned base64")?})
        }).collect::<Result<Vec<_>>>()?;
        native.verify_simulation(snapshot,&accounts,crate::exposure_pipeline::simulation_fee(&result)?)?
    } else {
        decode_amount(&returned[1],&expected.output_mint)?.checked_sub(before_output).ok_or("sell simulation output direction")?
    };
    let cu = result["value"]["unitsConsumed"]
        .as_u64()
        .ok_or("sell simulation CU")?;
    if before_input.checked_sub(after_input) != Some(expected.input)
        || output < expected.minimum_output
        || output > expected.quoted_output
        || cu == 0
        || cu > expected.maximum_cu
    {
        return Err("sell simulation postcondition".into());
    }
    feed.validate_fence(snapshot)?;
    Ok(PreparedSellCandidate {
        feed: feed.clone(),
        snapshot: snapshot.clone(),
        expected: Expected {
            quoted_output: output,
            ..expected
        },
        message_hash: Sha256::digest(message).into(),
        unsigned_wire: wire,
        simulated_output: output,
        simulated_cu: cu,
        last_valid_block_height,
        resources,
    })
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DeploymentProductFile {
    product_id: String,
    input_mint: String,
    policy: String,
    policy_version: u64,
}

#[derive(Clone)]
pub struct DeploymentProduct {
    pub input_mint: Pubkey,
    pub policy: Pubkey,
    pub policy_version: u64,
}

#[derive(Clone)]
pub struct PrepareProduct {
    pub product_id: String,
    pub identity: ProductIdentity,
    pub model: u8,
    pub numerator: u64,
    pub denominator: u64,
    pub conservative_bps: u16,
    pub minimum_output_atoms: u64,
}

pub struct PrepareIntent {
    pub quote_id: String,
    pub owner: Pubkey,
    pub instrument: String,
    pub input_mint: Pubkey,
    pub input_atoms: u64,
    pub minimum_exposure_q32: u64,
    pub maximum_slippage_bps: u16,
    pub admitted_product_ids: Vec<[u8; 32]>,
    pub product_policy_hash: [u8; 32],
    pub world_generation_hash: [u8; 32],
    pub maximum_cu: u64,
    pub economic_reflow: bool,
}

pub struct SellPrepareIntent {
    pub quote_id: String,
    pub owner: Pubkey,
    pub instrument: String,
    pub product_id: String,
    pub product_mint: Pubkey,
    pub output_mint: Pubkey,
    pub input_atoms: u64,
    pub minimum_output_atoms: u64,
    pub quoted_output_atoms: u64,
    pub world_generation_hash: [u8; 32],
    pub maximum_cu: u64,
}

#[derive(Clone)]
pub struct PreparedSellCandidate {
    pub feed: Arc<crate::feed::Feed>,
    pub snapshot: Arc<Snapshot>,
    pub expected: Expected,
    pub message_hash: [u8; 32],
    pub unsigned_wire: Vec<u8>,
    pub simulated_output: u64,
    pub simulated_cu: u64,
    pub last_valid_block_height: u64,
    pub resources: Vec<[u8; 32]>,
}

pub struct PrepareRuntime {
    rpc: Rpc,
    settlement_program: Pubkey,
    policy_authority: Pubkey,
    lookup_tables: LookupRegistry,
    executor: SigningKey,
    deadline_slots: u64,
    maximum_policy_age: u64,
    compute_unit_limit: u32,
    maximum_heap_frame_bytes: u32,
    compute_unit_price: u64,
    allow_underlying_closed: bool,
    products: BTreeMap<(String, Pubkey), DeploymentProduct>,
    bid_sequence: AtomicU64,
}

impl PrepareRuntime {
    pub fn load(rpc_url: String, manifest_path: &Path, executor_seed_path: &Path) -> Result<Self> {
        let bytes = fs::read(manifest_path).map_err(|error| error.to_string())?;
        if bytes.len() > 256 * 1024 {
            return Err("prepare deployment manifest size".into());
        }
        let manifest: DeploymentFile =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        if manifest.schema != "skew.stockmesh.prepare-deployment/v2"
            || manifest.network != "mainnet-beta"
            || !(1..=150).contains(&manifest.deadline_slots)
            || !(1..=150).contains(&manifest.maximum_policy_age_slots)
            || manifest.compute_unit_limit == 0
            || manifest.compute_unit_limit > 1_400_000
            || !(DEFAULT_HEAP_FRAME_BYTES..=MAXIMUM_HEAP_FRAME_BYTES)
                .contains(&manifest.maximum_heap_frame_bytes)
            || !manifest.maximum_heap_frame_bytes.is_multiple_of(1024)
            || manifest.compute_unit_price_micro_lamports > 10_000_000
            || manifest.products.is_empty()
            || manifest.products.len() > 256
        {
            return Err("prepare deployment manifest bounds".into());
        }
        let settlement_program = parse_key(&manifest.settlement_program)?;
        let policy_authority = parse_key(&manifest.policy_authority)?;
        let lookup_table = parse_key(&manifest.lookup_table)?;
        let executor = load_executor_seed(executor_seed_path)?;
        let lookup_tables = LookupRegistry::parse(&manifest.lookup_table, &manifest.instrument_lookup_tables,
            &[settlement_program, policy_authority, parse_key(&manifest.executor)?])?;
        if settlement_program == Pubkey::default()
            || policy_authority == Pubkey::default()
            || lookup_table == Pubkey::default()
            || settlement_program == policy_authority
            || settlement_program == lookup_table
            || policy_authority == lookup_table
            || manifest.executor != bs58::encode(executor.verifying_key().to_bytes()).into_string()
        {
            return Err("prepare deployment identity".into());
        }
        let mut products = BTreeMap::new();
        let mut policies = BTreeSet::new();
        for product in manifest.products {
            let input_mint = parse_key(&product.input_mint)?;
            let policy = parse_key(&product.policy)?;
            if product.product_id.len() != 39
                || !product.product_id.starts_with("stkprd_")
                || !product.product_id[7..]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                || product.policy_version == 0
                || input_mint == Pubkey::default()
                || policy == Pubkey::default()
                || !policies.insert(policy)
                || products
                    .insert(
                        (product.product_id, input_mint),
                        DeploymentProduct {
                            input_mint,
                            policy,
                            policy_version: product.policy_version,
                        },
                    )
                    .is_some()
            {
                return Err("prepare deployment product".into());
            }
        }
        let rpc = Rpc::pinned(rpc_url, MAINNET_GENESIS.into())?;
        verify_immutable_settlement_program(&rpc, settlement_program)?;
        Ok(Self {
            rpc,
            settlement_program,
            policy_authority,
            lookup_tables,
            executor,
            deadline_slots: manifest.deadline_slots,
            maximum_policy_age: manifest.maximum_policy_age_slots,
            compute_unit_limit: manifest.compute_unit_limit,
            maximum_heap_frame_bytes: manifest.maximum_heap_frame_bytes,
            compute_unit_price: manifest.compute_unit_price_micro_lamports,
            allow_underlying_closed: manifest.allow_underlying_closed,
            products,
            bid_sequence: AtomicU64::new(0),
        })
    }

    pub fn product_ids(&self) -> BTreeSet<String> {
        self.products
            .keys()
            .map(|(product_id, _)| product_id.clone())
            .collect()
    }

    pub fn product_bindings(&self) -> BTreeSet<(String, String)> {
        self.products
            .keys()
            .map(|(product_id, input_mint)| (product_id.clone(), input_mint.to_string()))
            .collect()
    }

    pub fn product(&self, product_id: &str, input_mint: Pubkey) -> Result<DeploymentProduct> {
        self.products
            .get(&(product_id.to_owned(), input_mint))
            .cloned()
            .ok_or_else(|| "product lacks deployed input-mint policy binding".into())
    }

    pub fn prepare_direct(
        &self,
        market_keys: &[String],
        discovery: &Snapshot,
        intent: PrepareIntent,
        proposals: &[NativeSwapProposal],
        products: Vec<PrepareProduct>,
    ) -> Result<Candidate> {
        let lookup_table = self.lookup_tables.for_instrument(&intent.instrument)?;
        let (funded, cash_mint) = execution_shape(intent.input_mint, proposals)?;
        let product_stage = if funded { 2 } else { 1 };
        if intent.quote_id.len() != 37
            || !intent.quote_id.starts_with("stkq_")
            || intent.owner == Pubkey::default()
            || intent.input_atoms == 0
            || intent.minimum_exposure_q32 == 0
            || intent.maximum_slippage_bps > 5_000
            || intent.maximum_cu == 0
            || intent.maximum_cu > 1_400_000
            || intent.world_generation_hash != discovery.hash
            || products.is_empty()
            || products.len() > 4
            || (funded && !intent.economic_reflow)
        {
            return Err("direct prepare bounds".into());
        }
        let input = native_wire::wallet_asset(intent.owner, intent.input_mint, discovery)?;
        let mut direct_products = Vec::with_capacity(products.len());
        for product in &products {
            let mint = parse_key(&product.identity.mint)?;
            // Still validate every final product edge. However opcodes 13/14/18
            // validate each product against the common cash root, including
            // USDC -> SPYx -> SPYon. The SPYx-bound policy remains for SELL.
            validate_product_edges(&product.product_id, mint, product_stage, proposals)?;
            let deployed = self.product(&product.product_id, cash_mint)?;
            let token_program = parse_key(&product.identity.token_program)?;
            let destination = native_wire::wallet_asset(intent.owner, mint, discovery)?;
            if destination.token_program != token_program
                || product.identity.instrument != intent.instrument
                || product.minimum_output_atoms == 0
                || !matches!(product.model, 0 | 1)
                || product.numerator == 0
                || product.denominator == 0
                || !(1..=10_000).contains(&product.conservative_bps)
            {
                return Err("direct prepare product metadata".into());
            }
            direct_products.push(DirectProduct {
                product_id: product.product_id.clone(),
                product: MeshProduct {
                    policy: deployed.policy,
                    claim: None,
                    destination: destination.token,
                    mint,
                    token_program,
                    model: product.model,
                    conservative_bps: product.conservative_bps,
                    policy_version: deployed.policy_version,
                    numerator: product.numerator,
                    denominator: product.denominator,
                },
                minimum_output_atoms: product.minimum_output_atoms,
            });
        }
        let planned = if funded {
            plan_funded_execution_bank(
                self.settlement_program,
                lookup_table,
                intent.owner,
                intent.input_mint,
                cash_mint,
                &direct_products,
                proposals,
                discovery,
            )?
        } else {
            plan_direct_execution_bank(
                self.settlement_program,
                lookup_table,
                intent.owner,
                intent.input_mint,
                &direct_products,
                proposals,
                discovery,
            )?
        };
        let response = self.rpc.call(
            "getMultipleAccounts",
            json!([planned.keys,{"encoding":"base64","commitment":"confirmed","minContextSlot":discovery.slot}]),
        )?;
        let execution_slot = response["context"]["slot"]
            .as_u64()
            .ok_or("execution bank slot missing")?;
        if execution_slot < discovery.slot {
            return Err("execution bank regressed; rebuild".into());
        }
        let feed = Arc::new(planned.feed(Duration::from_secs(20), 4 * 1024 * 1024)?);
        let snapshot = feed.publish(&response)?;
        // The catalog policy hash describes immutable product metadata. Receipt
        // admission must instead bind the exact mutable ProductPolicy account
        // bytes from this coherent execution bank (including authority, input
        // mint, version, updated slot, expiry and flags).
        let admissions = products
            .iter()
            .zip(&direct_products)
            .map(|(product, direct)| {
                let state = account_view(&snapshot, &direct.product.policy)?;
                if state.owner != self.settlement_program
                    || state.executable
                    || state.data.get(8..40) != Some(self.policy_authority.as_ref())
                {
                    return Err("ProductPolicy authority or owner differs from deployment".into());
                }
                Ok(ProductAdmission {
                    identity: product.identity.clone(),
                    policy: direct.product.policy.to_string(),
                    policy_data_hash: Sha256::digest(state.data).into(),
                    claim: None,
                    claim_data_hash: None,
                    policy_version: direct.product.policy_version,
                    model: product.model,
                    numerator: product.numerator,
                    denominator: product.denominator,
                    conservative_bps: product.conservative_bps,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let market = snapshot.project(market_keys)?;
        if market.hash != intent.world_generation_hash {
            return Err("execution bank market projection changed; rebuild".into());
        }
        let table = frozen_lookup_table(&snapshot, lookup_table)?;
        let read = |address: &Pubkey| account_view(&snapshot, address);
        let wrap = (intent.input_mint == WSOL).then_some(intent.input_atoms);
        let (setup, owner_sequence) = WalletSetup::plan(intent.owner, &planned.assets, wrap, read)?
            .with_observed_nonce(self.settlement_program, read)?;
        let deadline_slot = snapshot
            .slot
            .checked_add(self.deadline_slots)
            .ok_or("direct prepare deadline overflow")?;
        let frozen = FrozenIntent {
            intent_id: quote_intent_id(&intent.quote_id, intent.owner, owner_sequence),
            owner: intent.owner.to_string(),
            owner_nonce: owner_sequence,
            instrument: intent.instrument,
            input_mint: intent.input_mint.to_string(),
            input_atoms: intent.input_atoms,
            minimum_exposure_q32: u128::from(intent.minimum_exposure_q32),
            admitted_product_ids: intent.admitted_product_ids,
            product_policy_hash: intent.product_policy_hash,
            world_generation_hash: intent.world_generation_hash,
            deadline_slot,
        };
        frozen.commitment(snapshot.slot)?;
        let (blockhash, last_valid_block_height) = self.rpc.latest_blockhash_at(snapshot.slot)?;
        let compute_limit = self
            .compute_unit_limit
            .min(u32::try_from(intent.maximum_cu).map_err(|_| "direct prepare CU range")?);
        let setup_message =
            setup.compile_setup_only(std::slice::from_ref(&table), blockhash, compute_limit)?;
        let lowering = if let Some(message) = setup_message.as_deref() {
            simulate_wallet_setup(&feed, &snapshot, &self.rpc, &setup, message)?
        } else {
            (*snapshot).clone()
        };
        let settlement = if funded {
            let expected_cash = proposals
                .iter()
                .filter(|proposal| proposal.stage == 1)
                .try_fold(0u64, |total, proposal| {
                    total
                        .checked_add(proposal.expected_output_atoms)
                        .ok_or("funded prepare cash overflow")
                })?;
            let minimum_cash = u64::try_from(
                u128::from(expected_cash)
                    .checked_mul(u128::from(10_000 - intent.maximum_slippage_bps))
                    .ok_or("funded prepare cash floor")?
                    / 10_000,
            )
            .map_err(|_| "funded prepare cash range")?
            .max(1);
            let cash = native_wire::wallet_asset(intent.owner, cash_mint, &lowering)?;
            compile_funded_exposure_reflow(
                self.settlement_program,
                FundedExposureSpec {
                    buyer: intent.owner,
                    buyer_nonce: planned.nonce,
                    input,
                    cash,
                    buyer_sequence: owner_sequence,
                    input_atoms: intent.input_atoms,
                    minimum_cash_atoms: minimum_cash,
                    minimum_exposure_q32: intent.minimum_exposure_q32,
                    reflow_oracle_calls: 16,
                    deadline_slot,
                    maximum_policy_age: self.maximum_policy_age,
                    allow_underlying_closed: self.allow_underlying_closed,
                    products: direct_products,
                },
                proposals,
                &lowering,
            )?
        } else if intent.economic_reflow {
            compile_direct_exposure_reflow(
                self.settlement_program,
                DirectExposureSpec {
                    buyer: intent.owner,
                    buyer_nonce: planned.nonce,
                    input,
                    buyer_sequence: owner_sequence,
                    input_atoms: intent.input_atoms,
                    minimum_exposure_q32: intent.minimum_exposure_q32,
                    deadline_slot,
                    maximum_policy_age: self.maximum_policy_age,
                    allow_underlying_closed: self.allow_underlying_closed,
                    products: direct_products,
                },
                proposals,
                &lowering,
            )?
        } else {
            compile_direct_exposure(
                self.settlement_program,
                DirectExposureSpec {
                    buyer: intent.owner,
                    buyer_nonce: planned.nonce,
                    input,
                    buyer_sequence: owner_sequence,
                    input_atoms: intent.input_atoms,
                    minimum_exposure_q32: intent.minimum_exposure_q32,
                    deadline_slot,
                    maximum_policy_age: self.maximum_policy_age,
                    allow_underlying_closed: self.allow_underlying_closed,
                    products: direct_products,
                },
                proposals,
                &lowering,
            )?
        };
        let mut instructions = vec![compute_limit_instruction(compute_limit)];
        // Economic Reflow compiles several concentrated-liquidity curves in one
        // invocation. Bind its heap frame into the exact wallet message instead
        // of relying on the 32-KiB runtime default or an operator-side setting.
        // Exact simulation below remains the final CU and heap authority.
        if intent.economic_reflow && self.maximum_heap_frame_bytes > DEFAULT_HEAP_FRAME_BYTES {
            instructions.push(heap_frame_instruction(self.maximum_heap_frame_bytes));
        }
        if self.compute_unit_price > 0 {
            instructions.push(compute_price_instruction(self.compute_unit_price));
        }
        instructions.extend_from_slice(setup.instructions());
        instructions.push(settlement);
        if instructions.len() > crate::wallet_wire::MAX_STOCK_TRANSACTION_INSTRUCTIONS {
            return Err("direct prepare instruction count".into());
        }
        let message = crate::onebook_wire::compile_unsigned_v0(
            intent.owner,
            &instructions,
            std::slice::from_ref(&table),
            blockhash,
        )?;
        let admission = Admission {
            settlement_program: self.settlement_program.to_string(),
            product_policy_hash: intent.product_policy_hash,
            maximum_cu: u64::from(compute_limit),
            maximum_heap_frame_bytes: self.maximum_heap_frame_bytes,
            maximum_compute_price: self.compute_unit_price,
            allow_underlying_closed: self.allow_underlying_closed,
            products: admissions,
        };
        let prepared = PreparedExposure::simulate_market(
            &feed,
            &snapshot,
            market_keys,
            &self.rpc,
            &frozen,
            &admission,
            &message,
        )?;
        let sequence = self
            .bid_sequence
            .fetch_add(1, Ordering::AcqRel)
            .checked_add(1)
            .ok_or("executor bid sequence exhausted")?;
        let mut bid = ExecutorBid {
            executor: bs58::encode(self.executor.verifying_key().to_bytes()).into_string(),
            sequence,
            intent_commitment: frozen.commitment(snapshot.slot)?,
            world_generation_hash: frozen.world_generation_hash,
            transaction_message_hash: prepared.message_hash(),
            guaranteed_exposure_q32: u128::from(prepared.simulated_exposure()),
            executor_fee_input_atoms: 0,
            predicted_compute_units: u32::try_from(prepared.simulated_cu())
                .map_err(|_| "prepared CU range")?,
            expires_slot: deadline_slot,
            signature: String::new(),
        };
        bid.signature =
            bs58::encode(self.executor.sign(&bid.signing_bytes()?).to_bytes()).into_string();
        Ok(Candidate {
            intent: frozen,
            feed,
            snapshot,
            prepared,
            bid,
            last_valid_block_height,
        })
    }

    /// Prepare one exact issuer product for secondary sale into USDC or wSOL.
    /// The existing cash->product ProductPolicy is checked in reverse by
    /// opcode 20; no issuer API or caller-supplied CPI account enters the wire.
    pub fn prepare_sell(
        &self,
        market_keys: &[String],
        discovery: &Snapshot,
        intent: SellPrepareIntent,
        proposals: &[NativeSwapProposal],
    ) -> Result<PreparedSellCandidate> {
        let lookup_table = self.lookup_tables.for_instrument(&intent.instrument)?;
        if intent.quote_id.len() != 37
            || !intent.quote_id.starts_with("stkq_")
            || intent.owner == Pubkey::default()
            || intent.instrument.is_empty()
            || intent.product_id.is_empty()
            || intent.product_mint == Pubkey::default()
            || intent.output_mint == Pubkey::default()
            || intent.product_mint == intent.output_mint
            || intent.input_atoms == 0
            || intent.minimum_output_atoms == 0
            || intent.quoted_output_atoms < intent.minimum_output_atoms
            || intent.maximum_cu == 0
            || intent.maximum_cu > 1_400_000
            || intent.world_generation_hash != discovery.hash
            || proposals.is_empty()
            || proposals.len() > 4
            || proposals.iter().any(|proposal| proposal.product_id.as_deref() != Some(intent.product_id.as_str()))
        {
            return Err("sell prepare bounds".into());
        }
        let first_cash = proposals
            .first()
            .and_then(|proposal| proposal.market.output_mint.parse::<Pubkey>().ok())
            .ok_or("sell prepare policy cash")?;
        if proposals.first().is_none_or(|proposal| {
            proposal.market.input_mint != intent.product_mint.to_string() || proposal.stage != 1
        }) {
            return Err("sell prepare first product edge".into());
        }
        let deployed = self.product(&intent.product_id, first_cash)?;
        let payout=if intent.output_mint==WSOL {
            let rent=self.rpc.call("getMinimumBalanceForRentExemption",json!([165,{"commitment":"confirmed"}]))?
                .as_u64().ok_or("sell native payout rent quote")?;
            Some(crate::native_payout::NativePayout::new(intent.owner,&intent.quote_id,rent)?)
        } else {None};
        let planned = plan_sell_execution_bank(
            self.settlement_program,
            lookup_table,
            intent.owner,
            deployed.policy,
            intent.product_mint,
            intent.output_mint,
            intent.input_atoms,
            proposals,
            discovery,
            payout.as_ref(),
        )?;
        let response = self.rpc.call(
            "getMultipleAccounts",
            json!([planned.keys,{"encoding":"base64","commitment":"confirmed","minContextSlot":discovery.slot}]),
        )?;
        let execution_slot = response["context"]["slot"]
            .as_u64()
            .ok_or("sell execution bank slot missing")?;
        if execution_slot < discovery.slot {
            return Err("sell execution bank regressed; rebuild".into());
        }
        let feed = Arc::new(planned.feed(Duration::from_secs(20), 4 * 1024 * 1024)?);
        let snapshot = feed.publish(&response)?;
        let market = snapshot.project(market_keys)?;
        if market.hash != intent.world_generation_hash {
            return Err("sell execution bank market projection changed; rebuild".into());
        }
        let table = frozen_lookup_table(&snapshot, lookup_table)?;
        let read = |address: &Pubkey| account_view(&snapshot, address);
        let wallet_assets=planned.assets.iter().filter(|a|payout.as_ref().is_none_or(|_|a.mint!=WSOL)).copied().collect::<Vec<_>>();
        let (mut setup, owner_sequence) = WalletSetup::plan(intent.owner, &wallet_assets, None, read)?
            .with_observed_nonce(self.settlement_program, read)?;
        if let Some(payout)=&payout {setup=setup.with_native_payout(payout.clone(),read)?;}
        let deadline_slot = snapshot
            .slot
            .checked_add(self.deadline_slots)
            .ok_or("sell prepare deadline overflow")?;
        let (blockhash, last_valid_block_height) = self.rpc.latest_blockhash_at(snapshot.slot)?;
        let compute_limit = self
            .compute_unit_limit
            .min(u32::try_from(intent.maximum_cu).map_err(|_| "sell prepare CU range")?);
        let setup_message =
            setup.compile_setup_only(std::slice::from_ref(&table), blockhash, compute_limit)?;
        let lowering = if let Some(message) = setup_message.as_deref() {
            simulate_wallet_setup(&feed, &snapshot, &self.rpc, &setup, message)?
        } else {
            (*snapshot).clone()
        };
        let input = native_wire::wallet_asset(intent.owner, intent.product_mint, &lowering)?;
        let output = if let Some(payout)=&payout {payout.asset()?} else {native_wire::wallet_asset(intent.owner,intent.output_mint,&lowering)?};
        let mut intermediate_mints = Vec::new();
        for proposal in proposals {
            let mint: Pubkey = proposal
                .market
                .output_mint
                .parse()
                .map_err(|_| "sell prepare intermediate mint")?;
            if mint != intent.output_mint && !intermediate_mints.contains(&mint) {
                intermediate_mints.push(mint);
            }
        }
        let intermediates = intermediate_mints
            .iter()
            .map(|mint| native_wire::wallet_asset(intent.owner, *mint, &lowering))
            .collect::<Result<Vec<_>>>()?;
        let graph = native_wire::lower_native_graph(
            crate::swap_wire::SwapGraph {
                owner: intent.owner,
                sequence: owner_sequence,
                input_atoms: intent.input_atoms,
                minimum_output_atoms: intent.minimum_output_atoms,
                deadline_slot,
                input,
                output,
                intermediates,
                legs: Vec::new(),
            },
            proposals,
            &lowering,
        )?;
        let graph =
            crate::swap_wire::compile_swap_graph(self.settlement_program, &graph, |address| {
                account_view(&lowering, address)
            })?;
        let settlement = crate::onebook_wire::compile_reverse_stock_fill(
            self.settlement_program,
            deployed.policy,
            deployed.policy_version,
            self.maximum_policy_age,
            self.allow_underlying_closed,
            graph,
        )?;
        let mut instructions = vec![compute_limit_instruction(compute_limit)];
        if self.compute_unit_price > 0 {
            instructions.push(compute_price_instruction(self.compute_unit_price));
        }
        instructions.extend_from_slice(setup.instructions());
        instructions.push(settlement);
        if let Some(payout)=&payout {instructions.push(payout.close()?);}
        if instructions.len() > crate::wallet_wire::MAX_STOCK_TRANSACTION_INSTRUCTIONS {
            return Err("sell prepare instruction count".into());
        }
        let message = crate::onebook_wire::compile_unsigned_v0(
            intent.owner,
            &instructions,
            std::slice::from_ref(&table),
            blockhash,
        )?;
        let route: [u8; 32] = Sha256::digest(
            serde_json::to_vec(
                &proposals
                    .iter()
                    .map(NativeSwapProposal::json)
                    .collect::<Vec<_>>(),
            )
            .map_err(|error| error.to_string())?,
        )
        .into();
        simulate_sell_candidate(
            &feed,
            &snapshot,
            &self.rpc,
            &message,
            Expected {
                owner: intent.owner.to_string(),
                input_account: input.token.to_string(),
                input_mint: input.mint.to_string(),
                output_account: output.token.to_string(),
                output_mint: output.mint.to_string(),
                input: intent.input_atoms,
                minimum_output: intent.minimum_output_atoms,
                quoted_output: intent.quoted_output_atoms,
                maximum_cu: u64::from(compute_limit),
                route,
                native_output:payout.map(|payout|crate::native_payout::NativeOutput{
                    payout,settlement_program:self.settlement_program.to_string(),
                    created_accounts:setup.created_assets().map(|a|a.token.to_string())
                        .chain(setup.created_nonce().map(|n|n.to_string())).collect(),
                }),
            },
            last_valid_block_height,
        )
    }
}

fn execution_shape(input_mint: Pubkey, proposals: &[NativeSwapProposal]) -> Result<(bool, Pubkey)> {
    if proposals.is_empty() || proposals.len() > stocklana_adapters::graph::MAX_CANDIDATES {
        return Err("direct prepare proposal count".into());
    }
    let product_stage = proposals
        .iter()
        .map(|proposal| proposal.stage)
        .max()
        .ok_or("direct prepare empty stages")?;
    if !matches!(product_stage, 1 | 2) {
        return Err("direct prepare stage".into());
    }
    let funded = product_stage == 2;
    let cash_mint = if funded {
        let funding_outputs = proposals
            .iter()
            .filter(|proposal| proposal.stage == 1)
            .map(|proposal| parse_key(&proposal.market.output_mint))
            .collect::<Result<BTreeSet<_>>>()?;
        if funding_outputs.len() != 1 {
            return Err("direct prepare funding cash mint".into());
        }
        *funding_outputs
            .iter()
            .next()
            .ok_or("direct prepare cash mint")?
    } else {
        input_mint
    };
    let product_mints = proposals
        .iter()
        .filter(|proposal| proposal.stage == product_stage)
        .map(|proposal| parse_key(&proposal.market.output_mint))
        .collect::<Result<BTreeSet<_>>>()?;
    let direct = proposals
        .iter()
        .filter(|proposal| {
            proposal.stage == product_stage && proposal.market.input_mint == cash_mint.to_string()
        })
        .map(|proposal| parse_key(&proposal.market.output_mint))
        .collect::<Result<BTreeSet<_>>>()?;
    let valid = !product_mints.is_empty()
        && !direct.is_empty()
        && proposals.iter().all(|proposal| match proposal.stage {
            1 if funded => {
                proposal.product_id.is_none()
                    && proposal.market.input_mint == input_mint.to_string()
                    && proposal.market.output_mint == cash_mint.to_string()
            }
            stage if stage == product_stage => {
                let source = parse_key(&proposal.market.input_mint);
                let output = parse_key(&proposal.market.output_mint);
                proposal.product_id.is_some()
                    && source
                        .as_ref()
                        .is_ok_and(|source| *source == cash_mint || direct.contains(source))
                    && output
                        .as_ref()
                        .is_ok_and(|output| product_mints.contains(output))
                    && source.ok() != output.ok()
            }
            _ => false,
        });
    if !valid || (funded && cash_mint == input_mint) {
        return Err("direct prepare stage/cash binding".into());
    }
    Ok((funded, cash_mint))
}

fn validate_product_edges(
    product_id: &str,
    output_mint: Pubkey,
    product_stage: usize,
    proposals: &[NativeSwapProposal],
) -> Result<()> {
    let mut matched = 0usize;
    for proposal in proposals
        .iter()
        .filter(|proposal| proposal.product_id.as_deref() == Some(product_id))
    {
        if proposal.stage != product_stage
            || parse_key(&proposal.market.output_mint)? != output_mint
            || parse_key(&proposal.market.input_mint)? == output_mint
        {
            return Err("ProductPolicy final product leg differs from quote product".into());
        }
        matched = matched
            .checked_add(1)
            .ok_or("ProductPolicy final product leg count")?;
    }
    if matched == 0 {
        return Err("ProductPolicy final product edge is missing".into());
    }
    // The enclosing execution_shape validates every source against the cash
    // root and admitted direct products. Multiple paths into the same product
    // do not create multiple economic BUY policy identities.
    Ok(())
}

fn quote_intent_id(quote_id: &str, owner: Pubkey, sequence: u64) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"SKEW_STOCKMESH_QUOTE_INTENT_V1\0");
    hash.update(quote_id.as_bytes());
    hash.update(owner.to_string().as_bytes());
    hash.update(sequence.to_le_bytes());
    hash.finalize().into()
}

fn compute_limit_instruction(limit: u32) -> Instruction {
    let mut data = vec![2];
    data.extend_from_slice(&limit.to_le_bytes());
    Instruction {
        program_id: COMPUTE,
        accounts: Vec::new(),
        data,
    }
}

fn heap_frame_instruction(bytes: u32) -> Instruction {
    let mut data = vec![1];
    data.extend_from_slice(&bytes.to_le_bytes());
    Instruction {
        program_id: COMPUTE,
        accounts: Vec::new(),
        data,
    }
}

fn compute_price_instruction(price: u64) -> Instruction {
    let mut data = vec![3];
    data.extend_from_slice(&price.to_le_bytes());
    Instruction {
        program_id: COMPUTE,
        accounts: Vec::new(),
        data,
    }
}

fn account_view<'a>(snapshot: &'a Snapshot, address: &Pubkey) -> Result<AccountView<'a>> {
    let key = address.to_string();
    let mut rows = snapshot
        .accounts
        .iter()
        .filter(|account| account.key == key);
    let account = rows.next().ok_or("direct prepare account missing")?;
    if rows.next().is_some() {
        return Err("direct prepare ambiguous account".into());
    }
    Ok(AccountView {
        owner: parse_key(&account.owner)?,
        executable: account.executable,
        data: &account.data,
    })
}

/// A prepared wallet message may survive longer than an operator's activation
/// check. Require the deployed program to be immutable when the runtime starts,
/// so code cannot change between exact simulation and owner submission.
fn verify_immutable_settlement_program(rpc: &Rpc, program: Pubkey) -> Result<()> {
    let program_key = program.to_string();
    let probe_response = rpc.call(
        "getMultipleAccounts",
        json!([[program_key],{"encoding":"base64","commitment":"confirmed"}]),
    )?;
    let probe_feed = crate::feed::Feed::new(
        vec![program_key.clone()],
        Duration::from_secs(5),
        4 * 1024 * 1024,
    )?;
    let probe = probe_feed.publish(&probe_response)?;
    let program_view = account_view(&probe, &program)?;
    if program_view.owner != UPGRADEABLE_LOADER
        || !program_view.executable
        || program_view.data.len() != 36
        || program_view.data[..4] != 2u32.to_le_bytes()
    {
        return Err("prepare requires upgradeable-loader Program account".into());
    }
    let program_data = Pubkey::new_from_array(
        program_view.data[4..36]
            .try_into()
            .map_err(|_| "prepare ProgramData address")?,
    );
    let final_keys = vec![program_key, program_data.to_string()];
    let final_response = rpc.call(
        "getMultipleAccounts",
        json!([final_keys,{"encoding":"base64","commitment":"confirmed","minContextSlot":probe.slot}]),
    )?;
    let final_feed = crate::feed::Feed::new(final_keys, Duration::from_secs(5), 4 * 1024 * 1024)?;
    let final_snapshot = final_feed.publish(&final_response)?;
    let final_program = account_view(&final_snapshot, &program)?;
    let final_program_data = account_view(&final_snapshot, &program_data)?;
    if final_program.owner != UPGRADEABLE_LOADER
        || !final_program.executable
        || final_program.data != program_view.data
        || final_program_data.owner != UPGRADEABLE_LOADER
        || final_program_data.executable
        || final_program_data.data.len() < 13
        || final_program_data.data[..4] != 3u32.to_le_bytes()
        || final_program_data.data[12] != 0
    {
        return Err("prepare requires immutable settlement ProgramData".into());
    }
    Ok(())
}

fn parse_key(value: &str) -> Result<Pubkey> {
    Pubkey::from_str(value).map_err(|_| "prepare public key".into())
}

fn load_executor_seed(path: &Path) -> Result<SigningKey> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() > 256
    {
        return Err("executor seed file permissions/type".into());
    }
    let value = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let value = value.trim();
    let seed = if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        let mut seed = [0u8; 32];
        for (index, byte) in seed.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
                .map_err(|_| "executor seed hex")?;
        }
        seed
    } else {
        bs58::decode(value)
            .into_vec()
            .map_err(|_| "executor seed encoding")?
            .try_into()
            .map_err(|_| "executor seed length")?
    };
    if seed == [0; 32] {
        return Err("executor seed empty".into());
    }
    Ok(SigningKey::from_bytes(&seed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde_json::json;
    use std::{
        fs::OpenOptions,
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        os::unix::fs::OpenOptionsExt,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn deployment_files(mode: u32) -> (std::path::PathBuf, std::path::PathBuf, String, Pubkey) {
        let root = std::env::temp_dir().join(format!(
            "stockmesh-prepare-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let signer = SigningKey::from_bytes(&[29; 32]);
        let executor = bs58::encode(signer.verifying_key().to_bytes()).into_string();
        let manifest = root.join("deployment.json");
        let seed = root.join("executor.seed");
        let settlement = Pubkey::new_unique();
        fs::write(
            &manifest,
            serde_json::to_vec(&json!({
                "schema":"skew.stockmesh.prepare-deployment/v2",
                "network":"mainnet-beta",
                "settlementProgram":settlement.to_string(),
                "policyAuthority":Pubkey::new_unique().to_string(),
                "lookupTable":Pubkey::new_unique().to_string(),
                "executor":executor,
                "deadlineSlots":40,
                "maximumPolicyAgeSlots":100,
                "computeUnitLimit":400000,
                "maximumHeapFrameBytes":262144,
                "computeUnitPriceMicroLamports":0,
                "allowUnderlyingClosed":false,
                "products":[
                    {
                        "productId":format!("stkprd_{}", "a".repeat(32)),
                        "inputMint":USDC.to_string(),
                        "policy":Pubkey::new_unique().to_string(),
                        "policyVersion":11
                    },
                    {
                        "productId":format!("stkprd_{}", "a".repeat(32)),
                        "inputMint":WSOL.to_string(),
                        "policy":Pubkey::new_unique().to_string(),
                        "policyVersion":12
                    }
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&seed)
            .unwrap();
        write!(file, "{}", "1d".repeat(32)).unwrap();
        (
            manifest,
            seed,
            root.to_string_lossy().into_owned(),
            settlement,
        )
    }

    fn program_server(
        _settlement: Pubkey,
        immutable: bool,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let program_data = Pubkey::new_unique();
        let mut program_bytes = vec![0u8; 36];
        program_bytes[..4].copy_from_slice(&2u32.to_le_bytes());
        program_bytes[4..].copy_from_slice(program_data.as_ref());
        let mut program_data_bytes = vec![0u8; 13];
        program_data_bytes[..4].copy_from_slice(&3u32.to_le_bytes());
        program_data_bytes[12] = u8::from(!immutable);
        let account = |owner: Pubkey, executable: bool, data: &[u8]| {
            json!({
                "lamports":1,
                "owner":owner.to_string(),
                "executable":executable,
                "rentEpoch":0,
                "space":data.len(),
                "data":[STANDARD.encode(data),"base64"]
            })
        };
        let responses = vec![
            json!(MAINNET_GENESIS),
            json!({"context":{"slot":7},"value":[account(UPGRADEABLE_LOADER,true,&program_bytes)]}),
            json!({"context":{"slot":7},"value":[
                account(UPGRADEABLE_LOADER,true,&program_bytes),
                account(UPGRADEABLE_LOADER,false,&program_data_bytes)
            ]}),
        ];
        let server = std::thread::spawn(move || {
            for (expected, result) in [
                "getGenesisHash",
                "getMultipleAccounts",
                "getMultipleAccounts",
            ]
            .into_iter()
            .zip(responses)
            {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap();
                        }
                    }
                }
                let mut request = vec![0; length];
                reader.read_exact(&mut request).unwrap();
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&request).unwrap()["method"],
                    expected
                );
                let body = serde_json::to_vec(&json!({
                    "jsonrpc":"2.0",
                    "id":1,
                    "result":result
                }))
                .unwrap();
                write!(
                    reader.get_mut(),
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                reader.get_mut().write_all(&body).unwrap();
            }
        });
        (format!("http://{address}"), server)
    }

    #[test]
    fn quote_intent_identity_binds_wallet_and_observed_sequence() {
        let owner = Pubkey::new_unique();
        assert_ne!(
            quote_intent_id(&format!("stkq_{}", "a".repeat(32)), owner, 1),
            quote_intent_id(&format!("stkq_{}", "a".repeat(32)), owner, 2)
        );
        assert_ne!(
            quote_intent_id(&format!("stkq_{}", "a".repeat(32)), owner, 1),
            quote_intent_id(&format!("stkq_{}", "b".repeat(32)), owner, 1)
        );
    }

    #[test]
    fn unsigned_setup_budget_is_exact() {
        assert_eq!(compute_limit_instruction(321_000).data, [2, 232, 229, 4, 0]);
        assert_eq!(heap_frame_instruction(256 * 1024).data, [1, 0, 0, 4, 0]);
        assert_eq!(
            compute_price_instruction(7).data,
            [3, 7, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    fn sale_leg(input: Pubkey, output: Pubkey, spent: u64, received: u64) -> NativeSwapProposal {
        use crate::market::{MarketConfig, Venue};
        NativeSwapProposal {
            market: MarketConfig {
                venue: Venue::MeteoraDlmm, program: Venue::MeteoraDlmm.program().into(),
                pool: Pubkey::new_unique().to_string(), config: String::new(),
                input_mint: input.to_string(), output_mint: output.to_string(),
                tick_arrays: vec![], array_capacity: None, clock: String::new(),
            },
            stage: 1, product_id: Some("issuer-product".into()),
            input_atoms: spent, expected_output_atoms: received,
        }
    }

    #[test]
    fn sell_bank_uses_mint_hops_not_economic_stage_labels() {
        let [a,b,c,d]=std::array::from_fn(|_|Pubkey::new_unique());
        let legs=vec![sale_leg(a,b,100,80),sale_leg(b,c,80,60),sale_leg(c,d,60,50)];
        for (last,expected) in [(1,vec![a,b]),(2,vec![a,b,c]),(3,vec![a,b,c,d])] {
            assert_eq!(sell_asset_path(a,*expected.last().unwrap(),100,&legs[..last]).unwrap(),expected);
        }
    }

    #[test]
    fn sell_bank_keeps_parallel_splits_with_three_hops_and_four_cpis() {
        let [a,b,c,d]=std::array::from_fn(|_|Pubkey::new_unique());
        let legs=vec![sale_leg(a,b,33,20),sale_leg(a,b,67,60),sale_leg(b,c,80,60),sale_leg(c,d,60,50)];
        assert_eq!(sell_asset_path(a,d,100,&legs).unwrap(),[a,b,c,d]);
        let split_middle=vec![sale_leg(a,b,100,80),sale_leg(b,c,50,40),sale_leg(b,c,30,20),sale_leg(c,d,60,50)];
        assert_eq!(sell_asset_path(a,d,100,&split_middle).unwrap(),[a,b,c,d]);
    }

    #[test]
    fn sell_bank_rejects_stage_product_and_one_atom_conservation_changes() {
        let [a,b,c,d]=std::array::from_fn(|_|Pubkey::new_unique());
        let legs=vec![sale_leg(a,b,100,80),sale_leg(b,c,80,60),sale_leg(c,d,60,50)];
        for index in 0..legs.len() {
            let mut changed=legs.clone();changed[index].stage=2;
            assert!(sell_asset_path(a,d,100,&changed).is_err());
            let mut changed=legs.clone();changed[index].product_id=Some("different-product".into());
            assert!(sell_asset_path(a,d,100,&changed).is_err());
            for delta in [-1i64,1] {
                let mut changed=legs.clone();changed[index].input_atoms=(changed[index].input_atoms as i64+delta) as u64;
                assert!(sell_asset_path(a,d,100,&changed).is_err());
            }
        }
    }

    #[test]
    fn sell_bank_rejects_cycles_reused_pools_and_nonterminal_outputs() {
        let [a,b,c,d]=std::array::from_fn(|_|Pubkey::new_unique());
        let legs=vec![sale_leg(a,b,100,80),sale_leg(b,c,80,60),sale_leg(c,d,60,50)];
        let mut repeat=legs.clone();repeat[2].market.pool=repeat[0].market.pool.clone();
        assert!(sell_asset_path(a,d,100,&repeat).is_err());
        let mut cycle=legs.clone();cycle[1].market.output_mint=a.to_string();
        assert!(sell_asset_path(a,d,100,&cycle).is_err());
        assert!(sell_asset_path(a,c,100,&legs).is_err());
        let mut disconnected=legs.clone();disconnected.swap(1,2);
        assert!(sell_asset_path(a,d,100,&disconnected).is_err());
        let branch=vec![sale_leg(a,b,50,40),sale_leg(a,c,50,40),sale_leg(b,d,40,30)];
        assert!(sell_asset_path(a,d,100,&branch).is_err());
    }

    #[test]
    fn sell_bank_depth_and_integer_bounds_do_not_expand_with_wallet_assets() {
        let [a,b,c,d,e]=std::array::from_fn(|_|Pubkey::new_unique());
        let too_deep=vec![sale_leg(a,b,100,80),sale_leg(b,c,80,60),sale_leg(c,d,60,50),sale_leg(d,e,50,40)];
        assert!(sell_asset_path(a,e,100,&too_deep).is_err());
        let zero=vec![sale_leg(a,b,0,80)];
        assert!(sell_asset_path(a,b,100,&zero).is_err());
        let overflow=vec![sale_leg(a,b,50,u64::MAX),sale_leg(a,b,50,1)];
        assert!(sell_asset_path(a,b,100,&overflow).is_err());
    }

    #[test]
    fn execution_cash_follows_the_final_stage_in_both_directions() {
        use crate::market::{MarketConfig, Venue};

        let stock = Pubkey::new_unique();
        let proposal = |stage: usize,
                        product_id: Option<&str>,
                        input: Pubkey,
                        output: Pubkey,
                        pool: Pubkey| NativeSwapProposal {
            market: MarketConfig {
                venue: Venue::RaydiumClmm,
                program: Venue::RaydiumClmm.program().into(),
                pool: pool.to_string(),
                config: String::new(),
                input_mint: input.to_string(),
                output_mint: output.to_string(),
                tick_arrays: vec![],
                array_capacity: None,
                clock: String::new(),
            },
            stage,
            product_id: product_id.map(str::to_owned),
            input_atoms: 10,
            expected_output_atoms: 9,
        };

        let sol_to_usdc = vec![
            proposal(1, None, WSOL, USDC, Pubkey::new_unique()),
            proposal(2, Some("product"), USDC, stock, Pubkey::new_unique()),
        ];
        assert_eq!(execution_shape(WSOL, &sol_to_usdc).unwrap(), (true, USDC));

        let usdc_to_sol = vec![
            proposal(1, None, USDC, WSOL, Pubkey::new_unique()),
            proposal(2, Some("product"), WSOL, stock, Pubkey::new_unique()),
        ];
        assert_eq!(execution_shape(USDC, &usdc_to_sol).unwrap(), (true, WSOL));

        let direct = vec![proposal(
            1,
            Some("product"),
            USDC,
            stock,
            Pubkey::new_unique(),
        )];
        assert_eq!(execution_shape(USDC, &direct).unwrap(), (false, USDC));

        let converted = Pubkey::new_unique();
        let direct_composed = vec![
            proposal(1, Some("stock-a"), USDC, stock, Pubkey::new_unique()),
            proposal(1, Some("stock-b"), stock, converted, Pubkey::new_unique()),
        ];
        assert_eq!(
            execution_shape(USDC, &direct_composed).unwrap(),
            (false, USDC)
        );

        let funded_composed = vec![
            proposal(1, None, WSOL, USDC, Pubkey::new_unique()),
            proposal(2, Some("stock-a"), USDC, stock, Pubkey::new_unique()),
            proposal(2, Some("stock-b"), stock, converted, Pubkey::new_unique()),
        ];
        assert_eq!(
            execution_shape(WSOL, &funded_composed).unwrap(),
            (true, USDC)
        );

        let mut mismatched_bridge = sol_to_usdc.clone();
        mismatched_bridge[0].market.output_mint = WSOL.to_string();
        assert!(execution_shape(WSOL, &mismatched_bridge).is_err());

        let mut mixed_final_cash = sol_to_usdc;
        mixed_final_cash.push(proposal(
            2,
            Some("product"),
            WSOL,
            stock,
            Pubkey::new_unique(),
        ));
        assert!(execution_shape(WSOL, &mixed_final_cash).is_err());
    }

    #[test]
    fn product_edges_preserve_identity_across_direct_and_issuer_paths() {
        use crate::market::{MarketConfig, Venue};

        let spyx = Pubkey::new_unique();
        let spyon = Pubkey::new_unique();
        let proposal =
            |product: &str, input: Pubkey, output: Pubkey, pool: Pubkey| NativeSwapProposal {
                market: MarketConfig {
                    venue: Venue::RaydiumClmm,
                    program: Venue::RaydiumClmm.program().into(),
                    pool: pool.to_string(),
                    config: String::new(),
                    input_mint: input.to_string(),
                    output_mint: output.to_string(),
                    tick_arrays: vec![],
                    array_capacity: None,
                    clock: String::new(),
                },
                stage: 1,
                product_id: Some(product.into()),
                input_atoms: 10,
                expected_output_atoms: 9,
            };
        let proposals = vec![
            proposal("spyx", USDC, spyx, Pubkey::new_unique()),
            proposal("spyon", spyx, spyon, Pubkey::new_unique()),
        ];
        assert!(validate_product_edges("spyx", spyx, 1, &proposals).is_ok());
        assert!(validate_product_edges("spyon", spyon, 1, &proposals).is_ok());

        let mut ambiguous = proposals.clone();
        ambiguous.push(proposal("spyon", USDC, spyon, Pubkey::new_unique()));
        assert!(validate_product_edges("spyon", spyon, 1, &ambiguous).is_ok());
        assert_eq!(execution_shape(USDC,&ambiguous).unwrap(),(false,USDC));
        let mut wrong_source=ambiguous.clone();wrong_source[2].market.input_mint=Pubkey::new_unique().to_string();
        assert!(execution_shape(USDC,&wrong_source).is_err());
        assert!(validate_product_edges("spyon",spyon,2,&ambiguous).is_err());

        let mut substituted = proposals;
        substituted[1].market.output_mint = Pubkey::new_unique().to_string();
        assert!(validate_product_edges("spyon", spyon, 1, &substituted).is_err());
        assert!(validate_product_edges("missing", spyon, 1, &substituted).is_err());
    }

    #[test]
    fn deployment_runtime_loads_explicit_stock_lookup_selection() {
        let (manifest, seed, root, settlement) = deployment_files(0o600);
        let mut value: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        let nvda = Pubkey::new_unique(); let brk = Pubkey::new_unique();
        value["instrumentLookupTables"] = json!({"NVDA":nvda.to_string(),"BRK.B":brk.to_string()});
        fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
        let (rpc, server) = program_server(settlement, true);
        let runtime = PrepareRuntime::load(rpc, &manifest, &seed).unwrap();
        assert_eq!(runtime.lookup_tables.for_instrument("NVDA").unwrap(), nvda);
        assert_eq!(runtime.lookup_tables.for_instrument("BRK.B").unwrap(), brk);
        assert!(runtime.lookup_tables.for_instrument("BRKB").is_err());
        assert!(runtime.lookup_tables.for_instrument("TSLA").is_err());
        server.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deployment_runtime_has_no_api_credential_and_requires_private_bid_key() {
        let (manifest, seed, root, settlement) = deployment_files(0o600);
        let (rpc, server) = program_server(settlement, true);
        let runtime = PrepareRuntime::load(rpc, &manifest, &seed).unwrap();
        let product_id = format!("stkprd_{}", "a".repeat(32));
        assert_eq!(runtime.product_ids(), BTreeSet::from([product_id.clone()]));
        assert_eq!(
            runtime.product_bindings(),
            BTreeSet::from([
                (product_id.clone(), USDC.to_string()),
                (product_id.clone(), WSOL.to_string())
            ])
        );
        let usdc = runtime.product(&product_id, USDC).unwrap();
        let wsol = runtime.product(&product_id, WSOL).unwrap();
        assert_eq!((usdc.input_mint, usdc.policy_version), (USDC, 11));
        assert_eq!((wsol.input_mint, wsol.policy_version), (WSOL, 12));
        assert_ne!(usdc.policy, wsol.policy);
        assert!(runtime.product(&product_id, Pubkey::new_unique()).is_err());
        server.join().unwrap();
        fs::remove_dir_all(root).unwrap();

        let (manifest, seed, root, settlement) = deployment_files(0o600);
        let (rpc, server) = program_server(settlement, false);
        assert!(PrepareRuntime::load(rpc, &manifest, &seed).is_err());
        server.join().unwrap();
        fs::remove_dir_all(root).unwrap();

        let (manifest, seed, root, _) = deployment_files(0o644);
        assert!(PrepareRuntime::load("http://127.0.0.1:1".into(), &manifest, &seed).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
