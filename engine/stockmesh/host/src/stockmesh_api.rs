//! Authenticated StockMesh quote boundary.
//!
//! Each bank is one coherent Solana account read. Multiple homogeneous pair
//! worlds may be compiled from that bank, but a quote is rejected if publication
//! changed or became stale. Quotes are proposals; prepare remains fail closed
//! until a deployed settlement program and exact-simulation compiler are bound.
use crate::{
    catalog::{AssetKind, BasketRequest, Catalog, ExecutionClass, Product, Search, SourceKind},
    direct_prepare::{
        PrepareIntent, PrepareProduct, PrepareRuntime, PreparedSellCandidate, SellPrepareIntent,
    },
    direct_state::{DirectConfig, DirectMode, DirectSnapshot, DirectState},
    feed::{Feed, Snapshot},
    journal::{Journal, Phase},
    market::{USDC_MINT, WSOL_MINT},
    mesh::{FrozenIntent, ProductIdentity},
    native_wire,
    rpc::Rpc,
    sender::Sender,
    slot_signal::SlotSignal,
    world::{World, WorldConfig},
    Result,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use skew_engine::{
    clearing::{fold_lots, residuals_lots, FlowIntent, MAX_INTENTS, MAX_PIVOTS},
    WorkMeter, MAX_ATOMS, MAX_EDGES, MAX_LEGS,
};
use skew_native::{memo::QuoteMemo, ScaledUiAmount};
use solana_pubkey::Pubkey;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
// Catalog growth expands addressable banks, not hot polling concurrency.
// BankActivity continues to select at most seven provider-warmed banks.
const MAX_BANKS: usize = 128;
const MAX_LANES: usize = 256;
// Every market bank is refreshed independently. A provider-side delay or a
// rotated dependency horizon in one product must not revoke unrelated quotes.
// Requests are staggered below, so allowing one group per admitted bank does
// not turn slot notifications into an RPC burst.
const MAX_REFRESH_GROUPS: usize = MAX_BANKS;
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const MAX_QUOTES: usize = 4096;
const USDC_CLEARING_LOT: u64 = 1_000_000;
const MAX_CLEARING_EXPIRY_SLOTS: u64 = 150;
const MAX_CLEARING_LOT_ERROR_BPS: u64 = 5;
const MAX_RESOURCE_ADMISSION_VARIANTS: usize = 8;
const HOT_BANK_TTL_MS: u64 = 60_000;
const PROVIDER_WARM_BANKS: usize = 7;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub network: String,
    pub quote_ttl_ms: u64,
    #[serde(default)]
    pub deployment_ready: bool,
    /// When set, every lane must carry the canonical ProductPolicy document
    /// whose SHA-256 is committed by `product_policy_hash`. Deployment-ready
    /// manifests cannot opt out.
    #[serde(default)]
    pub policy_integrity_required: bool,
    pub banks: Vec<BankManifest>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BankManifest {
    pub name: String,
    pub snapshot: Option<String>,
    pub lanes: Vec<LaneManifest>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneManifest {
    pub instrument: String,
    pub input_symbol: String,
    pub product_id: String,
    pub issuer: String,
    pub output_symbol: String,
    pub output_mint: String,
    pub token_program: String,
    pub output_decimals: u8,
    pub rights_hash: String,
    /// Canonical JSON document committed by `rights_hash` and, for deployed
    /// lanes, by the ProductPolicy v2 PDA itself.
    pub rights_document: Value,
    pub product_policy_hash: String,
    #[serde(default)]
    pub product_policy_document: Value,
    pub backing_model: String,
    pub redemption_model: String,
    pub transfer_model: String,
    pub exposure_model: String,
    pub exposure_numerator: u64,
    pub exposure_denominator: u64,
    pub conservative_bps: u16,
    pub maximum_cu: u64,
    /// One world is a direct input -> stock lane. Two worlds are a coherent
    /// input -> bridge asset -> stock composition. Both stages compile from
    /// one bank, so no route may stitch state from different slots.
    pub worlds: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct LaneKey {
    instrument: String,
    input_symbol: String,
    product_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct IntentKey {
    instrument: String,
    input_symbol: String,
}

#[derive(Clone)]
struct Lane {
    key: LaneKey,
    issuer: String,
    output_symbol: String,
    output_mint: String,
    token_program: String,
    output_decimals: u8,
    rights_hash: String,
    product_policy_hash: String,
    backing_model: String,
    redemption_model: String,
    transfer_model: String,
    exposure_model: String,
    exposure_numerator: u64,
    exposure_denominator: u64,
    conservative_bps: u16,
    maximum_cu: u64,
    configs: Vec<WorldConfig>,
}

struct Publication {
    layout: Arc<BankLayout>,
    snapshot: Arc<Snapshot>,
    worlds: BTreeMap<LaneKey, Vec<Arc<World>>>,
    published_at_ms: u64,
}

struct BankLayout {
    feed: Arc<Feed>,
    lanes: Vec<Lane>,
}

struct Bank {
    name: String,
    max_age: Duration,
    layout: RwLock<Arc<BankLayout>>,
    publication: RwLock<Option<Arc<Publication>>>,
    last_rotation_slot: AtomicU64,
    rotations: AtomicU64,
    rotation_failures: AtomicU64,
}

#[derive(Clone)]
struct CachedQuote {
    intent: IntentKey,
    input: u64,
    minimum_exposure_q32: u64,
    slot: u64,
    snapshot_hash: String,
    product_policy_set_hash: String,
    native_allocation: Vec<crate::world::NativeSwapProposal>,
    /// Funding legs plus the bounded product candidate set carried into opcode
    /// 18 v2. Unlike `native_allocation`, stage-two inputs are seed quotes and
    /// deliberately do not freeze the execution-time split.
    native_candidates: Vec<crate::world::NativeSwapProposal>,
    /// Exact selected product atoms from the continuous exposure solver. These
    /// floors are independent of the larger execution candidate graph: venue
    /// alternatives must never be summed as if every alternative will execute.
    product_output_atoms: BTreeMap<String, u64>,
    /// The execution candidate set is a typed economic graph whose split is
    /// recomputed on chain. This is explicit because a one-stage USDC intent
    /// can still contain a two-CPI issuer-conversion path.
    global_reflow: bool,
    max_slippage_bps: u16,
    maximum_cu: u64,
    expires_at_ms: u64,
    expires: Instant,
}

#[derive(Default)]
struct QuoteCache {
    values: BTreeMap<String, CachedQuote>,
    order: VecDeque<String>,
}

#[derive(Clone)]
struct CachedSellQuote {
    intent: IntentKey,
    instrument: String,
    product_id: String,
    product_mint: String,
    output_symbol: String,
    input: u64,
    quoted_output: u64,
    minimum_output: u64,
    slot: u64,
    snapshot_hash: String,
    product_policy_hash: String,
    native_allocation: Vec<crate::world::NativeSwapProposal>,
    maximum_cu: u64,
    expires_at_ms: u64,
    expires: Instant,
}

#[derive(Default)]
struct SellQuoteCache {
    values: BTreeMap<String, CachedSellQuote>,
    order: VecDeque<String>,
}

struct StoredPreparedSell {
    candidate: PreparedSellCandidate,
    expires: Instant,
    expires_at: String,
}

#[derive(Default)]
struct PreparedSellCache {
    values: BTreeMap<(String, String), StoredPreparedSell>,
}

struct ExposureProduct {
    product_id: String,
    issuer: String,
    output_symbol: String,
    output_mint: String,
    token_program: String,
    output_decimals: u8,
    raw_output_atoms: u64,
    exposure_q32: u64,
    multiplier_q32: u64,
    conservative_bps: u16,
    rights_hash: String,
    product_policy_hash: String,
    backing_model: String,
    redemption_model: String,
    transfer_model: String,
    exposure_model: String,
}

struct ExposurePlan {
    exposure_q32: u64,
    route: Vec<Value>,
    native_allocation: Vec<crate::world::NativeSwapProposal>,
    native_candidates: Vec<crate::world::NativeSwapProposal>,
    products: Vec<ExposureProduct>,
    stage_count: usize,
    global_reflow: bool,
    maximum_cu: u64,
    oracle_calls: u64,
    native_evaluations: u64,
    planning_ns: u64,
    product_policy_set_hash: String,
}

struct SellPlan {
    output_atoms: u64,
    route: Vec<Value>,
    native_allocation: Vec<crate::world::NativeSwapProposal>,
    maximum_cu: u64,
    planning_ns: u64,
    oracle_calls: u64,
    native_evaluations: u64,
}

struct BankActivity {
    last_selected_ms: Vec<AtomicU64>,
}

impl BankActivity {
    fn new(bank_count: usize) -> Result<Self> {
        if bank_count == 0 || bank_count > MAX_BANKS {
            return Err("bank activity bounds".into());
        }
        let activity = Self {
            last_selected_ms: (0..bank_count).map(|_| AtomicU64::new(0)).collect(),
        };
        // The manifest is ordered by product priority. Keep its first bank hot
        // through startup so the initial exchange view never waits for a user
        // request before the first coherent publication.
        activity.mark(0);
        Ok(activity)
    }

    fn mark(&self, bank_index: usize) {
        if let (Some(last), Ok(now)) = (self.last_selected_ms.get(bank_index), now_ms()) {
            last.store(now, Ordering::Release);
        }
    }

    fn active(&self) -> BTreeSet<usize> {
        let now = now_ms().unwrap_or(0);
        self.last_selected_ms
            .iter()
            .enumerate()
            .filter_map(|(index, last)| {
                let selected = last.load(Ordering::Acquire);
                (selected != 0 && now.saturating_sub(selected) <= HOT_BANK_TTL_MS).then_some(index)
            })
            .collect()
    }

    fn bounded_active(&self, cold_cursor: &mut usize) -> BTreeSet<usize> {
        let active = self.active();
        if active.is_empty() { return BTreeSet::new(); }
        let mut recent: Vec<_> = active
            .into_iter()
            .map(|i| (self.last_selected_ms[i].load(Ordering::Acquire), i))
            .collect();
        recent.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let mut selected: BTreeSet<_> = recent
            .into_iter()
            .take(PROVIDER_WARM_BANKS - 1)
            .map(|(_, i)| i)
            .collect();
        // One rotating cold slot is always retained, even when six markets are
        // continuously selected. Never expand upstream polling with catalog size.
        for _ in 0..self.last_selected_ms.len() {
            if selected.len() >= PROVIDER_WARM_BANKS {
                break;
            }
            let index = *cold_cursor % self.last_selected_ms.len();
            *cold_cursor = cold_cursor.wrapping_add(1);
            selected.insert(index);
        }
        selected
    }
}

mod portfolio_discovery;
mod order_page;
mod strategies;

pub struct StockMesh {
    token: Vec<u8>,
    secret: [u8; 32],
    sequence: AtomicU64,
    quote_ttl: Duration,
    captured_fixture: bool,
    deployment_ready: bool,
    policy_integrity_required: bool,
    catalog: RwLock<Catalog>,
    strategies: Mutex<Option<crate::strategy::StrategyStore>>,
    banks: Vec<Arc<Bank>>,
    intent_bank: BTreeMap<IntentKey, usize>,
    bank_activity: Arc<BankActivity>,
    quotes: Mutex<QuoteCache>,
    sell_quotes: Mutex<SellQuoteCache>,
    prepared: Mutex<crate::prepared_quotes::PreparedQuotes>,
    sell_prepared: Mutex<PreparedSellCache>,
    basket_prepared: Mutex<strategies::BasketCache>,
    /// Read-only live RPC used for wallet portfolio discovery. It is never
    /// used by quote publication, preparation, or submission.
    portfolio_rpc: Arc<RwLock<Option<Rpc>>>,
    /// Optional local frozen-bank feed. It can replace only the quote-state
    /// hot path; wallet reads, preparation and submission retain their own
    /// explicitly pinned RPC clients.
    direct_state: Option<Arc<DirectState>>,
    /// Slot/root PubSub only wakes HTTP coherent reads. No WebSocket account
    /// payload is admitted into a quote bank.
    slot_signal: Option<Arc<SlotSignal>>,
    prepare_runtime: Option<Arc<PrepareRuntime>>,
    /// Present only when the operator explicitly arms exact-wire submission.
    /// This sender owns no private key and cannot alter the wallet-signed wire.
    sender: Option<Mutex<Sender>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct QuoteRequest {
    instrument: String,
    side: String,
    notional: String,
    notional_asset: String,
    max_slippage_bps: u16,
    notional_atoms: String,
    #[serde(default)]
    product_mint: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PrepareRequest {
    quote_id: String,
    owner: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SubmitRequest {
    quote_id: String,
    prepared_id: String,
    owner: String,
    signed_transaction_base64: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct OrdersRequest {
    owner: String,
    #[serde(default)]
    prepared_id: Option<String>,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    revision: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ClearingPreviewRequest {
    instrument: String,
    intents: Vec<ClearingIntentRequest>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ClearingIntentRequest {
    owner: String,
    nonce: String,
    sell_asset: String,
    buy_asset: String,
    amount_atoms: String,
    min_out_atoms: String,
    expires_at_slot: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ExposureClearingPreviewRequest {
    instrument: String,
    buyer: ExposureBuyerRequest,
    sellers: Vec<ExposureSellerRequest>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ExposureBuyerRequest {
    owner: String,
    nonce: String,
    input_asset: String,
    input_atoms: String,
    minimum_exposure_q32: String,
    expires_at_slot: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ExposureSellerRequest {
    owner: String,
    nonce: String,
    product_id: String,
    stock_atoms: String,
    cash_out_atoms: String,
    minimum_cash_out_atoms: String,
    expires_at_slot: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct FreezeRequest {
    quote_id: String,
    owner: String,
    owner_nonce: String,
    deadline_slot: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PortfolioRequest {
    owner: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PortfolioProduct {
    instrument: String,
    product_id: String,
    issuer: String,
    symbol: String,
    mint: String,
    token_program: String,
    raw_decimals: u8,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiReply {
    pub status: u16,
    pub body: Value,
}

impl ApiReply {
    fn ok(body: Value) -> Self {
        Self { status: 200, body }
    }

    fn error(status: u16, code: &str, message: &str) -> Self {
        Self {
            status,
            body: json!({"error":{"code":code,"message":message}}),
        }
    }
}

impl Manifest {
    fn validate(&self) -> Result<()> {
        if self.network != "mainnet-beta"
            || !(1_000..=90_000).contains(&self.quote_ttl_ms)
            || (self.deployment_ready && !self.policy_integrity_required)
            || self.banks.is_empty()
            || self.banks.len() > MAX_BANKS
        {
            return Err("StockMesh manifest bounds or unsupported deployment state".into());
        }
        let count: usize = self.banks.iter().map(|bank| bank.lanes.len()).sum();
        if count == 0 || count > MAX_LANES {
            return Err("StockMesh lane count".into());
        }
        Ok(())
    }
}

impl Bank {
    fn publish_value(&self, value: &Value) -> Result<()> {
        let layout = self
            .layout
            .read()
            .map_err(|_| "bank layout poisoned")?
            .clone();
        self.publish_layout(layout, value, false)
    }

    fn publish_layout(&self, layout: Arc<BankLayout>, value: &Value, install: bool) -> Result<()> {
        // Decode and compile beside the active publication. Quotes keep using
        // the prior coherent bank until every dependent World is ready.
        let prior = self
            .publication
            .read()
            .map_err(|_| "bank poisoned")?
            .clone();
        let compiled = (|| {
            let snapshot = layout.feed.prepare(value)?;
            let bank_keys = layout
                .lanes
                .iter()
                .flat_map(|lane| lane.configs.iter())
                .map(WorldConfig::keys)
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect::<BTreeSet<_>>();
            for lane in &layout.lanes {
                for config in &lane.configs {
                    for market in &config.markets {
                        let required = native_wire::execution_dependencies(market, &snapshot)?;
                        if required
                            .iter()
                            .map(ToString::to_string)
                            .any(|key| !bank_keys.contains(&key))
                        {
                            return Err("coherent bank omits native CPI dependency".into());
                        }
                    }
                }
            }
            let mut worlds = BTreeMap::new();
            for lane in &layout.lanes {
                let previous = prior
                    .as_ref()
                    .and_then(|publication| publication.worlds.get(&lane.key));
                let mut compiled = Vec::with_capacity(lane.configs.len());
                for (index, config) in lane.configs.iter().enumerate() {
                    let first = config.markets.first().ok_or("empty lane world")?;
                    compiled.push(Arc::new(
                        config.compile_admitted_pair(
                            &snapshot,
                            previous
                                .and_then(|worlds| worlds.get(index))
                                .map(AsRef::as_ref),
                            &first.input_mint,
                            &first.output_mint,
                        )?,
                    ));
                }
                worlds.insert(lane.key.clone(), compiled);
            }
            Ok(Publication {
                layout: layout.clone(),
                snapshot,
                worlds,
                published_at_ms: now_ms()?,
            })
        })();
        match compiled {
            Ok(next) => {
                // This is the complete bank commit barrier. Feed and derived
                // Worlds change together while readers are excluded only for
                // the pointer swap, never for decoding or compilation.
                let mut publication = self.publication.write().map_err(|_| "bank poisoned")?;
                layout.feed.install_prepared(next.snapshot.clone())?;
                if install {
                    *self.layout.write().map_err(|_| "bank layout poisoned")? = layout;
                }
                *publication = Some(Arc::new(next));
                Ok(())
            }
            // Preserve the old coherent bank only through its original
            // freshness horizon. A malformed replacement never becomes visible.
            Err(error) => Err(error),
        }
    }

    fn invalidate(&self) {
        if let Ok(mut publication) = self.publication.write() {
            if let Some(current) = publication.as_ref() {
                let _ = current.layout.feed.invalidate();
            }
            *publication = None;
        }
    }

    fn current(&self) -> Result<Arc<Publication>> {
        let guard = self.publication.read().map_err(|_| "bank poisoned")?;
        let publication = guard.as_ref().cloned().ok_or("bank unavailable")?;
        let snapshot = publication.layout.feed.read()?;
        if publication.snapshot.revision != snapshot.revision
            || publication.snapshot.slot != snapshot.slot
            || publication.snapshot.hash != snapshot.hash
        {
            return Err("bank publication fence".into());
        }
        Ok(publication)
    }

    fn layout(&self) -> Result<Arc<BankLayout>> {
        self.layout
            .read()
            .map_err(|_| "bank layout poisoned".into())
            .map(|layout| layout.clone())
    }

    fn last_slot(&self) -> u64 {
        self.publication
            .read()
            .ok()
            .and_then(|publication| {
                publication
                    .as_ref()
                    .map(|publication| publication.snapshot.slot)
            })
            .unwrap_or(0)
    }
}

impl StockMesh {
    /// Metadata-only service: loads real catalog admission without RPC,
    /// snapshots, refresh workers, an executor, or a transaction journal.
    pub fn load_publication(manifest_path: &Path, token: String) -> Result<Arc<Self>> {
        let (mesh, _) = Self::load(manifest_path, token, false)?;
        Ok(mesh)
    }

    pub fn load_fixture(manifest_path: &Path, token: String) -> Result<Arc<Self>> {
        let (mesh, manifests) = Self::load(manifest_path, token, true)?;
        if mesh.deployment_ready {
            return Err("fixture cannot enable settlement deployment".into());
        }
        for (bank, source) in mesh.banks.iter().zip(manifests) {
            let snapshot_path = source.snapshot.ok_or("fixture bank snapshot missing")?;
            let value: Value = serde_json::from_slice(&read_relative(
                manifest_path.parent().ok_or("manifest directory")?,
                &snapshot_path,
                8 * 1024 * 1024,
            )?)
            .map_err(|error| error.to_string())?;
            if let Err(error) = bank.publish_value(&value) {
                bank.invalidate();
                return Err(error);
            }
        }
        Ok(mesh)
    }

    pub fn load_live(manifest_path: &Path, token: String, rpc_url: String) -> Result<Arc<Self>> {
        Self::load_live_with_prepare(manifest_path, token, rpc_url, None, None)
    }

    pub fn load_live_with_prepare(
        manifest_path: &Path,
        token: String,
        rpc_url: String,
        deployment_path: Option<&Path>,
        executor_seed_path: Option<&Path>,
    ) -> Result<Arc<Self>> {
        Self::load_live_with_runtime(
            manifest_path,
            token,
            rpc_url,
            deployment_path,
            executor_seed_path,
            None,
        )
    }

    /// Bind live state, wallet-specific preparation and optionally the durable
    /// exact-wire sender. Submission is impossible unless all three deployment
    /// bindings are present and the manifest itself is deployment-ready.
    pub fn load_live_with_runtime(
        manifest_path: &Path,
        token: String,
        rpc_url: String,
        deployment_path: Option<&Path>,
        executor_seed_path: Option<&Path>,
        sender_journal_path: Option<&Path>,
    ) -> Result<Arc<Self>> {
        Self::load_live_with_runtime_and_direct(
            manifest_path,
            token,
            rpc_url,
            deployment_path,
            executor_seed_path,
            sender_journal_path,
            None,
        )
    }

    /// Hosted-provider development has no signer, sender or unmetered WS path.
    /// All quote refresh and portfolio reads share one run budget across clones.
    pub fn load_provider_readonly(
        manifest_path: &Path,
        token: String,
        rpc: Rpc,
    ) -> Result<Arc<Self>> {
        if rpc.genesis() != MAINNET_GENESIS || rpc.provider_metrics().is_none() {
            return Err("provider read-only loader requires budgeted mainnet reader".into());
        }
        let (mesh, _) = Self::load(manifest_path, token, false)?;
        if mesh.deployment_ready {
            return Err("provider read-only loader forbids deployment-ready manifest".into());
        }
        *mesh
            .portfolio_rpc
            .write()
            .map_err(|_| "portfolio reader ownership")? = Some(rpc.relaxed_read_clone()?);
        let banks = mesh.banks.clone();
        let portfolio_rpc = Arc::clone(&mesh.portfolio_rpc);
        let bank_activity = Arc::clone(&mesh.bank_activity);
        std::thread::spawn(move || {
            refresh_superbank(
                banks,
                String::new(),
                portfolio_rpc,
                None,
                None,
                bank_activity,
                Some(rpc),
            )
        });
        Ok(mesh)
    }

    pub fn load_live_with_runtime_and_direct(
        manifest_path: &Path,
        token: String,
        rpc_url: String,
        deployment_path: Option<&Path>,
        executor_seed_path: Option<&Path>,
        sender_journal_path: Option<&Path>,
        direct_config: Option<DirectConfig>,
    ) -> Result<Arc<Self>> {
        let (mut mesh, _) = Self::load(manifest_path, token, false)?;
        match (mesh.deployment_ready, deployment_path, executor_seed_path) {
            (true, Some(deployment), Some(seed)) => {
                let runtime = Arc::new(PrepareRuntime::load(rpc_url.clone(), deployment, seed)?);
                let mut expected = BTreeSet::new();
                for bank in &mesh.banks {
                    let layout = bank.layout()?;
                    let mut groups = BTreeMap::<IntentKey, Vec<&Lane>>::new();
                    for lane in &layout.lanes {
                        groups
                            .entry(IntentKey {
                                instrument: lane.key.instrument.clone(),
                                input_symbol: lane.key.input_symbol.clone(),
                            })
                            .or_default()
                            .push(lane);
                    }
                    for group in groups.values() {
                        let (_,cash)=intent_execution_shape(group)?;
                        for lane in group {
                            expected.insert((lane.key.product_id.clone(),cash.clone()));
                            expected.insert((
                                lane.key.product_id.clone(),
                                lane_policy_input_mint(lane)?,
                            ));
                        }
                    }
                }
                if runtime.product_bindings() != expected {
                    return Err(
                        "prepare deployment product/input-mint bindings differ from quote manifest"
                            .into(),
                    );
                }
                Arc::get_mut(&mut mesh)
                    .ok_or("prepare runtime ownership")?
                    .prepare_runtime = Some(runtime);
            }
            (false, None, None) => {}
            (true, _, _) => {
                return Err("deployment-ready manifest requires prepare bindings".into())
            }
            (false, _, _) => {
                return Err("prepare bindings require deployment-ready manifest".into())
            }
        }
        if let Some(path) = sender_journal_path {
            if !mesh.deployment_ready || mesh.prepare_runtime.is_none() {
                return Err("sender requires deployment-ready prepare bindings".into());
            }
            let journal = Journal::open(path, 4096, 64 * 1024 * 1024)?;
            let rpc = Rpc::pinned(rpc_url.clone(), MAINNET_GENESIS.into())?;
            Arc::get_mut(&mut mesh)
                .ok_or("sender runtime ownership")?
                .sender = Some(Mutex::new(Sender {
                journal,
                rpc,
                max_attempts: 3,
            }));
        }
        if let Some(config) = direct_config {
            let direct_banks = mesh.banks.clone();
            let direct = DirectState::start(config, move || direct_watch_keys(&direct_banks))?;
            Arc::get_mut(&mut mesh)
                .ok_or("direct state runtime ownership")?
                .direct_state = Some(direct);
        }
        let slot_signal = SlotSignal::start(&rpc_url)?;
        Arc::get_mut(&mut mesh)
            .ok_or("slot signal runtime ownership")?
            .slot_signal = Some(slot_signal);
        let banks = mesh.banks.clone();
        let portfolio_rpc = Arc::clone(&mesh.portfolio_rpc);
        let direct_state = mesh.direct_state.clone();
        let slot_signal = mesh.slot_signal.clone();
        let bank_activity = Arc::clone(&mesh.bank_activity);
        std::thread::spawn(move || {
            refresh_superbank(
                banks,
                rpc_url,
                portfolio_rpc,
                direct_state,
                slot_signal,
                bank_activity,
                None,
            )
        });
        Ok(mesh)
    }

    fn load(
        manifest_path: &Path,
        token: String,
        fixture: bool,
    ) -> Result<(Arc<Self>, Vec<BankManifest>)> {
        if token.len() < 32
            || token.len() > 256
            || token.bytes().any(|byte| !byte.is_ascii_graphic())
        {
            return Err("StockMesh token bounds".into());
        }
        let root = manifest_path.parent().ok_or("manifest directory")?;
        let manifest: Manifest =
            serde_json::from_slice(&std::fs::read(manifest_path).map_err(|e| e.to_string())?)
                .map_err(|error| error.to_string())?;
        manifest.validate()?;
        let mut banks = Vec::new();
        let mut intent_bank = BTreeMap::new();
        for (bank_index, bank_manifest) in manifest.banks.iter().enumerate() {
            if bank_manifest.name.is_empty()
                || bank_manifest.name.len() > 64
                || bank_manifest.lanes.is_empty()
                || bank_manifest.lanes.len() > MAX_LANES
                || (fixture && bank_manifest.snapshot.is_none())
                || (!fixture && bank_manifest.snapshot.is_some())
            {
                return Err("invalid bank manifest".into());
            }
            let mut lanes = Vec::new();
            let mut keys = BTreeSet::new();
            let mut lane_keys = BTreeSet::new();
            for lane_manifest in &bank_manifest.lanes {
                if lane_manifest.worlds.is_empty() || lane_manifest.worlds.len() > 3 {
                    return Err("StockMesh lane stage count".into());
                }
                let mut configs = Vec::with_capacity(lane_manifest.worlds.len());
                for world in &lane_manifest.worlds {
                    let config: WorldConfig =
                        serde_json::from_slice(&read_relative(root, world, 1024 * 1024)?)
                            .map_err(|error| error.to_string())?;
                    let execution_bank_keys = config.keys()?;
                    keys.extend(if fixture {
                        execution_bank_keys
                    } else {
                        config.quote_keys()?
                    });
                    configs.push(config);
                }
                validate_lane(lane_manifest, &configs, manifest.policy_integrity_required)?;
                let key = LaneKey {
                    instrument: lane_manifest.instrument.clone(),
                    input_symbol: lane_manifest.input_symbol.clone(),
                    product_id: lane_manifest.product_id.clone(),
                };
                if !lane_keys.insert(key.clone()) {
                    return Err("duplicate StockMesh product lane".into());
                }
                let intent = IntentKey {
                    instrument: key.instrument.clone(),
                    input_symbol: key.input_symbol.clone(),
                };
                if intent_bank
                    .insert(intent, bank_index)
                    .is_some_and(|prior| prior != bank_index)
                {
                    return Err("one economic intent spans incoherent banks".into());
                }
                lanes.push(Lane {
                    key,
                    issuer: lane_manifest.issuer.clone(),
                    output_symbol: lane_manifest.output_symbol.clone(),
                    output_mint: lane_manifest.output_mint.clone(),
                    token_program: lane_manifest.token_program.clone(),
                    output_decimals: lane_manifest.output_decimals,
                    rights_hash: lane_manifest.rights_hash.clone(),
                    product_policy_hash: lane_manifest.product_policy_hash.clone(),
                    backing_model: lane_manifest.backing_model.clone(),
                    redemption_model: lane_manifest.redemption_model.clone(),
                    transfer_model: lane_manifest.transfer_model.clone(),
                    exposure_model: lane_manifest.exposure_model.clone(),
                    exposure_numerator: lane_manifest.exposure_numerator,
                    exposure_denominator: lane_manifest.exposure_denominator,
                    conservative_bps: lane_manifest.conservative_bps,
                    maximum_cu: lane_manifest.maximum_cu,
                    configs,
                });
            }
            validate_intent_groups(&lanes)?;
            if keys.is_empty() || keys.len() > 100 {
                return Err("coherent bank account bound".into());
            }
            let max_age = if fixture {
                Duration::from_secs(3_600)
            } else {
                // Five production banks complete one staggered pass in about
                // two seconds. Preserve one additional pass for a transient
                // public-RPC miss; prepare still rechecks the content hash and
                // runs exact simulation before any unsigned wire is returned.
                Duration::from_secs(5)
            };
            let layout = Arc::new(BankLayout {
                feed: Arc::new(Feed::new(
                    keys.into_iter().collect(),
                    max_age,
                    4 * 1024 * 1024,
                )?),
                lanes,
            });
            banks.push(Arc::new(Bank {
                name: bank_manifest.name.clone(),
                max_age,
                layout: RwLock::new(layout),
                publication: RwLock::new(None),
                last_rotation_slot: AtomicU64::new(0),
                rotations: AtomicU64::new(0),
                rotation_failures: AtomicU64::new(0),
            }));
        }
        let mut secret = [0u8; 32];
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut secret))
            .map_err(|error| error.to_string())?;
        let sources = manifest.banks.clone();
        // These records derive only from the manifest lanes validated above.
        // Importing a public issuer list never inserts banks, policies or signers.
        let mut products = BTreeMap::new();
        let catalog_observed_at = now_ms()? / 1000;
        for bank in &sources {
            for lane in &bank.lanes {
                let source = lane.product_policy_document["source"]
                    .as_str()
                    .or_else(|| lane.rights_document["legalOverview"].as_str());
                // Legacy fixtures need not contain a public issuer source.
                let Some(source) = source else {
                    continue;
                };
                let product = Product {
                    product_id: lane.product_id.clone(),
                    instrument: lane.instrument.clone(),
                    name: lane.output_symbol.clone(),
                    issuer: lane.issuer.clone(),
                    kind: match lane.instrument.as_str() {
                        "SPY" | "QQQ" => AssetKind::ListedEtf,
                        "SPACEX" | "OPENAI" | "ANTHROPIC" => AssetKind::PreIpo,
                        "NVDA" | "TSLA" | "COIN" | "AAPL" | "MSFT" | "GOOGL" | "AMZN" | "META"
                        | "MSTR" | "NFLX" | "MU" | "HOOD" => AssetKind::Equity,
                        _ => AssetKind::Unclassified,
                    },
                    chain: format!("solana:{MAINNET_GENESIS}"),
                    address: lane.output_mint.clone(),
                    execution_class: ExecutionClass::SolanaNative,
                    source_kind: SourceKind::LocalAdmission,
                    source_url: source.into(),
                    source_sha256: lane.product_policy_hash.clone(),
                    observed_at: catalog_observed_at,
                    issuer_tradable: None,
                    fractional: None,
                    rights_hash: Some(lane.rights_hash.clone()),
                };
                if let Some(prior) = products.insert(product.product_id.clone(), product.clone()) {
                    if prior != product {
                        return Err("catalog admitted product conflict".into());
                    }
                }
            }
        }
        let catalog = Catalog::new(products.into_values().collect())?;
        let bank_activity = Arc::new(BankActivity::new(banks.len())?);
        Ok((
            Arc::new(Self {
                token: token.into_bytes(),
                secret,
                sequence: AtomicU64::new(0),
                quote_ttl: Duration::from_millis(manifest.quote_ttl_ms),
                captured_fixture: fixture,
                deployment_ready: manifest.deployment_ready,
                policy_integrity_required: manifest.policy_integrity_required,
                catalog: RwLock::new(catalog),
                strategies: Mutex::new(None),
                banks,
                intent_bank,
                bank_activity,
                quotes: Mutex::new(QuoteCache::default()),
                sell_quotes: Mutex::new(SellQuoteCache::default()),
                prepared: Mutex::new(crate::prepared_quotes::PreparedQuotes::default()),
                sell_prepared: Mutex::new(PreparedSellCache::default()),
                basket_prepared: Mutex::new(strategies::BasketCache::default()),
                portfolio_rpc: Arc::new(RwLock::new(None)),
                direct_state: None,
                slot_signal: None,
                prepare_runtime: None,
                sender: None,
            }),
            sources,
        ))
    }

    pub fn authorized(&self, provided: &str) -> bool {
        let candidate = provided.strip_prefix("Bearer ").unwrap_or("").as_bytes();
        constant_time_equal(candidate, &self.token)
    }

    pub fn load_discovery_catalog(&self, path: &Path) -> Result<()> {
        let imported = Catalog::read(path)?;
        let mut catalog = self.catalog.write().map_err(|_| "catalog lock")?;
        *catalog = catalog.clone().merge(imported)?;
        Ok(())
    }

    pub fn catalog_search(&self, body: &[u8]) -> ApiReply {
        let search: Search = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(_) => {
                return ApiReply::error(400, "STOCKLANA_CATALOG_REQUEST", "Invalid catalog search.")
            }
        };
        if search.query.len() > 128
            || !(1..=100).contains(&search.limit)
            || search.offset > crate::catalog::MAX_PRODUCTS
        {
            return ApiReply::error(
                400,
                "STOCKLANA_CATALOG_BOUNDS",
                "Catalog search exceeds bounds.",
            );
        }
        let catalog = match self.catalog.read() {
            Ok(value) => value,
            Err(_) => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_CATALOG_UNAVAILABLE",
                    "Catalog unavailable.",
                )
            }
        };
        if search
            .revision
            .as_ref()
            .is_some_and(|revision| *revision != catalog.revision)
        {
            return ApiReply::error(
                409,
                "STOCKLANA_CATALOG_CHANGED",
                "Restart pagination with the current catalog revision.",
            );
        }
        let query = search.query.trim().to_ascii_lowercase();
        let filtered: Vec<_> = catalog
            .products
            .iter()
            .filter(|p| {
                search.kind.as_ref().is_none_or(|kind| *kind == p.kind)
                    && (p.instrument.to_ascii_lowercase().contains(&query)
                        || p.name.to_ascii_lowercase().contains(&query))
            })
            .collect();
        let rows:Vec<_> = filtered.iter().skip(search.offset).take(search.limit).map(|p| {
            let mut admitted = false;
            let mut state_ready = false;
            for bank in &self.banks {
                if let Ok(layout) = bank.layout() {
                    if p.chain == format!("solana:{MAINNET_GENESIS}") && layout.lanes.iter().any(|lane|
                        lane.key.product_id == p.product_id && lane.output_mint == p.address
                            && p.rights_hash.as_deref() == Some(lane.rights_hash.as_str())) {
                        admitted = true;
                        state_ready |= bank.current().is_ok();
                    }
                }
            }
            json!({"product":p,"availability":{
                "state":if admitted { if state_ready {"QUOTE_REQUIRED"} else {"WARMING_OR_STALE"} } else {"DISCOVERY_ONLY"},
                "quoteEndpointAvailable":admitted,"stateReady":state_ready,
                "eligibility":"NOT_CHECKED","tradeAllowed":false,
                "reason":if admitted {"LIVE_QUOTE_AND_USER_ELIGIBILITY_REQUIRED"} else {"EXECUTION_ADAPTER_AND_RIGHTS_NOT_ADMITTED"}
            }})
        }).collect();
        ApiReply::ok(
            json!({"schema":"skew.stock-catalog-search/v1","revision":catalog.revision,
            "totalProducts":catalog.products.len(),"matchedProducts":filtered.len(),"offset":search.offset,
            "nextOffset": if search.offset.saturating_add(rows.len()) < filtered.len() {Some(search.offset+rows.len())} else {None},
            "products":rows,"catalogIsExecutionPermission":false}),
        )
    }

    /// Independent leg quotes are a preview, never an atomic basket promise.
    pub fn etf_preview(&self, body: &[u8]) -> ApiReply {
        let request: BasketRequest = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(_) => {
                return ApiReply::error(400, "STOCKLANA_BASKET_REQUEST", "Invalid basket request.")
            }
        };
        let catalog = match self.catalog.read() {
            Ok(value) => value.clone(),
            Err(_) => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_CATALOG_UNAVAILABLE",
                    "Catalog unavailable.",
                )
            }
        };
        if request.catalog_revision != catalog.revision {
            return ApiReply::error(
                409,
                "STOCKLANA_CATALOG_CHANGED",
                "Refresh the basket catalog revision.",
            );
        }
        let allocations = match request.allocate(&catalog.revision) {
            Ok(value) => value,
            Err(_) => return ApiReply::error(
                400,
                "STOCKLANA_BASKET_BOUNDS",
                "Use 2 to 16 unique stocks, weights summing to 10000 and valid integer USDC atoms.",
            ),
        };
        let mut quoted = 0u64;
        let mut blocked = 0u64;
        let mut legs = Vec::new();
        let mut shared = BTreeSet::new();
        let mut seen_pools = BTreeSet::new();
        for (leg, amount) in request.legs.iter().zip(allocations) {
            let admitted = catalog.products.iter().any(|p| {
                p.instrument == leg.instrument
                    && p.source_kind == SourceKind::LocalAdmission
                    && matches!(p.kind, AssetKind::Equity | AssetKind::ListedEtf)
            });
            if !admitted {
                blocked += amount;
                legs.push(json!({"instrument":leg.instrument,"inputAtoms":amount.to_string(),"status":"BLOCKED",
                    "reason":"LISTED_STOCK_EXECUTION_NOT_ADMITTED","reallocated":false}));
                continue;
            }
            let notional = format!("{}.{:06}", amount / 1_000_000, amount % 1_000_000);
            let quote = self.quote(
                &serde_json::to_vec(&json!({"instrument":leg.instrument,"side":"BUY",
                "notional":notional,"notionalAsset":"USDC","notionalAtoms":amount.to_string(),
                "maxSlippageBps":request.max_slippage_bps}))
                .expect("fixed quote JSON"),
            );
            if quote.status == 200 {
                quoted += amount;
                let mut pools = BTreeSet::new();
                collect_route_pools(&quote.body, &mut pools);
                for pool in pools {
                    if !seen_pools.insert(pool.clone()) {
                        shared.insert(pool);
                    }
                }
            } else {
                blocked += amount;
            }
            legs.push(json!({"instrument":leg.instrument,"inputAtoms":amount.to_string(),
                "status":if quote.status==200 {"QUOTED"} else {"BLOCKED"},"quote":quote.body,"reallocated":false}));
        }
        ApiReply::ok(
            json!({"schema":"skew.basket-preview/v1","catalogRevision":catalog.revision,
            "status":if blocked==0 {"PREVIEW_READY"} else {"PARTIAL_PREVIEW"},"inputAsset":"USDC",
            "inputAtoms":request.input_atoms,"quotedInputAtoms":quoted.to_string(),"blockedInputAtoms":blocked.to_string(),
            "legs":legs,"sharedPools":shared,"atomic":false,"submitAllowed":false,
            "executionBlocker":"JOINT_STATE_SIMULATION_AND_WALLET_AUTHORIZATION_REQUIRED"}),
        )
    }

    pub fn prepare_enabled(&self) -> bool {
        self.deployment_ready && self.prepare_runtime.is_some()
    }

    pub fn submission_enabled(&self) -> bool {
        self.prepare_enabled() && self.sender.is_some()
    }

    fn admits_instrument(&self, instrument: &str) -> bool {
        // Only validated execution manifests populate this map. Discovery
        // records and public token symbols never populate execution authority.
        self.intent_bank
            .keys()
            .any(|intent| intent.instrument == instrument)
    }

    fn select_bank(&self, bank_index: usize) {
        self.bank_activity.mark(bank_index);
        if let Some(signal) = self.slot_signal.as_ref() {
            signal.poke();
        }
    }

    pub fn status(&self) -> ApiReply {
        let mut products = BTreeMap::new();
        let mut ready = 0usize;
        let mut product_lanes_ready = 0usize;
        let mut product_lanes_required = 0usize;
        let mut maximum_slot = 0u64;
        let mut observed = 0u64;
        for bank in &self.banks {
            if let Ok(layout) = bank.layout() {
                product_lanes_required += layout.lanes.len();
                for lane in &layout.lanes {
                    products.insert(lane.output_mint.clone(), json!({
                        "instrument":lane.key.instrument, "mint":lane.output_mint,
                        "decimals":lane.output_decimals, "issuer":lane.issuer,
                        "productId":lane.key.product_id
                    }));
                }
            }
            if let Ok(publication) = bank.current() {
                product_lanes_ready += publication.layout.lanes.len();
                ready += publication
                    .layout
                    .lanes
                    .iter()
                    .map(|lane| IntentKey {
                        instrument: lane.key.instrument.clone(),
                        input_symbol: lane.key.input_symbol.clone(),
                    })
                    .collect::<BTreeSet<_>>()
                    .len();
                maximum_slot = maximum_slot.max(publication.snapshot.slot);
                observed = observed.max(publication.published_at_ms);
            }
        }
        let required = self.intent_bank.len();
        let dependency_rotations = self
            .banks
            .iter()
            .map(|bank| bank.rotations.load(Ordering::Acquire))
            .sum::<u64>();
        let dependency_rotation_failures = self
            .banks
            .iter()
            .map(|bank| bank.rotation_failures.load(Ordering::Acquire))
            .sum::<u64>();
        let (status, reason) = stockmesh_status(
            ready,
            required,
            self.deployment_ready,
            self.captured_fixture,
        );
        let direct = self.direct_state.as_ref().map(|state| state.telemetry());
        let slot_signal = self.slot_signal.as_ref().map(|signal| signal.telemetry());
        let provider = self
            .portfolio_rpc
            .read()
            .ok()
            .and_then(|rpc| rpc.as_ref().and_then(Rpc::provider_metrics));
        let quote_source = if self.captured_fixture {
            "captured_fixture"
        } else if let Some(telemetry) = direct.as_ref() {
            telemetry.active_source
        } else if provider.is_some() {
            "provider_observed"
        } else if slot_signal.as_ref().is_some_and(|telemetry| {
            telemetry.connected && telemetry.age_ms.is_some_and(|age| age <= 2_000)
        }) {
            "rpc_ws_triggered"
        } else {
            "live_rpc_fallback"
        };
        let direct_status = direct.map_or(Value::Null, |telemetry| {
            json!({
                "mode":telemetry.mode,
                "connected":telemetry.connected,
                "promoted":telemetry.promoted,
                "activeSource":telemetry.active_source,
                "latestSlot":telemetry.latest_slot,
                "latestAgeMs":telemetry.latest_age_ms,
                "frames":telemetry.frames,
                "rejected":telemetry.rejected,
                "overloads":telemetry.overloads,
                "gaps":telemetry.gaps,
                "disconnects":telemetry.disconnects,
                "directPublishes":telemetry.direct_publishes,
                "rpcFallbacks":telemetry.rpc_fallbacks,
                "directUnavailable":telemetry.direct_unavailable,
                "shadowMatches":telemetry.shadow_matches,
                "shadowMismatches":telemetry.shadow_mismatches,
                "lastFallback":telemetry.last_fallback,
                "lastUnavailable":telemetry.last_unavailable
            })
        });
        let slot_signal_status = slot_signal.map_or(Value::Null, |telemetry| {
            json!({
                "connected":telemetry.connected,
                "slot":telemetry.slot,
                "parent":telemetry.parent,
                "root":telemetry.root,
                "ageMs":telemetry.age_ms,
                "notifications":telemetry.notifications,
                "lineageGaps":telemetry.lineage_gaps,
                "rejected":telemetry.rejected,
                "reconnects":telemetry.reconnects
            })
        });
        let active_banks = self.bank_activity.active().len();
        ApiReply::ok(json!({
            "status":status,
            "network":"mainnet-beta",
            "mode":if self.submission_enabled() { "SUBMISSION_ARMED" } else if provider.is_some() { "PROVIDER_READ_ONLY" } else { "PREPARE_ONLY" },
            "products":products.into_values().collect::<Vec<_>>(),
            "stateSlot":maximum_slot,
            "observedAt":iso8601(observed),
            "reason":reason,
            "quoteLanesReady":ready,
            "quoteLanesRequired":required,
            "productLanesReady":product_lanes_ready,
            "productLanesRequired":product_lanes_required,
            "dependencyRotations":dependency_rotations,
            "dependencyRotationFailures":dependency_rotation_failures,
            "quoteSource":quote_source,
            "provider":provider,
            "directState":direct_status,
            "slotSignal":slot_signal_status,
            "activeQuoteBanks":active_banks,
            "quoteBanksTotal":self.banks.len(),
            "policyIntegrity":if self.policy_integrity_required { "CANONICAL_DOCUMENT_BOUND" } else { "LEGACY_MANIFEST" },
            "submitAllowed":self.submission_enabled()
        }))
    }

    pub fn portfolio(&self, body: &[u8]) -> ApiReply {
        let request: PortfolioRequest = match serde_json::from_slice(body) {
            Ok(request) => request,
            Err(_) => {
                return ApiReply::error(
                    422,
                    "STOCKLANA_PORTFOLIO_INVALID",
                    "Portfolio request must contain one valid Solana owner.",
                )
            }
        };
        let owner = match decode_key(&request.owner) {
            Ok(owner) if Pubkey::new_from_array(owner).to_string() == request.owner => {
                request.owner
            }
            _ => {
                return ApiReply::error(
                    422,
                    "STOCKLANA_PORTFOLIO_INVALID",
                    "Portfolio owner is not a canonical Solana address.",
                )
            }
        };
        let rpc = match self
            .portfolio_rpc
            .read()
            .ok()
            .and_then(|rpc| rpc.as_ref().cloned())
        {
            Some(rpc) => rpc,
            None => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_PORTFOLIO_UNAVAILABLE",
                    "Live wallet balances are unavailable in fixture mode.",
                )
            }
        };
        let catalog = match self.catalog.read() {
            Ok(catalog) => catalog.clone(),
            Err(_) => return ApiReply::error(503, "STOCKLANA_CATALOG_UNAVAILABLE", "Wallet catalog unavailable."),
        };
        match read_portfolio(&self.banks, &catalog, &rpc, &owner) {
            Ok((state_slot, holdings, cash, other_holdings)) => ApiReply::ok(json!({
                "schema":"skew.stockmesh.portfolio/v1",
                "owner":owner,
                "network":"mainnet-beta",
                "stateSlot":state_slot,
                "observedAt":iso8601(now_ms().unwrap_or(0)),
                "cash":cash,
                "coverage":"ADMITTED_STOCK_PRODUCTS_AND_SOL_USDC",
                "discoveryCoverage":"CATALOG_SOLANA_TOKENS",
                "catalogRevision":catalog.revision,
                "otherStockHoldingsTruncated":other_holdings.len() > 512,
                "otherStockHoldings":other_holdings.into_iter().take(512).collect::<Vec<_>>(),
                "costBasis":"UNKNOWN",
                "holdings":holdings.into_iter().map(|(product, atoms)| json!({
                    "instrument":product.instrument,
                    "productId":product.product_id,
                    "issuer":product.issuer,
                    "symbol":product.symbol,
                    "mint":product.mint,
                    "tokenProgram":product.token_program,
                    "rawDecimals":product.raw_decimals,
                    "atoms":atoms.to_string()
                })).collect::<Vec<_>>()
            })),
            Err(error) => {
                eprintln!("StockMesh portfolio read rejected: {error}");
                ApiReply::error(
                    503,
                    "STOCKLANA_PORTFOLIO_UNAVAILABLE",
                    "Current wallet balances could not be read.",
                )
            }
        }
    }

    pub fn quote(&self, body: &[u8]) -> ApiReply {
        let request: QuoteRequest = match serde_json::from_slice(body) {
            Ok(request) => request,
            Err(_) => {
                return ApiReply::error(
                    422,
                    "STOCKLANA_INTENT_INVALID",
                    "Use the exact Stocklana quote schema.",
                )
            }
        };
        if request.side == "SELL" {
            return self.quote_sell(request);
        }
        let atoms = match validate_quote_request(&request) {
            Ok(atoms) => atoms,
            Err(error) => return ApiReply::error(422, "STOCKLANA_INTENT_INVALID", &error),
        };
        let key = IntentKey {
            instrument: request.instrument.clone(),
            input_symbol: request.notional_asset.clone(),
        };
        let Some(&bank_index) = self.intent_bank.get(&key) else {
            return ApiReply::error(
                503,
                "STOCKLANA_LANE_UNAVAILABLE",
                "This stock/input lane has no admitted coherent market.",
            );
        };
        self.select_bank(bank_index);
        let bank = &self.banks[bank_index];
        let publication = match bank.current() {
            Ok(publication) => publication,
            Err(_) => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_STATE_STALE",
                    "The coherent liquidity state is refreshing.",
                )
            }
        };
        let plan = match solve_exposure(&publication, &key, atoms) {
            Ok(plan) => plan,
            Err(error) if error.contains("input bound") => {
                return ApiReply::error(
                    422,
                    "STOCKLANA_CAPSULE_REQUIRED",
                    "This notional must execute as bounded Flow Capsules.",
                )
            }
            Err(error) if error.contains("search budget") => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_SEARCH_BUDGET",
                    "The bounded exposure search did not finish at atomic precision.",
                )
            }
            Err(error) if error.contains("policy") || error.contains("scaled UI") => {
                return ApiReply::error(
                    503,
                    "STOCKMESH_PRODUCT_POLICY",
                    "No issuer product has a current, fully pinned exposure policy.",
                )
            }
            Err(_) => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_RUNTIME_BOUND",
                    "No product and venue allocation satisfies the bounded runtime.",
                )
            }
        };
        let minimum = (u128::from(plan.exposure_q32)
            * u128::from(10_000 - request.max_slippage_bps))
            / 10_000;
        let minimum = u64::try_from(minimum).unwrap_or(0).max(1);
        let expires_at_ms = match now_ms().and_then(|now| {
            now.checked_add(self.quote_ttl.as_millis() as u64)
                .ok_or("clock overflow".into())
        }) {
            Ok(value) => value,
            Err(_) => {
                return ApiReply::error(503, "STOCKLANA_CLOCK", "Quote expiry is unavailable.")
            }
        };
        let quote_id = self.quote_id(
            &key,
            atoms,
            plan.exposure_q32,
            publication.snapshot.hash,
            &plan.product_policy_set_hash,
        );
        let cached = CachedQuote {
            intent: key,
            input: atoms,
            minimum_exposure_q32: minimum,
            slot: publication.snapshot.slot,
            snapshot_hash: hex(&publication.snapshot.hash),
            product_policy_set_hash: plan.product_policy_set_hash.clone(),
            native_allocation: plan.native_allocation.clone(),
            native_candidates: plan.native_candidates.clone(),
            product_output_atoms: plan
                .products
                .iter()
                .filter(|product| product.raw_output_atoms > 0)
                .map(|product| (product.product_id.clone(), product.raw_output_atoms))
                .collect(),
            global_reflow: plan.global_reflow,
            max_slippage_bps: request.max_slippage_bps,
            maximum_cu: plan.maximum_cu,
            expires_at_ms,
            expires: Instant::now() + self.quote_ttl,
        };
        self.cache(quote_id.clone(), cached);
        let leg_count = plan.route.len();
        let selected_stages = plan
            .route
            .iter()
            .filter_map(|leg| leg.get("stage").and_then(Value::as_u64))
            .max()
            .unwrap_or(0);
        let products = plan
            .products
            .iter()
            .map(|product| {
                json!({
                    "productId":product.product_id,
                    "issuer":product.issuer,
                    "symbol":product.output_symbol,
                    "mint":product.output_mint,
                    "tokenProgram":product.token_program,
                    "rawDecimals":product.output_decimals,
                    "rawOutputAtoms":product.raw_output_atoms.to_string(),
                    "exposureQ32":product.exposure_q32.to_string(),
                    "multiplierQ32":product.multiplier_q32.to_string(),
                    "conservativeBps":product.conservative_bps,
                    "rightsHash":product.rights_hash,
                    "productPolicyHash":product.product_policy_hash,
                    "policyStatus":"SOURCE_HASH_PINNED",
                    "backingModel":product.backing_model,
                    "redemptionModel":product.redemption_model,
                    "transferModel":product.transfer_model,
                    "exposureModel":product.exposure_model
                })
            })
            .collect::<Vec<_>>();
        ApiReply::ok(json!({
            "schema":"skew.stockmesh.exposure-quote/v2",
            "quoteId":quote_id,
            "instrument":request.instrument,
            "inputSymbol":request.notional_asset,
            "inAmountAtoms":atoms.to_string(),
            "exposure":{
                "unit":"UNDERLYING_SHARE_Q32",
                "estimatedQ32":plan.exposure_q32.to_string(),
                "minimumQ32":minimum.to_string(),
                "rounding":"FLOOR",
                "productPolicySetHash":plan.product_policy_set_hash,
                "products":products
            },
            "stateSlot":publication.snapshot.slot,
            "expiresAt":iso8601(expires_at_ms),
            "route":plan.route,
            "execution":{"quotedLegs":leg_count,"candidateEdges":plan.native_candidates.len(),"stages":selected_stages,
                "candidatePathDepth":plan.stage_count,
                "maximumReflows":if plan.global_reflow { 4 } else { 0 },"maximumCu":plan.maximum_cu,"exactSimulation":"REQUIRED_BEFORE_PREPARE"},
            "planning":{"oracleCalls":plan.oracle_calls,"nativeEvaluations":plan.native_evaluations,"planningNs":plan.planning_ns,"objective":"MAXIMUM_CONSERVATIVE_EXPOSURE"}
        }))
    }

    fn quote_sell(&self, request: QuoteRequest) -> ApiReply {
        let Some(product_mint) = request.product_mint.as_deref() else {
            return ApiReply::error(
                422,
                "STOCKLANA_INTENT_INVALID",
                "A sell intent must name the exact issuer product mint held by the wallet.",
            );
        };
        if !self.admits_instrument(&request.instrument)
            || !matches!(request.notional_asset.as_str(), "USDC" | "SOL")
            || !(1..=100).contains(&request.max_slippage_bps)
            || decode_key(product_mint).is_err()
        {
            return ApiReply::error(
                422,
                "STOCKLANA_INTENT_INVALID",
                "Choose an admitted issuer product and USDC or SOL output.",
            );
        }
        let key = IntentKey {
            instrument: request.instrument.clone(),
            input_symbol: request.notional_asset.clone(),
        };
        let Some(&bank_index) = self.intent_bank.get(&key) else {
            return ApiReply::error(
                503,
                "STOCKLANA_LANE_UNAVAILABLE",
                "This stock/output lane has no admitted coherent market.",
            );
        };
        self.select_bank(bank_index);
        let publication = match self.banks[bank_index].current() {
            Ok(publication) => publication,
            Err(_) => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_STATE_STALE",
                    "The coherent liquidity state is refreshing.",
                )
            }
        };
        let Some(lane) = publication.layout.lanes.iter().find(|lane| {
            lane.key.instrument == request.instrument
                && lane.key.input_symbol == request.notional_asset
                && lane.output_mint == product_mint
        }) else {
            return ApiReply::error(
                422,
                "STOCKMESH_PRODUCT_NOT_ADMITTED",
                "The wallet product is outside this stock and output policy set.",
            );
        };
        let atoms = match decimal_atoms(&request.notional, u32::from(lane.output_decimals)) {
            Ok(atoms) if request.notional_atoms == atoms.to_string() => atoms,
            _ => {
                return ApiReply::error(
                    422,
                    "STOCKLANA_AMOUNT_INVALID",
                    "Product amount and atomic amount differ.",
                )
            }
        };
        let plan = match solve_sell(&publication, lane, atoms) {
            Ok(plan) => plan,
            Err(error) if error.contains("input bound") => {
                return ApiReply::error(
                    422,
                    "STOCKLANA_CAPSULE_REQUIRED",
                    "This product amount must execute as bounded Flow Capsules.",
                )
            }
            Err(_) => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_RUNTIME_BOUND",
                    "No reverse venue allocation satisfies the bounded runtime.",
                )
            }
        };
        let minimum = u64::try_from(
            u128::from(plan.output_atoms) * u128::from(10_000 - request.max_slippage_bps) / 10_000,
        )
        .unwrap_or(0)
        .max(1);
        let expires_at_ms = match now_ms().and_then(|now| {
            now.checked_add(self.quote_ttl.as_millis() as u64)
                .ok_or("clock overflow".into())
        }) {
            Ok(value) => value,
            Err(_) => {
                return ApiReply::error(503, "STOCKLANA_CLOCK", "Quote expiry is unavailable.")
            }
        };
        let quote_id = self.sell_quote_id(
            &key,
            &lane.key.product_id,
            atoms,
            plan.output_atoms,
            publication.snapshot.hash,
            &lane.product_policy_hash,
        );
        self.cache_sell(
            quote_id.clone(),
            CachedSellQuote {
                intent: key,
                instrument: request.instrument.clone(),
                product_id: lane.key.product_id.clone(),
                product_mint: lane.output_mint.clone(),
                output_symbol: request.notional_asset.clone(),
                input: atoms,
                quoted_output: plan.output_atoms,
                minimum_output: minimum,
                slot: publication.snapshot.slot,
                snapshot_hash: hex(&publication.snapshot.hash),
                product_policy_hash: lane.product_policy_hash.clone(),
                native_allocation: plan.native_allocation.clone(),
                maximum_cu: plan.maximum_cu,
                expires_at_ms,
                expires: Instant::now() + self.quote_ttl,
            },
        );
        let output_decimals = if request.notional_asset == "USDC" {
            6
        } else {
            9
        };
        ApiReply::ok(json!({
            "schema":"skew.stockmesh.liquidation-quote/v1",
            "quoteId":quote_id,
            "side":"SELL",
            "instrument":request.instrument,
            "inputProduct":{
                "productId":lane.key.product_id,
                "issuer":lane.issuer,
                "symbol":lane.output_symbol,
                "mint":lane.output_mint,
                "tokenProgram":lane.token_program,
                "rawDecimals":lane.output_decimals,
                "inputAtoms":atoms.to_string(),
                "rightsHash":lane.rights_hash,
                "productPolicyHash":lane.product_policy_hash,
                "policyStatus":"SOURCE_HASH_PINNED"
            },
            "output":{
                "symbol":request.notional_asset,
                "decimals":output_decimals,
                "estimatedAtoms":plan.output_atoms.to_string(),
                "minimumAtoms":minimum.to_string()
            },
            "stateSlot":publication.snapshot.slot,
            "expiresAt":iso8601(expires_at_ms),
            "route":plan.route,
            "execution":{"quotedLegs":plan.native_allocation.len(),"candidateEdges":plan.native_allocation.len(),
                "stages":lane.configs.len(),"candidatePathDepth":lane.configs.len(),"maximumReflows":0,
                "maximumCu":plan.maximum_cu,"exactSimulation":"REQUIRED_BEFORE_PREPARE"},
            "planning":{"oracleCalls":plan.oracle_calls,"nativeEvaluations":plan.native_evaluations,
                "planningNs":plan.planning_ns,"objective":"MAXIMUM_CASH_OUTPUT"}
        }))
    }

    /// Preview either the product-aware economic-exposure cell (v2) or the
    /// original three-asset fixed-lot Flow Folding cell (v1). The schemas are
    /// structurally disjoint, so callers cannot silently reinterpret raw token
    /// atoms as underlying-share exposure.
    pub fn clearing_preview(&self, body: &[u8]) -> ApiReply {
        let envelope: Value = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(_) => {
                return ApiReply::error(
                    422,
                    "STOCKLANA_CLEARING_INVALID",
                    "Use an exact StockMesh clearing preview schema.",
                )
            }
        };
        if envelope.get("buyer").is_some() || envelope.get("sellers").is_some() {
            self.exposure_clearing_preview(body)
        } else {
            self.flow_clearing_preview(body)
        }
    }

    /// Build the host plan consumed by on-chain opcode 14. Sellers name exact
    /// issuer products and raw atoms; the buyer signs only a cash debit and a
    /// minimum conservative underlying-share Q32 exposure. Internal products
    /// and the externally routed residual are aggregated in that one unit.
    fn exposure_clearing_preview(&self, body: &[u8]) -> ApiReply {
        let request: ExposureClearingPreviewRequest = match serde_json::from_slice(body) {
            Ok(request) => request,
            Err(_) => {
                return ApiReply::error(
                    422,
                    "STOCKMESH_EXPOSURE_CLEARING_INVALID",
                    "Use the exact product-aware StockMesh clearing schema.",
                )
            }
        };
        if !self.admits_instrument(&request.instrument)
            || request.buyer.input_asset != "USDC"
            || request.sellers.is_empty()
            || request.sellers.len() > 3
            || decode_key(&request.buyer.owner).is_err()
        {
            return ApiReply::error(
                422,
                "STOCKMESH_EXPOSURE_CLEARING_INVALID",
                "One admitted stock buyer and one to three exact-product sellers are required.",
            );
        }
        let buyer_input = match parse_u64_string(&request.buyer.input_atoms, false) {
            Ok(value) if value <= MAX_ATOMS => value,
            _ => return ApiReply::error(422, "STOCKMESH_BUYER_BOUND", "Buyer input is invalid."),
        };
        let buyer_nonce = match parse_u64_string(&request.buyer.nonce, false) {
            Ok(value) if value < u64::MAX => value,
            _ => return ApiReply::error(422, "STOCKMESH_BUYER_BOUND", "Buyer nonce is invalid."),
        };
        let buyer_minimum = match parse_u64_string(&request.buyer.minimum_exposure_q32, false) {
            Ok(value) => value,
            _ => {
                return ApiReply::error(
                    422,
                    "STOCKMESH_BUYER_BOUND",
                    "Buyer exposure floor is invalid.",
                )
            }
        };
        let buyer_expiry = match parse_u64_string(&request.buyer.expires_at_slot, false) {
            Ok(value) => value,
            _ => return ApiReply::error(422, "STOCKMESH_BUYER_BOUND", "Buyer expiry is invalid."),
        };
        let key = IntentKey {
            instrument: request.instrument.clone(),
            input_symbol: "USDC".into(),
        };
        let Some(&bank_index) = self.intent_bank.get(&key) else {
            return ApiReply::error(
                503,
                "STOCKLANA_LANE_UNAVAILABLE",
                "No coherent USDC stock market is admitted.",
            );
        };
        let publication = match self.banks[bank_index].current() {
            Ok(publication) => publication,
            Err(_) => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_STATE_STALE",
                    "The coherent liquidity state is refreshing.",
                )
            }
        };
        let maximum_expiry = publication
            .snapshot
            .slot
            .saturating_add(MAX_CLEARING_EXPIRY_SLOTS);
        if buyer_expiry < publication.snapshot.slot || buyer_expiry > maximum_expiry {
            return ApiReply::error(
                422,
                "STOCKMESH_BUYER_BOUND",
                "Buyer expiry is outside the current clearing window.",
            );
        }
        let lanes = publication
            .layout
            .lanes
            .iter()
            .filter(|lane| {
                lane.key.instrument == request.instrument && lane.key.input_symbol == "USDC"
            })
            .collect::<Vec<_>>();
        if lanes.is_empty() {
            return ApiReply::error(
                503,
                "STOCKMESH_PRODUCT_POLICY",
                "No issuer product policy is current.",
            );
        }
        let mut sellers = Vec::with_capacity(request.sellers.len());
        let mut owners = BTreeSet::new();
        owners.insert(request.buyer.owner.clone());
        let mut internal_cash = 0u64;
        let mut internal_exposure = 0u64;
        let mut allocations = BTreeMap::<String, (u64, u64)>::new();
        for source in &request.sellers {
            let Some(lane) = lanes
                .iter()
                .copied()
                .find(|lane| lane.key.product_id == source.product_id)
            else {
                return ApiReply::error(
                    422,
                    "STOCKMESH_PRODUCT_NOT_ADMITTED",
                    "A seller named a product outside the current instrument policy set.",
                );
            };
            let owner_valid =
                decode_key(&source.owner).is_ok() && owners.insert(source.owner.clone());
            let parsed = (|| -> Result<(u64, u64, u64, u64, u64)> {
                if !owner_valid {
                    return Err("seller owner".into());
                }
                let nonce = parse_u64_string(&source.nonce, false)?;
                let stock = parse_u64_string(&source.stock_atoms, false)?;
                let cash = parse_u64_string(&source.cash_out_atoms, false)?;
                let minimum_cash = parse_u64_string(&source.minimum_cash_out_atoms, false)?;
                let expiry = parse_u64_string(&source.expires_at_slot, false)?;
                if nonce == u64::MAX
                    || stock > MAX_ATOMS
                    || cash > buyer_input
                    || minimum_cash > cash
                    || expiry < publication.snapshot.slot
                    || expiry > maximum_expiry
                {
                    return Err("seller bounds".into());
                }
                let scaled = product_scaled_amount(lane, &publication.snapshot)?;
                let exposure = scaled
                    .exposure_q32(
                        stock,
                        lane.exposure_numerator,
                        lane.exposure_denominator,
                        lane.conservative_bps,
                    )
                    .map_err(|error| format!("seller exposure: {error:?}"))?;
                if exposure == 0 {
                    return Err("seller exposure".into());
                }
                Ok((nonce, stock, cash, minimum_cash, exposure))
            })();
            let (nonce, stock, cash, minimum_cash, exposure) = match parsed {
                Ok(value) => value,
                Err(_) => return ApiReply::error(
                    422,
                    "STOCKMESH_SELLER_BOUND",
                    "Every seller must bind one admitted product, exact raw atoms, cash amount, limit, nonce and expiry.",
                ),
            };
            internal_cash = match internal_cash.checked_add(cash) {
                Some(value) if value <= buyer_input => value,
                _ => {
                    return ApiReply::error(
                        422,
                        "STOCKMESH_CASH_CONSERVATION",
                        "Internal seller cash exceeds the buyer debit.",
                    )
                }
            };
            internal_exposure = match internal_exposure.checked_add(exposure) {
                Some(value) => value,
                None => {
                    return ApiReply::error(
                        422,
                        "STOCKMESH_EXPOSURE_OVERFLOW",
                        "Internal exposure exceeds the bounded unit.",
                    )
                }
            };
            let entry = allocations.entry(lane.key.product_id.clone()).or_default();
            entry.0 = match entry.0.checked_add(stock) {
                Some(value) => value,
                None => {
                    return ApiReply::error(
                        422,
                        "STOCKMESH_PRODUCT_OVERFLOW",
                        "Product atoms exceed the bounded unit.",
                    )
                }
            };
            entry.1 = match entry.1.checked_add(exposure) {
                Some(value) => value,
                None => {
                    return ApiReply::error(
                        422,
                        "STOCKMESH_EXPOSURE_OVERFLOW",
                        "Product exposure exceeds the bounded unit.",
                    )
                }
            };
            sellers.push(json!({
                "owner":source.owner,
                "nonce":nonce.to_string(),
                "productId":lane.key.product_id,
                "issuer":lane.issuer,
                "symbol":lane.output_symbol,
                "stockAtoms":stock.to_string(),
                "cashOutAtoms":cash.to_string(),
                "minimumCashOutAtoms":minimum_cash.to_string(),
                "conservativeExposureQ32":exposure.to_string(),
                "expiresAtSlot":source.expires_at_slot
            }));
        }
        let residual_input = buyer_input - internal_cash;
        let residual_plan = if residual_input == 0 {
            None
        } else {
            match solve_exposure(&publication, &key, residual_input) {
                Ok(plan) => Some(plan),
                Err(_) => {
                    return ApiReply::error(
                        503,
                        "STOCKMESH_RESIDUAL_UNAVAILABLE",
                        "The residual cannot be allocated within the current runtime bounds.",
                    )
                }
            }
        };
        if let Some(plan) = &residual_plan {
            for product in &plan.products {
                let entry = allocations.entry(product.product_id.clone()).or_default();
                entry.0 = match entry.0.checked_add(product.raw_output_atoms) {
                    Some(value) => value,
                    None => {
                        return ApiReply::error(
                            503,
                            "STOCKMESH_PRODUCT_OVERFLOW",
                            "Residual product atoms exceed the bounded unit.",
                        )
                    }
                };
                entry.1 = match entry.1.checked_add(product.exposure_q32) {
                    Some(value) => value,
                    None => {
                        return ApiReply::error(
                            503,
                            "STOCKMESH_EXPOSURE_OVERFLOW",
                            "Residual exposure exceeds the bounded unit.",
                        )
                    }
                };
            }
        }
        let residual_exposure = residual_plan.as_ref().map_or(0, |plan| plan.exposure_q32);
        let total_exposure = match internal_exposure.checked_add(residual_exposure) {
            Some(value) => value,
            None => {
                return ApiReply::error(
                    503,
                    "STOCKMESH_EXPOSURE_OVERFLOW",
                    "Aggregate exposure exceeds the bounded unit.",
                )
            }
        };
        if total_exposure < buyer_minimum {
            return ApiReply::error(422, "STOCKMESH_AGGREGATE_MINIMUM", "Internal clearing plus residual execution does not satisfy the buyer exposure floor.");
        }
        let products = lanes
            .iter()
            .filter_map(|lane| {
                allocations
                    .get(&lane.key.product_id)
                    .filter(|(raw, _)| *raw > 0)
                    .map(|(raw, exposure)| {
                        json!({
                            "productId":lane.key.product_id,
                            "issuer":lane.issuer,
                            "symbol":lane.output_symbol,
                            "mint":lane.output_mint,
                            "rawAtoms":raw.to_string(),
                            "conservativeExposureQ32":exposure.to_string(),
                            "rightsHash":lane.rights_hash,
                            "productPolicyHash":lane.product_policy_hash
                        })
                    })
            })
            .collect::<Vec<_>>();
        let policy_set_hash = product_policy_set_hash(&lanes);
        let route = residual_plan
            .as_ref()
            .map_or_else(Vec::new, |plan| plan.route.clone());
        let mut digest = Sha256::new();
        digest.update(b"SKEW_STOCKMESH_EXPOSURE_CLEARING_V2\0");
        digest.update(publication.snapshot.hash);
        digest.update(policy_set_hash.as_bytes());
        digest.update(body);
        let clearing_id = format!("stkm_{}", hex(&digest.finalize()[..16]));
        ApiReply::ok(json!({
            "schema":"skew.stockmesh.exposure-clearing-preview/v2",
            "clearingId":clearing_id,
            "instrument":request.instrument,
            "stateSlot":publication.snapshot.slot,
            "productPolicySetHash":policy_set_hash,
            "buyer":{
                "owner":request.buyer.owner,
                "nonce":buyer_nonce.to_string(),
                "inputAsset":"USDC",
                "inputAtoms":buyer_input.to_string(),
                "minimumExposureQ32":buyer_minimum.to_string(),
                "totalConservativeExposureQ32":total_exposure.to_string(),
                "products":products
            },
            "internalClearing":{
                "sellerCount":sellers.len(),
                "cashAtoms":internal_cash.to_string(),
                "conservativeExposureQ32":internal_exposure.to_string(),
                "sellers":sellers
            },
            "residualExecution":{
                "inputAtoms":residual_input.to_string(),
                "conservativeExposureQ32":residual_exposure.to_string(),
                "route":route,
                "requiresExternalLiquidity":residual_input>0
            },
            "executorCompetition":{
                "frozenIntentRequired":true,
                "winnerMustBindStateHash":hex(&publication.snapshot.hash),
                "selection":"MAX_EXPOSURE_THEN_FEE_THEN_CU"
            },
            "settlement":{
                "abiOpcode":14,
                "atomic":true,
                "computeCeiling":1_400_000,
                "exactSimulation":"REQUIRED_BEFORE_PREPARE",
                "deploymentBound":self.deployment_ready
            }
        }))
    }

    /// Preview one instrument-local Flow Folding cell against a single
    /// coherent bank. The result binds exact internal transfers and residual
    /// groups but is deliberately unsigned and unprepared; owner signatures,
    /// residual SBF graphs and final on-chain simulation remain mandatory.
    fn flow_clearing_preview(&self, body: &[u8]) -> ApiReply {
        let request: ClearingPreviewRequest = match serde_json::from_slice(body) {
            Ok(request) => request,
            Err(_) => {
                return ApiReply::error(
                    422,
                    "STOCKLANA_CLEARING_INVALID",
                    "Use the exact StockMesh clearing preview schema.",
                )
            }
        };
        if !self.admits_instrument(&request.instrument)
            || request.intents.len() < 2
            || request.intents.len() > MAX_INTENTS
        {
            return ApiReply::error(
                422,
                "STOCKLANA_CLEARING_INVALID",
                "A clearing cell requires 2 to 32 intents for one admitted stock.",
            );
        }
        let usdc_intent = IntentKey {
            instrument: request.instrument.clone(),
            input_symbol: "USDC".into(),
        };
        let sol_intent = IntentKey {
            instrument: request.instrument.clone(),
            input_symbol: "SOL".into(),
        };
        let (Some(&bank_index), Some(&sol_bank_index)) = (
            self.intent_bank.get(&usdc_intent),
            self.intent_bank.get(&sol_intent),
        ) else {
            return ApiReply::error(
                503,
                "STOCKLANA_LANE_UNAVAILABLE",
                "The instrument does not have both coherent settlement lanes.",
            );
        };
        if bank_index != sol_bank_index {
            return ApiReply::error(
                503,
                "STOCKLANA_CLEARING_BANK_SPLIT",
                "The stock and settlement assets are not published by one bank.",
            );
        }
        let bank = &self.banks[bank_index];
        let publication = match bank.current() {
            Ok(publication) => publication,
            Err(_) => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_STATE_STALE",
                    "The coherent liquidity state is refreshing.",
                )
            }
        };
        let usdc_lanes = publication
            .layout
            .lanes
            .iter()
            .filter(|lane| {
                lane.key.instrument == request.instrument && lane.key.input_symbol == "USDC"
            })
            .collect::<Vec<_>>();
        let sol_lanes = publication
            .layout
            .lanes
            .iter()
            .filter(|lane| {
                lane.key.instrument == request.instrument && lane.key.input_symbol == "SOL"
            })
            .collect::<Vec<_>>();
        if usdc_lanes.len() != 1
            || sol_lanes.len() != 1
            || usdc_lanes[0].key.product_id != sol_lanes[0].key.product_id
        {
            return ApiReply::error(
                503,
                "STOCKMESH_CLEARING_PRODUCT_CONVERSION_REQUIRED",
                "Cross-product clearing requires an admitted synchronous conversion edge.",
            );
        }
        let usdc_key = &usdc_lanes[0].key;
        let sol_key = &sol_lanes[0].key;
        let (Some(usdc_worlds), Some(sol_worlds)) = (
            publication.worlds.get(usdc_key),
            publication.worlds.get(sol_key),
        ) else {
            return ApiReply::error(
                503,
                "STOCKLANA_LANE_UNAVAILABLE",
                "The liquidity compiler did not publish both settlement lanes.",
            );
        };
        let stock_symbol = usdc_lanes[0].output_symbol.as_str();
        let stock_lot = match revalidate_output(usdc_worlds, USDC_CLEARING_LOT) {
            Ok(output) if output > 0 && output <= MAX_ATOMS => output,
            _ => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_CLEARING_ANCHOR",
                    "The coherent USDC stock anchor could not be compiled.",
                )
            }
        };
        let (sol_lot, sol_anchor_output, lot_error_bps) =
            match derive_sol_lot(sol_worlds, stock_lot) {
                Ok(value) if value.2 <= MAX_CLEARING_LOT_ERROR_BPS => value,
                _ => {
                    return ApiReply::error(
                        503,
                        "STOCKLANA_CLEARING_ANCHOR",
                        "The coherent SOL stock anchor exceeds the clearing error bound.",
                    )
                }
            };
        let lots = [USDC_CLEARING_LOT, sol_lot, stock_lot];
        let maximum_expiry = publication
            .snapshot
            .slot
            .saturating_add(MAX_CLEARING_EXPIRY_SLOTS);
        let mut orders = Vec::with_capacity(request.intents.len());
        for intent in &request.intents {
            let parsed: Result<FlowIntent> = (|| {
                let owner = decode_key(&intent.owner)?;
                let nonce = parse_u64_string(&intent.nonce, false)?;
                let sell = clearing_asset(&intent.sell_asset, stock_symbol)?;
                let buy = clearing_asset(&intent.buy_asset, stock_symbol)?;
                let amount = parse_u64_string(&intent.amount_atoms, false)?;
                let min_out = parse_u64_string(&intent.min_out_atoms, true)?;
                let expires_at_slot = parse_u64_string(&intent.expires_at_slot, false)?;
                if sell == buy
                    || amount > MAX_ATOMS
                    || min_out > MAX_ATOMS
                    || expires_at_slot < publication.snapshot.slot
                    || expires_at_slot > maximum_expiry
                {
                    return Err("clearing intent bounds".into());
                }
                Ok(FlowIntent {
                    owner,
                    nonce,
                    sell,
                    buy,
                    amount,
                    min_out,
                    expires_at_slot,
                })
            })();
            match parsed {
                Ok(intent) => orders.push(intent),
                Err(_) => {
                    return ApiReply::error(
                        422,
                        "STOCKLANA_CLEARING_INTENT_INVALID",
                        "Every owner, nonce, asset, amount, limit and expiry must satisfy the bounded cell schema.",
                    )
                }
            }
        }
        let mut meter = WorkMeter::new(1_000_000);
        let report = match fold_lots(
            &orders,
            &lots,
            publication.snapshot.slot,
            MAX_PIVOTS,
            &mut meter,
        ) {
            Ok(report) if report.optimal_at_fixed_lots => report,
            Ok(_) => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_CLEARING_SEARCH_BUDGET",
                    "The fixed-lot circulation did not reach its optimality certificate.",
                )
            }
            Err(_) => {
                return ApiReply::error(
                    422,
                    "STOCKLANA_CLEARING_REJECTED",
                    "The intent graph violates a clearing invariant or user limit.",
                )
            }
        };
        let residual = match residuals_lots(&orders, &report) {
            Ok(residual) => residual,
            Err(_) => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_CLEARING_INVARIANT",
                    "The residual vector failed its conservation proof.",
                )
            }
        };
        let asset_names = ["USDC", "SOL", stock_symbol];
        let fills = request
            .intents
            .iter()
            .zip(orders.iter())
            .zip(report.fills.iter())
            .map(|((source, order), fill)| {
                json!({
                    "owner":source.owner,
                    "nonce":source.nonce,
                    "sellAsset":source.sell_asset,
                    "buyAsset":source.buy_asset,
                    "requestedInputAtoms":order.amount.to_string(),
                    "minimumOutputAtoms":order.min_out.to_string(),
                    "internalInputAtoms":fill.input.to_string(),
                    "internalOutputAtoms":fill.output.to_string(),
                    "residualInputAtoms":(order.amount-fill.input).to_string(),
                    "residualMinimumOutputAtoms":order.min_out.saturating_sub(fill.output).to_string()
                })
            })
            .collect::<Vec<_>>();
        let residuals = residual.groups[..residual.len]
            .iter()
            .map(|group| {
                let members = (0..orders.len())
                    .filter(|index| group.members & (1u32 << index) != 0)
                    .collect::<Vec<_>>();
                json!({
                    "sellAsset":asset_names[group.sell as usize],
                    "buyAsset":asset_names[group.buy as usize],
                    "amountInAtoms":group.amount_in.to_string(),
                    "minimumOutAtoms":group.min_out.to_string(),
                    "intentIndexes":members
                })
            })
            .collect::<Vec<_>>();
        let gross_notional = report
            .internally_cleared_lots
            .saturating_mul(u128::from(USDC_CLEARING_LOT));
        let lot_sizes = BTreeMap::from([
            ("USDC".to_string(), USDC_CLEARING_LOT.to_string()),
            ("SOL".to_string(), sol_lot.to_string()),
            (stock_symbol.to_string(), stock_lot.to_string()),
        ]);
        let mut digest = Sha256::new();
        digest.update(b"SKEW_STOCKMESH_CLEARING_PREVIEW_V1\0");
        digest.update(publication.snapshot.hash);
        digest.update(body);
        digest.update(sol_lot.to_le_bytes());
        digest.update(stock_lot.to_le_bytes());
        let digest = digest.finalize();
        ApiReply::ok(json!({
            "schema":"skew.stockmesh.clearing-preview/v1",
            "clearingId":format!("stkc_{}",hex(&digest[..16])),
            "instrument":request.instrument,
            "product":stock_symbol,
            "stateSlot":publication.snapshot.slot,
            "anchor":{
                "numeraire":"USDC",
                "targetAtoms":USDC_CLEARING_LOT.to_string(),
                "lotSizes":lot_sizes,
                "solLotStockOutputAtoms":sol_anchor_output.to_string(),
                "maximumObservedErrorBps":lot_error_bps,
                "source":if self.captured_fixture {"captured_coherent_executable_bank"} else {"live_coherent_executable_bank"}
            },
            "flowFolding":{
                "intents":fills,
                "requestedLots":report.requested_lots.to_string(),
                "internallyClearedLots":report.internally_cleared_lots.to_string(),
                "grossInternalNotionalUsdcAtoms":gross_notional.to_string(),
                "pivots":report.pivots,
                "reverseEdgePivots":report.reverse_edge_pivots,
                "optimalAtFixedLots":report.optimal_at_fixed_lots
            },
            "residualExecution":{"groups":residuals,"groupCount":residual.len,"boundedAtomicReflowRequired":residual.len>0},
            "execution":{
                "ownerSignaturesVerified":false,
                "executionAdmitted":false,
                "exactSimulation":"REQUIRED_BEFORE_PREPARE",
                "flowCellIntentBound":4,
                "flowCapsulesRequired":orders.len()>4
            }
        }))
    }

    pub fn prepare(&self, body: &[u8]) -> ApiReply {
        let request: PrepareRequest = match serde_json::from_slice(body) {
            Ok(request) => request,
            Err(_) => {
                return ApiReply::error(
                    422,
                    "STOCKLANA_PREPARE_INVALID",
                    "Use the exact Stocklana prepare schema.",
                )
            }
        };
        if !valid_id(&request.quote_id, "stkq_") || decode_key(&request.owner).is_err() {
            return ApiReply::error(
                422,
                "STOCKLANA_PREPARE_INVALID",
                "Quote or wallet identity is invalid.",
            );
        }
        if self
            .sell_quotes
            .lock()
            .ok()
            .is_some_and(|cache| cache.values.contains_key(&request.quote_id))
        {
            return self.prepare_sell_quote(&request);
        }
        let cached = {
            let cache = match self.quotes.lock() {
                Ok(cache) => cache,
                Err(_) => {
                    return ApiReply::error(503, "STOCKLANA_CACHE", "Quote state is unavailable.")
                }
            };
            cache.values.get(&request.quote_id).cloned()
        };
        let Some(cached) = cached else {
            return ApiReply::error(410, "STOCKLANA_QUOTE_GONE", "Quote expired or is unknown.");
        };
        if cached.expires <= Instant::now() || cached.expires_at_ms <= now_ms().unwrap_or(u64::MAX)
        {
            return ApiReply::error(
                410,
                "STOCKLANA_QUOTE_EXPIRED",
                "Quote expired. Refresh the executable price.",
            );
        }
        let Some(&bank_index) = self.intent_bank.get(&cached.intent) else {
            return ApiReply::error(
                503,
                "STOCKLANA_LANE_UNAVAILABLE",
                "The quoted lane is no longer admitted.",
            );
        };
        let current = match self.banks[bank_index].current() {
            Ok(current) => current,
            Err(_) => {
                return ApiReply::error(
                    409,
                    "STOCKLANA_QUOTE_REVOKED",
                    "Liquidity state changed. Refresh the quote.",
                )
            }
        };
        let current_lanes = current
            .layout
            .lanes
            .iter()
            .filter(|lane| {
                lane.key.instrument == cached.intent.instrument
                    && lane.key.input_symbol == cached.intent.input_symbol
            })
            .collect::<Vec<_>>();
        if current_lanes.is_empty()
            || product_policy_set_hash(&current_lanes) != cached.product_policy_set_hash
        {
            return ApiReply::error(
                409,
                "STOCKLANA_QUOTE_REVOKED",
                "The admitted issuer product policy changed. Refresh the quote.",
            );
        }
        let state_changed = current.snapshot.slot != cached.slot
            || hex(&current.snapshot.hash) != cached.snapshot_hash;
        let refreshed_plan = if state_changed {
            match solve_exposure(&current, &cached.intent, cached.input) {
                Ok(plan) if plan.exposure_q32 >= cached.minimum_exposure_q32 => Some(plan),
                _ => return ApiReply::error(
                    409,
                    "STOCKLANA_QUOTE_REVOKED",
                    "The current conservative stock exposure no longer satisfies the signed floor.",
                ),
            }
        } else {
            None
        };
        match self
            .prepared
            .lock()
            .map_err(|_| "prepared cache poisoned".to_string())
            .and_then(|cache| {
                cache.review(
                    &request.quote_id,
                    &request.owner,
                    current.snapshot.hash,
                    current.snapshot.slot,
                )
            }) {
            Ok(Some(mut review)) => {
                review["submitAllowed"] = json!(self.submission_enabled());
                return ApiReply::ok(review);
            }
            Err(_) => {
                return ApiReply::error(
                    409,
                    "STOCKLANA_PREPARED_REVOKED",
                    "Prepared execution expired or state changed. Refresh the quote.",
                )
            }
            Ok(None) => {}
        }
        if self.prepare_runtime.is_some() {
            let (allocation, product_outputs, maximum_cu) = if let Some(plan) = &refreshed_plan {
                (
                    plan.native_candidates.as_slice(),
                    plan.products
                        .iter()
                        .filter(|product| product.raw_output_atoms > 0)
                        .map(|product| (product.product_id.clone(), product.raw_output_atoms))
                        .collect::<BTreeMap<_, _>>(),
                    plan.maximum_cu,
                )
            } else {
                (
                    cached.native_candidates.as_slice(),
                    cached.product_output_atoms.clone(),
                    cached.maximum_cu,
                )
            };
            if let Err(error) = self.produce_direct_prepared(
                &request.quote_id,
                &request.owner,
                &cached,
                &current,
                &current_lanes,
                &product_outputs,
                allocation,
                maximum_cu,
            ) {
                let (status, code, message) = if error.contains("changed")
                    || error.contains("advanced")
                    || error.contains("rebuild")
                    || error.contains("stale")
                {
                    (
                        409,
                        "STOCKLANA_PREPARE_RETRY",
                        "The execution bank advanced. Refresh the executable price.",
                    )
                } else {
                    (
                        503,
                        "STOCKLANA_PREPARE_REJECTED",
                        "The wallet-specific on-chain preparation failed a deployment or simulation gate.",
                    )
                };
                return ApiReply::error(status, code, message);
            }
            return match self
                .prepared
                .lock()
                .map_err(|_| "prepared cache poisoned".to_string())
                .and_then(|cache| {
                    cache
                        .review(
                            &request.quote_id,
                            &request.owner,
                            current.snapshot.hash,
                            current.snapshot.slot,
                        )
                        .and_then(|value| value.ok_or_else(|| "prepared result missing".into()))
                }) {
                Ok(mut review) => {
                    review["submitAllowed"] = json!(self.submission_enabled());
                    ApiReply::ok(review)
                }
                Err(_) => ApiReply::error(
                    409,
                    "STOCKLANA_PREPARED_REVOKED",
                    "Prepared execution expired or state changed. Refresh the quote.",
                ),
            };
        }
        ApiReply::error(
            503,
            "STOCKLANA_DEPLOYMENT_REQUIRED",
            "The settlement program and wallet-specific exact simulation are not deployed yet.",
        )
    }

    fn prepare_sell_quote(&self, request: &PrepareRequest) -> ApiReply {
        let cached = match self
            .sell_quotes
            .lock()
            .ok()
            .and_then(|cache| cache.values.get(&request.quote_id).cloned())
        {
            Some(cached) => cached,
            None => {
                return ApiReply::error(410, "STOCKLANA_QUOTE_GONE", "Quote expired or is unknown.")
            }
        };
        if cached.expires <= Instant::now() || cached.expires_at_ms <= now_ms().unwrap_or(u64::MAX)
        {
            return ApiReply::error(
                410,
                "STOCKLANA_QUOTE_EXPIRED",
                "Quote expired. Refresh the executable price.",
            );
        }
        let Some(&bank_index) = self.intent_bank.get(&cached.intent) else {
            return ApiReply::error(
                503,
                "STOCKLANA_LANE_UNAVAILABLE",
                "The quoted sell lane is no longer admitted.",
            );
        };
        let current = match self.banks[bank_index].current() {
            Ok(current) => current,
            Err(_) => {
                return ApiReply::error(
                    409,
                    "STOCKLANA_QUOTE_REVOKED",
                    "Liquidity state changed. Refresh the quote.",
                )
            }
        };
        let Some(lane) = current.layout.lanes.iter().find(|lane| {
            lane.key.instrument == cached.instrument
                && lane.key.input_symbol == cached.output_symbol
                && lane.key.product_id == cached.product_id
                && lane.output_mint == cached.product_mint
        }) else {
            return ApiReply::error(
                409,
                "STOCKLANA_QUOTE_REVOKED",
                "The issuer product lane changed. Refresh the quote.",
            );
        };
        if lane.product_policy_hash != cached.product_policy_hash {
            return ApiReply::error(
                409,
                "STOCKLANA_QUOTE_REVOKED",
                "The issuer product policy changed. Refresh the quote.",
            );
        }
        let state_changed = current.snapshot.slot != cached.slot
            || hex(&current.snapshot.hash) != cached.snapshot_hash;
        let plan = if state_changed {
            match solve_sell(&current, lane, cached.input) {
                Ok(plan) if plan.output_atoms >= cached.minimum_output => plan,
                _ => {
                    return ApiReply::error(
                        409,
                        "STOCKLANA_QUOTE_REVOKED",
                        "The current cash output no longer satisfies the quoted floor.",
                    )
                }
            }
        } else {
            SellPlan {
                output_atoms: cached.quoted_output,
                route: Vec::new(),
                native_allocation: cached.native_allocation.clone(),
                maximum_cu: cached.maximum_cu,
                planning_ns: 0,
                oracle_calls: 0,
                native_evaluations: 0,
            }
        };
        if let Ok(cache) = self.sell_prepared.lock() {
            if let Some(stored) = cache
                .values
                .get(&(request.quote_id.clone(), request.owner.clone()))
            {
                if stored.expires > Instant::now()
                    && stored
                        .candidate
                        .feed
                        .validate_fence(&stored.candidate.snapshot)
                        .is_ok()
                {
                    return ApiReply::ok(sell_prepared_review(
                        request,
                        stored,
                        self.submission_enabled(),
                    ));
                }
            }
        }
        let Some(runtime) = &self.prepare_runtime else {
            return ApiReply::error(
                503,
                "STOCKLANA_DEPLOYMENT_REQUIRED",
                "The settlement program and wallet-specific exact simulation are not deployed yet.",
            );
        };
        let owner = match decode_key(&request.owner) {
            Ok(owner) => Pubkey::new_from_array(owner),
            Err(_) => {
                return ApiReply::error(
                    422,
                    "STOCKLANA_PREPARE_INVALID",
                    "Wallet identity is invalid.",
                )
            }
        };
        let output_mint = if cached.output_symbol == "USDC" {
            USDC_MINT
        } else {
            WSOL_MINT
        };
        let candidate = match runtime.prepare_sell(
            current.layout.feed.keys(),
            &current.snapshot,
            SellPrepareIntent {
                quote_id: request.quote_id.clone(),
                owner,
                instrument: cached.instrument.clone(),
                product_id: cached.product_id.clone(),
                product_mint: match cached.product_mint.parse() {
                    Ok(mint) => mint,
                    Err(_) => {
                        return ApiReply::error(
                            503,
                            "STOCKLANA_PREPARE_REJECTED",
                            "The product mint is invalid.",
                        )
                    }
                },
                output_mint: match output_mint.parse() {
                    Ok(mint) => mint,
                    Err(_) => {
                        return ApiReply::error(
                            503,
                            "STOCKLANA_PREPARE_REJECTED",
                            "The output mint is invalid.",
                        )
                    }
                },
                input_atoms: cached.input,
                minimum_output_atoms: cached.minimum_output,
                quoted_output_atoms: plan.output_atoms,
                world_generation_hash: current.snapshot.hash,
                maximum_cu: plan.maximum_cu,
            },
            &plan.native_allocation,
        ) {
            Ok(candidate) => candidate,
            Err(error) => {
                let status = if error.contains("changed")
                    || error.contains("advanced")
                    || error.contains("rebuild")
                {
                    409
                } else {
                    503
                };
                return ApiReply::error(
                    status,
                    "STOCKLANA_PREPARE_REJECTED",
                    "The reverse execution failed its deployment or exact-simulation gate.",
                );
            }
        };
        let stored = StoredPreparedSell {
            candidate,
            expires: cached.expires,
            expires_at: iso8601(cached.expires_at_ms),
        };
        let response = sell_prepared_review(request, &stored, self.submission_enabled());
        let mut cache = match self.sell_prepared.lock() {
            Ok(cache) => cache,
            Err(_) => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_CACHE",
                    "Prepared sell state is unavailable.",
                )
            }
        };
        cache
            .values
            .retain(|_, value| value.expires > Instant::now());
        if cache.values.len() >= 256 {
            return ApiReply::error(503, "STOCKLANA_CACHE", "Prepared sell capacity is full.");
        }
        cache
            .values
            .insert((request.quote_id.clone(), request.owner.clone()), stored);
        ApiReply::ok(response)
    }

    /// Accept only the exact wallet-signed wire previously compiled and
    /// simulated for this quote, then hand that immutable wire to the durable
    /// status-first sender. The request cannot provide routes, accounts,
    /// postconditions, resources or receipt expectations.
    /// Durable order observation is independent of expiring quote caches.
    /// This endpoint has no send path and cannot mint a new authorization.
    pub fn orders(&self, body: &[u8]) -> ApiReply {
        let request: OrdersRequest = match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(_) => return ApiReply::error(422, "STOCKLANA_ORDERS_INVALID", "Use a wallet owner and optional prepared ID."),
        };
        if decode_key(&request.owner).is_err()
            || request.prepared_id.as_ref().is_some_and(|id| !valid_id(id, "stkp_"))
            || request.offset > 1_000_000 || (request.prepared_id.is_some() && request.offset != 0)
            || request.revision.as_ref().is_some_and(|r| r.len() != 64 || !r.bytes().all(|c| c.is_ascii_hexdigit())) {
            return ApiReply::error(422, "STOCKLANA_ORDERS_INVALID", "Invalid wallet or prepared ID.");
        }
        let Some(sender) = &self.sender else {
            return ApiReply::error(503, "STOCKLANA_HISTORY_UNAVAILABLE", "Execution history is not connected on this deployment.");
        };
        let mut sender = match sender.lock() {
            Ok(s) => s,
            Err(_) => return ApiReply::error(503, "STOCKLANA_SENDER_UNAVAILABLE", "Execution history is unavailable."),
        };
        let matching = sender.journal.entries().filter(|e| {
            let owner = e.wallet_owner();
            owner == Some(request.owner.as_str())
                && request.prepared_id.as_ref().is_none_or(|id| id == &e.id)
        }).map(|e| (e.id.clone(), !matches!(e.phase, Phase::Failed | Phase::Reconciled))).collect::<Vec<_>>();
        let (ids, revision, total, active_count) = match order_page::select(matching, request.offset, request.revision.as_deref()) {
            Ok(page) => page,
            Err(_) => return ApiReply::error(409, "STOCKLANA_ORDERS_CHANGED", "Order history changed. Refresh from the first page."),
        };
        let next_offset = (request.offset + ids.len() < total).then_some(request.offset + ids.len());
        let truncated = next_offset.is_some();
        let observed = ids.is_empty() || sender.observe_batch(&ids.iter().map(String::as_str).collect::<Vec<_>>()).is_ok();
        // Bound receipt reads independently from status batching. Later polls
        // drain finalized receipts; historical reconciled rows need no reread.
        for id in ids.iter().filter(|id| sender.journal.get(id).is_some_and(|e| e.phase == Phase::Finalized)).take(4).cloned().collect::<Vec<_>>() {
            let is_sell = sender.journal.get(&id).is_some_and(|e| e.expected_swap.is_some());
            if sender.journal.get(&id).is_some_and(|e|e.expected_basket.is_some()){let _=sender.reconcile_basket(&id);}
            else if is_sell { let _ = sender.reconcile_swap(&id); }
            else { let _ = sender.reconcile_exposure(&id); }
        }
        let orders = ids.iter().filter_map(|id| sender.journal.get(id)).map(|e| json!({
            "preparedId":e.id, "signature":e.signature, "phase":phase_name(&e.phase),
            "attempts":e.attempts, "receiptVerified":e.phase == Phase::Reconciled,
            "side":if e.expected_swap.is_some() { "SELL" } else { "BUY" },
            "kind":if e.expected_basket.is_some(){"BASKET"}else{"STOCK"}
        })).collect::<Vec<_>>();
        ApiReply::ok(json!({ "schema":"skew.stockmesh.orders/v1", "owner":request.owner,
            "observation":"PROVIDER_OBSERVED", "refreshSucceeded":observed,
            "page":{"revision":revision,"offset":request.offset,"nextOffset":next_offset,"totalOrders":total,"activeOrders":active_count},
            "observedAt":iso8601(now_ms().unwrap_or(0)), "truncated":truncated, "orders":orders }))
    }

    pub fn submit(&self, body: &[u8]) -> ApiReply {
        let request: SubmitRequest = match serde_json::from_slice(body) {
            Ok(request) => request,
            Err(_) => {
                return ApiReply::error(
                    422,
                    "STOCKLANA_SUBMIT_INVALID",
                    "Use the exact Stocklana signed-transaction schema.",
                )
            }
        };
        if !valid_id(&request.quote_id, "stkq_")
            || !valid_id(&request.prepared_id, "stkp_")
            || decode_key(&request.owner).is_err()
            || request.signed_transaction_base64.len() > 1_644
        {
            return ApiReply::error(
                422,
                "STOCKLANA_SUBMIT_INVALID",
                "Quote, prepared execution, wallet or signed transaction is invalid.",
            );
        }
        let signed_wire = match STANDARD.decode(&request.signed_transaction_base64) {
            Ok(wire) if (65..=1232).contains(&wire.len()) => wire,
            _ => {
                return ApiReply::error(
                    422,
                    "STOCKLANA_SUBMIT_INVALID",
                    "The signed transaction is not a bounded Solana wire.",
                )
            }
        };
        let Some(sender) = &self.sender else {
            return ApiReply::error(
                503,
                "STOCKLANA_SUBMISSION_DISABLED",
                "Exact-wire submission is not armed on this deployment.",
            );
        };
        // A lost HTTP response is not permission for a second economic action.
        // Consult the durable identity before any expiring quote/state cache.
        // Recovery only observes; it never calls the sender's transmitting path.
        {
            let mut sender = match sender.lock() {
                Ok(sender) => sender,
                Err(_) => return ApiReply::error(503, "STOCKLANA_SENDER_UNAVAILABLE", "The durable sender is unavailable."),
            };
            if let Some(reply) = recover_submission(&mut sender, &request, &signed_wire) {
                return reply;
            }
        }
        if self.basket_prepared.lock().ok().is_some_and(|cache|cache.contains(&request.quote_id)) {
            return self.submit_basket(&request,&signed_wire);
        }
        if self
            .sell_quotes
            .lock()
            .ok()
            .is_some_and(|cache| cache.values.contains_key(&request.quote_id))
        {
            return self.submit_sell(&request, &signed_wire);
        }
        let cached = {
            let cache = match self.quotes.lock() {
                Ok(cache) => cache,
                Err(_) => {
                    return ApiReply::error(503, "STOCKLANA_CACHE", "Quote state is unavailable.")
                }
            };
            cache.values.get(&request.quote_id).cloned()
        };
        let Some(cached) = cached else {
            return ApiReply::error(410, "STOCKLANA_QUOTE_GONE", "Quote expired or is unknown.");
        };
        if cached.expires <= Instant::now() || cached.expires_at_ms <= now_ms().unwrap_or(u64::MAX)
        {
            return ApiReply::error(
                410,
                "STOCKLANA_QUOTE_EXPIRED",
                "Quote expired. Refresh the executable price.",
            );
        }
        let Some(&bank_index) = self.intent_bank.get(&cached.intent) else {
            return ApiReply::error(
                503,
                "STOCKLANA_LANE_UNAVAILABLE",
                "The quoted lane is no longer admitted.",
            );
        };
        let current = match self.banks[bank_index].current() {
            Ok(current) => current,
            Err(_) => {
                return ApiReply::error(
                    409,
                    "STOCKLANA_PREPARED_REVOKED",
                    "Liquidity state changed. Refresh the executable price.",
                )
            }
        };
        let mut entry = match self
            .prepared
            .lock()
            .map_err(|_| "prepared cache poisoned".to_string())
            .and_then(|cache| {
                cache.authorize_submission(
                    &request.quote_id,
                    &request.owner,
                    &request.prepared_id,
                    current.snapshot.hash,
                    current.snapshot.slot,
                    &signed_wire,
                )
            }) {
            Ok(entry) => entry,
            Err(_) => {
                return ApiReply::error(
                    409,
                    "STOCKLANA_PREPARED_REVOKED",
                    "The signed transaction differs from the current prepared execution.",
                )
            }
        };
        entry.quote_id = Some(request.quote_id.clone());
        let mut sender = match sender.lock() {
            Ok(sender) => sender,
            Err(_) => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_SENDER_UNAVAILABLE",
                    "The durable sender is unavailable.",
                )
            }
        };
        if let Some(existing) = sender.journal.get(&entry.id) {
            match same_authorization(existing, &entry) {
                Ok(true) => return recover_submission(&mut sender, &request, &signed_wire)
                    .unwrap_or_else(|| ApiReply::error(503, "STOCKLANA_SENDER_UNAVAILABLE", "The recorded order is unavailable.")),
                _ => {
                    return ApiReply::error(
                        409,
                        "STOCKLANA_SUBMISSION_CONFLICT",
                        "This prepared execution already has different durable authorization.",
                    )
                }
            }
        } else if sender.journal.insert(entry.clone()).is_err() {
            return ApiReply::error(
                409,
                "STOCKLANA_SUBMISSION_CONFLICT",
                "The exact wire could not reserve its durable execution resources.",
            );
        }
        let phase = match sender.step(&entry.id) {
            Ok(phase) => phase,
            Err(_) => {
                return ApiReply {
                    status: 503,
                    body: json!({
                        "error":{
                            "code":"STOCKLANA_SUBMISSION_UNCERTAIN",
                            "message":"The outcome is not yet known. Retry this exact signed transaction."
                        },
                        "preparedId":entry.id,
                        "signature":entry.signature,
                        "sameSignedWire":true
                    }),
                }
            }
        };
        let verified = if matches!(phase, Phase::Finalized | Phase::Reconciled) {
            match sender.reconcile_exposure(&entry.id) {
                Ok(receipt) => Some(receipt),
                Err(_) => {
                    return ApiReply {
                        status: 503,
                        body: json!({
                            "error":{
                                "code":"STOCKLANA_RECEIPT_PENDING",
                                "message":"The transaction finalized, but its economic receipt is not verified yet. Retry this exact signed transaction."
                            },
                            "preparedId":entry.id,
                            "signature":entry.signature,
                            "phase":"FINALIZED",
                            "sameSignedWire":true
                        }),
                    }
                }
            }
        } else {
            None
        };
        let persisted = match sender.journal.get(&entry.id) {
            Some(entry) => entry,
            None => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_SENDER_UNAVAILABLE",
                    "The durable sender state is unavailable.",
                )
            }
        };
        ApiReply::ok(json!({
            "schema":"skew.stocklana.submission/v1",
            "quoteId":request.quote_id,
            "preparedId":request.prepared_id,
            "signature":persisted.signature,
            "phase":phase_name(&persisted.phase),
            "attempts":persisted.attempts,
            "sameSignedWire":true,
            "verifiedExposure":verified
        }))
    }

    fn submit_sell(&self, request: &SubmitRequest, signed_wire: &[u8]) -> ApiReply {
        let cached = match self
            .sell_quotes
            .lock()
            .ok()
            .and_then(|cache| cache.values.get(&request.quote_id).cloned())
        {
            Some(cached) => cached,
            None => {
                return ApiReply::error(410, "STOCKLANA_QUOTE_GONE", "Quote expired or is unknown.")
            }
        };
        if cached.expires <= Instant::now() || cached.expires_at_ms <= now_ms().unwrap_or(u64::MAX)
        {
            return ApiReply::error(
                410,
                "STOCKLANA_QUOTE_EXPIRED",
                "Quote expired. Refresh the executable price.",
            );
        }
        let Some(&bank_index) = self.intent_bank.get(&cached.intent) else {
            return ApiReply::error(
                503,
                "STOCKLANA_LANE_UNAVAILABLE",
                "The sell lane is no longer admitted.",
            );
        };
        let current = match self.banks[bank_index].current() {
            Ok(current) => current,
            Err(_) => {
                return ApiReply::error(
                    409,
                    "STOCKLANA_PREPARED_REVOKED",
                    "Liquidity state changed. Refresh the executable price.",
                )
            }
        };
        let cache = match self.sell_prepared.lock() {
            Ok(cache) => cache,
            Err(_) => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_CACHE",
                    "Prepared sell state is unavailable.",
                )
            }
        };
        let Some(stored) = cache
            .values
            .get(&(request.quote_id.clone(), request.owner.clone()))
        else {
            return ApiReply::error(
                409,
                "STOCKLANA_PREPARED_REVOKED",
                "The sell transaction was not prepared for this wallet.",
            );
        };
        let expected_id = sell_prepared_id(&request.quote_id, &request.owner, &stored.candidate);
        let market_matches = stored
            .candidate
            .snapshot
            .project(current.layout.feed.keys())
            .is_ok_and(|projection| {
                projection.slot == current.snapshot.slot && projection.hash == current.snapshot.hash
            });
        if request.prepared_id != expected_id
            || stored.expires <= Instant::now()
            || !market_matches
            || stored
                .candidate
                .feed
                .validate_fence(&stored.candidate.snapshot)
                .is_err()
            || signed_wire.len() != stored.candidate.unsigned_wire.len()
            || signed_wire.get(65..) != stored.candidate.unsigned_wire.get(65..)
        {
            return ApiReply::error(
                409,
                "STOCKLANA_PREPARED_REVOKED",
                "The signed sell transaction differs from the current prepared execution.",
            );
        }
        let mut entry = match crate::sender::authorize(
            signed_wire,
            crate::sender::Authorization {
                intent_id: expected_id,
                message_hash: stored.candidate.message_hash,
                last_valid_height: stored.candidate.last_valid_block_height,
                resources: stored.candidate.resources.clone(),
            },
        ) {
            Ok(entry) => entry,
            Err(_) => {
                return ApiReply::error(
                    409,
                    "STOCKLANA_PREPARED_REVOKED",
                    "The wallet signature does not bind the prepared sell transaction.",
                )
            }
        };
        entry.expected_swap = Some(stored.candidate.expected.clone());
        entry.quote_id = Some(request.quote_id.clone());
        drop(cache);
        let Some(sender) = &self.sender else {
            return ApiReply::error(
                503,
                "STOCKLANA_SUBMISSION_DISABLED",
                "Exact-wire submission is not armed on this deployment.",
            );
        };
        let mut sender = match sender.lock() {
            Ok(sender) => sender,
            Err(_) => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_SENDER_UNAVAILABLE",
                    "The durable sender is unavailable.",
                )
            }
        };
        if let Some(existing) = sender.journal.get(&entry.id) {
            if !same_authorization(existing, &entry).unwrap_or(false) {
                return ApiReply::error(
                    409,
                    "STOCKLANA_SUBMISSION_CONFLICT",
                    "This prepared execution already has different durable authorization.",
                );
            }
            return recover_submission(&mut sender, request, signed_wire)
                .unwrap_or_else(|| ApiReply::error(503, "STOCKLANA_SENDER_UNAVAILABLE", "The recorded order is unavailable."));
        } else if sender.journal.insert(entry.clone()).is_err() {
            return ApiReply::error(
                409,
                "STOCKLANA_SUBMISSION_CONFLICT",
                "The exact wire could not reserve its durable execution resources.",
            );
        }
        let phase = match sender.step(&entry.id) {
            Ok(phase) => phase,
            Err(_) => {
                return ApiReply {
                    status: 503,
                    body: json!({"error":{"code":"STOCKLANA_SUBMISSION_UNCERTAIN","message":"The outcome is not yet known. Retry this exact signed transaction."},
                    "preparedId":entry.id,"signature":entry.signature,"sameSignedWire":true}),
                }
            }
        };
        let verified = if matches!(phase, Phase::Finalized | Phase::Reconciled) {
            match sender.reconcile_swap(&entry.id) {
                Ok(receipt) => Some(receipt),
                Err(_) => {
                    return ApiReply {
                        status: 503,
                        body: json!({"error":{"code":"STOCKLANA_RECEIPT_PENDING","message":"The transaction finalized, but its cash receipt is not verified yet. Retry this exact signed transaction."},
                        "preparedId":entry.id,"signature":entry.signature,"phase":"FINALIZED","sameSignedWire":true}),
                    }
                }
            }
        } else {
            None
        };
        let persisted = match sender.journal.get(&entry.id) {
            Some(entry) => entry,
            None => {
                return ApiReply::error(
                    503,
                    "STOCKLANA_SENDER_UNAVAILABLE",
                    "The durable sender state is unavailable.",
                )
            }
        };
        ApiReply::ok(json!({
            "schema":"skew.stocklana.submission/v1",
            "quoteId":request.quote_id,"preparedId":request.prepared_id,
            "signature":persisted.signature,"phase":phase_name(&persisted.phase),
            "attempts":persisted.attempts,"sameSignedWire":true,"verifiedSwap":verified
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn produce_direct_prepared(
        &self,
        quote_id: &str,
        owner: &str,
        cached: &CachedQuote,
        current: &Arc<Publication>,
        lanes: &[&Lane],
        product_outputs: &BTreeMap<String, u64>,
        proposals: &[crate::world::NativeSwapProposal],
        maximum_cu: u64,
    ) -> Result<()> {
        let runtime = self
            .prepare_runtime
            .as_ref()
            .ok_or("prepare runtime unavailable")?;
        if proposals.is_empty()
            || proposals.len() > stocklana_adapters::graph::MAX_CANDIDATES
            || proposals
                .iter()
                .any(|proposal| !matches!(proposal.stage, 1 | 2))
        {
            return Err("direct prepare stage/leg bound".into());
        }
        if product_outputs.is_empty() {
            return Err("direct prepare selected product output".into());
        }
        let mut product_floors = product_outputs
            .iter()
            .map(|(product, output)| (product.clone(), Some(*output)))
            .collect::<BTreeMap<_, _>>();
        // Economic Reflow may admit a product as an intermediate or an
        // execution-time alternative even when the continuous solver selected
        // zero atoms for it at quote time. Its policy and ATA must still be in
        // the exact bank, but it must not inherit output from substitute venues.
        for product_id in proposals
            .iter()
            .filter_map(|proposal| proposal.product_id.as_ref())
        {
            product_floors.entry(product_id.clone()).or_insert(None);
        }
        let mut products = Vec::with_capacity(product_floors.len());
        for (product_id, quoted_output) in product_floors {
            let lane = lanes
                .iter()
                .find(|lane| lane.key.product_id == product_id)
                .ok_or("direct prepare product lane")?;
            let minimum = match quoted_output {
                Some(quoted_output) => u64::try_from(
                    (u128::from(quoted_output) * u128::from(10_000 - cached.max_slippage_bps))
                        / 10_000,
                )
                .map_err(|_| "direct prepare product minimum")?
                .max(1),
                None => 1,
            };
            products.push(PrepareProduct {
                product_id,
                identity: ProductIdentity {
                    instrument: lane.key.instrument.clone(),
                    issuer: lane.issuer.clone(),
                    mint: lane.output_mint.clone(),
                    token_program: lane.token_program.clone(),
                    rights_hash: decode_hex_32(&lane.rights_hash)?,
                    raw_decimals: lane.output_decimals,
                },
                model: match lane.exposure_model.as_str() {
                    "FIXED_RATIONAL" => 0,
                    "TOKEN_2022_SCALED_UI" => 1,
                    _ => return Err("direct prepare exposure model".into()),
                },
                numerator: lane.exposure_numerator,
                denominator: lane.exposure_denominator,
                conservative_bps: lane.conservative_bps,
                minimum_output_atoms: minimum,
            });
        }
        let admitted_product_ids = lanes
            .iter()
            .map(|lane| {
                ProductIdentity {
                    instrument: lane.key.instrument.clone(),
                    issuer: lane.issuer.clone(),
                    mint: lane.output_mint.clone(),
                    token_program: lane.token_program.clone(),
                    rights_hash: decode_hex_32(&lane.rights_hash)?,
                    raw_decimals: lane.output_decimals,
                }
                .id()
            })
            .collect::<Result<Vec<_>>>()?;
        let owner = Pubkey::new_from_array(decode_key(owner)?);
        let input_mint = match cached.intent.input_symbol.as_str() {
            "USDC" => Pubkey::new_from_array(decode_key(USDC_MINT)?),
            "SOL" => Pubkey::new_from_array(decode_key(WSOL_MINT)?),
            _ => return Err("direct prepare input asset".into()),
        };
        let variants = resource_admission_variants(proposals)?;
        let candidate = exact_resource_admission(variants, |variant| {
            runtime.prepare_direct(
                current.layout.feed.keys(),
                &current.snapshot,
                PrepareIntent {
                    quote_id: quote_id.into(),
                    owner,
                    instrument: cached.intent.instrument.clone(),
                    input_mint,
                    input_atoms: cached.input,
                    minimum_exposure_q32: cached.minimum_exposure_q32,
                    maximum_slippage_bps: cached.max_slippage_bps,
                    admitted_product_ids: admitted_product_ids.clone(),
                    product_policy_hash: decode_hex_32(&cached.product_policy_set_hash)?,
                    world_generation_hash: current.snapshot.hash,
                    maximum_cu,
                    economic_reflow: cached.global_reflow,
                },
                variant,
                products.clone(),
            )
        })?;
        self.admit_prepared(quote_id, candidate)
    }

    /// Trusted in-process executor handoff; there is deliberately no HTTP
    /// endpoint accepting client-supplied simulation results or policy records.
    pub fn admit_prepared(
        &self,
        quote_id: &str,
        candidate: crate::prepared_quotes::Candidate,
    ) -> Result<()> {
        if !self.captured_fixture
            && (!self.deployment_ready || candidate.prepared.genesis() != MAINNET_GENESIS)
        {
            return Err("prepared settlement deployment/genesis is not admitted".into());
        }
        let cached = self
            .quotes
            .lock()
            .map_err(|_| "quote cache poisoned")?
            .values
            .get(quote_id)
            .cloned()
            .ok_or("prepared quote not found")?;
        let intent = &candidate.intent;
        let bank = self
            .banks
            .get(
                *self
                    .intent_bank
                    .get(&cached.intent)
                    .ok_or("prepared bank missing")?,
            )
            .ok_or("prepared bank index")?;
        let current = bank.current()?;
        let lanes = current
            .layout
            .lanes
            .iter()
            .filter(|lane| {
                lane.key.instrument == cached.intent.instrument
                    && lane.key.input_symbol == cached.intent.input_symbol
            })
            .collect::<Vec<_>>();
        let mut ids = lanes
            .iter()
            .map(|lane| {
                ProductIdentity {
                    instrument: lane.key.instrument.clone(),
                    issuer: lane.issuer.clone(),
                    mint: lane.output_mint.clone(),
                    token_program: lane.token_program.clone(),
                    rights_hash: decode_hex_32(&lane.rights_hash)?,
                    raw_decimals: lane.output_decimals,
                }
                .id()
            })
            .collect::<Result<Vec<_>>>()?;
        ids.sort();
        ids.dedup();
        let mut actual = intent.admitted_product_ids.clone();
        actual.sort();
        let input_mint = match cached.intent.input_symbol.as_str() {
            "USDC" => USDC_MINT,
            "SOL" => WSOL_MINT,
            _ => return Err("prepared input asset".into()),
        };
        let mut identity = Sha256::new();
        identity.update(b"SKEW_STOCKMESH_QUOTE_INTENT_V1\0");
        identity.update(quote_id.as_bytes());
        identity.update(intent.owner.as_bytes());
        identity.update(intent.owner_nonce.to_le_bytes());
        if cached.expires <= Instant::now()
            || cached.expires_at_ms <= now_ms()?
            || intent.intent_id != <[u8; 32]>::from(identity.finalize())
            || intent.deadline_slot
                > current
                    .snapshot
                    .slot
                    .saturating_add(MAX_CLEARING_EXPIRY_SLOTS)
            || intent.instrument != cached.intent.instrument
            || intent.input_atoms != cached.input
            || intent.input_mint != input_mint
            || intent.minimum_exposure_q32 != u128::from(cached.minimum_exposure_q32)
            || actual != ids
            || hex(&intent.product_policy_hash) != cached.product_policy_set_hash
            || product_policy_set_hash(&lanes) != cached.product_policy_set_hash
            || candidate.snapshot.project(current.layout.feed.keys())?.hash
                != intent.world_generation_hash
            || current.snapshot.hash != intent.world_generation_hash
        {
            return Err(
                "prepared candidate differs from cached economic quote/current market".into(),
            );
        }
        self.prepared
            .lock()
            .map_err(|_| "prepared cache poisoned")?
            .admit(
                quote_id,
                candidate,
                cached.expires,
                iso8601(cached.expires_at_ms),
            )
    }

    /// Freeze the economic intent that every executor must bid against. This
    /// endpoint does not impersonate the wallet: it returns the exact
    /// commitment for owner signature and rejects a quote after any state or
    /// issuer-policy generation change.
    pub fn freeze(&self, body: &[u8]) -> ApiReply {
        let request: FreezeRequest = match serde_json::from_slice(body) {
            Ok(request) => request,
            Err(_) => {
                return ApiReply::error(
                    422,
                    "STOCKMESH_FREEZE_INVALID",
                    "Use the exact StockMesh frozen-intent schema.",
                )
            }
        };
        let owner_nonce = match parse_u64_string(&request.owner_nonce, true) {
            Ok(value) if value < u64::MAX => value,
            _ => {
                return ApiReply::error(422, "STOCKMESH_FREEZE_INVALID", "Owner nonce is invalid.")
            }
        };
        let deadline_slot = match parse_u64_string(&request.deadline_slot, false) {
            Ok(value) => value,
            _ => {
                return ApiReply::error(
                    422,
                    "STOCKMESH_FREEZE_INVALID",
                    "Deadline slot is invalid.",
                )
            }
        };
        if !valid_id(&request.quote_id, "stkq_") || decode_key(&request.owner).is_err() {
            return ApiReply::error(
                422,
                "STOCKMESH_FREEZE_INVALID",
                "Quote or wallet identity is invalid.",
            );
        }
        let cached = match self
            .quotes
            .lock()
            .ok()
            .and_then(|cache| cache.values.get(&request.quote_id).cloned())
        {
            Some(cached) => cached,
            None => {
                return ApiReply::error(410, "STOCKLANA_QUOTE_GONE", "Quote expired or is unknown.")
            }
        };
        if cached.expires <= Instant::now() || cached.expires_at_ms <= now_ms().unwrap_or(u64::MAX)
        {
            return ApiReply::error(
                410,
                "STOCKLANA_QUOTE_EXPIRED",
                "Quote expired. Refresh the executable price.",
            );
        }
        let Some(&bank_index) = self.intent_bank.get(&cached.intent) else {
            return ApiReply::error(
                503,
                "STOCKLANA_LANE_UNAVAILABLE",
                "The quoted lane is no longer admitted.",
            );
        };
        let publication = match self.banks[bank_index].current() {
            Ok(publication) => publication,
            Err(_) => {
                return ApiReply::error(
                    409,
                    "STOCKLANA_QUOTE_REVOKED",
                    "Liquidity state changed. Refresh the quote.",
                )
            }
        };
        if publication.snapshot.slot != cached.slot
            || hex(&publication.snapshot.hash) != cached.snapshot_hash
            || deadline_slot < publication.snapshot.slot
            || deadline_slot
                > publication
                    .snapshot
                    .slot
                    .saturating_add(MAX_CLEARING_EXPIRY_SLOTS)
        {
            return ApiReply::error(
                409,
                "STOCKMESH_FREEZE_REVOKED",
                "The quote state or requested deadline is no longer current.",
            );
        }
        let lanes = publication
            .layout
            .lanes
            .iter()
            .filter(|lane| {
                lane.key.instrument == cached.intent.instrument
                    && lane.key.input_symbol == cached.intent.input_symbol
            })
            .collect::<Vec<_>>();
        if lanes.is_empty() || product_policy_set_hash(&lanes) != cached.product_policy_set_hash {
            return ApiReply::error(
                409,
                "STOCKMESH_FREEZE_REVOKED",
                "The admitted issuer product set changed.",
            );
        }
        let admitted_product_ids = match lanes
            .iter()
            .map(|lane| {
                ProductIdentity {
                    instrument: lane.key.instrument.clone(),
                    issuer: lane.issuer.clone(),
                    mint: lane.output_mint.clone(),
                    token_program: lane.token_program.clone(),
                    rights_hash: decode_hex_32(&lane.rights_hash)?,
                    raw_decimals: lane.output_decimals,
                }
                .id()
            })
            .collect::<Result<Vec<_>>>()
        {
            Ok(ids) => ids,
            Err(_) => {
                return ApiReply::error(
                    503,
                    "STOCKMESH_PRODUCT_POLICY",
                    "A product identity could not be committed.",
                )
            }
        };
        let mut intent_id_hash = Sha256::new();
        intent_id_hash.update(b"SKEW_STOCKMESH_QUOTE_INTENT_V1\0");
        intent_id_hash.update(request.quote_id.as_bytes());
        intent_id_hash.update(request.owner.as_bytes());
        intent_id_hash.update(owner_nonce.to_le_bytes());
        let intent_id: [u8; 32] = intent_id_hash.finalize().into();
        let input_mint = if cached.intent.input_symbol == "USDC" {
            USDC_MINT
        } else {
            WSOL_MINT
        };
        let frozen = FrozenIntent {
            intent_id,
            owner: request.owner.clone(),
            owner_nonce,
            instrument: cached.intent.instrument.clone(),
            input_mint: input_mint.into(),
            input_atoms: cached.input,
            minimum_exposure_q32: u128::from(cached.minimum_exposure_q32),
            admitted_product_ids,
            product_policy_hash: match decode_hex_32(&cached.product_policy_set_hash) {
                Ok(hash) => hash,
                Err(_) => {
                    return ApiReply::error(
                        503,
                        "STOCKMESH_PRODUCT_POLICY",
                        "The product policy set hash is invalid.",
                    )
                }
            },
            world_generation_hash: publication.snapshot.hash,
            deadline_slot,
        };
        let commitment = match frozen.commitment(publication.snapshot.slot) {
            Ok(commitment) => commitment,
            Err(_) => {
                return ApiReply::error(
                    422,
                    "STOCKMESH_FREEZE_INVALID",
                    "The frozen economic intent violates a commitment bound.",
                )
            }
        };
        ApiReply::ok(json!({
            "schema":"skew.stockmesh.frozen-intent/v1",
            "quoteId":request.quote_id,
            "intentCommitment":hex(&commitment),
            "candidateAllocation":{
                "schema":"skew.stockmesh.native-allocation/v1",
                "stateSlot":cached.slot.to_string(),
                "bankHash":cached.snapshot_hash,
                "legs":cached.native_allocation.iter().map(|leg| leg.json()).collect::<Vec<_>>(),
                "status":"PROPOSAL_REQUIRES_ACCOUNT_BINDING_AND_EXACT_SIMULATION"
            },
            "executionCandidateGraph":{
                "schema":"skew.stockmesh.native-candidates/v1",
                "edges":cached.native_candidates.iter().map(|leg| leg.json()).collect::<Vec<_>>(),
                "split":"RECOMPUTED_ONCHAIN_AFTER_FUNDING_AND_EACH_CPI"
            },
            "intent":{
                "intentId":hex(&frozen.intent_id),
                "owner":frozen.owner,
                "ownerNonce":frozen.owner_nonce.to_string(),
                "instrument":frozen.instrument,
                "inputMint":frozen.input_mint,
                "inputAtoms":frozen.input_atoms.to_string(),
                "minimumExposureQ32":frozen.minimum_exposure_q32.to_string(),
                "admittedProductIds":frozen.admitted_product_ids.iter().map(|id| hex(id)).collect::<Vec<_>>(),
                "productPolicyHash":hex(&frozen.product_policy_hash),
                "worldGenerationHash":hex(&frozen.world_generation_hash),
                "deadlineSlot":frozen.deadline_slot.to_string()
            },
            "authorization":{
                "ownerSignatureRequired":true,
                "serverSigned":false,
                "executorBidMustBind":["intentCommitment","worldGenerationHash","transactionMessageHash"]
            }
        }))
    }

    fn quote_id(
        &self,
        intent: &IntentKey,
        input: u64,
        exposure_q32: u64,
        snapshot: [u8; 32],
        product_policy_set_hash: &str,
    ) -> String {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let mut hash = Sha256::new();
        hash.update(b"SKEW_STOCKMESH_EXPOSURE_QUOTE_V2\0");
        hash.update(self.secret);
        hash.update(sequence.to_le_bytes());
        hash.update(intent.instrument.as_bytes());
        hash.update([0]);
        hash.update(intent.input_symbol.as_bytes());
        hash.update(input.to_le_bytes());
        hash.update(exposure_q32.to_le_bytes());
        hash.update(snapshot);
        hash.update(product_policy_set_hash.as_bytes());
        let digest = hash.finalize();
        format!("stkq_{}", hex(&digest[..16]))
    }

    fn sell_quote_id(
        &self,
        intent: &IntentKey,
        product_id: &str,
        input: u64,
        output: u64,
        snapshot: [u8; 32],
        product_policy_hash: &str,
    ) -> String {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let mut hash = Sha256::new();
        hash.update(b"SKEW_STOCKMESH_LIQUIDATION_QUOTE_V1\0");
        hash.update(self.secret);
        hash.update(sequence.to_le_bytes());
        hash.update(intent.instrument.as_bytes());
        hash.update([0]);
        hash.update(intent.input_symbol.as_bytes());
        hash.update(product_id.as_bytes());
        hash.update(input.to_le_bytes());
        hash.update(output.to_le_bytes());
        hash.update(snapshot);
        hash.update(product_policy_hash.as_bytes());
        let digest = hash.finalize();
        format!("stkq_{}", hex(&digest[..16]))
    }

    fn cache(&self, id: String, value: CachedQuote) {
        if let Ok(mut cache) = self.quotes.lock() {
            while cache.order.len() >= MAX_QUOTES {
                if let Some(old) = cache.order.pop_front() {
                    cache.values.remove(&old);
                }
            }
            cache.order.push_back(id.clone());
            cache.values.insert(id, value);
        }
    }

    fn cache_sell(&self, id: String, value: CachedSellQuote) {
        if let Ok(mut cache) = self.sell_quotes.lock() {
            while cache.order.len() >= MAX_QUOTES {
                if let Some(old) = cache.order.pop_front() {
                    cache.values.remove(&old);
                }
            }
            cache.order.push_back(id.clone());
            cache.values.insert(id, value);
        }
    }
}

fn venue_heap_pressure(venue: crate::market::Venue) -> u8 {
    use crate::market::Venue;
    match venue {
        Venue::ByrealClmm => 0,
        Venue::RaydiumClmm => 1,
        Venue::MeteoraDlmm => 2,
        Venue::OrcaWhirlpool => 3,
    }
}

/// Produce a bounded, deterministic exact-simulation sequence. The full
/// Global Marginal Book stays first. If compiling every substitute curve in
/// one SBF invocation exceeds CU or heap, later variants retain every economic
/// pair while selecting one physical venue for each interchangeable pair.
fn resource_admission_variants(
    proposals: &[crate::world::NativeSwapProposal],
) -> Result<Vec<Vec<crate::world::NativeSwapProposal>>> {
    if proposals.is_empty() || proposals.len() > stocklana_adapters::graph::MAX_CANDIDATES {
        return Err("resource admission candidate bound".into());
    }
    type Group = (usize, Option<String>, String, String);
    let mut groups = BTreeMap::<Group, Vec<usize>>::new();
    let mut base = BTreeSet::new();
    for (index, proposal) in proposals.iter().enumerate() {
        // Funding legs are a fixed, input-conserving split, not alternative
        // residual quotes. Dropping one silently loses part of the root input.
        if proposal.product_id.is_none() {
            base.insert(index);
            continue;
        }
        groups
            .entry((
                proposal.stage,
                proposal.product_id.clone(),
                proposal.market.input_mint.clone(),
                proposal.market.output_mint.clone(),
            ))
            .or_default()
            .push(index);
    }
    let mut interchangeable = Vec::new();
    for indices in groups.values_mut() {
        indices.sort_by_key(|index| {
            let proposal = &proposals[*index];
            (
                venue_heap_pressure(proposal.market.venue),
                proposal.market.tick_arrays.len(),
                proposal.market.pool.as_str(),
            )
        });
        base.insert(indices[0]);
        if indices.len() > 1 {
            interchangeable.push(indices.clone());
        }
    }
    let build = |selected: &BTreeSet<usize>| {
        proposals
            .iter()
            .enumerate()
            .filter(|(index, _)| selected.contains(index))
            .map(|(_, proposal)| proposal.clone())
            .collect::<Vec<_>>()
    };
    let mut variants = vec![proposals.to_vec()];
    if interchangeable.is_empty() {
        return Ok(variants);
    }
    variants.push(build(&base));
    'groups: for group in interchangeable {
        for alternative in group.into_iter().skip(1) {
            if variants.len() == MAX_RESOURCE_ADMISSION_VARIANTS {
                break 'groups;
            }
            let mut selected = base.clone();
            let base_member = selected
                .iter()
                .copied()
                .find(|index| {
                    let chosen = &proposals[*index];
                    let alternate = &proposals[alternative];
                    chosen.stage == alternate.stage
                        && chosen.product_id == alternate.product_id
                        && chosen.market.input_mint == alternate.market.input_mint
                        && chosen.market.output_mint == alternate.market.output_mint
                })
                .ok_or("resource admission base group")?;
            selected.remove(&base_member);
            selected.insert(alternative);
            variants.push(build(&selected));
        }
    }
    Ok(variants)
}

fn exact_resource_admission<T, F>(
    variants: Vec<Vec<crate::world::NativeSwapProposal>>,
    mut prepare: F,
) -> Result<T>
where
    F: FnMut(&[crate::world::NativeSwapProposal]) -> Result<T>,
{
    let variant_count = variants.len();
    let mut rejected = 0usize;
    for variant in variants {
        match prepare(&variant) {
            Ok(candidate) => return Ok(candidate),
            // Packet/lock limits reject a candidate before simulation. Try the
            // same bounded economic alternatives rather than aborting the whole
            // stock order. Policy, freshness, wallet and nonce failures are not
            // resource failures and must never enter this retry path.
            Err(error)
                if matches!(error.as_str(),
                    "economic simulation resource rejected"
                    | "economic simulation candidate rejected"
                    | "v0 transaction packet bound"
                    | "v0 account lock bound") =>
            {
                rejected += 1;
            }
            Err(error) => return Err(error),
        }
    }
    Err(format!(
        "exact execution resource admission rejected {rejected}/{variant_count} variants"
    ))
}

#[cfg(test)]
fn validate_native_allocation(legs: &[crate::world::NativeSwapProposal], input: u64) -> Result<()> {
    if legs.is_empty() || legs.len() > MAX_LEGS || input == 0 {
        return Err("native allocation bounds".into());
    }
    let stages = legs.last().ok_or("native allocation empty")?.stage;
    if !(1..=2).contains(&stages)
        || legs.iter().any(|leg| !(1..=stages).contains(&leg.stage))
        || legs.windows(2).any(|pair| pair[0].stage > pair[1].stage)
    {
        return Err("native allocation stage order".into());
    }
    let mut expected_input = input;
    let mut bridge_mint = None;
    let mut pools = BTreeSet::new();
    for stage in 1..=stages {
        let group = legs
            .iter()
            .filter(|leg| leg.stage == stage)
            .collect::<Vec<_>>();
        let first = group.first().ok_or("native allocation missing stage")?;
        if bridge_mint.is_some_and(|mint: &str| mint != first.market.input_mint) {
            return Err("native allocation bridge mint".into());
        }
        let mut spent = 0u64;
        let mut produced = 0u64;
        for leg in &group {
            if leg.input_atoms == 0
                || leg.expected_output_atoms == 0
                || leg.input_atoms > MAX_ATOMS
                || leg.market.program != leg.market.venue.program()
                || leg.market.input_mint != first.market.input_mint
                || leg.market.input_mint == leg.market.output_mint
                || !pools.insert(&leg.market.pool)
                || if stage == stages {
                    leg.product_id.as_ref().is_none_or(|id| id.is_empty())
                } else {
                    leg.product_id.is_some()
                }
                || (stage != stages && leg.market.output_mint != first.market.output_mint)
            {
                return Err("native allocation market/product binding".into());
            }
            spent = spent
                .checked_add(leg.input_atoms)
                .ok_or("native allocation input overflow")?;
            if stage != stages {
                produced = produced
                    .checked_add(leg.expected_output_atoms)
                    .ok_or("native allocation bridge overflow")?;
            }
        }
        if spent != expected_input {
            return Err("native allocation exact input conservation".into());
        }
        expected_input = produced;
        bridge_mint = Some(first.market.output_mint.as_str());
    }
    Ok(())
}

fn solve_exposure(
    publication: &Publication,
    intent: &IntentKey,
    input: u64,
) -> Result<ExposurePlan> {
    let lanes = publication
        .layout
        .lanes
        .iter()
        .filter(|lane| {
            lane.key.instrument == intent.instrument && lane.key.input_symbol == intent.input_symbol
        })
        .collect::<Vec<_>>();
    if lanes.is_empty() {
        return Err("economic intent has no product lane".into());
    }
    let (funding_prefix, _) = intent_execution_shape(&lanes)?;
    let policy_set_hash = product_policy_set_hash(&lanes);
    let mut bridge_candidates = vec![(input, Vec::new(), Vec::new(), 0u64, 0u64, 0u64)];
    if funding_prefix == 1 {
        let worlds = publication
            .worlds
            .get(&lanes[0].key)
            .ok_or("bridge world unavailable")?;
        let bridge = worlds.first().ok_or("bridge world unavailable")?;
        let proposal = bridge.quote(input)?;
        if proposal["budgetExhausted"] == true {
            return Err("bridge search budget".into());
        }
        bridge_candidates.clear();
        for (output, legs) in proposal_options(&proposal) {
            if legs.len() >= MAX_LEGS {
                continue;
            }
            let route = route_stage(legs, input, 1)?;
            let allocation = legs
                .iter()
                .map(|leg| {
                    bridge.proposal_leg(
                        leg["pool"].as_str().ok_or("bridge pool")?,
                        leg["inputAtoms"].as_u64().ok_or("bridge exact input")?,
                        leg["outputAtoms"].as_u64().ok_or("bridge exact output")?,
                        1,
                        None,
                    )
                })
                .collect::<Result<Vec<_>>>()?;
            bridge_candidates.push((
                output,
                route,
                allocation,
                proposal["oracleCalls"].as_u64().unwrap_or(0),
                proposal["nativeEvaluations"].as_u64().unwrap_or(0),
                proposal["planningNs"].as_u64().unwrap_or(0),
            ));
        }
        if bridge_candidates.is_empty() {
            return Err("bridge runtime bound".into());
        }
    }

    let mut best: Option<ExposurePlan> = None;
    for (
        final_input,
        bridge_route,
        mut bridge_allocation,
        bridge_calls,
        bridge_evaluations,
        bridge_ns,
    ) in bridge_candidates
    {
        let remaining_legs = MAX_LEGS.saturating_sub(bridge_route.len());
        let Ok(mut candidate) = solve_final_exposure(
            publication,
            &lanes,
            final_input,
            funding_prefix,
            remaining_legs,
            policy_set_hash.clone(),
        ) else {
            continue;
        };
        let mut route = bridge_route;
        route.append(&mut candidate.route);
        candidate.route = route;
        let mut execution_candidates = bridge_allocation.clone();
        bridge_allocation.append(&mut candidate.native_allocation);
        candidate.native_allocation = bridge_allocation;
        execution_candidates.append(&mut candidate.native_candidates);
        candidate.native_candidates = execution_candidates;
        if funding_prefix == 1 {
            let funding = candidate
                .native_allocation
                .iter()
                .filter(|proposal| proposal.stage == 1)
                .collect::<Vec<_>>();
            if funding.is_empty()
                || funding.iter().any(|proposal| proposal.product_id.is_some())
                || funding.iter().try_fold(0u64, |total, proposal| {
                    total.checked_add(proposal.input_atoms)
                }) != Some(input)
            {
                return Err("funding allocation conservation".into());
            }
        }
        candidate.oracle_calls = candidate.oracle_calls.saturating_add(bridge_calls);
        candidate.native_evaluations = candidate
            .native_evaluations
            .saturating_add(bridge_evaluations);
        candidate.planning_ns = candidate.planning_ns.saturating_add(bridge_ns);
        if best.as_ref().is_none_or(|prior| {
            candidate.exposure_q32 > prior.exposure_q32
                || (candidate.exposure_q32 == prior.exposure_q32
                    && candidate.route.len() < prior.route.len())
        }) {
            best = Some(candidate);
        }
    }
    best.ok_or_else(|| "no bounded exposure allocation".into())
}

mod sell_route;

fn solve_sell(publication: &Publication, lane: &Lane, input: u64) -> Result<SellPlan> {
    if input == 0 || input > MAX_ATOMS || lane.configs.is_empty() || lane.configs.len() > 3 {
        return Err("sell input bound".into());
    }
    let started = Instant::now();
    let mut route = Vec::new();
    let mut native_allocation = Vec::new();
    let worlds = lane.configs.iter().rev().map(|config| {
        let reversed = config.reversed();
        let first = reversed.markets.first().ok_or("sell world")?;
        reversed.compile_admitted_pair(
            &publication.snapshot,
            None,
            &first.input_mint,
            &first.output_mint,
        )
    }).collect::<Result<Vec<_>>>()?;
    let selected = sell_route::choose(worlds.len(), input, |stage, amount| worlds[stage].quote(amount))?;
    for (position, choice) in selected.stages.iter().enumerate() {
        let world = &worlds[position];
        let legs = &choice.legs;
        let stage = position + 1;
        for leg in legs {
            let pool = leg["pool"].as_str().ok_or("sell route pool")?;
            let leg_input = leg["inputAtoms"].as_u64().ok_or("sell route input")?;
            let leg_output = leg["outputAtoms"].as_u64().ok_or("sell route output")?;
            native_allocation.push(world.proposal_leg(
                pool,
                leg_input,
                leg_output,
                // A reverse sale is one economic graph. Hop depth belongs in
                // the mint DAG/display route, not BUY's funding-stage label.
                1,
                Some(lane.key.product_id.clone()),
            )?);
        }
        let mut display = route_stage(legs, choice.input, stage)?;
        for leg in &mut display {
            leg["productId"] = json!(lane.key.product_id);
            leg["issuer"] = json!(lane.issuer);
            leg["outputMint"] = json!(native_allocation.last().ok_or("sell allocation")?.market.output_mint);
        }
        route.append(&mut display);
    }
    let stage_input = selected.stages.last().ok_or("sell allocation")?.output;
    if native_allocation.is_empty() || native_allocation.len() > MAX_LEGS || stage_input == 0 {
        return Err("sell allocation bound".into());
    }
    Ok(SellPlan {
        output_atoms: stage_input,
        route,
        native_allocation,
        maximum_cu: lane.maximum_cu,
        planning_ns: started.elapsed().as_nanos() as u64,
        oracle_calls: selected.oracle_calls,
        native_evaluations: selected.native_evaluations,
    })
}

#[derive(Clone, Copy)]
struct GlobalStep<'a> {
    world: &'a World,
    world_edge: usize,
}

