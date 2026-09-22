//! Generation-pinned native allocation. This produces proposals, never executable
//! admission: the exact instruction graph must still pass SBF and signed limits.
use crate::{
    feed::Snapshot,
    market::{Market, MarketConfig, Venue},
    Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use skew_native::u64_at;
use std::{collections::BTreeSet, sync::Arc, time::Instant};
#[derive(Clone, Serialize, Deserialize)]
pub struct ObservedOnly {
    pub venue: String,
    pub keys: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct WorldConfig {
    pub markets: Vec<MarketConfig>,
    /// Pool-derived CPI dependencies that are not required to decode a quote
    /// curve (vaults, token programs, oracle/event PDAs and program accounts).
    /// They are fetched in the same bank so a frozen allocation can be lowered
    /// without stitching a second state read.
    #[serde(default)]
    pub execution_keys: Vec<String>,
    #[serde(default)]
    pub observed_only: Vec<ObservedOnly>,
}
#[derive(Deserialize)]
#[serde(untagged)]
pub enum LaneConfig {
    Single(MarketConfig),
    World(WorldConfig),
}
impl LaneConfig {
    pub fn world(self) -> WorldConfig {
        match self {
            Self::Single(m) => WorldConfig {
                markets: vec![m],
                execution_keys: vec![],
                observed_only: vec![],
            },
            Self::World(w) => w,
        }
    }
}
struct Edge {
    fingerprint: [u8; 32],
    market: Arc<Market>,
    config: MarketConfig,
}
pub struct World {
    edges: Vec<Edge>,
    cache_namespace: [u8; 32],
    pub slot: u64,
    pub generation: u64,
    pub snapshot_hash: String,
    pub metrics: Value,
}

/// Exact native proposal kept separately from display percentages. The market
/// identity and tick/bin horizon are copied from the same admitted World. This
/// is not a CPI recipe or execution authorization.
#[derive(Clone)]
pub struct NativeSwapProposal {
    pub market: MarketConfig,
    pub stage: usize,
    pub product_id: Option<String>,
    pub input_atoms: u64,
    pub expected_output_atoms: u64,
}

impl NativeSwapProposal {
    pub fn json(&self) -> Value {
        json!({"stage":self.stage,"productId":self.product_id,"market":self.market,
            "inputAtoms":self.input_atoms.to_string(),"expectedOutputAtoms":self.expected_output_atoms.to_string()})
    }
}
impl WorldConfig {
    /// Compile the same admitted pools in the opposite economic direction.
    /// Multi-hop callers also reverse the order of WorldConfig values.
    pub fn reversed(&self) -> Self {
        Self {
            markets: self.markets.iter().map(MarketConfig::reversed).collect(),
            execution_keys: self.execution_keys.clone(),
            observed_only: self.observed_only.clone(),
        }
    }

    pub fn keys(&self) -> Result<Vec<String>> {
        self.keys_inner(true)
    }

    /// Accounts that determine marginal quote curves. Execution-only
    /// dependencies remain bound by `keys()` and are fetched as one complete
    /// bank before prepare instead of consuming the hot quote polling budget.
    pub fn quote_keys(&self) -> Result<Vec<String>> {
        self.keys_inner(false)
    }

    fn keys_inner(&self, include_execution: bool) -> Result<Vec<String>> {
        if self.markets.is_empty()
            || self.markets.len() + self.observed_only.len() > 8
            || self.execution_keys.len() > 64
        {
            return Err("world edge bound".into());
        }
        let mut seen = BTreeSet::new();
        let mut pools = BTreeSet::new();
        let mut keys = Vec::new();
        let mut pair: Option<(&str, &str)> = None;
        for c in &self.markets {
            let current_pair = (c.input_mint.as_str(), c.output_mint.as_str());
            if pair.is_some_and(|expected| expected != current_pair)
                || !pools.insert(&c.pool)
                || c.tick_arrays.is_empty()
                || c.tick_arrays.len() > 8
                || c.array_capacity.is_some_and(|capacity| {
                    capacity == 0
                        || usize::from(capacity) < c.tick_arrays.len()
                        || usize::from(capacity) > 8
                })
            {
                return Err("duplicate pool or array bound".into());
            }
            pair = Some(current_pair);
            for k in c.keys() {
                if seen.insert(k.clone()) {
                    keys.push(k);
                }
            }
        }
        for o in &self.observed_only {
            if o.keys.is_empty() || o.keys.len() > 24 {
                return Err("observed dependency bound".into());
            }
            for k in &o.keys {
                if seen.insert(k.clone()) {
                    keys.push(k.clone());
                }
            }
        }
        if include_execution {
            for key in &self.execution_keys {
                if seen.insert(key.clone()) {
                    keys.push(key.clone());
                }
            }
        }
        if keys.len() > 100 {
            return Err("coherent RPC account bound".into());
        }
        Ok(keys)
    }
    pub fn compile(&self, s: &Snapshot, prior: Option<&World>) -> Result<World> {
        self.compile_inner(s, prior, None)
    }

    /// Discovery tooling can measure an explicitly declared native pair without
    /// adding it to the live ProductPolicy registry. The returned quote always
    /// retains executionAdmitted=false; it is not a prepared transaction.
    pub fn probe_declared_pair(&self, snapshot: &Snapshot, input: u64) -> Result<Value> {
        let first = self.markets.first().ok_or("probe market missing")?;
        self.compile_admitted_pair(snapshot, None, &first.input_mint, &first.output_mint)?.quote(input)
    }

    pub(crate) fn compile_admitted_pair(
        &self,
        s: &Snapshot,
        prior: Option<&World>,
        input_mint: &str,
        output_mint: &str,
    ) -> Result<World> {
        self.compile_inner(s, prior, Some((input_mint, output_mint)))
    }

    fn compile_inner(
        &self,
        s: &Snapshot,
        prior: Option<&World>,
        admitted_pair: Option<(&str, &str)>,
    ) -> Result<World> {
        let started = Instant::now();
        self.keys()?;
        let account = |key: &str| {
            s.accounts
                .iter()
                .find(|a| a.key == key)
                .ok_or_else(|| format!("missing {key}"))
        };
        let clock = account("SysvarC1ock11111111111111111111111111111111")?;
        if clock.owner != "Sysvar1111111111111111111111111111111111111"
            || clock.executable
            || clock.data.len() != 40
            || u64_at(&clock.data, 0).map_err(|e| format!("{e:?}"))? != s.slot
        {
            return Err("clock bank binding".into());
        }
        let mut edges = Vec::new();
        let mut rejected = Vec::new();
        let mut reused = 0;
        let mut decoded = 0;
        for c in &self.markets {
            let mut hash = Sha256::new();
            hash.update(serde_json::to_vec(c).map_err(|e| e.to_string())?);
            for key in c.keys() {
                let a = account(&key)?;
                hash.update(a.key.as_bytes());
                hash.update(a.owner.as_bytes());
                hash.update([u8::from(a.executable)]);
                if key == c.clock {
                    hash.update(&a.data[16..24]); // epoch changes transfer fee selection
                    if c.venue != Venue::RaydiumClmm {
                        hash.update(&a.data[32..40]);
                    }
                } else {
                    hash.update((a.data.len() as u64).to_le_bytes());
                    hash.update(&a.data);
                }
            }
            let fingerprint: [u8; 32] = hash.finalize().into();
            let cached = prior.and_then(|p| p.edges.iter().find(|e| e.fingerprint == fingerprint));
            let market = if let Some(e) = cached {
                reused += 1;
                e.market.clone()
            } else {
                decoded += 1;
                match if let Some((input_mint, output_mint)) = admitted_pair {
                    c.compile_exact_pair(s, input_mint, output_mint)
                } else {
                    c.compile(s)
                } {
                    Ok(m) => Arc::new(m),
                    Err(e) => {
                        rejected.push(json!({"pool":c.pool,"venue":c.venue,"reason":e}));
                        continue;
                    }
                }
            };
            edges.push(Edge {
                fingerprint,
                market,
                config: c.clone(),
            });
        }
        if edges.is_empty() {
            return Err(format!("no admitted native edge: {rejected:?}"));
        }
        let snapshot_hash = s.hash.iter().map(|b| format!("{b:02x}")).collect();
        let metrics = json!({"compiled_ns":started.elapsed().as_nanos() as u64,"native_edges":edges.len(),"decoded_edges":decoded,"reused_edges":reused,"rejected_edges":rejected,"observed_only":self.observed_only.iter().map(|o|json!({"venue":o.venue,"status":"REQUIRES_SBF_QUOTER"})).collect::<Vec<_>>()});
        let mut namespace = Sha256::new();
        namespace.update(s.hash);
        namespace.update(serde_json::to_vec(self).map_err(|e| e.to_string())?);
        Ok(World {
            edges,
            cache_namespace: namespace.finalize().into(),
            slot: s.slot,
            generation: s.generation,
            snapshot_hash,
            metrics,
        })
    }
}
impl World {
    pub(crate) fn proposal_leg(
        &self,
        pool: &str,
        input_atoms: u64,
        expected_output_atoms: u64,
        stage: usize,
        product_id: Option<String>,
    ) -> Result<NativeSwapProposal> {
        if input_atoms == 0 || expected_output_atoms == 0 || !(1..=2).contains(&stage) {
            return Err("native proposal amount/stage".into());
        }
        let edge = self
            .edges
            .iter()
            .find(|edge| edge.config.pool == pool)
            .ok_or("native proposal pool not admitted")?;
        Ok(NativeSwapProposal {
            market: edge.config.clone(),
            stage,
            product_id,
            input_atoms,
            expected_output_atoms,
        })
    }

    pub(crate) fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub(crate) fn cache_namespace(&self) -> [u8; 32] {
        self.cache_namespace
    }

    pub(crate) fn quote_edge(&self, index: usize, input: u64) -> Result<u64> {
        if input == 0 {
            return Ok(0);
        }
        self.edges
            .get(index)
            .ok_or_else(|| "world edge index".to_string())?
            .market
            .quote(input)
            .map_err(|error| format!("native edge quote: {error:?}"))
    }

    pub(crate) fn edge_identity(&self, index: usize) -> Result<(&str, &str)> {
        let edge = self.edges.get(index).ok_or("world edge index")?;
        Ok((
            match edge.config.venue {
                Venue::RaydiumClmm => "raydium_clmm",
                Venue::ByrealClmm => "byreal_clmm",
                Venue::MeteoraDlmm => "meteora_dlmm",
                Venue::OrcaWhirlpool => "orca_whirlpool",
            },
            edge.config.pool.as_str(),
        ))
    }

    pub fn quote_intent(
        &self,
        context: &crate::stock::StockContext,
        intent: &crate::stock::EconomicIntent,
    ) -> Result<Value> {
        let commitment = context.admit(intent, self.slot, crate::stock::Kind::Secondary)?;
        if self.edges.iter().any(|e| {
            e.config.input_mint != intent.input_mint || e.config.output_mint != intent.output_mint
        }) {
            return Err("world and economic intent mint mismatch".into());
        }
        let mut proposal = self.quote(intent.input_atoms)?;
        if proposal["outputAtoms"].as_u64().ok_or("missing output")? < intent.min_output_atoms {
            return Err("economic intent minimum output unavailable".into());
        }
        proposal["stockCommitment"] = json!(commitment
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>());
        proposal["instrument"] = json!(intent.instrument);
        proposal["instrumentVersion"] = json!(intent.version);
        proposal["issuer"] = json!(intent.issuer);
        proposal["signedStateSequence"] = json!(context.state.sequence);
        proposal["deadlineSlot"] = json!(intent.deadline_slot);
        proposal["minOutputAtoms"] = json!(intent.min_output_atoms);
        proposal["stockGuardRequiredBeforeSigning"] = json!(true);
        Ok(proposal)
    }
    pub fn quote(&self, input: u64) -> Result<Value> {
        self.quote_with_memo(input, &mut skew_native::memo::QuoteMemo::default())
    }
    pub fn quote_with_memo(
        &self,
        input: u64,
        memo: &mut skew_native::memo::QuoteMemo,
    ) -> Result<Value> {
        if input == 0 || input > 500_000_000_000 {
            return Err("input bound".into());
        }
        let started = Instant::now();
        memo.begin(self.cache_namespace);
        let plan = skew_engine::optimizer::oracle::refine(self.edges.len(), input, 2048, |i, q| {
            memo.quote(i, q, || {
                self.edges[i]
                    .market
                    .quote(q)
                    .map_err(|_| skew_native::Error::Capacity)
            })
            .map_err(|_| skew_engine::Error::Capacity)
        })
        .map_err(|e| format!("native allocation: {e:?}"))?;
        let legs=self.edges.iter().enumerate().filter(|(i,_)|plan.inputs[*i]>0).map(|(i,e)|json!({"venue":e.config.venue,"pool":e.config.pool,"inputAtoms":plan.inputs[i],"outputAtoms":plan.outputs[i]})).collect::<Vec<_>>();
        // A good price proposal may exceed transaction CU. Provide bounded
        // independent candidates so the SBF gate can select an executable fallback.
        let mut alternatives = Vec::new();
        for (i, e) in self.edges.iter().enumerate() {
            if plan.inputs[i] == input {
                continue;
            }
            if let Ok(out) = memo.quote(i, input, || {
                e.market
                    .quote(input)
                    .map_err(|_| skew_native::Error::Capacity)
            }) {
                alternatives.push(json!({"outputAtoms":out,"legs":[{"venue":e.config.venue,"pool":e.config.pool,"inputAtoms":input,"outputAtoms":out}]}));
            }
        }
        alternatives.sort_by_key(|a| std::cmp::Reverse(a["outputAtoms"].as_u64().unwrap()));
        alternatives.truncate(3);
        Ok(
            json!({"inputAtoms":input,"outputAtoms":plan.output,"slot":self.slot,"generation":self.generation,"snapshotHash":self.snapshot_hash,"legs":legs,"alternatives":alternatives,"maxExactCandidates":4,"oracleCalls":plan.oracle_calls,"nativeEvaluations":memo.evaluations,"memoHits":memo.hits,"memoProbeFallbacks":memo.probe_fallbacks,"budgetExhausted":plan.exhausted,"planningNs":started.elapsed().as_nanos() as u64,"requiresSimulation":true,"executionAdmitted":false,"nativeEdges":self.edges.len(),"optimality":"best observed native allocation; no global certificate"}),
        )
    }
}
