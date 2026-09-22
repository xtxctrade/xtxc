//! Bounded native-allocation -> typed SBF graph lowering. No opaque CPI bytes,
//! caller-supplied privileges, signing or submission. The account reader must
//! refer to ONE frozen bank; exact simulation and economic admission still follow.
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{pubkey, Pubkey};
use std::{collections::BTreeSet, str::FromStr};
use stocklana_adapters::{
    self as dex,
    graph::{Graph, Venue, MAX_ACCOUNTS, MAX_ASSETS, MAX_CANDIDATES, MAX_LEGS},
};

type Result<T> = std::result::Result<T, String>;
const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN22: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

#[derive(Clone, Copy, Debug)]
pub struct AccountView<'a> {
    pub owner: Pubkey,
    pub executable: bool,
    pub data: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TokenAsset {
    pub token: Pubkey,
    pub mint: Pubkey,
    pub token_program: Pubkey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Budget {
    /// Integer solver allocation. Never derive this from display basis points.
    Exact(u64),
    /// Only the last consumer of this asset. The SBF runtime uses the signed
    /// input remainder, or the observed positive intermediate balance delta.
    Remaining,
}

#[derive(Clone, Debug)]
pub struct SwapLeg {
    pub venue: Venue,
    pub direction: bool,
    pub source: Pubkey,
    pub destination: Pubkey,
    pub budget: Budget,
    /// Canonical typed venue ABI order, including current tick/bin accounts.
    /// Programs and writable/signer privileges are derived, not accepted here.
    pub accounts: Vec<Pubkey>,
}

#[derive(Clone, Debug)]
pub struct SwapGraph {
    pub owner: Pubkey,
    pub sequence: u64,
    pub input_atoms: u64,
    pub minimum_output_atoms: u64,
    pub deadline_slot: u64,
    pub input: TokenAsset,
    pub output: TokenAsset,
    pub intermediates: Vec<TokenAsset>,
    pub legs: Vec<SwapLeg>,
}

fn ensure(value: bool, message: &str) -> Result<()> {
    if value {
        Ok(())
    } else {
        Err(message.into())
    }
}

pub fn compile_swap_graph<'a>(
    program: Pubkey,
    spec: &SwapGraph,
    read: impl Fn(&Pubkey) -> Result<AccountView<'a>>,
) -> Result<Instruction> {
    compile_graph(program, spec, Routing::Fixed, read)
}

/// Opcodes 4/9 treat legs as competing candidates for the SAME input/output,
/// not an already allocated split. Exact budgets cap a candidate execution;
/// Remaining admits up to the residual signed intent. At most four actual
/// CPIs execute, even when the candidate set has up to eight members.
/// A seed is a signed first-candidate amount; subsequent rounds solve only
/// the observed residual. Admission does not guarantee the route fits CU.
pub fn compile_reflow_graph<'a>(
    program: Pubkey,
    spec: &SwapGraph,
    queries_per_round: u8,
    seed_input_atoms: Option<u64>,
    read: impl Fn(&Pubkey) -> Result<AccountView<'a>>,
) -> Result<Instruction> {
    ensure(
        (8..=64).contains(&queries_per_round),
        "reflow oracle work bound",
    )?;
    ensure(
        spec.intermediates.is_empty(),
        "reflow direct candidate assets",
    )?;
    if let Some(seed) = seed_input_atoms {
        ensure(seed > 0 && seed < spec.input_atoms, "reflow seed bounds")?;
    }
    for leg in &spec.legs {
        ensure(
            leg.source == spec.input.token
                && leg.destination == spec.output.token
                && matches!(leg.venue, Venue::RaydiumClmm | Venue::ByrealClmm),
            "reflow native candidate admission",
        )?;
    }
    compile_graph(
        program,
        spec,
        Routing::Reflow {
            calls: queries_per_round,
            seed: seed_input_atoms,
        },
        read,
    )
}