struct GlobalPath<'a> {
    lane: &'a Lane,
    steps: Vec<GlobalStep<'a>>,
    scaled: ScaledUiAmount,
}

struct QuotedStep<'a> {
    step: GlobalStep<'a>,
    input: u64,
    output: u64,
}

struct QuotedPath<'a> {
    raw: u64,
    steps: Vec<QuotedStep<'a>>,
}

fn quote_global_path<'a>(path: &GlobalPath<'a>, input: u64) -> Result<QuotedPath<'a>> {
    if input == 0 {
        return Err("economic path input".into());
    }
    let mut amount = input;
    let mut steps = Vec::with_capacity(path.steps.len());
    for step in &path.steps {
        let output = step.world.quote_edge(step.world_edge, amount)?;
        if output == 0 {
            return Err("economic path zero output".into());
        }
        steps.push(QuotedStep {
            step: *step,
            input: amount,
            output,
        });
        amount = output;
    }
    Ok(QuotedPath { raw: amount, steps })
}

fn product_for_output<'a>(lanes: &'a [&Lane], output_mint: &str) -> Result<&'a Lane> {
    let mut matches = lanes
        .iter()
        .copied()
        .filter(|lane| lane.output_mint == output_mint);
    let lane = matches.next().ok_or("economic path output product")?;
    if matches.next().is_some() {
        return Err("economic path ambiguous output product".into());
    }
    Ok(lane)
}

