//! Authenticated, bounded execution-policy intervals. Independence is an owner
//! configuration assumption, not something signatures or this model can prove.
//! Private flow and realized fills never vote on the price center.
use crate::{
    stock::{EconomicIntent, Kind, StockContext},
    Result,
};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use skew_engine::fair::{self, Band, Crossing, Interval, Side};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub id: u16,
    pub group: u16,
    pub public_key: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub instrument: String,
    pub issuer: String,
    pub base_mint: String,
    pub quote_mint: String,
    pub version: u64,
    /// Pin the full stock policy, including attestor and issuer identities.
    pub stock_policy_hash: [u8; 32],
    pub sources: Vec<Source>,
    pub faulty_groups: usize,
    pub max_age_slots: u64,
    pub max_age_ms: u64,
    pub max_width_bps: u16,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub source_id: u16,
    pub policy_hash: [u8; 32],
    pub stock_state_hash: [u8; 32],
    pub sequence: u64,
    pub slot: u64,
    pub observed_ms: u64,
    pub expires_slot: u64,
    pub expires_ms: u64,
    pub low_q32: u64,
    pub high_q32: u64,
}
impl Observation {
    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        let mut b = b"SKEW_EXECUTION_INTERVAL_V1\0".to_vec();
        b.extend(serde_json::to_vec(self).map_err(|e| e.to_string())?);
        Ok(b)
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedObservation {
    pub observation: Observation,
    pub signature: String,
}
fn hash<T: Serialize>(v: &T) -> Result<[u8; 32]> {
    Ok(Sha256::digest(serde_json::to_vec(v).map_err(|e| e.to_string())?).into())
}
fn key(s: &str) -> Result<[u8; 32]> {
    bs58::decode(s)
        .into_vec()
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "public key length".into())
}