/// Compile one bounded candidate graph whose destinations are economically
/// equivalent issuer products rather than the same mint. `output` plus every
/// `intermediate` is a stock-product asset. A product can also be the first hop
/// of a bounded issuer conversion, such as cash->SPYx->SPYon. Every noncash
/// source must have a direct cash producer; the settlement program consumes
/// only that producer's observed delta and applies the terminal product's
/// signed exposure conversion.
pub fn compile_economic_reflow_graph<'a>(
    program: Pubkey,
    spec: &SwapGraph,
    queries_per_round: u8,
    read: impl Fn(&Pubkey) -> Result<AccountView<'a>>,
) -> Result<Instruction> {
    ensure(
        (8..=64).contains(&queries_per_round),
        "economic reflow oracle work bound",
    )?;
    let outputs = spec
        .intermediates
        .iter()
        .chain(core::iter::once(&spec.output))
        .map(|asset| asset.token)
        .collect::<BTreeSet<_>>();
    let direct = spec
        .legs
        .iter()
        .filter(|leg| leg.source == spec.input.token)
        .map(|leg| leg.destination)
        .collect::<BTreeSet<_>>();
    ensure(
        !outputs.is_empty()
            && spec.legs.iter().all(|leg| {
                outputs.contains(&leg.destination)
                    && (leg.source == spec.input.token || direct.contains(&leg.source))
                    && matches!(
                        leg.venue,
                        Venue::RaydiumClmm
                            | Venue::ByrealClmm
                            | Venue::OrcaWhirlpool
                            | Venue::MeteoraDlmm
                    )
            })
            && outputs
                .iter()
                .all(|output| spec.legs.iter().any(|leg| leg.destination == *output)),
        "economic reflow bounded path admission",
    )?;
    compile_graph(
        program,
        spec,
        Routing::Economic {
            calls: queries_per_round,
        },
        read,
    )
}

#[derive(Clone, Copy)]
enum Routing {
    Fixed,
    Reflow { calls: u8, seed: Option<u64> },
    Economic { calls: u8 },
}