fn step_proposal(
    lanes: &[&Lane],
    step: &QuotedStep,
    proposal_stage: usize,
) -> Result<crate::world::NativeSwapProposal> {
    let (_, pool) = step.step.world.edge_identity(step.step.world_edge)?;
    let bare = step
        .step
        .world
        .proposal_leg(pool, step.input, step.output, proposal_stage, None)?;
    let destination = product_for_output(lanes, &bare.market.output_mint)?;
    step.step.world.proposal_leg(
        pool,
        step.input,
        step.output,
        proposal_stage,
        Some(destination.key.product_id.clone()),
    )
}

/// Carry the complete bounded physical graph into opcode 14/18 Economic
/// Reflow. Seed amounts prove every native quote and dependency against this
/// bank; the runtime recomputes the split after each observed CPI.
fn bounded_execution_candidates(
    publication: &Publication,
    lanes: &[&Lane],
    paths: &[GlobalPath],
    selected_inputs: &[u64],
    input: u64,
    proposal_stage: usize,
) -> Result<Vec<crate::world::NativeSwapProposal>> {
    if paths.len() != selected_inputs.len() || input == 0 || !matches!(proposal_stage, 1 | 2) {
        return Err("economic candidate bounds".into());
    }
    let mut chosen = BTreeMap::<String, crate::world::NativeSwapProposal>::new();
    for selected_only in [true, false] {
        for (index, path) in paths.iter().enumerate() {
            if selected_only != (selected_inputs[index] > 0) {
                continue;
            }
            let mut seed = if selected_inputs[index] > 0 {
                selected_inputs[index]
            } else {
                input
            };
            let quoted = loop {
                if let Ok(value) = quote_global_path(path, seed) {
                    break value;
                }
                if seed == 1 {
                    return Err("economic candidate has no positive bounded seed".into());
                }
                seed = (seed / 2).max(1);
            };
            for step in &quoted.steps {
                let proposal = step_proposal(lanes, step, proposal_stage)?;
                let pool = proposal.market.pool.clone();
                match chosen.get(&pool) {
                    Some(prior)
                        if prior.market.input_mint != proposal.market.input_mint
                            || prior.market.output_mint != proposal.market.output_mint =>
                    {
                        return Err("one pool appears with incompatible economic pairs".into())
                    }
                    Some(_) => {}
                    None => {
                        chosen.insert(pool, proposal);
                    }
                }
            }
        }
    }
    let chosen = chosen.into_values().collect::<Vec<_>>();
    let cash_mint = paths
        .first()
        .and_then(|path| path.steps.first())
        .ok_or("economic candidate cash path")?
        .world
        .proposal_leg(
            paths[0].steps[0]
                .world
                .edge_identity(paths[0].steps[0].world_edge)?
                .1,
            1,
            1,
            proposal_stage,
            None,
        )?
        .market
        .input_mint;
    let direct = chosen
        .iter()
        .filter(|proposal| proposal.market.input_mint == cash_mint)
        .map(|proposal| proposal.market.output_mint.as_str())
        .collect::<BTreeSet<_>>();
    if chosen.is_empty()
        || chosen.len() > stocklana_adapters::graph::MAX_CANDIDATES
        || chosen
            .iter()
            .any(|proposal| proposal.stage != proposal_stage)
        || chosen
            .iter()
            .filter_map(|proposal| proposal.product_id.as_deref())
            .collect::<BTreeSet<_>>()
            .len()
            != lanes
                .iter()
                .map(|lane| lane.key.product_id.as_str())
                .collect::<BTreeSet<_>>()
                .len()
        || chosen.iter().any(|proposal| {
            proposal.market.input_mint != cash_mint
                && !direct.contains(proposal.market.input_mint.as_str())
        })
    {
        return Err("economic candidate final coverage".into());
    }
    for proposal in &chosen {
        native_wire::execution_dependencies(&proposal.market, &publication.snapshot)?;
    }
    Ok(chosen)
}