pub struct Engine {
    policy: Policy,
    policy_hash: [u8; 32],
    keys: Vec<VerifyingKey>,
    groups: BTreeSet<u16>,
    latest: BTreeMap<u16, (u64, [u8; 32])>,
    last_clock: Option<(u64, u64)>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Watermarks {
    pub policy_hash: [u8; 32],
    pub latest: BTreeMap<u16, (u64, [u8; 32])>,
    pub last_clock: Option<(u64, u64)>,
}
/// Constructible only through authenticated admission. A band is a conditional
/// policy envelope, not an assertion that the stock's fundamental value is here.
pub struct Decision {
    band: Band,
    commitment: [u8; 32],
    stock_hash: [u8; 32],
    policy: Policy,
    slot: u64,
    now_ms: u64,
    expires_slot: u64,
    expires_ms: u64,
}
impl Engine {
    pub fn new(policy: Policy) -> Result<Self> {
        let n = policy.sources.len();
        if !(3..=32).contains(&n)
            || policy.version == 0
            || policy.instrument.is_empty()
            || policy.instrument.len() > 32
            || policy.issuer.is_empty()
            || policy.issuer.len() > 64
            || policy.max_age_slots == 0
            || policy.max_age_slots > 150
            || policy.max_age_ms == 0
            || policy.max_age_ms > 60_000
            || !(1..=2000).contains(&policy.max_width_bps)
            || key(&policy.base_mint)? == key(&policy.quote_mint)?
        {
            return Err("fair policy bounds".into());
        }
        let mut ids = BTreeSet::new();
        let mut unique_keys = BTreeSet::new();
        let mut groups = BTreeSet::new();
        let mut keys = Vec::with_capacity(n);
        for source in &policy.sources {
            let k = key(&source.public_key)?;
            if !ids.insert(source.id) || !unique_keys.insert(k) {
                return Err("duplicate source identity/key".into());
            }
            keys.push(VerifyingKey::from_bytes(&k).map_err(|e| e.to_string())?);
            groups.insert(source.group);
        }
        let n = groups.len();
        if !(3..=fair::MAX_GROUPS).contains(&n)
            || policy.faulty_groups == 0
            || policy.faulty_groups >= n
            || n <= 2 * policy.faulty_groups
        {
            return Err("independent group fault budget".into());
        }
        Ok(Self {
            policy_hash: hash(&policy)?,
            policy,
            keys,
            groups,
            latest: BTreeMap::new(),
            last_clock: None,
        })
    }
    pub fn policy_hash(&self) -> [u8; 32] {
        self.policy_hash
    }
    pub fn watermarks(&self) -> Watermarks {
        Watermarks {
            policy_hash: self.policy_hash,
            latest: self.latest.clone(),
            last_clock: self.last_clock,
        }
    }
    /// Restore only replay/freshness state after reconstructing and validating
    /// the configured policy. A snapshot can never replace keys or policy.
    pub fn restore_watermarks(&mut self, state: Watermarks) -> Result<()> {
        let configured: BTreeSet<_> = self.policy.sources.iter().map(|s| s.id).collect();
        if state.policy_hash != self.policy_hash
            || state.latest.len() > configured.len()
            || state
                .latest
                .iter()
                .any(|(id, (sequence, _))| !configured.contains(id) || *sequence == 0)
            || state
                .last_clock
                .is_some_and(|(slot, now_ms)| slot == 0 || now_ms == 0)
        {
            return Err("fair watermark snapshot binding".into());
        }
        self.latest = state.latest;
        self.last_clock = state.last_clock;
        Ok(())
    }
    /// The caller supplies a trusted clock and configured policy, never values
    /// selected by an incoming intent. Replay watermarks advance atomically.
    pub fn evaluate(
        &mut self,
        context: &StockContext,
        xs: &[SignedObservation],
        slot: u64,
        now_ms: u64,
    ) -> Result<Decision> {
        if xs.len() < self.groups.len()
            || xs.len() > self.policy.sources.len()
            || self.last_clock.is_some_and(|(s, t)| slot < s || now_ms < t)
            || hash(&context.policy)? != self.policy.stock_policy_hash
            || context.policy.instrument != self.policy.instrument
            || context.policy.version != self.policy.version
            || context.policy.quote_mint != self.policy.quote_mint
            || !context
                .policy
                .products
                .iter()
                .any(|p| p.mint == self.policy.base_mint && p.issuer == self.policy.issuer)
        {
            return Err("fair policy/context/clock binding".into());
        }
        let stock_hash = context.verify(slot)?;
        let state = context
            .state
            .products
            .iter()
            .find(|p| p.mint == self.policy.base_mint)
            .ok_or("missing stock state")?;
        if state.corporate_action_halt || !state.secondary {
            return Err("stock execution unavailable".into());
        }
        let mut groups: BTreeMap<u16, Interval> = BTreeMap::new();
        let mut next = self.latest.clone();
        let mut seen = BTreeSet::new();
        let mut proofs = BTreeMap::new();
        let mut expires_slot = context.state.expires_slot;
        let mut expires_ms = u64::MAX;
        for signed in xs {
            let o = &signed.observation;
            let i = self
                .policy
                .sources
                .iter()
                .position(|s| s.id == o.source_id)
                .ok_or("unknown price source")?;
            if !seen.insert(o.source_id)
                || o.policy_hash != self.policy_hash
                || o.stock_state_hash != stock_hash
                || o.sequence == 0
                || o.slot > slot
                || o.observed_ms > now_ms
                || slot - o.slot > self.policy.max_age_slots
                || now_ms - o.observed_ms > self.policy.max_age_ms
                || o.expires_slot < slot
                || o.expires_ms < now_ms
                || o.low_q32 == 0
                || o.high_q32 < o.low_q32
            {
                return Err("interval identity/freshness/shape".into());
            }
            let bytes = o.signing_bytes()?;
            let sig_bytes = bs58::decode(&signed.signature)
                .into_vec()
                .map_err(|e| e.to_string())?;
            let sig = Signature::from_slice(&sig_bytes).map_err(|e| e.to_string())?;
            self.keys[i]
                .verify_strict(&bytes, &sig)
                .map_err(|_| "interval signature")?;
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            if next.get(&o.source_id).is_some_and(|(seq, old)| {
                o.sequence < *seq || (o.sequence == *seq && digest != *old)
            }) {
                return Err("interval replay/equivocation".into());
            }
            next.insert(o.source_id, (o.sequence, digest));
            proofs.insert(o.source_id, digest);
            let group = self.policy.sources[i].group;
            groups
                .entry(group)
                .and_modify(|g| {
                    // Correlated copies can widen one group's uncertainty, never add votes.
                    g.low = g.low.min(o.low_q32);
                    g.high = g.high.max(o.high_q32);
                })
                .or_insert(Interval {
                    group,
                    low: o.low_q32,
                    high: o.high_q32,
                });
            expires_slot = expires_slot.min(o.expires_slot).min(
                o.slot
                    .checked_add(self.policy.max_age_slots)
                    .ok_or("slot overflow")?,
            );
            expires_ms = expires_ms.min(o.expires_ms).min(
                o.observed_ms
                    .checked_add(self.policy.max_age_ms)
                    .ok_or("time overflow")?,
            );
        }
        // Missing a configured group must not silently lower the quorum.
        if groups.len() != self.groups.len() {
            return Err("missing independent price group".into());
        }
        let band = fair::consensus(
            &groups.values().copied().collect::<Vec<_>>(),
            self.policy.faulty_groups,
            self.policy.max_width_bps,
        )
        .map_err(|e| format!("no admissible interval: {e:?}"))?;
        let commitment = hash(&(
            self.policy_hash,
            stock_hash,
            proofs,
            band.low,
            band.high,
            expires_slot,
            expires_ms,
        ))?;
        self.latest = next;
        self.last_clock = Some((slot, now_ms));
        Ok(Decision {
            band,
            commitment,
            stock_hash,
            policy: self.policy.clone(),
            slot,
            now_ms,
            expires_slot,
            expires_ms,
        })
    }
}
impl Decision {
    pub fn band(&self) -> Band {
        self.band
    }
    pub fn commitment(&self) -> [u8; 32] {
        self.commitment
    }
    pub fn validate(&self, context: &StockContext, slot: u64, now_ms: u64) -> Result<()> {
        if slot < self.slot
            || now_ms < self.now_ms
            || slot > self.expires_slot
            || now_ms > self.expires_ms
            || context.verify(slot)? != self.stock_hash
        {
            return Err("fair decision revoked/expired".into());
        }
        Ok(())
    }
    pub fn tighten(
        &self,
        context: &StockContext,
        intent: &EconomicIntent,
        slot: u64,
        now_ms: u64,
    ) -> Result<EconomicIntent> {
        self.validate(context, slot, now_ms)?;
        context.admit(intent, slot, Kind::Secondary)?;
        if intent.issuer != self.policy.issuer || intent.output_mint != self.policy.base_mint {
            return Err("fair decision product binding".into());
        }
        let mut out = intent.clone();
        out.min_output_atoms = self
            .band
            .min_output(intent.input_atoms, Side::BuyBase, intent.min_output_atoms)
            .map_err(|e| format!("{e:?}"))?;
        out.deadline_slot = out.deadline_slot.min(self.expires_slot);
        Ok(out)
    }
    /// Quantities are proposals. The settlement compiler must bind both owners,
    /// sequences, mints, full-order floors and every residual leg to signatures.
    #[allow(clippy::too_many_arguments)] // Explicit two-sided quantities and freshness fences.
    pub fn crossing(
        &self,
        context: &StockContext,
        slot: u64,
        now_ms: u64,
        buy_quote: u64,
        buy_min_base: u64,
        sell_base: u64,
        sell_min_quote: u64,
    ) -> Result<Crossing> {
        self.validate(context, slot, now_ms)?;
        if !context.state.underlying_open {
            return Err("closed-market crossing requires a separate signed opt-in lane".into());
        }
        fair::cross(
            self.band,
            buy_quote,
            buy_min_base,
            sell_base,
            sell_min_quote,
        )
        .map_err(|e| format!("cross rejected: {e:?}"))
    }
}