fn compile_graph<'a>(
    program: Pubkey,
    spec: &SwapGraph,
    routing: Routing,
    read: impl Fn(&Pubkey) -> Result<AccountView<'a>>,
) -> Result<Instruction> {
    let reflow = !matches!(routing, Routing::Fixed);
    let economic = matches!(routing, Routing::Economic { .. });
    // Instructions sysvar is synthesized by the runtime from the exact
    // transaction. It is not a bank account and cannot be obtained by RPC.
    // We only derive its read-only identity here; exact-wire SBF simulation
    // must supply and verify the real instruction context.
    let read_bank = read;
    let read = |key: &Pubkey| {
        if *key == pubkey!("Sysvar1nstructions1111111111111111111111111") {
            Ok(AccountView {
                owner: pubkey!("Sysvar1111111111111111111111111111111111111"),
                executable: false,
                data: &[] as &[u8],
            })
        } else {
            read_bank(key)
        }
    };
    ensure(
        program != Pubkey::default()
            && spec.owner != Pubkey::default()
            && program != spec.owner
            && spec.sequence != u64::MAX
            && spec.deadline_slot != 0
            && spec.minimum_output_atoms != 0
            && (1..=dex::MAX_INPUT).contains(&spec.input_atoms)
            && (1..=if reflow { MAX_CANDIDATES } else { MAX_LEGS }).contains(&spec.legs.len())
            && spec.intermediates.len() + 2 <= MAX_ASSETS,
        "swap graph bounds",
    )?;
    let nonce = Pubkey::find_program_address(&[b"stocklana", spec.owner.as_ref()], &program).0;
    let n = read(&nonce)?;
    ensure(
        n.owner == program
            && !n.executable
            && n.data.len() == 64
            && &n.data[..8] == b"SKEWSEQ1"
            && n.data[8..40] == spec.owner.to_bytes()
            && dex::u64_at(n.data, 40).map_err(|_| "nonce layout")? == spec.sequence,
        "swap graph nonce binding",
    )?;

    let mut assets = vec![spec.input];
    assets.extend_from_slice(&spec.intermediates);
    assets.push(spec.output);
    let mut tokens = BTreeSet::new();
    let mut mints = BTreeSet::new();
    let mut readonly = BTreeSet::from([
        program,
        pubkey!("Sysvar1nstructions1111111111111111111111111"),
    ]);
    for asset in &assets {
        ensure(
            tokens.insert(asset.token)
                && mints.insert(asset.mint)
                && [TOKEN, TOKEN22].contains(&asset.token_program),
            "swap asset identity",
        )?;
        readonly.insert(asset.mint);
        readonly.insert(asset.token_program);
        let t = read(&asset.token)?;
        let m = read(&asset.mint)?;
        let p = read(&asset.token_program)?;
        ensure(
            t.owner == asset.token_program
                && m.owner == asset.token_program
                && !t.executable
                && !m.executable
                && p.executable
                && t.data.len() >= 165
                && t.data[108] == 1
                && t.data[72..76] == [0; 4]
                && t.data[121..129] == [0; 8]
                && t.data[129..133] == [0; 4]
                && t.data[..32] == asset.mint.to_bytes()
                && t.data[32..64] == spec.owner.to_bytes()
                && m.data.len() >= 82
                && m.data[45] == 1,
            "swap asset bank binding",
        )?;
    }
    ensure(
        tokens.is_disjoint(&readonly)
            && !tokens.contains(&spec.owner)
            && !tokens.contains(&nonce)
            && !readonly.contains(&spec.owner)
            && !readonly.contains(&nonce),
        "swap reserved account alias",
    )?;

    // Stable asset topological order. The leg execution order itself is never
    // silently changed: all producers must run before an intermediate drains.
    let mut ordered = vec![spec.input];
    while ordered.len() + 1 < assets.len() {
        let next = assets
            .iter()
            .find(|asset| {
                asset.token != spec.output.token
                    && !ordered.contains(asset)
                    && spec
                        .legs
                        .iter()
                        .filter(|leg| leg.destination == asset.token)
                        .all(|leg| ordered.iter().any(|a| a.token == leg.source))
            })
            .copied()
            .ok_or("swap graph cycle")?;
        ordered.push(next);
    }
    ordered.push(spec.output);
    let mut fixed_input = 0u64;
    let mut input_remainder = false;
    for (i, leg) in spec.legs.iter().enumerate() {
        let source = ordered
            .iter()
            .position(|a| a.token == leg.source)
            .ok_or("swap source asset")?;
        let destination = ordered
            .iter()
            .position(|a| a.token == leg.destination)
            .ok_or("swap destination asset")?;
        ensure(source < destination, "swap topological edge")?;
        if reflow {
            if let Budget::Exact(amount) = leg.budget {
                ensure(
                    (1..=dex::MAX_INPUT).contains(&amount),
                    "reflow candidate cap",
                )?;
            }
            continue;
        }
        let last = !spec.legs[i + 1..]
            .iter()
            .any(|later| later.source == leg.source);
        if source != 0 {
            ensure(
                spec.legs[..i]
                    .iter()
                    .any(|earlier| earlier.destination == leg.source)
                    && !spec.legs[i..]
                        .iter()
                        .any(|later| later.destination == leg.source),
                "swap intermediate producer order",
            )?;
            ensure(
                !last || leg.budget == Budget::Remaining,
                "swap intermediate must drain observed output",
            )?;
        }
        match leg.budget {
            Budget::Remaining => {
                ensure(last, "swap remainder must be last consumer")?;
                input_remainder |= source == 0;
            }
            Budget::Exact(amount) => {
                ensure((1..=dex::MAX_INPUT).contains(&amount), "swap exact budget")?;
                if source == 0 {
                    fixed_input = fixed_input
                        .checked_add(amount)
                        .ok_or("swap input overflow")?;
                }
            }
        }
    }
    ensure(
        if reflow {
            true
        } else if input_remainder {
            fixed_input < spec.input_atoms
        } else {
            fixed_input == spec.input_atoms
        },
        "swap input allocation conservation",
    )?;
    for asset in ordered.iter().skip(1) {
        ensure(
            spec.legs.iter().any(|leg| leg.destination == asset.token),
            "swap unproduced asset",
        )?;
        if !economic && asset.token != spec.output.token {
            ensure(
                spec.legs.iter().any(|leg| leg.source == asset.token),
                "swap unconsumed asset",
            )?;
        }
    }

    let mut metas = vec![
        AccountMeta::new_readonly(spec.owner, true),
        AccountMeta::new(nonce, false),
    ];
    let mut index = |key: Pubkey, writable: bool| -> Result<u8> {
        ensure(
            !writable || !readonly.contains(&key),
            "swap writable protected account",
        )?;
        if let Some(i) = metas.iter().position(|meta| meta.pubkey == key) {
            metas[i].is_writable |= writable;
            Ok(i as u8)
        } else {
            ensure(metas.len() < MAX_ACCOUNTS, "swap account bound")?;
            let i = metas.len() as u8;
            metas.push(AccountMeta {
                pubkey: key,
                is_writable: writable,
                is_signer: key == spec.owner,
            });
            Ok(i)
        }
    };
    let (opcode, calls, seed) = match routing {
        Routing::Fixed => (2, 0, None),
        Routing::Reflow { calls, seed } => (if seed.is_some() { 9 } else { 4 }, calls, seed),
        Routing::Economic { calls } => (4, calls, None),
    };
    let mut data = vec![opcode, ordered.len() as u8, spec.legs.len() as u8, calls];
    for value in [
        spec.sequence,
        spec.input_atoms,
        spec.minimum_output_atoms,
        spec.deadline_slot,
    ] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    for asset in &ordered {
        data.extend_from_slice(&[
            index(asset.token, true)?,
            index(asset.mint, false)?,
            index(asset.token_program, false)?,
        ]);
    }
    if let Some(seed) = seed {
        data.extend_from_slice(&seed.to_le_bytes());
    }
    for leg in &spec.legs {
        let id = Pubkey::from_str(leg.venue.program()).map_err(|_| "swap program identity")?;
        ensure(
            id != program && read(&id)?.executable,
            "swap executable program",
        )?;
        let (lo, hi) = leg.venue.account_bounds();
        ensure(
            (lo..=hi).contains(&leg.accounts.len()),
            "swap CPI account count",
        )?;
        let (pool, signer, src, dst) = leg.venue.bindings(leg.direction);
        ensure(
            leg.accounts[signer] == spec.owner
                && leg.accounts[src] == leg.source
                && leg.accounts[dst] == leg.destination
                && read(&leg.accounts[pool])?.owner == id
                && !read(&leg.accounts[pool])?.executable,
            "swap venue account binding",
        )?;
        let si = ordered
            .iter()
            .position(|a| a.token == leg.source)
            .ok_or("swap source")?;
        let di = ordered
            .iter()
            .position(|a| a.token == leg.destination)
            .ok_or("swap sink")?;
        let budget = match leg.budget {
            Budget::Exact(amount) => amount,
            Budget::Remaining => u64::MAX,
        };
        data.extend_from_slice(&[leg.venue as u8, si as u8, di as u8, u8::from(leg.direction)]);
        data.extend_from_slice(&budget.to_le_bytes());
        data.extend_from_slice(&[index(id, false)?, leg.accounts.len() as u8]);
        for (position, key) in leg.accounts.iter().enumerate() {
            ensure(
                *key != nonce && (!tokens.contains(key) || position == src || position == dst),
                "swap CPI asset alias",
            )?;
            let info = read(key)?;
            let writable = *key != id && leg.venue.writable_with_owner(position, info.owner == id);
            ensure(!writable || !info.executable, "swap writable executable")?;
            if writable && [TOKEN, TOKEN22].contains(&info.owner) && info.data.len() >= 165 {
                ensure(
                    info.data[32..64] != spec.owner.to_bytes() || tokens.contains(key),
                    "swap undeclared wallet token",
                )?;
            }
            data.push(index(*key, writable)?);
        }
    }
    let decoded =
        Graph::decode(&data, metas.len()).map_err(|error| format!("swap graph ABI: {error:?}"))?;
    if reflow {
        // Native marginal curves assume independent market resources. Shared
        // read-only oracles are fine; any cross-candidate read/write conflict
        // outside the owner's two balance accounts invalidates that model.
        for (i, leg) in decoded.legs[..decoded.leg_count]
            .iter()
            .flatten()
            .enumerate()
        {
            for prior in decoded.legs[..i].iter().flatten() {
                for index in leg.accounts {
                    let meta = &metas[usize::from(*index)];
                    if meta.pubkey != spec.owner
                        && !ordered.iter().any(|asset| asset.token == meta.pubkey)
                        && meta.is_writable
                        && prior.accounts.contains(index)
                    {
                        return Err("reflow shared market resource".into());
                    }
                }
            }
        }
    }
    Ok(Instruction {
        program_id: program,
        accounts: metas,
        data,
    })
}

#[cfg(test)]
#[path = "swap_wire/tests.rs"]
mod tests;