fn solve_final_exposure(
    publication: &Publication,
    lanes: &[&Lane],
    input: u64,
    funding_prefix: usize,
    maximum_legs: usize,
    product_policy_set_hash: String,
) -> Result<ExposurePlan> {
    if maximum_legs == 0 {
        return Err("final runtime bound".into());
    }
    let mut paths = Vec::new();
    let mut product_scales = BTreeMap::new();
    let mut namespace = Sha256::new();
    namespace.update(b"SKEW_STOCKMESH_GLOBAL_EXPOSURE_V2\0");
    namespace.update(publication.snapshot.hash);
    namespace.update(product_policy_set_hash.as_bytes());
    for lane in lanes {
        let worlds = publication
            .worlds
            .get(&lane.key)
            .ok_or("product world unavailable")?;
        let residual = worlds
            .get(funding_prefix..)
            .filter(|worlds| matches!(worlds.len(), 1 | 2))
            .ok_or("product residual world unavailable")?;
        let scaled = product_scaled_amount(lane, &publication.snapshot)?;
        for world in residual {
            namespace.update(world.cache_namespace());
        }
        namespace.update(lane.key.product_id.as_bytes());
        product_scales.insert(lane.key.product_id.clone(), scaled);
        for first in 0..residual[0].edge_count() {
            if residual.len() == 1 {
                paths.push(GlobalPath {
                    lane,
                    steps: vec![GlobalStep {
                        world: residual[0].as_ref(),
                        world_edge: first,
                    }],
                    scaled,
                });
            } else {
                for second in 0..residual[1].edge_count() {
                    paths.push(GlobalPath {
                        lane,
                        steps: vec![
                            GlobalStep {
                                world: residual[0].as_ref(),
                                world_edge: first,
                            },
                            GlobalStep {
                                world: residual[1].as_ref(),
                                world_edge: second,
                            },
                        ],
                        scaled,
                    });
                }
            }
        }
    }
    if paths.is_empty() || paths.len() > MAX_EDGES {
        return Err("global path bound".into());
    }
    let mut memo = QuoteMemo::default();
    memo.begin(namespace.finalize().into());
    let started = Instant::now();
    let plan = skew_engine::optimizer::oracle::refine_bounded_legs(
        paths.len(),
        input,
        2_048,
        maximum_legs as u64,
        maximum_legs.min(paths.len()),
        |index, amount| {
            memo.quote(index, amount, || {
                let path = &paths[index];
                let raw = quote_global_path(path, amount)
                    .map(|quote| quote.raw)
                    .map_err(|_| skew_native::Error::Capacity)?;
                path.scaled.exposure_q32(
                    raw,
                    path.lane.exposure_numerator,
                    path.lane.exposure_denominator,
                    path.lane.conservative_bps,
                )
            })
            .map(|output| (output, paths[index].steps.len() as u64))
            .map_err(|_| skew_engine::Error::Capacity)
        },
    )
    .map_err(|error| format!("exposure allocation: {error:?}"))?;
    if plan.exhausted {
        return Err("exposure search budget".into());
    }
    let selected = plan
        .inputs
        .iter()
        .take(paths.len())
        .filter(|amount| **amount > 0)
        .count();
    if selected == 0 || plan.cost > maximum_legs as u64 {
        return Err("exposure leg bound".into());
    }
    let shares = route_shares(&plan.inputs[..paths.len()], input)?;
    let mut route = Vec::with_capacity(plan.cost as usize);
    let mut native_allocation = Vec::with_capacity(plan.cost as usize);
    let mut raw_by_product = BTreeMap::<String, u64>::new();
    let mut exposure_by_product = BTreeMap::<String, u64>::new();
    let mut checked_exposure = 0u64;
    let proposal_stage = if funding_prefix == 1 { 2 } else { 1 };
    for (index, path) in paths.iter().enumerate() {
        let amount = plan.inputs[index];
        if amount == 0 {
            continue;
        }
        let quoted = quote_global_path(path, amount)?;
        let exposure = path
            .scaled
            .exposure_q32(
                quoted.raw,
                path.lane.exposure_numerator,
                path.lane.exposure_denominator,
                path.lane.conservative_bps,
            )
            .map_err(|error| format!("scaled UI exposure: {error:?}"))?;
        checked_exposure = checked_exposure
            .checked_add(exposure)
            .ok_or("exposure sum")?;
        let raw_total = raw_by_product
            .entry(path.lane.key.product_id.clone())
            .or_default();
        *raw_total = raw_total.checked_add(quoted.raw).ok_or("raw product sum")?;
        let exposure_total = exposure_by_product
            .entry(path.lane.key.product_id.clone())
            .or_default();
        *exposure_total = exposure_total
            .checked_add(exposure)
            .ok_or("product exposure sum")?;
        for (hop, step) in quoted.steps.iter().enumerate() {
            let (venue, market) = step.step.world.edge_identity(step.step.world_edge)?;
            let proposal = step_proposal(lanes, step, proposal_stage)?;
            let destination = product_for_output(lanes, &proposal.market.output_mint)?;
            native_allocation.push(proposal);
            route.push(json!({
                "venue":venue_name(venue)?,
                "market":market,
                "shareBps":shares[index],
                "inputAtoms":step.input.to_string(),
                "expectedOutputAtoms":step.output.to_string(),
                "stage":funding_prefix + hop + 1,
                "path":index,
                "productId":destination.key.product_id,
                "issuer":destination.issuer,
                "outputMint":destination.output_mint,
                "outputSymbol":destination.output_symbol,
                "terminalProductId":path.lane.key.product_id
            }));
        }
    }
    if checked_exposure != plan.output {
        return Err("exposure allocation conservation".into());
    }
    let products = lanes
        .iter()
        .map(|lane| {
            let scaled = product_scales[&lane.key.product_id];
            ExposureProduct {
                product_id: lane.key.product_id.clone(),
                issuer: lane.issuer.clone(),
                output_symbol: lane.output_symbol.clone(),
                output_mint: lane.output_mint.clone(),
                token_program: lane.token_program.clone(),
                output_decimals: lane.output_decimals,
                raw_output_atoms: raw_by_product
                    .get(&lane.key.product_id)
                    .copied()
                    .unwrap_or(0),
                exposure_q32: exposure_by_product
                    .get(&lane.key.product_id)
                    .copied()
                    .unwrap_or(0),
                multiplier_q32: scaled.multiplier_q32,
                conservative_bps: lane.conservative_bps,
                rights_hash: lane.rights_hash.clone(),
                product_policy_hash: lane.product_policy_hash.clone(),
                backing_model: lane.backing_model.clone(),
                redemption_model: lane.redemption_model.clone(),
                transfer_model: lane.transfer_model.clone(),
                exposure_model: lane.exposure_model.clone(),
            }
        })
        .collect();
    let global_reflow =
        funding_prefix == 1 || lanes.len() > 1 || paths.iter().any(|path| path.steps.len() == 2);
    let native_candidates = if global_reflow {
        bounded_execution_candidates(
            publication,
            lanes,
            &paths,
            &plan.inputs[..paths.len()],
            input,
            proposal_stage,
        )?
    } else {
        native_allocation.clone()
    };
    Ok(ExposurePlan {
        exposure_q32: plan.output,
        route,
        native_allocation,
        native_candidates,
        products,
        stage_count: funding_prefix
            + paths
                .iter()
                .map(|path| path.steps.len())
                .max()
                .ok_or("economic path depth")?,
        global_reflow,
        maximum_cu: lanes
            .iter()
            .map(|lane| lane.maximum_cu)
            .min()
            .ok_or("CU policy")?,
        oracle_calls: u64::from(plan.oracle_calls),
        native_evaluations: memo.evaluations,
        planning_ns: started.elapsed().as_nanos() as u64,
        product_policy_set_hash,
    })
}

