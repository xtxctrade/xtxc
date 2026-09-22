//! Coherent state -> fair-policy floor -> typed signed constraints -> exact RPC
//! simulation -> owner-provided signature. This module contains no signing key.
use crate::{
    fair::Decision,
    feed::{Feed, Snapshot},
    receipt::Expected,
    rpc::Rpc,
    sender::{self, Authorization},
    stock::{EconomicIntent, StockContext},
    Result,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use bincode::Options;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use solana_message::VersionedMessage;
use stocklana_adapters::{graph::Graph, u64_at};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub settlement_program: String,
    pub stock_policy_account: String,
    pub maximum_cu: u64,
    pub maximum_compute_price: u64,
    pub quoted_output: u64,
    pub route: [u8; 32],
}
pub struct Prepared {
    hash: [u8; 32],
    fair_commitment: [u8; 32],
    revision: u64,
    state_hash: [u8; 32],
    slot: u64,
    expected: Expected,
    resources: Vec<[u8; 32]>,
    simulation_cu: u64,
    simulation_output: u64,
}
fn key(s: &str) -> Result<[u8; 32]> {
    bs58::decode(s)
        .into_vec()
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "public key length".into())
}
pub fn decode(message: &[u8]) -> Result<VersionedMessage> {
    let msg: VersionedMessage = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(1232)
        .reject_trailing_bytes()
        .deserialize(message)
        .map_err(|e| e.to_string())?;
    msg.sanitize().map_err(|e| e.to_string())?;
    if msg.serialize() != message {
        return Err("noncanonical message".into());
    }
    Ok(msg)
}
pub fn resolved(msg: &VersionedMessage, s: &Snapshot) -> Result<Vec<String>> {
    let mut keys: Vec<_> = msg
        .static_account_keys()
        .iter()
        .map(|k| k.to_string())
        .collect();
    if let Some(tables) = msg.address_table_lookups() {
        if tables.len() > 4 {
            return Err("ALT count".into());
        }
        let mut writable = Vec::new();
        let mut readonly = Vec::new();
        for table in tables {
            let a = s
                .accounts
                .iter()
                .find(|a| a.key == table.account_key.to_string())
                .ok_or("ALT not in coherent snapshot")?;
            let d = &a.data;
            if a.owner != "AddressLookupTab1e1111111111111111111111111"
                || a.executable
                || d.len() < 56
                || d[..4] != 1u32.to_le_bytes()
                || u64_at(d, 4).map_err(|_| "ALT layout")? != u64::MAX
                || u64_at(d, 12).map_err(|_| "ALT layout")? >= s.slot
                || d[21] != 0
                || (d.len() - 56) % 32 != 0
            {
                return Err("only frozen active ALT from an earlier slot is admitted".into());
            }
            for (indices, out) in [
                (&table.writable_indexes, &mut writable),
                (&table.readonly_indexes, &mut readonly),
            ] {
                for i in indices {
                    let start = 56 + usize::from(*i) * 32;
                    out.push(
                        bs58::encode(d.get(start..start + 32).ok_or("ALT index")?).into_string(),
                    );
                }
            }
        }
        keys.extend(writable);
        keys.extend(readonly);
    }
    if keys.len() > 64 || keys.iter().collect::<std::collections::BTreeSet<_>>().len() != keys.len()
    {
        return Err("account count/alias".into());
    }
    Ok(keys)
}
fn constraints(
    s: &Snapshot,
    context: &StockContext,
    intent: &EconomicIntent,
    limits: &Limits,
    message: &[u8],
) -> Result<(Expected, Vec<[u8; 32]>)> {
    let msg = decode(message)?;
    if msg.header().num_required_signatures != 1
        || message.len() + 65 > 1232
        || limits.maximum_cu == 0
        || limits.maximum_cu > 1_400_000
    {
        return Err("single-owner lane resource bounds".into());
    }
    let keys = resolved(&msg, s)?;
    let mut graph_ix = None;
    let mut cu = None;
    let mut price = None;
    for ix in msg.instructions() {
        let program = keys
            .get(ix.program_id_index as usize)
            .ok_or("program index")?;
        if program == &limits.settlement_program {
            if graph_ix.replace(ix).is_some() {
                return Err("one settlement only".into());
            }
        } else if program == "ComputeBudget111111111111111111111111111111" && ix.accounts.is_empty()
        {
            match ix.data.first() {
                Some(2) if ix.data.len() == 5 && cu.is_none() => {
                    cu = Some(u32::from_le_bytes(ix.data[1..5].try_into().unwrap()) as u64)
                }
                Some(3) if ix.data.len() == 9 && price.is_none() => {
                    price = Some(u64_at(&ix.data, 1).map_err(|_| "compute price")?)
                }
                _ => return Err("compute instruction not admitted".into()),
            }
        } else {
            return Err("unexpected top-level action".into());
        }
    }
    if cu.is_none_or(|c| c == 0 || c > limits.maximum_cu)
        || price.unwrap_or(0) > limits.maximum_compute_price
    {
        return Err("compute fee/budget bounds".into());
    }
    let ix = graph_ix.ok_or("settlement missing")?;
    let d = &ix.data;
    if d.len() < 20
        || d[0] != 7
        || d[3] != 0
        || d[2] != u8::from(intent.allow_underlying_closed)
        || u64_at(d, 4).map_err(|_| "stock version")? != intent.version
        || !(1..=context.policy.max_state_lag_slots)
            .contains(&u64_at(d, 12).map_err(|_| "stock age")?)
    {
        return Err("signed stock guard required".into());
    }
    let accounts: Vec<_> = ix
        .accounts
        .iter()
        .map(|i| {
            keys.get(*i as usize)
                .cloned()
                .ok_or_else(|| "account index".to_string())
        })
        .collect::<Result<_>>()?;
    if accounts.get(d[1] as usize) != Some(&limits.stock_policy_account) {
        return Err("stock policy substitution".into());
    }
    let g = Graph::decode(&d[20..], accounts.len()).map_err(|_| "typed graph rejected")?;
    let account = |i: u8| {
        accounts
            .get(i as usize)
            .cloned()
            .ok_or_else(|| "asset index".to_string())
    };
    if g.asset_count != 2
        || g.input != intent.input_atoms
        || g.min_out < intent.min_output_atoms
        || g.deadline > intent.deadline_slot
        || g.deadline < s.slot
        || account(g.assets[0].mint)? != intent.input_mint
        || account(g.assets[1].mint)? != intent.output_mint
        || accounts[0] != keys[0]
    {
        return Err("signed economic constraints differ from admitted intent".into());
    }
    let x = Expected {
        native_output: None,
        owner: keys[0].clone(),
        input_account: account(g.assets[0].token)?,
        input_mint: intent.input_mint.clone(),
        output_account: account(g.assets[1].token)?,
        output_mint: intent.output_mint.clone(),
        input: g.input,
        minimum_output: g.min_out,
        quoted_output: limits.quoted_output,
        maximum_cu: limits.maximum_cu,
        route: limits.route,
    };
    if x.quoted_output < x.minimum_output {
        return Err("quoted output below signed floor".into());
    }
    // Writable message accounts include the owner nonce and all DEX write sets.
    let resources = keys
        .iter()
        .enumerate()
        .filter(|(i, _)| msg.is_maybe_writable(*i, None))
        .map(|(_, k)| key(k))
        .collect::<Result<Vec<_>>>()?;
    Ok((x, resources))
}
fn balance(s: &Snapshot, k: &str, mint: &str, owner: &str) -> Result<u64> {
    let a = s
        .accounts
        .iter()
        .find(|a| a.key == k)
        .ok_or("wallet account missing from coherent snapshot")?;
    token(&a.data, mint, owner)
}
fn token(d: &[u8], mint: &str, owner: &str) -> Result<u64> {
    if d.len() < 165 || d[..32] != key(mint)? || d[32..64] != key(owner)? || d[108] != 1 {
        return Err("wallet token identity".into());
    }
    u64_at(d, 64).map_err(|_| "wallet balance".into())
}
impl Prepared {
    #[allow(clippy::too_many_arguments)] // Keep the independent admission bindings visible.
    pub fn simulate(
        feed: &Feed,
        s: &Snapshot,
        rpc: &Rpc,
        context: &StockContext,
        decision: &Decision,
        intent: &EconomicIntent,
        limits: &Limits,
        message: &[u8],
        now_ms: u64,
    ) -> Result<Self> {
        feed.validate_fence(s)?;
        let strict = decision.tighten(context, intent, s.slot, now_ms)?;
        let (mut expected, resources) = constraints(s, context, &strict, limits, message)?;
        let before_in = balance(
            s,
            &expected.input_account,
            &expected.input_mint,
            &expected.owner,
        )?;
        let before_out = balance(
            s,
            &expected.output_account,
            &expected.output_mint,
            &expected.owner,
        )?;
        let mut unsigned = vec![1];
        unsigned.extend([0; 64]);
        unsigned.extend(message);
        rpc.check_genesis()?;
        let result=rpc.call("simulateTransaction",json!([STANDARD.encode(unsigned),{"encoding":"base64","sigVerify":false,
            "replaceRecentBlockhash":false,"commitment":"confirmed","minContextSlot":s.slot,
            "accounts":{"encoding":"base64","addresses":[expected.input_account,expected.output_account]}}]))?;
        if result["context"]["slot"].as_u64() != Some(s.slot)
            || !result["value"]
                .get("err")
                .ok_or("simulation status")?
                .is_null()
        {
            return Err("simulation failed or bank changed; rebuild".into());
        }
        let cu = result["value"]["unitsConsumed"]
            .as_u64()
            .ok_or("simulation CU missing")?;
        let accounts = result["value"]["accounts"]
            .as_array()
            .ok_or("simulation balances missing")?;
        if accounts.len() != 2 || cu == 0 || cu > limits.maximum_cu {
            return Err("simulation resource bound".into());
        }
        let mut amounts = [0u64; 2];
        for (i, a) in accounts.iter().enumerate() {
            let data = STANDARD
                .decode(a["data"][0].as_str().ok_or("simulation account")?)
                .map_err(|e| e.to_string())?;
            amounts[i] = token(
                &data,
                if i == 0 {
                    &expected.input_mint
                } else {
                    &expected.output_mint
                },
                &expected.owner,
            )?;
        }
        let output = amounts[1]
            .checked_sub(before_out)
            .ok_or("simulation output direction")?;
        if before_in.checked_sub(amounts[0]) != Some(expected.input)
            || output < expected.minimum_output
        {
            return Err("simulation postcondition".into());
        }
        feed.validate_fence(s)?;
        expected.quoted_output = output;
        Ok(Self {
            hash: Sha256::digest(message).into(),
            fair_commitment: decision.commitment(),
            revision: s.revision,
            state_hash: s.hash,
            slot: s.slot,
            expected,
            resources,
            simulation_cu: cu,
            simulation_output: output,
        })
    }
    pub fn summary(&self) -> Value {
        json!({"approvedMessageHash":self.hash,"fairCommitment":self.fair_commitment,
        "revision":self.revision,"simulationCU":self.simulation_cu,"simulationOutput":self.simulation_output,
        "minimumOutput":self.expected.minimum_output,"requiresOwnerSignature":true})
    }
    pub fn expected(&self) -> &Expected {
        &self.expected
    }
    #[allow(clippy::too_many_arguments)] // Recheck all bindings at the external signer boundary.
    pub fn authorize(
        &self,
        feed: &Feed,
        s: &Snapshot,
        context: &StockContext,
        decision: &Decision,
        wire: &[u8],
        id: String,
        last_valid_height: u64,
        now_ms: u64,
    ) -> Result<crate::journal::Entry> {
        feed.validate_fence(s)?;
        decision.validate(context, s.slot, now_ms)?;
        if s.revision != self.revision
            || s.hash != self.state_hash
            || s.slot != self.slot
            || decision.commitment() != self.fair_commitment
        {
            return Err("prepared decision revoked".into());
        }
        sender::authorize(
            wire,
            Authorization {
                intent_id: id,
                message_hash: self.hash,
                last_valid_height,
                resources: self.resources.clone(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stock::{Policy, State};
    use std::time::Instant;
    fn fixture() -> (Snapshot, StockContext, EconomicIntent, Limits, Vec<u8>) {
        let mut keys: Vec<_> = (1u8..=17)
            .map(|i| bs58::encode([i; 32]).into_string())
            .collect();
        keys[16] = "ComputeBudget111111111111111111111111111111".into();
        let i = EconomicIntent {
            instrument: "NVDA".into(),
            version: 1,
            input_mint: keys[3].clone(),
            output_mint: keys[6].clone(),
            issuer: "fixture".into(),
            input_atoms: 100,
            min_output_atoms: 50,
            deadline_slot: 102,
            allow_underlying_closed: false,
        };
        let c = StockContext {
            policy: Policy {
                instrument: "NVDA".into(),
                version: 1,
                attestor: keys[0].clone(),
                quote_mint: keys[3].clone(),
                products: vec![],
                max_state_lag_slots: 2,
            },
            state: State {
                instrument: "NVDA".into(),
                version: 1,
                sequence: 1,
                slot: 100,
                expires_slot: 102,
                underlying_open: true,
                products: vec![],
            },
            signature: String::new(),
        };
        let l = Limits {
            settlement_program: keys[15].clone(),
            stock_policy_account: keys[14].clone(),
            maximum_cu: 1400000,
            maximum_compute_price: 0,
            quoted_output: 55,
            route: [7; 32],
        };
        let mut graph = vec![2, 2, 1, 0];
        for n in [0u64, 100, 50, 102] {
            graph.extend(n.to_le_bytes());
        }
        graph.extend([2, 3, 4, 5, 6, 4]);
        graph.extend([1, 0, 1, 1]);
        graph.extend(100u64.to_le_bytes());
        graph.extend([13, 13]);
        graph.extend([0, 7, 8, 9, 2, 5, 10, 11, 4, 4, 3, 6, 12]);
        let mut guard = vec![7, 14, 0, 0];
        guard.extend(1u64.to_le_bytes());
        guard.extend(2u64.to_le_bytes());
        guard.extend(graph);
        let mut msg = vec![1, 0, 2, 17];
        for k in &keys {
            msg.extend(key(k).unwrap());
        }
        msg.extend([0; 32]);
        msg.extend([2, 16, 0, 5, 2]);
        msg.extend(1400000u32.to_le_bytes());
        msg.extend([15, 15]);
        msg.extend(0u8..15);
        assert!(guard.len() < 128);
        msg.push(guard.len() as u8);
        msg.extend(guard);
        let now = Instant::now();
        let s = Snapshot {
            slot: 100,
            generation: 1,
            hash: [1; 32],
            accounts: vec![],
            observed: now,
            slot_advanced: now,
            revision: 1,
        };
        (s, c, i, l, msg)
    }
    #[test]
    fn signed_message_cannot_weaken_model_floor_or_add_an_action() {
        let (s, c, i, l, bytes) = fixture();
        constraints(&s, &c, &i, &l, &bytes).unwrap();
        for attack in [
            "floor",
            "input",
            "deadline",
            "version",
            "policy",
            "mint",
            "owner",
            "unguarded",
            "extra_action",
            "cu",
        ] {
            let mut msg = decode(&bytes).unwrap();
            let VersionedMessage::Legacy(ref mut m) = msg else {
                unreachable!()
            };
            match attack {
                "floor" => m.instructions[1].data[40..48].copy_from_slice(&49u64.to_le_bytes()),
                "input" => m.instructions[1].data[32..40].copy_from_slice(&101u64.to_le_bytes()),
                "deadline" => m.instructions[1].data[48..56].copy_from_slice(&103u64.to_le_bytes()),
                "version" => m.instructions[1].data[4..12].copy_from_slice(&2u64.to_le_bytes()),
                "policy" => m.instructions[1].data[1] = 13,
                "mint" => m.instructions[1].data[57] = 6,
                "owner" => m.instructions[1].accounts[0] = 7,
                "unguarded" => m.instructions[1].data.drain(..20).for_each(drop),
                "extra_action" => m.instructions.push(m.instructions[1].clone()),
                "cu" => m.instructions[0].data[1..5].copy_from_slice(&1400001u32.to_le_bytes()),
                _ => unreachable!(),
            }
            assert!(
                constraints(&s, &c, &i, &l, &msg.serialize()).is_err(),
                "{attack}"
            );
        }
    }
}