fn product_scaled_amount(lane: &Lane, snapshot: &Snapshot) -> Result<ScaledUiAmount> {
    let mint = snapshot
        .accounts
        .iter()
        .find(|account| account.key == lane.output_mint)
        .ok_or("product mint policy account missing")?;
    if mint.owner != lane.token_program
        || mint.executable
        || mint.data.get(44).copied() != Some(lane.output_decimals)
    {
        return Err("product mint policy mismatch".into());
    }
    let clock = snapshot
        .accounts
        .iter()
        .find(|account| account.key == "SysvarC1ock11111111111111111111111111111111")
        .ok_or("product clock policy account missing")?;
    let timestamp = i64::from_le_bytes(
        clock
            .data
            .get(32..40)
            .ok_or("product clock layout")?
            .try_into()
            .map_err(|_| "product clock layout")?,
    );
    match lane.exposure_model.as_str() {
        "TOKEN_2022_SCALED_UI" => {
            if lane.token_program != TOKEN_2022_PROGRAM {
                return Err("scaled UI requires Token-2022".into());
            }
            let scaled = ScaledUiAmount::decode(&mint.data, timestamp)
                .map_err(|error| format!("scaled UI product policy: {error:?}"))?;
            let activation_distance = i128::from(timestamp)
                .checked_sub(i128::from(scaled.next_multiplier_effective_timestamp))
                .ok_or("scaled UI activation arithmetic")?
                .abs();
            if scaled.next_multiplier_effective_timestamp > 0 && activation_distance <= 900 {
                return Err("scaled UI corporate action guard".into());
            }
            Ok(scaled)
        }
        "FIXED_RATIONAL" => Ok(ScaledUiAmount {
            decimals: lane.output_decimals,
            multiplier_q32: 1u64 << 32,
            next_multiplier_effective_timestamp: i64::MAX,
            next_multiplier_q32: 1u64 << 32,
        }),
        _ => Err("unknown exposure policy".into()),
    }
}

fn product_policy_set_hash(lanes: &[&Lane]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"SKEW_STOCKMESH_PRODUCT_POLICY_SET_V2\0");
    for lane in lanes {
        hash.update(lane.key.product_id.as_bytes());
        hash.update([0]);
        hash.update(lane.product_policy_hash.as_bytes());
        hash.update([0]);
    }
    hex(&hash.finalize())
}

fn route_shares(inputs: &[u64], total: u64) -> Result<Vec<u64>> {
    let nonzero = inputs.iter().filter(|input| **input > 0).count();
    if nonzero == 0
        || total == 0
        || inputs
            .iter()
            .try_fold(0u64, |sum, input| sum.checked_add(*input))
            != Some(total)
    {
        return Err("route share conservation".into());
    }
    let distributable = 10_000u64
        .checked_sub(nonzero as u64)
        .ok_or("route share count")?;
    let mut shares = vec![0u64; inputs.len()];
    let mut assigned = 0u64;
    let mut largest = 0usize;
    for (index, input) in inputs.iter().enumerate() {
        if *input == 0 {
            continue;
        }
        if inputs[index] > inputs[largest] {
            largest = index;
        }
        shares[index] =
            1 + (u128::from(*input) * u128::from(distributable) / u128::from(total)) as u64;
        assigned = assigned.checked_add(shares[index]).ok_or("route shares")?;
    }
    shares[largest] = shares[largest]
        .checked_add(10_000u64.checked_sub(assigned).ok_or("route shares")?)
        .ok_or("route shares")?;
    Ok(shares)
}

fn revalidate_output(worlds: &[Arc<World>], input: u64) -> Result<u64> {
    let mut stage_input = input;
    let mut legs = 0usize;
    for world in worlds {
        let proposal = world.quote(stage_input)?;
        if proposal["budgetExhausted"] == true {
            return Err("revalidation search budget".into());
        }
        let stage_legs = proposal["legs"].as_array().ok_or("revalidation legs")?;
        if stage_legs.is_empty() || legs + stage_legs.len() > 4 {
            return Err("revalidation runtime bound".into());
        }
        legs += stage_legs.len();
        stage_input = proposal["outputAtoms"]
            .as_u64()
            .filter(|output| *output > 0)
            .ok_or("revalidation output")?;
    }
    Ok(stage_input)
}

fn derive_sol_lot(worlds: &[Arc<World>], stock_lot: u64) -> Result<(u64, u64, u64)> {
    let mut candidate = 1_000_000_000u64;
    let mut best: Option<(u64, u64, u64)> = None;
    for _ in 0..8 {
        let output = revalidate_output(worlds, candidate)?;
        let difference = output.abs_diff(stock_lot);
        if best.is_none_or(|(_, _, prior)| difference < prior) {
            best = Some((candidate, output, difference));
        }
        let numerator = u128::from(candidate)
            .checked_mul(u128::from(stock_lot))
            .ok_or("SOL lot arithmetic")?;
        let next = numerator
            .div_ceil(u128::from(output))
            .clamp(1, u128::from(MAX_ATOMS));
        let next = u64::try_from(next).map_err(|_| "SOL lot range")?;
        if next == candidate {
            break;
        }
        candidate = next;
    }
    let (candidate, output, difference) = best.ok_or("SOL lot unavailable")?;
    let error_bps = u64::try_from(
        u128::from(difference)
            .checked_mul(10_000)
            .ok_or("SOL lot error")?
            .div_ceil(u128::from(stock_lot)),
    )
    .map_err(|_| "SOL lot error range")?;
    Ok((candidate, output, error_bps))
}

fn stockmesh_status(
    ready: usize,
    required: usize,
    deployment_ready: bool,
    captured_fixture: bool,
) -> (&'static str, Option<&'static str>) {
    if ready == required && deployment_ready {
        ("ready", None)
    } else if ready == required {
        (
            "degraded",
            Some(if captured_fixture {
                "Captured coherent quote banks are ready; settlement deployment is not bound."
            } else {
                "Live coherent quote banks are ready; settlement deployment is not bound."
            }),
        )
    } else if ready > 0 {
        (
            "degraded",
            Some("One or more coherent liquidity banks are unavailable."),
        )
    } else {
        (
            "unavailable",
            Some("No coherent liquidity bank is current."),
        )
    }
}

fn read_portfolio(
    banks: &[Arc<Bank>],
    catalog: &Catalog,
    rpc: &Rpc,
    owner: &str,
) -> Result<(u64, Vec<(PortfolioProduct, u64)>, Value, Vec<portfolio_discovery::DiscoveredHolding>)> {
    let mut products = BTreeMap::<String, PortfolioProduct>::new();
    for bank in banks {
        let layout = bank.layout()?;
        for lane in &layout.lanes {
            let product = PortfolioProduct {
                instrument: lane.key.instrument.clone(),
                product_id: lane.key.product_id.clone(),
                issuer: lane.issuer.clone(),
                symbol: lane.output_symbol.clone(),
                mint: lane.output_mint.clone(),
                token_program: lane.token_program.clone(),
                raw_decimals: lane.output_decimals,
            };
            if products
                .insert(product.mint.clone(), product.clone())
                .is_some_and(|prior| prior != product)
            {
                return Err("portfolio product identity conflict".into());
            }
        }
    }
    let catalog_products = catalog.products.iter()
        .filter(|p| p.chain == format!("solana:{MAINNET_GENESIS}"))
        .map(|p| (p.address.clone(), p.clone())).collect::<BTreeMap<_,_>>();
    const WALLET_USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    // Cash is a wallet balance, not a newly admitted stock execution product.
    products.insert(WALLET_USDC.into(), PortfolioProduct {
        instrument: "USDC".into(), product_id: "wallet-cash".into(), issuer: "Circle".into(),
        symbol: "USDC".into(), mint: WALLET_USDC.into(), token_program: TOKEN_PROGRAM.into(), raw_decimals: 6,
    });
    let mut token_programs = products
        .values()
        .map(|product| product.token_program.clone())
        .collect::<BTreeSet<_>>();
    if !catalog_products.is_empty() { token_programs.extend([TOKEN_PROGRAM.to_string(), TOKEN_2022_PROGRAM.to_string()]); }
    if token_programs.is_empty()
        || token_programs.len() > 2
        || token_programs
            .iter()
            .any(|program| !matches!(program.as_str(), TOKEN_PROGRAM | TOKEN_2022_PROGRAM))
    {
        return Err("portfolio token program bound".into());
    }
    let mut state_slot = 0u64;
    let mut usdc_slot = 0u64;
    let mut balances = BTreeMap::<String, u64>::new();
    let mut other_holdings = BTreeMap::new();
    // At most two admitted token programs are queried. Keep these reads
    // sequential: public and entry-tier RPCs commonly apply a method bucket,
    // and a two-request burst made otherwise valid wallet reads intermittently
    // fail with 429.
    for token_program in token_programs {
        let response = portfolio_rpc_call(rpc, owner, &token_program)?;
        let program_slot = portfolio_token_balances(
            &response,
            owner,
            &token_program,
            &products,
            &mut balances,
        )?;
        portfolio_discovery::collect(&response, owner, &token_program, &catalog_products, &products, &mut other_holdings)?;
        if token_program == TOKEN_PROGRAM { usdc_slot = program_slot; }
        state_slot = state_slot.max(program_slot);
    }
    if state_slot == 0 {
        return Err("portfolio state slot".into());
    }
    let native = portfolio_read_retry(
        || rpc.call("getBalance", json!([owner, {"commitment":"confirmed","minContextSlot":state_slot}])),
        std::thread::sleep,
    )?;
    let sol_atoms = native["value"].as_u64().ok_or("native wallet balance")?;
    let sol_slot = native["context"]["slot"].as_u64().filter(|s| *s >= state_slot).ok_or("native wallet balance slot")?;
    let usdc_atoms = balances.remove(WALLET_USDC).unwrap_or(0);
    let cash = json!([
        {"symbol":"SOL","atoms":sol_atoms.to_string(),"decimals":9,"stateSlot":sol_slot},
        {"symbol":"USDC","atoms":usdc_atoms.to_string(),"decimals":6,"stateSlot":usdc_slot}
    ]);
    let mut holdings = balances
        .into_iter()
        .filter_map(|(mint, atoms)| {
            (atoms > 0).then(|| products.get(&mint).cloned().map(|product| (product, atoms)))?
        })
        .collect::<Vec<_>>();
    holdings.sort_by(|left, right| {
        left.0
            .instrument
            .cmp(&right.0.instrument)
            .then_with(|| left.0.symbol.cmp(&right.0.symbol))
    });
    Ok((state_slot, holdings, cash, other_holdings.into_values().collect()))
}

fn portfolio_rpc_call(rpc: &Rpc, owner: &str, token_program: &str) -> Result<Value> {
    portfolio_read_retry(
        || rpc.call("getTokenAccountsByOwner", json!([owner,{"programId":token_program},{"encoding":"jsonParsed","commitment":"confirmed"}])),
        std::thread::sleep,
    )
}

// Wallet reads share the actual provider quota with state refresh. A transient
// local permit denial is not an empty wallet or a permanent balance failure.
// Never retry exhausted/expired budgets, malformed data or arbitrary methods.
fn portfolio_read_retry(
    mut call: impl FnMut() -> Result<Value>,
    mut wait: impl FnMut(Duration),
) -> Result<Value> {
    let delays = [0u64, 225, 675, 1_575];
    for (attempt, delay) in delays.into_iter().enumerate() {
        if delay > 0 {
            wait(Duration::from_millis(delay));
        }
        match call() {
            Ok(response) => return Ok(response),
            Err(error)
                if matches!(error.as_str(), "RPC HTTP status 429" | "provider rate limit reached" | "provider in-flight limit reached")
                    && attempt + 1 < delays.len() => {}
            Err(error) => return Err(error),
        }
    }
    Err("portfolio RPC retry bound".into())
}

fn portfolio_token_balances(
    response: &Value,
    owner: &str,
    token_program: &str,
    products: &BTreeMap<String, PortfolioProduct>,
    balances: &mut BTreeMap<String, u64>,
) -> Result<u64> {
    let slot = response["context"]["slot"]
        .as_u64()
        .filter(|slot| *slot > 0)
        .ok_or("portfolio context slot")?;
    let accounts = response["value"]
        .as_array().filter(|v| v.len() <= 8192)
        .ok_or("portfolio token accounts")?;
    let mut seen = BTreeSet::new();
    for value in accounts {
        let account = value.get("account").ok_or("portfolio token account")?;
        if account["owner"].as_str() != Some(token_program) {
            return Err("portfolio token account program".into());
        }
        let info = &account["data"]["parsed"]["info"];
        let mint = info["mint"].as_str().ok_or("portfolio token mint")?;
        let Some(product) = products.get(mint) else {
            continue;
        };
        let address = value["pubkey"].as_str().ok_or("portfolio token account address")?;
        decode_key(address)?;
        if product.token_program != token_program
            || info["owner"].as_str() != Some(owner) || !seen.insert(address)
            || info["tokenAmount"]["decimals"].as_u64() != Some(u64::from(product.raw_decimals))
        {
            return Err("portfolio admitted token identity".into());
        }
        let atoms = info["tokenAmount"]["amount"]
            .as_str()
            .ok_or("portfolio token amount")?
            .parse::<u64>()
            .map_err(|_| "portfolio token amount range")?;
        let next = balances
            .get(mint)
            .copied()
            .unwrap_or(0)
            .checked_add(atoms)
            .ok_or("portfolio token balance overflow")?;
        balances.insert(mint.to_string(), next);
    }
    Ok(slot)
}

fn rotation_group(needs_rotation: &[bool], cursor: usize) -> usize {
    if needs_rotation.is_empty() { return 0; }
    let start = cursor % needs_rotation.len();
    (0..needs_rotation.len()).map(|offset| (start + offset) % needs_rotation.len())
        .find(|index| needs_rotation[*index]).unwrap_or(start)
}

fn refresh_superbank(
    banks: Vec<Arc<Bank>>,
    rpc_url: String,
    portfolio_rpc: Arc<RwLock<Option<Rpc>>>,
    direct_state: Option<Arc<DirectState>>,
    slot_signal: Option<Arc<SlotSignal>>,
    bank_activity: Arc<BankActivity>,
    initial_rpc: Option<Rpc>,
) {
    let bounded_provider = initial_rpc
        .as_ref()
        .is_some_and(|reader| reader.provider_metrics().is_some());
    let mut rpc = initial_rpc;
    let mut rpc_startup_failures = 0u32;
    let mut last_rpc_attempt: Option<Instant> = None;
    let mut consecutive_failures = [0u32; MAX_REFRESH_GROUPS];
    let mut proactive_group_cursor = 0usize;
    let mut cold_cursor = 0usize;
    loop {
        let began = Instant::now();
        let direct_required = direct_state
            .as_ref()
            .is_some_and(|state| state.mode().forbids_rpc());
        if rpc.is_none() && !direct_required {
            let retry_ms = 500u64
                .saturating_mul(1u64 << rpc_startup_failures.saturating_sub(1).min(5))
                .min(16_000);
            let due = last_rpc_attempt
                .is_none_or(|attempt| attempt.elapsed() >= Duration::from_millis(retry_ms));
            if due {
                last_rpc_attempt = Some(Instant::now());
                match Rpc::pinned_live_feed(rpc_url.clone(), MAINNET_GENESIS.into()) {
                    Ok(verified) => {
                        match verified.relaxed_read_clone() {
                            Ok(portfolio_reader) => {
                                if let Ok(mut published) = portfolio_rpc.write() {
                                    *published = Some(portfolio_reader);
                                }
                            }
                            Err(error) => {
                                eprintln!("StockMesh portfolio RPC client rejected: {error}");
                            }
                        }
                        rpc = Some(verified);
                        rpc_startup_failures = 0;
                    }
                    Err(error) => {
                        rpc_startup_failures = rpc_startup_failures.saturating_add(1);
                        if rpc_startup_failures == 1 || rpc_startup_failures.is_power_of_two() {
                            eprintln!(
                                "StockMesh fallback RPC rejected ({rpc_startup_failures} consecutive): {error}"
                            );
                        }
                    }
                }
            }
        }
        // Rotation can replace a bank's account horizon. Rebuild the packing
        // before every coherent read so no worker retains a pre-rotation union.
        let hot_banks = bank_activity.active();
        if bounded_provider && hot_banks.is_empty() {
            // A catalog page is not demand for 128 fresh execution banks.
            // After the last quote lease expires, park the paid reader. A
            // new quote marks its bank and resumes within this short tick.
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        let mut groups = match superbank_groups_prioritized(&banks, &hot_banks) {
            Ok(groups) => groups,
            Err(error) => {
                eprintln!("StockMesh superbank regrouping rejected: {error}");
                if let Some(delay) = Duration::from_millis(250).checked_sub(began.elapsed()) {
                    std::thread::sleep(delay);
                }
                continue;
            }
        };
        if bounded_provider && banks.len() > PROVIDER_WARM_BANKS {
            let selected = bank_activity.bounded_active(&mut cold_cursor);
            groups.retain(|indexes| indexes.iter().any(|index| selected.contains(index)));
        }
        let group_count = groups.len();
        let watch_keys = direct_watch_keys(&banks);
        let direct_candidate = direct_state.as_ref().and_then(|state| {
            watch_keys
                .as_ref()
                .ok()
                .and_then(|keys| state.snapshot(keys).ok())
        });
        if direct_state
            .as_ref()
            .is_some_and(|state| state.mode().uses_direct_quotes())
        {
            let direct_outcome = match direct_candidate.as_ref() {
                Some(snapshot) => groups
                    .iter()
                    .try_for_each(|indexes| refresh_direct_group_once(&banks, snapshot, indexes)),
                None => Err("direct state unavailable or warming".into()),
            };
            match direct_outcome {
                Ok(()) => {
                    if let Some(state) = direct_state.as_ref() {
                        state.record_direct_publish();
                    }
                    consecutive_failures.fill(0);
                    if let Some(delay) = Duration::from_millis(100).checked_sub(began.elapsed()) {
                        std::thread::sleep(delay);
                    }
                    continue;
                }
                Err(error) => {
                    if let Some(state) = direct_state.as_ref() {
                        if state.mode().forbids_rpc() {
                            state.record_direct_unavailable(&error);
                            for bank in &banks {
                                bank.invalidate();
                            }
                            if let Some(delay) =
                                Duration::from_millis(100).checked_sub(began.elapsed())
                            {
                                std::thread::sleep(delay);
                            }
                            continue;
                        }
                        state.record_rpc_fallback(&error);
                    }
                }
            }
        }
        let Some(rpc) = rpc.as_ref() else {
            for bank in &banks {
                bank.invalidate();
            }
            if let Some(state) = direct_state.as_ref() {
                state.record_rpc_fallback("fallback RPC unavailable");
            }
            if let Some(delay) = Duration::from_millis(250).checked_sub(began.elapsed()) {
                std::thread::sleep(delay);
            }
            continue;
        };
        // Sustain at most 2.5 coherent account reads per second regardless of
        // the admitted bank count. With five markets this is one refresh per
        // bank every two seconds; adding markets increases cold-bank latency
        // instead of exceeding the public provider envelope.
        let cadence = Duration::from_millis(
            u64::try_from(group_count)
                .unwrap_or(MAX_REFRESH_GROUPS as u64)
                .saturating_mul(400)
                .max(800),
        );
        let proactive_group = if bounded_provider {
            // Spend the single rotation permit on a selected, unavailable
            // market before cold maintenance. Large catalogs must not make a
            // user's newly selected stock wait an entire universe sweep.
            let needs_rotation: Vec<bool> = groups.iter().map(|indexes| indexes.iter()
                .any(|index| hot_banks.contains(index) && banks[*index].current().is_err())).collect();
            // A permanently broken first hot bank must not consume every
            // rotation permit while later selected stocks remain cold.
            rotation_group(&needs_rotation, proactive_group_cursor)
        } else {
            proactive_group_cursor % group_count
        };
        let outcome_groups = groups.clone();
        let outcomes = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(group_count);
            for (group_index, bank_indexes) in groups.into_iter().enumerate() {
                let group_rpc = rpc.clone();
                let group_banks = &banks;
                let allow_rotation = group_index == proactive_group;
                let stagger = Duration::from_millis(
                    u64::try_from(cadence.as_millis()).unwrap_or(750)
                        * u64::try_from(group_index).unwrap_or(0)
                        / u64::try_from(group_count).unwrap_or(1),
                );
                handles.push((
                    group_index,
                    scope.spawn(move || {
                        if !stagger.is_zero() {
                            std::thread::sleep(stagger);
                        }
                        refresh_superbank_group_once(
                            group_banks,
                            &group_rpc,
                            &bank_indexes,
                            allow_rotation,
                        )
                    }),
                ));
            }
            handles
                .into_iter()
                .map(|(group_index, handle)| {
                    (
                        group_index,
                        handle
                            .join()
                            .unwrap_or_else(|_| Err("refresh worker panicked".into())),
                    )
                })
                .collect::<Vec<_>>()
        });
        proactive_group_cursor = proactive_group.wrapping_add(1);
        let mut successful_groups = 0usize;
        for (group_index, outcome) in outcomes {
            match outcome {
                Ok(response) => {
                    consecutive_failures[group_index] = 0;
                    successful_groups += 1;
                    if let (Some(state), Some(snapshot)) =
                        (direct_state.as_ref(), direct_candidate.as_ref())
                    {
                        if state.mode() == DirectMode::Shadow {
                            if let Ok(keys) = group_watch_keys(&banks, &outcome_groups[group_index])
                            {
                                if response["context"]["slot"].as_u64() == Some(snapshot.slot) {
                                    if let Ok(direct_response) = snapshot.response_for(&keys) {
                                        state.record_shadow(
                                            response["value"] == direct_response["value"],
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                Err(error) => {
                    let failures = &mut consecutive_failures[group_index];
                    *failures = failures.saturating_add(1);
                    // A transient miss does not erase a still-current bank.
                    // Groups remain evenly staggered inside the bounded 750 ms
                    // cycle instead of synchronizing retries into an RPC burst.
                    if *failures == 1 || failures.is_power_of_two() {
                        eprintln!(
                            "StockMesh superbank group {group_index} refresh rejected ({failures} consecutive): {error}"
                        );
                    }
                }
            }
        }
        for failures in consecutive_failures.iter_mut().skip(group_count) {
            *failures = 0;
        }
        // When every coherent read is rejected, back off the whole refresh
        // cycle instead of hammering a throttled provider. Any successful
        // group keeps the normal hot-path cadence so healthy banks stay live.
        let cycle_cadence = if successful_groups == 0 {
            let failures = consecutive_failures
                .iter()
                .take(group_count)
                .copied()
                .min()
                .unwrap_or(1);
            cadence
                .checked_mul(1u32 << failures.saturating_sub(1).min(5))
                .unwrap_or(Duration::from_secs(24))
        } else {
            cadence
        };
        if let Some(signal) = slot_signal.as_ref() {
            // Coalesce notifications received while RPC reads were in flight.
            // Replaying that backlog would create a permanently busy loop on
            // a chain whose slots advance faster than a full multi-group
            // refresh.
            let observed_signal = signal.sequence();
            if let Some(delay) = cycle_cadence.checked_sub(began.elapsed()) {
                let _ = signal.wait_for_change(observed_signal, delay);
            }
        } else if let Some(delay) = cycle_cadence.checked_sub(began.elapsed()) {
            std::thread::sleep(delay);
        }
    }
}

fn direct_watch_keys(banks: &[Arc<Bank>]) -> Result<Vec<String>> {
    let keys = banks
        .iter()
        .map(|bank| bank.layout())
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flat_map(|layout| layout.feed.keys().to_vec())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if keys.is_empty() || keys.len() > 256 {
        return Err("direct watch horizon exceeds 256 accounts".into());
    }
    Ok(keys)
}

fn group_watch_keys(banks: &[Arc<Bank>], bank_indexes: &[usize]) -> Result<Vec<String>> {
    let mut keys = BTreeSet::new();
    for bank_index in bank_indexes {
        let bank = banks.get(*bank_index).ok_or("refresh bank index")?;
        keys.extend(bank.layout()?.feed.keys().iter().cloned());
    }
    if keys.is_empty() || keys.len() > 100 {
        return Err("coherent account group bound after rotation".into());
    }
    Ok(keys.into_iter().collect())
}

fn superbank_groups_prioritized(
    banks: &[Arc<Bank>],
    hot_banks: &BTreeSet<usize>,
) -> Result<Vec<Vec<usize>>> {
    let bank_keys = banks
        .iter()
        .enumerate()
        .map(|(bank_index, bank)| {
            bank.layout().map(|layout| {
                (
                    bank_index,
                    layout.feed.keys().iter().cloned().collect::<BTreeSet<_>>(),
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    prioritize_superbank_key_sets(bank_keys, hot_banks)
}

fn prioritize_superbank_key_sets(
    mut bank_keys: Vec<(usize, BTreeSet<String>)>,
    hot_banks: &BTreeSet<usize>,
) -> Result<Vec<Vec<usize>>> {
    if bank_keys.is_empty() || bank_keys.len() > MAX_REFRESH_GROUPS {
        return Err("StockMesh quote bank count exceeds refresh group bound".into());
    }
    if bank_keys
        .iter()
        .any(|(_, keys)| keys.is_empty() || keys.len() > 100)
    {
        return Err("single coherent bank account bound".into());
    }
    // Do not union independent products. Dependency rotation can increase a
    // previously admissible union beyond getMultipleAccounts' 100-key bound,
    // which used to strand every bank in that group. Hot banks go first in the
    // same bounded, evenly staggered request schedule.
    bank_keys.sort_by(|left, right| {
        hot_banks
            .contains(&right.0)
            .cmp(&hot_banks.contains(&left.0))
            .then_with(|| left.0.cmp(&right.0))
    });
    Ok(bank_keys
        .into_iter()
        .map(|(bank_index, _)| vec![bank_index])
        .collect())
}

#[cfg(test)]
fn pack_superbank_key_sets(
    mut bank_keys: Vec<(usize, BTreeSet<String>)>,
) -> Result<Vec<Vec<usize>>> {
    // Larger horizons are placed first. Among admissible groups prefer the
    // greatest overlap and then the fullest union; this reduces both group
    // count and duplicated RPC accounts while remaining deterministic.
    bank_keys.sort_by(|left, right| {
        right
            .1
            .len()
            .cmp(&left.1.len())
            .then_with(|| left.0.cmp(&right.0))
    });
    let mut groups: Vec<(Vec<usize>, BTreeSet<String>)> = Vec::new();
    for (bank_index, keys) in bank_keys {
        if keys.is_empty() || keys.len() > 100 {
            return Err("single coherent bank account bound".into());
        }
        let destination = groups
            .iter()
            .enumerate()
            .filter_map(|(group_index, (_, group_keys))| {
                let union_size = group_keys.union(&keys).count();
                (union_size <= 100).then(|| {
                    let added = keys.difference(group_keys).count();
                    (added, 100usize.saturating_sub(union_size), group_index)
                })
            })
            .min()
            .map(|(_, _, group_index)| group_index);
        if let Some(group) = destination {
            groups[group].0.push(bank_index);
            groups[group].1.extend(keys);
        } else {
            groups.push((vec![bank_index], keys));
        }
    }
    if groups.is_empty() {
        return Err("StockMesh superbank has no account groups".into());
    }
    if groups.len() > MAX_REFRESH_GROUPS {
        return Err("StockMesh quote dependency horizon exceeds refresh group bound".into());
    }
    Ok(groups.into_iter().map(|(indexes, _)| indexes).collect())
}

fn refresh_direct_group_once(
    banks: &[Arc<Bank>],
    snapshot: &DirectSnapshot,
    bank_indexes: &[usize],
) -> Result<()> {
    let keys = group_watch_keys(banks, bank_indexes)?;
    let response = snapshot.response_for(&keys)?;
    let index = key_index(&keys);
    let mut failures = Vec::new();
    for bank_index in bank_indexes {
        let bank = Arc::clone(banks.get(*bank_index).ok_or("direct bank index")?);
        let layout = match bank.layout() {
            Ok(layout) => layout,
            Err(error) => {
                failures.push(format!("{}: {error}", bank.name));
                continue;
            }
        };
        if let Err(error) = publish_bank(&bank, &index, &response) {
            failures.push(format!("{}: {error}", bank.name));
            continue;
        }
        let rotation_due = snapshot.slot
            > bank
                .last_rotation_slot
                .load(Ordering::Acquire)
                .saturating_add(32);
        if rotation_due && layout_needs_rotation(&layout, &index, &response)? {
            failures.push(format!(
                "{}: direct dependency horizon requires rotation",
                bank.name
            ));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "direct bank refresh failures: {}",
            failures.join("; ")
        ))
    }
}

fn refresh_superbank_group_once(
    banks: &[Arc<Bank>],
    rpc: &Rpc,
    bank_indexes: &[usize],
    allow_rotation: bool,
) -> Result<Value> {
    let keys = group_watch_keys(banks, bank_indexes)?;
    let minimum_slot = bank_indexes
        .iter()
        .map(|bank_index| banks[*bank_index].last_slot())
        .max()
        .unwrap_or(0);
    let response = rpc.call(
        "getMultipleAccounts",
        json!([keys,{"encoding":"base64","commitment":"confirmed","minContextSlot":minimum_slot}]),
    )?;
    let index = key_index(&keys);
    let slot = response["context"]["slot"]
        .as_u64()
        .ok_or("superbank slot")?;
    let mut failures = Vec::new();
    let mut urgent_rotations = Vec::new();
    let mut proactive_rotations = Vec::new();

    // Publish the common response into every bank before issuing any
    // secondary rotation RPC. A slow probe must not age out banks that
    // were already present in this coherent group response.
    for bank_index in bank_indexes {
        let bank = Arc::clone(&banks[*bank_index]);
        let layout = match bank.layout() {
            Ok(layout) => layout,
            Err(error) => {
                failures.push(format!("{}: {error}", bank.name));
                continue;
            }
        };
        match publish_bank(&bank, &index, &response) {
            Ok(()) => {
                let rotation_due = slot
                    > bank
                        .last_rotation_slot
                        .load(Ordering::Acquire)
                        .saturating_add(32);
                if rotation_due {
                    match layout_needs_rotation(&layout, &index, &response) {
                        Ok(true) => proactive_rotations.push((bank, layout)),
                        Ok(false) => {}
                        Err(error) => {
                            bank.rotation_failures.fetch_add(1, Ordering::AcqRel);
                            eprintln!(
                                "StockMesh bank {} rotation horizon rejected after base publication: {error}",
                                bank.name
                            );
                        }
                    }
                }
            }
            Err(first_error) => urgent_rotations.push((bank, layout, first_error)),
        }
    }

    // A bank whose active arrays no longer compile requires immediate
    // rotation. Other banks in the group are already current while these
    // recovery RPCs run.
    let mut rotation_consumed = false;
    for (bank, layout, first_error) in urgent_rotations {
        if allow_rotation && !rotation_consumed {
            rotation_consumed = true;
            bank.last_rotation_slot.store(slot, Ordering::Release);
            if let Err(rotate_error) = rotate_bank(&bank, rpc, &layout, &index, &response) {
                bank.rotation_failures.fetch_add(1, Ordering::AcqRel);
                failures.push(format!(
                    "{}: {first_error}; rotation: {rotate_error}",
                    bank.name
                ));
            }
        }
        // Other stale layouts stay invalid and are retried when their group
        // receives the single rotation permit in a later cycle. Treating a
        // deliberate deferral as provider failure would exponentially back
        // off healthy coherent reads and starve the warm-up sequence.
    }

    // Bound maintenance work to one proactive rotation per group refresh.
    if allow_rotation && !rotation_consumed {
        if let Some((bank, layout)) = proactive_rotations.into_iter().next() {
            bank.last_rotation_slot.store(slot, Ordering::Release);
            if let Err(error) = rotate_bank(&bank, rpc, &layout, &index, &response) {
                bank.rotation_failures.fetch_add(1, Ordering::AcqRel);
                eprintln!(
                    "StockMesh bank {} proactive rotation rejected after base publication: {error}",
                    bank.name
                );
            }
        }
    }
    if failures.is_empty() {
        Ok(response)
    } else {
        Err(format!("bank refresh failures: {}", failures.join("; ")))
    }
}

fn publish_bank(bank: &Arc<Bank>, index: &BTreeMap<String, usize>, response: &Value) -> Result<()> {
    let context = response.get("context").ok_or("superbank context")?;
    let values = response["value"].as_array().ok_or("superbank values")?;
    if values.len() != index.len() {
        return Err("superbank account count".into());
    }
    let layout = bank.layout()?;
    let projected: Vec<&Value> = layout
        .feed
        .keys()
        .iter()
        .map(|key| {
            index
                .get(key)
                .and_then(|position| values.get(*position))
                .ok_or_else(|| "superbank projection".to_string())
        })
        .collect::<Result<_>>()?;
    let bank_response = json!({"context":context,"value":projected});
    bank.publish_value(&bank_response)
        .map_err(|error| format!("bank {} compile: {error}", bank.name))
}

fn layout_needs_rotation(
    layout: &BankLayout,
    index: &BTreeMap<String, usize>,
    response: &Value,
) -> Result<bool> {
    for lane in &layout.lanes {
        for config in &lane.configs {
            for market in &config.markets {
                let pool = response_data(response, index, &market.pool)?;
                if market.array_horizon_needs_rotation(&pool)? {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn rotate_bank(
    bank: &Arc<Bank>,
    rpc: &Rpc,
    layout: &Arc<BankLayout>,
    source_index: &BTreeMap<String, usize>,
    source: &Value,
) -> Result<()> {
    let source_slot = source["context"]["slot"]
        .as_u64()
        .ok_or("rotation source slot")?;
    let mut candidates_by_pool = BTreeMap::new();
    for lane in &layout.lanes {
        for config in &lane.configs {
            for market in &config.markets {
                if candidates_by_pool.contains_key(&market.pool) {
                    continue;
                }
                let pool = response_data(source, source_index, &market.pool)?;
                candidates_by_pool.insert(
                    market.pool.clone(),
                    (
                        market.active_array_ordinal(&pool)?,
                        market.dynamic_array_candidates(&pool)?,
                    ),
                );
            }
        }
    }
    let candidate_keys: Vec<String> = candidates_by_pool
        .values()
        .flat_map(|(_, candidates)| candidates.iter().map(|(_, key)| key.clone()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if candidate_keys.is_empty() || candidate_keys.len() > 100 {
        return Err("rotation candidate account bound".into());
    }
    let probe = rpc.call(
        "getMultipleAccounts",
        json!([candidate_keys,{"encoding":"base64","commitment":"confirmed","minContextSlot":source_slot}]),
    )?;
    let probe_slot = probe["context"]["slot"]
        .as_u64()
        .ok_or("rotation probe slot")?;
    let probe_values = probe["value"].as_array().ok_or("rotation probe values")?;
    if probe_values.len() != candidate_keys.len() {
        return Err("rotation probe account count".into());
    }
    let present: BTreeSet<&str> = candidate_keys
        .iter()
        .zip(probe_values)
        .filter_map(|(key, value)| (!value.is_null()).then_some(key.as_str()))
        .collect();
    let mut lanes = layout.lanes.clone();
    for lane in &mut lanes {
        for config in &mut lane.configs {
            for market in &mut config.markets {
                let (active, candidates) = candidates_by_pool
                    .get(&market.pool)
                    .ok_or("rotation pool candidate")?;
                market.tick_arrays = crate::market::select_contiguous_array_horizon(
                    candidates, *active, &present,
                    market.array_capacity.map_or(market.tick_arrays.len(), usize::from),
                )?;
            }
        }
    }
    let keys: Vec<String> = lanes
        .iter()
        .flat_map(|lane| lane.configs.iter())
        .map(WorldConfig::quote_keys)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if keys.is_empty() || keys.len() > 100 {
        return Err("rotated coherent bank account bound".into());
    }
    let final_response = rpc.call(
        "getMultipleAccounts",
        json!([keys,{"encoding":"base64","commitment":"confirmed","minContextSlot":probe_slot}]),
    )?;
    let next = Arc::new(BankLayout {
        feed: Arc::new(Feed::new(keys, bank.max_age, 4 * 1024 * 1024)?),
        lanes,
    });
    bank.publish_layout(next, &final_response, true)
        .map_err(|error| format!("rotated final bank rejected: {error}"))?;
    let rotations = bank.rotations.fetch_add(1, Ordering::AcqRel) + 1;
    eprintln!(
        "StockMesh bank {} dependency horizon rotated at slot {} ({} total)",
        bank.name, probe_slot, rotations
    );
    Ok(())
}

fn response_data(response: &Value, index: &BTreeMap<String, usize>, key: &str) -> Result<Vec<u8>> {
    let position = *index.get(key).ok_or("response account index")?;
    let value = response["value"]
        .as_array()
        .and_then(|values| values.get(position))
        .ok_or("response account value")?;
    if value.is_null() {
        return Err("response account missing".into());
    }
    STANDARD
        .decode(value["data"][0].as_str().ok_or("response account data")?)
        .map_err(|error| error.to_string())
}

fn key_index(keys: &[String]) -> BTreeMap<String, usize> {
    keys.iter()
        .enumerate()
        .map(|(position, key)| (key.clone(), position))
        .collect()
}

fn validate_lane(
    lane: &LaneManifest,
    configs: &[WorldConfig],
    policy_integrity_required: bool,
) -> Result<()> {
    if lane.output_decimals > 12
        || lane.maximum_cu == 0
        || lane.maximum_cu > 1_400_000
        || manifest_product_id(lane).as_deref() != Some(lane.product_id.as_str())
        || lane.issuer.is_empty()
        || lane.issuer.len() > 64
        || crate::onebook_wire::issuer_id(&lane.issuer).is_err()
        || crate::onebook_wire::instrument_id(&lane.instrument).is_err()
        || lane.output_symbol.is_empty()
        || lane.output_symbol.len() > 20
        || decode_key(&lane.output_mint).is_err()
        || !matches!(
            lane.token_program.as_str(),
            TOKEN_PROGRAM | TOKEN_2022_PROGRAM
        )
        || !valid_hash(&lane.rights_hash)
        || !valid_rights_document(lane)
        || !valid_hash(&lane.product_policy_hash)
        || (policy_integrity_required && !valid_product_policy_document(lane))
        || !matches!(
            lane.backing_model.as_str(),
            "ONE_TO_ONE_UNDERLYING" | "CASH_VALUE_TRACKER"
        )
        || !matches!(
            lane.redemption_model.as_str(),
            "KYC_PRIMARY_OR_SECONDARY" | "SECONDARY_ONLY"
        )
        || !matches!(
            lane.transfer_model.as_str(),
            "PERMISSIONLESS_JURISDICTION_RESTRICTED" | "POLICY_GATED"
        )
        || !matches!(
            lane.exposure_model.as_str(),
            "TOKEN_2022_SCALED_UI" | "FIXED_RATIONAL"
        )
        || lane.exposure_numerator == 0
        || lane.exposure_numerator > 1_000_000_000
        || lane.exposure_denominator == 0
        || lane.exposure_denominator > 1_000_000_000
        || lane.conservative_bps == 0
        || lane.conservative_bps > 10_000
        || !matches!(lane.input_symbol.as_str(), "USDC" | "SOL")
        || configs.is_empty()
        || configs.len() > 3
    {
        return Err("invalid StockMesh lane metadata".into());
    }
    let mut expected_input = if lane.input_symbol == "USDC" {
        USDC_MINT
    } else {
        WSOL_MINT
    };
    for (stage, config) in configs.iter().enumerate() {
        if config.markets.is_empty() {
            return Err("empty StockMesh world".into());
        }
        let expected_output = config.markets[0].output_mint.as_str();
        if expected_output == expected_input {
            return Err("StockMesh stage cannot be an identity pair".into());
        }
        for market in &config.markets {
            let final_stage = stage + 1 == configs.len();
            let valid = market.input_mint == expected_input
                && market.output_mint == expected_output
                && (!final_stage || market.output_mint == lane.output_mint);
            if !valid {
                return Err("lane economic identity does not match venue pair".into());
            }
        }
        expected_input = expected_output;
    }
    Ok(())
}

fn valid_rights_document(lane: &LaneManifest) -> bool {
    let Some(document) = lane.rights_document.as_object() else {
        return false;
    };
    let common = BTreeSet::from([
        "schema",
        "issuer",
        "legalClassification",
        "backingModel",
        "redemptionModel",
        "transferModel",
        "legalOverview",
        "mechanics",
        "multiplierSpecification",
        "legalDocuments",
        "underlyingSymbol",
    ]);
    let schema = document.get("schema").and_then(Value::as_str);
    let identity_valid = match schema {
        Some("skew.stockmesh.rights/v1") => {
            let mut expected = common.clone();
            expected.extend(["productIsin", "underlyingIsin"]);
            document.keys().map(String::as_str).collect::<BTreeSet<_>>() == expected
                && document
                    .get("productIsin")
                    .and_then(Value::as_str)
                    .is_some_and(valid_isin)
                && document
                    .get("underlyingIsin")
                    .and_then(Value::as_str)
                    .is_some_and(valid_isin)
        }
        Some("skew.stockmesh.rights/v2") => {
            let mut expected = common.clone();
            expected.extend([
                "productIdentifier",
                "productIdentifierScheme",
                "productIdentifierStatus",
                "underlyingIdentifier",
                "underlyingIdentifierScheme",
                "underlyingIdentifierStatus",
            ]);
            let product_scheme = document
                .get("productIdentifierScheme")
                .and_then(Value::as_str);
            let product_identifier = document.get("productIdentifier").and_then(Value::as_str);
            let product_status = document
                .get("productIdentifierStatus")
                .and_then(Value::as_str);
            let product_valid = match product_scheme {
                Some("ISIN") => {
                    product_identifier.is_some_and(valid_isin)
                        && product_status == Some("VERIFIED_ISSUER_SOURCE")
                }
                Some("NONE") => {
                    product_identifier == Some("") && product_status == Some("ISSUER_NOT_PUBLISHED")
                }
                _ => false,
            };
            let underlying_scheme = document
                .get("underlyingIdentifierScheme")
                .and_then(Value::as_str);
            let underlying_identifier =
                document.get("underlyingIdentifier").and_then(Value::as_str);
            let underlying_status = document
                .get("underlyingIdentifierStatus")
                .and_then(Value::as_str);
            document.keys().map(String::as_str).collect::<BTreeSet<_>>() == expected
                && product_valid
                && underlying_scheme == Some("ISIN")
                && underlying_identifier.is_some_and(valid_isin)
                && underlying_status == Some("VERIFIED_REFERENCE_SECURITY")
        }
        Some("skew.stockmesh.rights/v3") => {
            let mut expected = common.clone();
            expected.extend([
                "productIdentifier",
                "productIdentifierScheme",
                "productIdentifierStatus",
                "underlyingIdentifier",
                "underlyingIdentifierScheme",
                "underlyingIdentifierStatus",
            ]);
            document.keys().map(String::as_str).collect::<BTreeSet<_>>() == expected
                && document
                    .get("productIdentifierScheme")
                    .and_then(Value::as_str)
                    == Some("SOLANA_MINT")
                && document.get("productIdentifier").and_then(Value::as_str)
                    == Some(lane.output_mint.as_str())
                && document
                    .get("productIdentifierStatus")
                    .and_then(Value::as_str)
                    == Some("VERIFIED_ISSUER_SOURCE")
                && document
                    .get("underlyingIdentifierScheme")
                    .and_then(Value::as_str)
                    == Some("PRIVATE_COMPANY_REFERENCE")
                && document.get("underlyingIdentifier").and_then(Value::as_str)
                    == Some(lane.instrument.as_str())
                && document
                    .get("underlyingIdentifierStatus")
                    .and_then(Value::as_str)
                    == Some("ISSUER_SPV_REFERENCE")
        }
        _ => false,
    };
    if !identity_valid
        || document.get("issuer").and_then(Value::as_str) != Some(lane.issuer.as_str())
        || document.get("backingModel").and_then(Value::as_str) != Some(lane.backing_model.as_str())
        || document.get("redemptionModel").and_then(Value::as_str)
            != Some(lane.redemption_model.as_str())
        || document.get("transferModel").and_then(Value::as_str)
            != Some(lane.transfer_model.as_str())
        || document.get("underlyingSymbol").and_then(Value::as_str)
            != Some(lane.instrument.as_str())
    {
        return false;
    }
    if !document
        .get("legalClassification")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty() && value.len() <= 128)
    {
        return false;
    }
    for key in [
        "legalOverview",
        "mechanics",
        "multiplierSpecification",
        "legalDocuments",
    ] {
        if !document
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|value| value.starts_with("https://") && value.len() <= 512)
        {
            return false;
        }
    }
    serde_json::to_vec(&lane.rights_document)
        .ok()
        .filter(|bytes| !bytes.is_empty() && bytes.len() <= 8 * 1024)
        .is_some_and(|bytes| hex(&Sha256::digest(bytes)) == lane.rights_hash)
}

fn valid_product_policy_document(lane: &LaneManifest) -> bool {
    let Some(document) = lane.product_policy_document.as_object() else {
        return false;
    };
    if document.get("schema").and_then(Value::as_str) != Some("skew.stockmesh.product-policy/v2")
        || !document
            .get("source")
            .and_then(Value::as_str)
            .is_some_and(|value| value.starts_with("https://") && value.len() <= 1_024)
        || document.get("symbol").and_then(Value::as_str) != Some(lane.output_symbol.as_str())
        || document.get("instrument").and_then(Value::as_str) != Some(lane.instrument.as_str())
        || document.get("mint").and_then(Value::as_str) != Some(lane.output_mint.as_str())
        || document.get("exposureModel").and_then(Value::as_str)
            != Some(lane.exposure_model.as_str())
        || document.get("exposureNumerator").and_then(Value::as_u64)
            != Some(lane.exposure_numerator)
        || document.get("exposureDenominator").and_then(Value::as_u64)
            != Some(lane.exposure_denominator)
        || document.get("conservativeBps").and_then(Value::as_u64)
            != Some(u64::from(lane.conservative_bps))
        || document.get("rights") != Some(&lane.rights_document)
        || document.get("supportedExecution").and_then(Value::as_str) != Some("SECONDARY_ONLY")
        || !matches!(
            document.get("primaryMintRedeem").and_then(Value::as_str),
            Some("ISSUER_PRIMARY_NOT_INTEGRATED")
                | Some("ISSUER_ATTESTATION_REQUIRED_NOT_INTEGRATED")
        )
    {
        return false;
    }
    let eligibility = document.get("executionEligibility").and_then(Value::as_str);
    if !matches!(
        (lane.transfer_model.as_str(), eligibility),
        (
            "PERMISSIONLESS_JURISDICTION_RESTRICTED",
            Some("JURISDICTION_POLICY_REQUIRED")
        ) | (
            "POLICY_GATED",
            Some("JURISDICTION_AND_ISSUER_ATTESTATION_REQUIRED")
        )
    ) {
        return false;
    }
    let Some(evidence) = document.get("sourceEvidence").and_then(Value::as_object) else {
        return false;
    };
    if evidence.is_empty() || evidence.len() > 16 {
        return false;
    }
    if evidence.iter().any(|(name, source)| {
        let Some(source) = source.as_object() else {
            return true;
        };
        name.is_empty()
            || name.len() > 32
            || source.keys().map(String::as_str).collect::<BTreeSet<_>>()
                != BTreeSet::from(["bytes", "sha256", "url"])
            || !source
                .get("url")
                .and_then(Value::as_str)
                .is_some_and(|value| value.starts_with("https://") && value.len() <= 1_024)
            || !source
                .get("sha256")
                .and_then(Value::as_str)
                .is_some_and(valid_hash)
            || !source
                .get("bytes")
                .and_then(Value::as_u64)
                .is_some_and(|bytes| (1..=8 * 1024 * 1024).contains(&bytes))
    }) {
        return false;
    }
    serde_json::to_vec(&lane.product_policy_document)
        .ok()
        .filter(|bytes| !bytes.is_empty() && bytes.len() <= 64 * 1024)
        .is_some_and(|bytes| hex(&Sha256::digest(bytes)) == lane.product_policy_hash)
}

/// Validate every content-addressed product identity field needed before a
/// ProductPolicy v2 account may be authored. This is deliberately independent
/// of RPC state: the deployment authoring tool calls it before it derives any
/// policy PDA, while the live loader still performs the full world/account
/// validation later.
pub fn validate_policy_lane_document(lane: &LaneManifest) -> bool {
    manifest_product_id(lane).as_deref() == Some(lane.product_id.as_str())
        && valid_hash(&lane.rights_hash)
        && valid_rights_document(lane)
        && valid_hash(&lane.product_policy_hash)
        && valid_product_policy_document(lane)
}

fn valid_isin(value: &str) -> bool {
    if value.len() != 12
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_uppercase())
    {
        return false;
    }
    let mut digits = Vec::with_capacity(24);
    for byte in value.bytes() {
        if byte.is_ascii_digit() {
            digits.push(byte - b'0');
        } else {
            let number = byte - b'A' + 10;
            digits.extend([number / 10, number % 10]);
        }
    }
    let sum = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(index, digit)| {
            let value = if index % 2 == 1 { digit * 2 } else { *digit };
            u16::from(value / 10 + value % 10)
        })
        .sum::<u16>();
    sum % 10 == 0
}

fn validate_intent_groups(lanes: &[Lane]) -> Result<()> {
    let mut groups = BTreeMap::<IntentKey, Vec<&Lane>>::new();
    for lane in lanes {
        groups
            .entry(IntentKey {
                instrument: lane.key.instrument.clone(),
                input_symbol: lane.key.input_symbol.clone(),
            })
            .or_default()
            .push(lane);
    }
    for group in groups.values() {
        if group.is_empty() || group.len() > 4 {
            return Err("issuer product bound".into());
        }
        let mut products = BTreeSet::new();
        let mut mints = BTreeSet::new();
        for lane in group {
            if !products.insert(&lane.key.product_id) || !mints.insert(&lane.output_mint) {
                return Err("issuer product group identity".into());
            }
        }
        let (prefix, cash_mint) = intent_execution_shape(group)?;
        let mut physical = BTreeSet::new();
        let mut logical_paths = 0usize;
        let mut direct_products = BTreeSet::new();
        for lane in group {
            let residual = lane.configs.get(prefix..).ok_or("product residual path")?;
            if residual.is_empty() || residual.len() > 2 {
                return Err("bounded issuer conversion depth".into());
            }
            let mut current = cash_mint.as_str();
            let mut path_count = 1usize;
            for (position, world) in residual.iter().enumerate() {
                let first = world.markets.first().ok_or("product world")?;
                if first.input_mint != current
                    || world.markets.iter().any(|market| {
                        market.input_mint != first.input_mint
                            || market.output_mint != first.output_mint
                    })
                {
                    return Err("issuer path pair mismatch".into());
                }
                let terminal = position + 1 == residual.len();
                if terminal {
                    if first.output_mint != lane.output_mint {
                        return Err("issuer path terminal product".into());
                    }
                } else if !mints.contains(&first.output_mint) {
                    return Err("issuer path transit is not an admitted product".into());
                }
                path_count = path_count
                    .checked_mul(world.markets.len())
                    .ok_or("global path count")?;
                for market in &world.markets {
                    physical.insert((
                        market.pool.clone(),
                        market.input_mint.clone(),
                        market.output_mint.clone(),
                    ));
                }
                current = &first.output_mint;
            }
            if residual.len() == 1 {
                direct_products.insert(lane.output_mint.as_str());
            }
            logical_paths = logical_paths
                .checked_add(path_count)
                .ok_or("global path count")?;
        }
        for lane in group {
            let residual = &lane.configs[prefix..];
            if residual.len() == 2 {
                let transit = residual[0]
                    .markets
                    .first()
                    .ok_or("issuer transit world")?
                    .output_mint
                    .as_str();
                if !direct_products.contains(transit) {
                    return Err("issuer conversion lacks direct cash producer".into());
                }
            }
        }
        if logical_paths == 0
            || logical_paths > MAX_EDGES
            || physical.is_empty()
            || physical.len() > stocklana_adapters::graph::MAX_CANDIDATES
        {
            return Err("global issuer path/candidate bound".into());
        }
    }
    Ok(())
}

/// Split a lane group into an optional shared non-product funding prefix and
/// one/two-hop product paths. A shared SOL->USDC world is funding; a shared
/// USDC->SPYx world remains part of the economic graph because SPYx itself is
/// an admitted terminal product.
fn intent_execution_shape(lanes: &[&Lane]) -> Result<(usize, String)> {
    let first_lane = lanes.first().ok_or("empty intent group")?;
    let root = match first_lane.key.input_symbol.as_str() {
        "USDC" => USDC_MINT,
        "SOL" => WSOL_MINT,
        _ => return Err("intent input asset".into()),
    };
    if lanes.iter().any(|lane| {
        lane.key.instrument != first_lane.key.instrument
            || lane.key.input_symbol != first_lane.key.input_symbol
            || lane
                .configs
                .first()
                .and_then(|world| world.markets.first())
                .is_none_or(|market| market.input_mint != root)
    }) {
        return Err("intent group root mismatch".into());
    }
    let product_mints = lanes
        .iter()
        .map(|lane| lane.output_mint.as_str())
        .collect::<BTreeSet<_>>();
    let first_world = first_lane.configs.first().ok_or("intent first world")?;
    let first_output = first_world
        .markets
        .first()
        .ok_or("intent first market")?
        .output_mint
        .clone();
    let funding = !product_mints.contains(first_output.as_str());
    if funding {
        if !matches!(first_output.as_str(), USDC_MINT | WSOL_MINT) || first_output == root {
            return Err("shared funding output is not an admitted cash asset".into());
        }
        let encoded = serde_json::to_vec(first_world).map_err(|error| error.to_string())?;
        if lanes.iter().any(|lane| {
            lane.configs.len() < 2
                || serde_json::to_vec(&lane.configs[0])
                    .map_or(true, |candidate| candidate != encoded)
        }) {
            return Err("shared funding world mismatch".into());
        }
        Ok((1, first_output))
    } else {
        Ok((0, root.into()))
    }
}

fn lane_policy_input_mint(lane: &Lane) -> Result<String> {
    let final_world = lane.configs.last().ok_or("policy product world")?;
    let final_market = final_world.markets.first().ok_or("policy product market")?;
    if final_market.output_mint != lane.output_mint
        || final_world.markets.iter().any(|market| {
            market.input_mint != final_market.input_mint
                || market.output_mint != final_market.output_mint
        })
    {
        return Err("policy final product pair mismatch".into());
    }
    Ok(final_market.input_mint.clone())
}

fn manifest_product_id(lane: &LaneManifest) -> Option<String> {
    let identity = ProductIdentity {
        instrument: lane.instrument.clone(),
        issuer: lane.issuer.clone(),
        mint: lane.output_mint.clone(),
        token_program: lane.token_program.clone(),
        rights_hash: decode_hex_32(&lane.rights_hash).ok()?,
        raw_decimals: lane.output_decimals,
    };
    let id = identity.id().ok()?;
    Some(format!("stkprd_{}", hex(&id[..16])))
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && value.bytes().any(|byte| byte != b'0')
}

fn validate_quote_request(request: &QuoteRequest) -> Result<u64> {
    if request.side != "BUY"
        || request.product_mint.is_some()
        || crate::onebook_wire::instrument_id(&request.instrument).is_err()
        || !matches!(request.notional_asset.as_str(), "USDC" | "SOL")
        || !(1..=100).contains(&request.max_slippage_bps)
    {
        return Err("Unsupported stock intent or slippage bound.".into());
    }
    let decimals = if request.notional_asset == "USDC" {
        6
    } else {
        9
    };
    let atoms = decimal_atoms(&request.notional, decimals)?;
    if request.notional_atoms != atoms.to_string() {
        return Err("Decimal notional and atomic amount differ.".into());
    }
    Ok(atoms)
}

fn collect_route_pools(value: &Value, pools: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            if let Some(pool) = object.get("pool").and_then(Value::as_str) {
                pools.insert(pool.into());
            }
            if let Some(pool) = object.get("market").and_then(Value::as_str) {
                pools.insert(pool.into());
            }
            for child in object.values() {
                collect_route_pools(child, pools);
            }
        }
        Value::Array(array) => {
            for child in array {
                collect_route_pools(child, pools);
            }
        }
        _ => {}
    }
}

fn clearing_asset(value: &str, stock_symbol: &str) -> Result<u8> {
    match value {
        "USDC" => Ok(0),
        "SOL" => Ok(1),
        value if value == stock_symbol => Ok(2),
        _ => Err("asset is outside this instrument clearing cell".into()),
    }
}

fn parse_u64_string(value: &str, allow_zero: bool) -> Result<u64> {
    if value.is_empty()
        || value.len() > 20
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("invalid integer string".into());
    }
    let parsed = value.parse::<u64>().map_err(|_| "integer range")?;
    if !allow_zero && parsed == 0 {
        return Err("integer must be positive".into());
    }
    Ok(parsed)
}

fn decimal_atoms(value: &str, decimals: u32) -> Result<u64> {
    if value.is_empty() || value.len() > 32 || value.starts_with('+') || value.starts_with('-') {
        return Err("Invalid decimal notional.".into());
    }
    let mut split = value.split('.');
    let whole = split.next().ok_or("decimal")?;
    let fraction = split.next().unwrap_or("");
    if split.next().is_some()
        || whole.is_empty()
        || whole.len() > 11
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > decimals as usize
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || (whole.len() > 1 && whole.starts_with('0'))
    {
        return Err("Invalid decimal notional.".into());
    }
    let fraction_digits = fraction.len() as u32;
    let scale = 10u128.pow(decimals);
    let whole: u128 = whole.parse().map_err(|_| "decimal whole")?;
    let fraction: u128 = if fraction.is_empty() {
        0
    } else {
        fraction.parse().map_err(|_| "decimal fraction")?
    };
    let fraction_scale = 10u128.pow(decimals - fraction_digits);
    let atoms = whole
        .checked_mul(scale)
        .and_then(|value| value.checked_add(fraction * fraction_scale))
        .ok_or("decimal overflow")?;
    let atoms = u64::try_from(atoms).map_err(|_| "decimal range")?;
    if atoms == 0 {
        return Err("Amount must be positive.".into());
    }
    Ok(atoms)
}

fn proposal_options(proposal: &Value) -> Vec<(u64, &[Value])> {
    let mut options = Vec::with_capacity(4);
    if let (Some(output), Some(legs)) = (
        proposal["outputAtoms"]
            .as_u64()
            .filter(|output| *output > 0),
        proposal["legs"].as_array().filter(|legs| !legs.is_empty()),
    ) {
        options.push((output, legs.as_slice()));
    }
    if let Some(alternatives) = proposal["alternatives"].as_array() {
        for alternative in alternatives.iter().take(3) {
            if let (Some(output), Some(legs)) = (
                alternative["outputAtoms"]
                    .as_u64()
                    .filter(|output| *output > 0),
                alternative["legs"]
                    .as_array()
                    .filter(|legs| !legs.is_empty()),
            ) {
                options.push((output, legs.as_slice()));
            }
        }
    }
    options
}

fn route_stage(legs: &[Value], total: u64, stage: usize) -> Result<Vec<Value>> {
    let count = legs.len();
    let remainder_pool = 10_000u64.checked_sub(count as u64).ok_or("route count")?;
    let mut shares = Vec::with_capacity(count);
    let mut assigned = 0u64;
    let mut largest = 0usize;
    let mut largest_input = 0u64;
    for (index, leg) in legs.iter().enumerate() {
        let input = leg["inputAtoms"].as_u64().ok_or("route input")?;
        if input == 0 {
            return Err("zero route input".into());
        }
        if input > largest_input {
            largest = index;
            largest_input = input;
        }
        let share = 1 + (u128::from(input) * u128::from(remainder_pool) / u128::from(total)) as u64;
        assigned = assigned.checked_add(share).ok_or("route share")?;
        shares.push(share);
    }
    if legs.iter().try_fold(0u64, |sum, leg| {
        sum.checked_add(leg["inputAtoms"].as_u64()?)
    }) != Some(total)
        || assigned > 10_000
    {
        return Err("route conservation".into());
    }
    shares[largest] += 10_000 - assigned;
    legs.iter()
        .zip(shares)
        .map(|(leg, share)| {
            let venue = venue_name(leg["venue"].as_str().ok_or("venue")?)?;
            let market = leg["pool"].as_str().ok_or("pool")?;
            decode_key(market)?;
            let input = leg["inputAtoms"].as_u64().ok_or("route exact input")?;
            let output = leg["outputAtoms"]
                .as_u64()
                .filter(|amount| *amount > 0)
                .ok_or("route exact output")?;
            Ok(
                json!({"venue":venue,"market":market,"shareBps":share,"stage":stage,
                "inputAtoms":input.to_string(),"expectedOutputAtoms":output.to_string()}),
            )
        })
        .collect()
}

fn venue_name(value: &str) -> Result<&'static str> {
    match value {
        "raydium_clmm" => Ok("Raydium CLMM"),
        "byreal_clmm" => Ok("Byreal CLMM"),
        "meteora_dlmm" => Ok("Meteora DLMM"),
        "orca_whirlpool" => Ok("Orca Whirlpool"),
        _ => Err("unknown venue".into()),
    }
}

fn read_relative(root: &Path, value: &str, maximum: usize) -> Result<Vec<u8>> {
    let relative = PathBuf::from(value);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("manifest path must be relative and normalized".into());
    }
    let mut bytes = Vec::new();
    File::open(root.join(relative))
        .map_err(|error| error.to_string())?
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > maximum {
        return Err("manifest dependency exceeds bound".into());
    }
    Ok(bytes)
}

fn decode_key(value: &str) -> Result<[u8; 32]> {
    bs58::decode(value)
        .into_vec()
        .map_err(|_| "invalid public key")?
        .try_into()
        .map_err(|_| "public key length".into())
}

fn valid_id(value: &str, prefix: &str) -> bool {
    value.len() == prefix.len() + 32
        && value.starts_with(prefix)
        && value[prefix.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// The journal, not a renewed quote or client-supplied expectation, is the
/// recovery authority. No send, blockhash replacement, or resource admission
/// occurs here. A prepared-but-never-sent entry remains observable uncertainty
/// until an explicitly authorized sender resumes it.
fn recover_submission(sender: &mut Sender, request: &SubmitRequest, signed_wire: &[u8]) -> Option<ApiReply> {
    let entry = sender.journal.get(&request.prepared_id)?.clone();
    let owner = entry.wallet_owner();
    if entry.quote_id.as_deref() != Some(request.quote_id.as_str())
        || owner != Some(request.owner.as_str()) || entry.wire != signed_wire
        || !crate::sender::authorize(signed_wire, crate::sender::Authorization {
            intent_id: entry.id.clone(), message_hash: entry.message_hash,
            last_valid_height: entry.last_valid_height, resources: entry.resources.clone(),
        }).is_ok_and(|checked| checked.signature == entry.signature)
    {
        return Some(ApiReply::error(409, "STOCKLANA_SUBMISSION_CONFLICT",
            "This request does not match the recorded wallet, quote and signed transaction. Check the recorded order; do not create a replacement."));
    }
    let refreshed = sender.observe_batch(&[entry.id.as_str()]).is_ok();
    let phase = sender.journal.get(&entry.id)?.phase.clone();
    let mut verified_exposure = None;
    let mut verified_swap = None;
    let mut verified_basket = None;
    if matches!(phase, Phase::Finalized | Phase::Reconciled) {
        let receipt = if entry.expected_basket.is_some(){
            sender.reconcile_basket(&entry.id).map(|v|{verified_basket=Some(v);})
        } else if entry.expected_swap.is_some() {
            sender.reconcile_swap(&entry.id).map(|v| { verified_swap = Some(v); })
        } else {
            sender.reconcile_exposure(&entry.id).map(|v| { verified_exposure = Some(v); })
        };
        if receipt.is_err() {
            return Some(ApiReply { status: 503, body: json!({
                "error":{"code":"STOCKLANA_RECEIPT_PENDING","message":"The recorded transaction needs receipt verification. Check this order; do not send a replacement."},
                "preparedId":entry.id,"signature":entry.signature,"phase":phase_name(&phase),
                "sameSignedWire":true,"recovered":true
            }) });
        }
    }
    // Failed observation cannot turn absence into failure, erase an accepted
    // wire, release reservations, or return a fresh prepare opportunity.
    let persisted = sender.journal.get(&entry.id)?;
    Some(ApiReply::ok(json!({
        "schema":"skew.stocklana.submission/v1","quoteId":request.quote_id,
        "preparedId":persisted.id,"signature":persisted.signature,
        "phase":phase_name(&persisted.phase),"attempts":persisted.attempts,
        "sameSignedWire":true,"recovered":true,"refreshSucceeded":refreshed,
        "verifiedExposure":verified_exposure,"verifiedSwap":verified_swap,"verifiedBasket":verified_basket
    })))
}

fn same_authorization(left: &crate::journal::Entry, right: &crate::journal::Entry) -> Result<bool> {
    let left_expected = serde_json::to_vec(&left.expected_exposure).map_err(|e| e.to_string())?;
    let right_expected = serde_json::to_vec(&right.expected_exposure).map_err(|e| e.to_string())?;
    let left_swap = serde_json::to_vec(&left.expected_swap).map_err(|e| e.to_string())?;
    let right_swap = serde_json::to_vec(&right.expected_swap).map_err(|e| e.to_string())?;
    let left_basket=serde_json::to_vec(&left.expected_basket).map_err(|e|e.to_string())?;
    let right_basket=serde_json::to_vec(&right.expected_basket).map_err(|e|e.to_string())?;
    Ok(left.id == right.id
        && left.quote_id == right.quote_id
        && left.signature == right.signature
        && left.wire == right.wire
        && left.message_hash == right.message_hash
        && left.last_valid_height == right.last_valid_height
        && left.resources == right.resources
        && left_expected == right_expected
        && left_swap == right_swap && left_basket==right_basket)
}

#[cfg(test)]
pub(crate) mod submission_recovery_tests;

fn phase_name(phase: &Phase) -> &'static str {
    match phase {
        Phase::Prepared => "PREPARED",
        Phase::Submitted => "SUBMITTED",
        Phase::Unknown => "UNKNOWN",
        Phase::Finalized => "FINALIZED",
        Phase::Failed => "FAILED",
        Phase::Reconciled => "RECONCILED",
    }
}

fn sell_prepared_id(quote_id: &str, owner: &str, candidate: &PreparedSellCandidate) -> String {
    let mut hash = Sha256::new();
    hash.update(b"SKEW_PREPARED_SELL_V1\0");
    hash.update(quote_id.as_bytes());
    hash.update(owner.as_bytes());
    hash.update(candidate.message_hash);
    format!("stkp_{}", hex(&hash.finalize()[..16]))
}

fn sell_prepared_review(
    request: &PrepareRequest,
    stored: &StoredPreparedSell,
    submit_allowed: bool,
) -> Value {
    json!({
        "schema":"skew.stocklana.prepared/v1",
        "preparedId":sell_prepared_id(&request.quote_id, &request.owner, &stored.candidate),
        "quoteId":request.quote_id,
        "owner":request.owner,
        "expiresAt":stored.expires_at,
        "estimatedCu":stored.candidate.simulated_cu,
        "lastValidBlockHeight":stored.candidate.last_valid_block_height.to_string(),
        "transactionBase64":STANDARD.encode(&stored.candidate.unsigned_wire),
        "submitAllowed":submit_allowed,
        "economicExecution":{
            "schema":"skew.stockmesh.prepared-liquidation/v1",
            "simulationOutputAtoms":stored.candidate.simulated_output.to_string(),
            "minimumOutputAtoms":stored.candidate.expected.minimum_output.to_string(),
            "simulationCU":stored.candidate.simulated_cu,
            "stateSlot":stored.candidate.snapshot.slot,
            "requiresOwnerSignatures":true,
            "submitted":false
        }
    })
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

fn now_ms() -> Result<u64> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "system clock")?
            .as_millis(),
    )
    .map_err(|_| "system clock range".into())
}

fn iso8601(milliseconds: u64) -> String {
    let seconds = milliseconds / 1_000;
    let days = (seconds / 86_400) as i64;
    let day_seconds = seconds % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        day_seconds / 3_600,
        day_seconds % 3_600 / 60,
        day_seconds % 60,
        milliseconds % 1_000
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex_32(value: &str) -> Result<[u8; 32]> {
    if !valid_hash(value) {
        return Err("invalid 32-byte hash".into());
    }
    let mut output = [0u8; 32];
    for (index, byte) in output.iter_mut().enumerate() {
        let start = index * 2;
        *byte =
            u8::from_str_radix(&value[start..start + 2], 16).map_err(|_| "invalid 32-byte hash")?;
    }
    Ok(output)
}

#[cfg(test)]
#[path = "stockmesh_api/universe_execution_tests.rs"]
mod universe_execution_tests;

#[cfg(test)]
pub(crate) mod tests {
    #[test]
    fn broken_first_bank_does_not_starve_rotation_of_other_hot_banks() {
        let mut cursor = 0;
        let mut observed = Vec::new();
        for _ in 0..6 {
            let selected = super::rotation_group(&[true, true, false, true], cursor);
            observed.push(selected);
            cursor = selected + 1;
        }
        assert_eq!(observed, vec![0, 1, 3, 0, 1, 3]);
        assert_eq!(super::rotation_group(&[false, false, false], 7), 1);
        assert_eq!(super::rotation_group(&[false, true], usize::MAX), 1);
    }
    use super::*;

    fn key_range(start: usize, end: usize) -> BTreeSet<String> {
        (start..end)
            .map(|index| format!("key-{index:03}"))
            .collect()
    }

    #[test]
    fn superbank_repacking_tracks_rotated_account_horizons() {
        let initial = pack_superbank_key_sets(vec![
            (0, key_range(0, 60)),
            (1, key_range(40, 90)),
            (2, key_range(100, 160)),
            (3, key_range(160, 200)),
        ])
        .unwrap();
        assert_eq!(initial, vec![vec![0, 1], vec![2, 3]]);

        let rotated = vec![
            (0, key_range(0, 60)),
            (1, key_range(160, 210)),
            (2, key_range(100, 160)),
            (3, key_range(160, 200)),
        ];
        let stale_union = rotated[0].1.union(&rotated[1].1).count();
        assert!(stale_union > 100);
        let dynamic = pack_superbank_key_sets(rotated.clone()).unwrap();
        assert_eq!(dynamic, vec![vec![0], vec![2], vec![1, 3]]);
        assert_eq!(dynamic.iter().map(Vec::len).sum::<usize>(), rotated.len());
        for group in dynamic {
            let union = group
                .into_iter()
                .flat_map(|index| rotated[index].1.iter().cloned())
                .collect::<BTreeSet<_>>();
            assert!(union.len() <= 100);
        }
    }

    #[test]
    fn selected_bank_is_an_isolated_first_refresh_group() {
        let banks = vec![
            (0, key_range(0, 50)),
            (1, key_range(25, 80)),
            (2, key_range(70, 130)),
        ];
        let groups = prioritize_superbank_key_sets(banks, &BTreeSet::from([1])).unwrap();
        assert_eq!(groups.first(), Some(&vec![1]));
        assert!(groups.iter().all(|group| group.len() == 1));
        assert_eq!(groups.iter().map(Vec::len).sum::<usize>(), 3);
    }

    #[test]
    fn independent_refresh_groups_never_share_a_rotating_horizon() {
        let banks = vec![
            (0, key_range(0, 70)),
            (1, key_range(40, 100)),
            (2, key_range(95, 155)),
        ];
        assert_eq!(
            prioritize_superbank_key_sets(banks, &BTreeSet::new()).unwrap(),
            vec![vec![0], vec![1], vec![2]]
        );
    }

    #[test]
    fn partial_quote_banks_are_reported_as_liquidity_degradation() {
        assert_eq!(
            stockmesh_status(4, 34, false, false),
            (
                "degraded",
                Some("One or more coherent liquidity banks are unavailable.")
            )
        );
        assert_eq!(
            stockmesh_status(34, 34, false, false),
            (
                "degraded",
                Some("Live coherent quote banks are ready; settlement deployment is not bound.")
            )
        );
        assert_eq!(stockmesh_status(34, 34, true, false), ("ready", None));
    }

    #[test]
    fn wallet_read_waits_on_transient_admission_but_keeps_exact_result() {
        let mut attempts = 0;
        let mut delays = Vec::new();
        let observed = json!({"context":{"slot":9},"value":"9007199254740993"});
        let result = portfolio_read_retry(|| {
            attempts += 1;
            match attempts {
                1 => Err("provider rate limit reached".into()),
                2 => Err("provider in-flight limit reached".into()),
                3 => Err("RPC HTTP status 429".into()),
                _ => Ok(observed.clone()),
            }
        }, |delay| delays.push(delay.as_millis())).unwrap();
        assert_eq!(attempts,4);assert_eq!(delays,vec![225,675,1575]);
        assert_eq!(result,observed);
    }

    #[test]
    fn wallet_read_never_retries_budget_expiry_or_invalid_data_and_is_bounded() {
        for error in ["provider run request budget exhausted", "provider run response budget exhausted", "provider quota period expired", "RPC response identity mismatch", "RPC HTTP status 401"] {
            let mut attempts=0;
            assert_eq!(portfolio_read_retry(|| {attempts+=1;Err(error.into())}, |_| panic!("must not wait")).unwrap_err(),error);
            assert_eq!(attempts,1);
        }
        let mut attempts=0;
        assert!(portfolio_read_retry(|| {attempts+=1;Err("provider rate limit reached".into())}, |_| {}).is_err());
        assert_eq!(attempts,4);
    }

    #[test]
    fn portfolio_parser_filters_and_aggregates_only_admitted_products() {
        let token_program = TOKEN_2022_PROGRAM.to_string();
        let product = PortfolioProduct {
            instrument: "NVDA".into(),
            product_id: format!("stkprd_{}", "a".repeat(32)),
            issuer: "Issuer".into(),
            symbol: "NVDAx".into(),
            mint: "admitted-mint".into(),
            token_program: token_program.clone(),
            raw_decimals: 8,
        };
        let products = BTreeMap::from([(product.mint.clone(), product)]);
        let account = |id: u8, mint: &str, amount: &str| {
            json!({
                "pubkey":Pubkey::new_from_array([id;32]).to_string(),
                "account":{
                    "owner":token_program,
                    "data":{"parsed":{"info":{
                        "owner":"test-wallet",
                        "mint":mint,
                        "tokenAmount":{"amount":amount,"decimals":8}
                    }}}
                }
            })
        };
        let response = json!({
            "context":{"slot":447_000_000},
            "value":[
                account(4, "admitted-mint", "10"),
                account(5, "unrelated-mint", "999"),
                account(6, "admitted-mint", "15")
            ]
        });
        let mut balances = BTreeMap::new();
        assert_eq!(
            portfolio_token_balances(&response, "test-wallet", &token_program, &products, &mut balances).unwrap(),
            447_000_000
        );
        assert_eq!(balances, BTreeMap::from([("admitted-mint".into(), 25)]));
    }

    pub(crate) fn prepared_fixture(
        f: &mut crate::exposure_receipt::tests::Fixture,
    ) -> (StockMesh, String, Vec<String>) {
        let keys = f
            .admission
            .products
            .iter()
            .map(|p| p.identity.mint.clone())
            .collect::<Vec<_>>();
        let feed = Arc::new(Feed::new(keys.clone(), Duration::from_secs(60), 1_000_000).unwrap());
        let projection = f.snapshot.project(&keys).unwrap();
        let value = json!({"context":{"slot":projection.slot},"value":projection.accounts.iter().map(|a|json!({"owner":a.owner,"executable":a.executable,"lamports":a.lamports,"data":[STANDARD.encode(&a.data),"base64"]})).collect::<Vec<_>>()});
        let snapshot = feed.publish(&value).unwrap();
        let lanes = f
            .admission
            .products
            .iter()
            .map(|p| Lane {
                key: LaneKey {
                    instrument: "NVDA".into(),
                    input_symbol: "USDC".into(),
                    product_id: hex(&p.identity.id().unwrap()),
                },
                issuer: p.identity.issuer.clone(),
                output_symbol: "FIXTURE".into(),
                output_mint: p.identity.mint.clone(),
                token_program: p.identity.token_program.clone(),
                output_decimals: p.identity.raw_decimals,
                rights_hash: hex(&p.identity.rights_hash),
                product_policy_hash: hex(&p.policy_data_hash),
                backing_model: "FIXTURE".into(),
                redemption_model: "FIXTURE".into(),
                transfer_model: "FIXTURE".into(),
                exposure_model: "FIXED_RATIONAL".into(),
                exposure_numerator: p.numerator,
                exposure_denominator: p.denominator,
                conservative_bps: p.conservative_bps,
                maximum_cu: 300000,
                configs: vec![],
            })
            .collect::<Vec<_>>();
        let policy_hash = product_policy_set_hash(&lanes.iter().collect::<Vec<_>>());
        f.admission.product_policy_hash = decode_hex_32(&policy_hash).unwrap();
        f.intent.product_policy_hash = f.admission.product_policy_hash;
        f.intent.world_generation_hash = snapshot.hash;
        let quote_id = format!("stkq_{}", "a".repeat(32));
        let mut id = Sha256::new();
        id.update(b"SKEW_STOCKMESH_QUOTE_INTENT_V1\0");
        id.update(quote_id.as_bytes());
        id.update(f.intent.owner.as_bytes());
        id.update(f.intent.owner_nonce.to_le_bytes());
        f.intent.intent_id = id.finalize().into();
        let key = IntentKey {
            instrument: "NVDA".into(),
            input_symbol: "USDC".into(),
        };
        let cached = CachedQuote {
            intent: key.clone(),
            input: f.intent.input_atoms,
            minimum_exposure_q32: f.intent.minimum_exposure_q32 as u64,
            slot: snapshot.slot,
            snapshot_hash: hex(&snapshot.hash),
            product_policy_set_hash: policy_hash,
            native_allocation: vec![],
            native_candidates: vec![],
            product_output_atoms: f
                .admission
                .products
                .iter()
                .map(|product| (hex(&product.identity.id().unwrap()), 1))
                .collect(),
            global_reflow: false,
            max_slippage_bps: 20,
            maximum_cu: 300_000,
            expires_at_ms: now_ms().unwrap() + 30000,
            expires: Instant::now() + Duration::from_secs(30),
        };
        let layout = Arc::new(BankLayout { feed, lanes });
        let publication = Arc::new(Publication {
            layout: layout.clone(),
            snapshot,
            worlds: BTreeMap::new(),
            published_at_ms: now_ms().unwrap(),
        });
        let bank = Arc::new(Bank {
            name: "fixture".into(),
            max_age: Duration::from_secs(60),
            layout: RwLock::new(layout),
            publication: RwLock::new(Some(publication)),
            last_rotation_slot: AtomicU64::new(0),
            rotations: AtomicU64::new(0),
            rotation_failures: AtomicU64::new(0),
        });
        (
            StockMesh {
                token: vec![],
                secret: [0; 32],
                sequence: AtomicU64::new(0),
                quote_ttl: Duration::from_secs(30),
                captured_fixture: true,
                deployment_ready: false,
                policy_integrity_required: false,
                catalog: RwLock::new(Catalog::new(vec![]).unwrap()),
                strategies: Mutex::new(None),
                banks: vec![bank],
                intent_bank: BTreeMap::from([(key, 0)]),
                bank_activity: Arc::new(BankActivity::new(1).unwrap()),
                quotes: Mutex::new(QuoteCache {
                    values: BTreeMap::from([(quote_id.clone(), cached)]),
                    order: VecDeque::from([quote_id.clone()]),
                }),
                sell_quotes: Mutex::new(SellQuoteCache::default()),
                prepared: Mutex::new(crate::prepared_quotes::PreparedQuotes::default()),
                sell_prepared: Mutex::new(PreparedSellCache::default()),
                basket_prepared: Mutex::new(strategies::BasketCache::default()),
                portfolio_rpc: Arc::new(RwLock::new(None)),
                direct_state: None,
                slot_signal: None,
                prepare_runtime: None,
                sender: None,
            },
            quote_id,
            keys,
        )
    }

    #[test]
    fn canonical_rights_document_is_hash_and_semantics_bound() {
        let rights = json!({
            "schema":"skew.stockmesh.rights/v1",
            "issuer":"Backed Assets (JE) Limited",
            "legalClassification":"TRACKER_CERTIFICATE_NO_VOTING_RIGHTS",
            "backingModel":"ONE_TO_ONE_UNDERLYING",
            "redemptionModel":"KYC_PRIMARY_OR_SECONDARY",
            "transferModel":"PERMISSIONLESS_JURISDICTION_RESTRICTED",
            "legalOverview":"https://example.invalid/legal-overview",
            "mechanics":"https://example.invalid/mechanics",
            "multiplierSpecification":"https://example.invalid/multiplier",
            "legalDocuments":"https://example.invalid/documents",
            "productIsin":"CH1436219195",
            "underlyingSymbol":"NVDA",
            "underlyingIsin":"US67066G1040"
        });
        let rights_hash = hex(&Sha256::digest(serde_json::to_vec(&rights).unwrap()));
        let mut lane: LaneManifest = serde_json::from_value(json!({
            "instrument":"NVDA",
            "input_symbol":"USDC",
            "product_id":format!("stkprd_{}", "a".repeat(32)),
            "issuer":"Backed Assets (JE) Limited",
            "output_symbol":"NVDAx",
            "output_mint":Pubkey::new_unique().to_string(),
            "token_program":TOKEN_2022_PROGRAM,
            "output_decimals":8,
            "rights_hash":rights_hash,
            "rights_document":rights,
            "product_policy_hash":"b".repeat(64),
            "backing_model":"ONE_TO_ONE_UNDERLYING",
            "redemption_model":"KYC_PRIMARY_OR_SECONDARY",
            "transfer_model":"PERMISSIONLESS_JURISDICTION_RESTRICTED",
            "exposure_model":"TOKEN_2022_SCALED_UI",
            "exposure_numerator":1,
            "exposure_denominator":1,
            "conservative_bps":10000,
            "maximum_cu":1400000,
            "worlds":["nvda-usdc.json"]
        }))
        .unwrap();
        assert!(valid_rights_document(&lane));
        lane.transfer_model = "POLICY_GATED".into();
        assert!(!valid_rights_document(&lane));
        lane.transfer_model = "PERMISSIONLESS_JURISDICTION_RESTRICTED".into();
        lane.rights_document["productIsin"] = json!("CHANGED");
        assert!(!valid_rights_document(&lane));

        lane.rights_document["productIsin"] = json!("CH1436219195");
        let policy = json!({
            "schema":"skew.stockmesh.product-policy/v2",
            "source":"https://api.xstocks.fi/api/v2/public/assets/NVDAx",
            "sourceEvidence":{"asset":{"url":"https://api.xstocks.fi/api/v2/public/assets/NVDAx","sha256":"c".repeat(64),"bytes":1024}},
            "assetId":"fixture",
            "symbol":"NVDAx",
            "instrument":"NVDA",
            "mint":lane.output_mint.clone(),
            "solanaAtomicSwapDeclared":true,
            "supportedExecution":"SECONDARY_ONLY",
            "primaryMintRedeem":"ISSUER_PRIMARY_NOT_INTEGRATED",
            "executionEligibility":"JURISDICTION_POLICY_REQUIRED",
            "exposureModel":"TOKEN_2022_SCALED_UI",
            "exposureNumerator":1,
            "exposureDenominator":1,
            "conservativeBps":10000,
            "rights":lane.rights_document.clone()
        });
        lane.product_policy_hash = hex(&Sha256::digest(serde_json::to_vec(&policy).unwrap()));
        lane.product_policy_document = policy;
        assert!(valid_product_policy_document(&lane));

        // Raw issuer evidence and eligibility are a policy generation. They
        // can revoke a quote through product_policy_set_hash, but must not
        // rename an otherwise unchanged economic product.
        let stable_product_id = manifest_product_id(&lane).unwrap();
        let original_policy_hash = lane.product_policy_hash.clone();
        lane.product_policy_hash = "d".repeat(64);
        assert_eq!(manifest_product_id(&lane).unwrap(), stable_product_id);
        lane.product_policy_hash = original_policy_hash;
        let original_rights_hash = lane.rights_hash.clone();
        lane.rights_hash = "e".repeat(64);
        assert_ne!(manifest_product_id(&lane).unwrap(), stable_product_id);
        lane.rights_hash = original_rights_hash;

        lane.product_policy_document["supportedExecution"] = json!("PRIMARY");
        assert!(!valid_product_policy_document(&lane));

        let rights = json!({
            "schema":"skew.stockmesh.rights/v2",
            "issuer":"Ondo Global Markets (BVI) Limited",
            "legalClassification":"TOKENIZED_TRACKER_CERTIFICATE_NO_UNDERLYING_DELIVERY",
            "backingModel":"CASH_VALUE_TRACKER",
            "redemptionModel":"KYC_PRIMARY_OR_SECONDARY",
            "transferModel":"POLICY_GATED",
            "legalOverview":"https://docs.ondo.finance/legal/disclaimers",
            "mechanics":"https://github.com/ondoprotocol/global-markets-solana",
            "multiplierSpecification":"https://cdn.sanity.io/files/8k2tqa6n/production/6f66dfe94fa8256fc18dce75edd162e3ba62fc3f.pdf",
            "legalDocuments":"https://app.ondo.finance/assets/spyon",
            "productIdentifierScheme":"ISIN",
            "productIdentifier":"VGG7001AAA21",
            "productIdentifierStatus":"VERIFIED_ISSUER_SOURCE",
            "underlyingSymbol":"SPY",
            "underlyingIdentifierScheme":"ISIN",
            "underlyingIdentifier":"US78462F1030",
            "underlyingIdentifierStatus":"VERIFIED_REFERENCE_SECURITY"
        });
        lane.instrument = "SPY".into();
        lane.issuer = "Ondo Global Markets (BVI) Limited".into();
        lane.backing_model = "CASH_VALUE_TRACKER".into();
        lane.redemption_model = "KYC_PRIMARY_OR_SECONDARY".into();
        lane.transfer_model = "POLICY_GATED".into();
        lane.rights_hash = hex(&Sha256::digest(serde_json::to_vec(&rights).unwrap()));
        lane.rights_document = rights;
        assert!(valid_rights_document(&lane));
        lane.rights_document["productIdentifierStatus"] = json!("ISSUER_NOT_PUBLISHED");
        assert!(!valid_rights_document(&lane));

        lane.rights_document["productIdentifierStatus"] = json!("VERIFIED_ISSUER_SOURCE");
        lane.output_symbol = "SPYon".into();
        let policy = json!({
            "schema":"skew.stockmesh.product-policy/v2",
            "source":"https://app.ondo.finance/assets/spyon",
            "sourceEvidence":{"finalTerms":{"url":"https://cdn.sanity.io/final-terms.pdf","sha256":"d".repeat(64),"bytes":2048}},
            "symbol":"SPYon",
            "instrument":"SPY",
            "mint":lane.output_mint.clone(),
            "issuerProgram":"XzTT4XB8m7sLD2xi6snefSasaswsKCxx5Tifjondogm",
            "supportedExecution":"SECONDARY_ONLY",
            "primaryMintRedeem":"ISSUER_ATTESTATION_REQUIRED_NOT_INTEGRATED",
            "executionEligibility":"JURISDICTION_AND_ISSUER_ATTESTATION_REQUIRED",
            "exposureModel":"TOKEN_2022_SCALED_UI",
            "exposureNumerator":1,
            "exposureDenominator":1,
            "conservativeBps":10000,
            "rights":lane.rights_document.clone()
        });
        lane.product_policy_hash = hex(&Sha256::digest(serde_json::to_vec(&policy).unwrap()));
        lane.product_policy_document = policy;
        assert!(valid_product_policy_document(&lane));
        lane.product_policy_document["sourceEvidence"]["finalTerms"]["sha256"] =
            json!("not-a-hash");
        assert!(!valid_product_policy_document(&lane));

        let rights = json!({
            "schema":"skew.stockmesh.rights/v3",
            "issuer":"PreStocks",
            "legalClassification":"SPV_PRICE_TRACKING_TOKEN_NO_DIRECT_COMPANY_EQUITY",
            "backingModel":"CASH_VALUE_TRACKER",
            "redemptionModel":"SECONDARY_ONLY",
            "transferModel":"PERMISSIONLESS_JURISDICTION_RESTRICTED",
            "legalOverview":"https://prestocks.com/openai",
            "mechanics":"https://prestocks.com/products",
            "multiplierSpecification":"https://prestocks.com/api/prestocks",
            "legalDocuments":"https://prestocks.com/products",
            "underlyingSymbol":"OPENAI",
            "productIdentifierScheme":"SOLANA_MINT",
            "productIdentifier":lane.output_mint.clone(),
            "productIdentifierStatus":"VERIFIED_ISSUER_SOURCE",
            "underlyingIdentifierScheme":"PRIVATE_COMPANY_REFERENCE",
            "underlyingIdentifier":"OPENAI",
            "underlyingIdentifierStatus":"ISSUER_SPV_REFERENCE"
        });
        lane.instrument = "OPENAI".into();
        lane.issuer = "PreStocks".into();
        lane.backing_model = "CASH_VALUE_TRACKER".into();
        lane.redemption_model = "SECONDARY_ONLY".into();
        lane.transfer_model = "PERMISSIONLESS_JURISDICTION_RESTRICTED".into();
        lane.rights_hash = hex(&Sha256::digest(serde_json::to_vec(&rights).unwrap()));
        lane.rights_document = rights;
        assert!(valid_rights_document(&lane));
        lane.rights_document["underlyingIdentifier"] = json!("ANTHROPIC");
        assert!(!valid_rights_document(&lane));

        assert!(valid_isin("CH1436219195"));
        assert!(valid_isin("US67066G1040"));
        assert!(valid_isin("US78462F1030"));
        assert!(!valid_isin("US78462F1031"));
    }

    #[test]
    fn native_allocations_preserve_one_bridge_and_all_product_inputs() {
        use crate::{
            market::{MarketConfig, Venue},
            world::NativeSwapProposal,
        };
        let leg =
            |stage, id: Option<&str>, input: &str, output: &str, pool: &str, amount, received| {
                NativeSwapProposal {
                    market: MarketConfig {
                        venue: Venue::RaydiumClmm,
                        program: Venue::RaydiumClmm.program().into(),
                        pool: pool.into(),
                        config: String::new(),
                        input_mint: input.into(),
                        output_mint: output.into(),
                        tick_arrays: vec![],
                        array_capacity: None,
                        clock: String::new(),
                    },
                    stage,
                    product_id: id.map(str::to_owned),
                    input_atoms: amount,
                    expected_output_atoms: received,
                }
            };
        let legs = vec![
            leg(1, None, "SOL", "USDC", "bridge", 100_000_003, 99_999_997),
            leg(2, Some("product-a"), "USDC", "A", "pool-a", 37_123_457, 15),
            leg(2, Some("product-b"), "USDC", "B", "pool-b", 62_876_540, 30),
        ];
        validate_native_allocation(&legs, 100_000_003).unwrap();
        assert_eq!(legs[1].json()["inputAtoms"], "37123457");
        let mut bad = legs.clone();
        bad[1].input_atoms += 1;
        assert!(validate_native_allocation(&bad, 100_000_003).is_err());
        let mut bad = legs.clone();
        bad[2].market.input_mint = "SOL".into();
        assert!(validate_native_allocation(&bad, 100_000_003).is_err());
        let mut bad = legs.clone();
        bad.insert(1, legs[0].clone());
        assert!(validate_native_allocation(&bad, 100_000_003).is_err());
        let mut bad = legs.clone();
        bad[0].product_id = Some("not-a-funding-leg".into());
        assert!(validate_native_allocation(&bad, 100_000_003).is_err());
    }

    #[test]
    fn resource_admission_keeps_economic_coverage_and_prunes_substitute_curves() {
        use crate::{
            market::{MarketConfig, Venue},
            world::NativeSwapProposal,
        };
        let leg = |stage, id: Option<&str>, input: &str, output: &str, pool: &str, venue| {
            NativeSwapProposal {
                market: MarketConfig {
                    venue,
                    program: venue.program().into(),
                    pool: pool.into(),
                    config: String::new(),
                    input_mint: input.into(),
                    output_mint: output.into(),
                    tick_arrays: vec![format!("{pool}-array")],
                    array_capacity: None,
                    clock: String::new(),
                },
                stage,
                product_id: id.map(str::to_owned),
                input_atoms: 100,
                expected_output_atoms: 90,
            }
        };
        let proposals = vec![
            leg(1, None, "SOL", "USDC", "fund", Venue::RaydiumClmm),
            leg(
                2,
                Some("spyx"),
                "USDC",
                "SPYX",
                "orca",
                Venue::OrcaWhirlpool,
            ),
            leg(2, Some("spyx"), "USDC", "SPYX", "byreal", Venue::ByrealClmm),
            leg(
                2,
                Some("spyon"),
                "SPYX",
                "SPYON",
                "conversion",
                Venue::MeteoraDlmm,
            ),
        ];
        let variants = resource_admission_variants(&proposals).unwrap();
        assert_eq!(variants.len(), 3);
        assert_eq!(variants[0].len(), 4);
        assert_eq!(
            variants[1]
                .iter()
                .map(|proposal| proposal.market.pool.as_str())
                .collect::<Vec<_>>(),
            vec!["fund", "byreal", "conversion"]
        );
        assert_eq!(
            variants[2]
                .iter()
                .map(|proposal| proposal.market.pool.as_str())
                .collect::<Vec<_>>(),
            vec!["fund", "orca", "conversion"]
        );
        assert_eq!(
            resource_admission_variants(&[proposals[0].clone()])
                .unwrap()
                .len(),
            1
        );
        let mut split=proposals.clone();
        split[0].input_atoms=40;
        let mut second=leg(1,None,"SOL","USDC","fund-second",Venue::OrcaWhirlpool);
        second.input_atoms=60;
        split.insert(1,second);
        for candidate in resource_admission_variants(&split).unwrap() {
            let funding=candidate.iter().filter(|p|p.product_id.is_none()).collect::<Vec<_>>();
            assert_eq!(funding.len(),2);
            assert_eq!(funding.iter().map(|p|p.input_atoms).sum::<u64>(),100);
            assert_eq!(funding[0].input_atoms,40);
            assert_eq!(funding[1].input_atoms,60);
        }
    }

    #[test]
    fn exact_resource_admission_retries_only_same_bank_candidate_failures() {
        use crate::{
            market::{MarketConfig, Venue},
            world::NativeSwapProposal,
        };
        let proposal = |pool: &str| NativeSwapProposal {
            market: MarketConfig {
                venue: Venue::ByrealClmm,
                program: Venue::ByrealClmm.program().into(),
                pool: pool.into(),
                config: String::new(),
                input_mint: "USDC".into(),
                output_mint: "SPYX".into(),
                tick_arrays: vec![],
                array_capacity: None,
                clock: String::new(),
            },
            stage: 1,
            product_id: Some("spyx".into()),
            input_atoms: 1,
            expected_output_atoms: 1,
        };
        let variants = vec![
            vec![proposal("full-a"), proposal("full-b")],
            vec![proposal("safe")],
        ];
        let mut attempts = Vec::new();
        let selected = exact_resource_admission(variants, |variant| {
            attempts.push(variant[0].market.pool.clone());
            if attempts.len() == 1 {
                Err("economic simulation resource rejected".into())
            } else {
                Ok(variant[0].market.pool.clone())
            }
        })
        .unwrap();
        assert_eq!(selected, "safe");
        assert_eq!(attempts, vec!["full-a", "safe"]);

        let mut stale_attempts = 0;
        let error = exact_resource_admission(
            vec![vec![proposal("full")], vec![proposal("fallback")]],
            |_| {
                stale_attempts += 1;
                Err::<(), _>("execution bank advanced; rebuild".into())
            },
        )
        .unwrap_err();
        assert!(error.contains("advanced"));
        assert_eq!(stale_attempts, 1);
    }

    #[test]
    fn exact_resource_admission_recovers_from_real_v0_packet_and_lock_limits() {
        use solana_instruction::{AccountMeta, Instruction};
        use solana_message::VersionedMessage;
        let payer = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        // Exercise the actual wire compiler, not a mocked error string. These
        // are structural envelopes, not submitted or simulated DEX trades.
        for (large_count, expected_error) in [(40, "v0 transaction packet bound"), (65, "v0 account lock bound")] {
            let large = Instruction {
                program_id:program, data:vec![1; 32],
                accounts:(0..large_count).map(|_| AccountMeta::new_readonly(Pubkey::new_unique(), false)).collect(),
            };
            let small = Instruction { accounts:large.accounts[..4].to_vec(), ..large.clone() };
            assert_eq!(crate::onebook_wire::compile_unsigned_v0(payer, &[large.clone()], &[], [7;32]).unwrap_err(), expected_error);
            let mut attempts = 0;
            let message = exact_resource_admission(vec![vec![],vec![]], |_| {
                attempts += 1;
                crate::onebook_wire::compile_unsigned_v0(payer, &[if attempts == 1 {large.clone()} else {small.clone()}], &[], [7;32])
            }).unwrap();
            assert_eq!(attempts, 2);
            assert!(message.len() + 65 <= 1232);
            let decoded: VersionedMessage = bincode::deserialize(&message).unwrap();
            assert_eq!(decoded.header().num_required_signatures, 1);
            assert_eq!(decoded.static_account_keys()[0], payer);
        }
    }

    #[test]
    fn resource_retry_does_not_hide_admission_or_prefixed_errors() {
        for failure in ["ProductPolicy authority or owner differs from deployment",
            "wallet setup requires frozen active ALT", "swap graph nonce binding",
            "native wire stale native quote", "execution bank market projection changed; rebuild",
            "untrusted diagnostic: economic simulation resource rejected"] {
            let mut attempts = 0;
            let error = exact_resource_admission(vec![vec![],vec![]], |_| {
                attempts += 1;
                Err::<(), _>(failure.to_string())
            }).unwrap_err();
            assert_eq!(error, failure);
            assert_eq!(attempts, 1);
        }
        let mut attempts = 0;
        let error = exact_resource_admission(vec![vec![],vec![]], |_| {
            attempts += 1;
            Err::<(), _>("v0 transaction packet bound".into())
        }).unwrap_err();
        assert_eq!(attempts, 2);
        assert_eq!(error, "exact execution resource admission rejected 2/2 variants");
    }

    #[test]
    fn mixed_direct_and_issuer_conversion_lanes_share_one_economic_market() {
        use crate::market::{MarketConfig, Venue};

        let spyx = Pubkey::new_unique().to_string();
        let spyon = Pubkey::new_unique().to_string();
        let world = |input: &str, output: &str| WorldConfig {
            markets: vec![MarketConfig {
                venue: Venue::MeteoraDlmm,
                program: Venue::MeteoraDlmm.program().into(),
                pool: Pubkey::new_unique().to_string(),
                config: String::new(),
                input_mint: input.into(),
                output_mint: output.into(),
                tick_arrays: vec![Pubkey::new_unique().to_string()],
                array_capacity: None,
                clock: "SysvarC1ock11111111111111111111111111111111".into(),
            }],
            execution_keys: vec![],
            observed_only: vec![],
        };
        let lane = |product: &str, mint: &str, input_symbol: &str, configs| Lane {
            key: LaneKey {
                instrument: "SPY".into(),
                input_symbol: input_symbol.into(),
                product_id: product.into(),
            },
            issuer: product.into(),
            output_symbol: product.into(),
            output_mint: mint.into(),
            token_program: TOKEN_2022_PROGRAM.into(),
            output_decimals: 6,
            rights_hash: "a".repeat(64),
            product_policy_hash: "b".repeat(64),
            backing_model: "ONE_TO_ONE_UNDERLYING".into(),
            redemption_model: "SECONDARY_ONLY".into(),
            transfer_model: "POLICY_GATED".into(),
            exposure_model: "FIXED_RATIONAL".into(),
            exposure_numerator: 1,
            exposure_denominator: 1,
            conservative_bps: 9_900,
            maximum_cu: 1_400_000,
            configs,
        };

        let usdc = vec![
            lane("spyx", &spyx, "USDC", vec![world(USDC_MINT, &spyx)]),
            lane(
                "spyon",
                &spyon,
                "USDC",
                vec![world(USDC_MINT, &spyx), world(&spyx, &spyon)],
            ),
        ];
        validate_intent_groups(&usdc).unwrap();
        assert_eq!(
            intent_execution_shape(&usdc.iter().collect::<Vec<_>>()).unwrap(),
            (0, USDC_MINT.into())
        );
        assert_eq!(lane_policy_input_mint(&usdc[0]).unwrap(), USDC_MINT);
        assert_eq!(lane_policy_input_mint(&usdc[1]).unwrap(), spyx);

        let funding = world(WSOL_MINT, USDC_MINT);
        let sol = vec![
            lane(
                "spyx",
                &spyx,
                "SOL",
                vec![funding.clone(), world(USDC_MINT, &spyx)],
            ),
            lane(
                "spyon",
                &spyon,
                "SOL",
                vec![funding, world(USDC_MINT, &spyx), world(&spyx, &spyon)],
            ),
        ];
        validate_intent_groups(&sol).unwrap();
        assert_eq!(
            intent_execution_shape(&sol.iter().collect::<Vec<_>>()).unwrap(),
            (1, USDC_MINT.into())
        );
        assert_eq!(lane_policy_input_mint(&sol[0]).unwrap(), USDC_MINT);
        assert_eq!(lane_policy_input_mint(&sol[1]).unwrap(), spyx);

        let mut ambiguous_policy = sol[1].clone();
        ambiguous_policy
            .configs
            .last_mut()
            .unwrap()
            .markets
            .push(world(WSOL_MINT, &spyon).markets.remove(0));
        assert!(lane_policy_input_mint(&ambiguous_policy).is_err());

        let unrooted = vec![lane(
            "spyon",
            &spyon,
            "USDC",
            vec![world(USDC_MINT, &spyx), world(&spyx, &spyon)],
        )];
        assert!(validate_intent_groups(&unrooted).is_err());
    }

    #[test]
    fn decimal_and_atomic_forms_must_match() {
        assert_eq!(decimal_atoms("50000.25", 6).unwrap(), 50_000_250_000);
        assert_eq!(decimal_atoms("1.000000001", 9).unwrap(), 1_000_000_001);
        for invalid in ["0", "01", "-1", "1.0000001", "1.2.3", ""] {
            assert!(decimal_atoms(invalid, 6).is_err(), "{invalid}");
        }
    }

    #[test]
    fn route_rounding_is_positive_and_conservative() {
        let legs = vec![
            json!({"venue":"raydium_clmm","pool":"11111111111111111111111111111111","inputAtoms":1,"outputAtoms":2}),
            json!({"venue":"orca_whirlpool","pool":"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA","inputAtoms":9_999,"outputAtoms":19_998}),
        ];
        let route = route_stage(&legs, 10_000, 1).unwrap();
        assert_eq!(
            route
                .iter()
                .map(|row| row["shareBps"].as_u64().unwrap())
                .sum::<u64>(),
            10_000
        );
        assert!(route
            .iter()
            .all(|row| row["shareBps"].as_u64().unwrap() > 0));
        assert!(route.iter().all(|row| row["stage"] == 1));
        assert_eq!(route[0]["market"], "11111111111111111111111111111111");
        assert_eq!(route[0]["inputAtoms"], "1");
        assert_eq!(route[1]["inputAtoms"], "9999");
        assert_eq!(route[1]["expectedOutputAtoms"], "19998");
    }

    #[test]
    fn bearer_comparison_and_ids_are_strict() {
        assert!(constant_time_equal(b"same", b"same"));
        assert!(!constant_time_equal(b"same", b"samf"));
        assert!(!constant_time_equal(b"same", b"same-longer"));
        assert!(valid_id(&format!("stkq_{}", "a".repeat(32)), "stkq_"));
        assert!(!valid_id(&format!("stkq_{}", "A".repeat(32)), "stkq_"));
    }

    #[test]
    fn expiry_format_is_utc_and_stable() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(1_789_322_400_123), "2026-09-13T18:00:00.123Z");
    }

    #[test]
    fn bounded_provider_refresh_preserves_hot_banks_without_starving_cold_ones() {
        let activity = BankActivity::new(24).unwrap();
        let now = now_ms().unwrap();
        for i in 0..6 {
            activity.last_selected_ms[i].store(now, Ordering::Release);
        }
        let mut cursor = 0;
        let mut observed = BTreeSet::new();
        for _ in 0..24 {
            let selected = activity.bounded_active(&mut cursor);
            assert!(selected.len() <= 7);
            assert!((0..6).all(|i| selected.contains(&i)));
            observed.extend(selected);
        }
        assert_eq!(observed.len(), 24);
        activity.last_selected_ms[23].store(now + 1, Ordering::Release);
        assert!(activity.bounded_active(&mut cursor).contains(&23));
        for last in &activity.last_selected_ms { last.store(0, Ordering::Release); }
        let before = cursor;
        assert!(activity.bounded_active(&mut cursor).is_empty());
        assert_eq!(cursor, before);
        activity.mark(23);
        assert!(activity.bounded_active(&mut cursor).contains(&23));
    }

    #[test]
    fn discovery_import_never_enables_a_quote_or_signer() {
        let mut fixture = crate::exposure_receipt::tests::fixture();
        let (mut api, _, _) = prepared_fixture(&mut fixture);
        let product = Product {
            product_id: "catalog:fixture:1".into(),
            instrument: "PENNY".into(),
            name: "Fixture token".into(),
            kind: AssetKind::Equity,
            issuer: "Fixture".into(),
            chain: "eip155:1".into(),
            address: format!("0x{}", "1".repeat(40)),
            execution_class: ExecutionClass::EvmNative,
            source_kind: SourceKind::IssuerCatalog,
            source_url: "https://example.invalid/catalog.json".into(),
            source_sha256: "a".repeat(64),
            observed_at: 1,
            issuer_tradable: Some(true),
            fractional: Some(true),
            rights_hash: Some("b".repeat(64)),
        };
        *api.catalog.write().unwrap() = Catalog::new(vec![product]).unwrap();
        assert!(!api.admits_instrument("PENNY"));
        let reply = api.catalog_search(br#"{"query":"PENNY"}"#);
        assert_eq!(reply.status, 200);
        assert_eq!(
            reply.body["products"][0]["availability"]["state"],
            "DISCOVERY_ONLY"
        );
        assert_eq!(
            reply.body["products"][0]["availability"]["tradeAllowed"],
            false
        );
        let quote=api.quote(br#"{"instrument":"PENNY","side":"BUY","notional":"1","notionalAsset":"USDC","notionalAtoms":"1000000","maxSlippageBps":20}"#);
        assert_eq!(quote.body["error"]["code"], "STOCKLANA_LANE_UNAVAILABLE");
        assert!(!api.submission_enabled());
        assert_eq!(api.catalog_search(br#"{"revision":"changed"}"#).status, 409);
        assert_eq!(api.catalog_search(br#"{"limit":101}"#).status, 400);
        // This models the trusted, validated-manifest map independently of
        // metadata; it proves only the admission boundary, not an actual fill.
        api.intent_bank.insert(
            IntentKey {
                instrument: "PENNY".into(),
                input_symbol: "USDC".into(),
            },
            0,
        );
        assert!(api.admits_instrument("PENNY"));
    }

    #[test]
    fn basket_preview_keeps_unadmitted_cash_and_detects_shared_pool_ids() {
        let mut fixture = crate::exposure_receipt::tests::fixture();
        let (api, _, _) = prepared_fixture(&mut fixture);
        let revision = api.catalog.read().unwrap().revision.clone();
        let body=serde_json::to_vec(&json!({"catalogRevision":revision,"inputAtoms":"1000001","maxSlippageBps":20,
            "legs":[{"instrument":"PENNY","weightBps":5000},{"instrument":"OTHER","weightBps":5000}]})).unwrap();
        let reply = api.etf_preview(&body);
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body["blockedInputAtoms"], "1000001");
        assert_eq!(reply.body["quotedInputAtoms"], "0");
        assert_eq!(reply.body["submitAllowed"], false);
        let mut pools = BTreeSet::new();
        collect_route_pools(
            &json!({"route":[{"market":"pool-1"}],"native":{"pool":"pool-1"}}),
            &mut pools,
        );
        assert_eq!(pools, BTreeSet::from(["pool-1".to_string()]));
    }
}
