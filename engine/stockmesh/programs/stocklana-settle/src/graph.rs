//! Typed multi-venue execution. Each CPI spends only its signed budget or funds
//! actually produced by earlier legs. All asset balances are checked per CPI.
use super::*;
use dex::graph::{Graph, Leg, Venue, MAX_ASSETS, MAX_CPI_ACCOUNTS};

pub(super) const TOKEN_2022: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const PROGRAMS: [Pubkey; 9] = [
    PHOENIX,
    RAYDIUM,
    pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"),
    pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK"),
    pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo"),
    pubkey!("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG"),
    pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"),
    pubkey!("REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2"),
    pubkey!("riptK81hDxhe5pW5jSzSM9iRA8azgEgLJ4dXkPtBS7j"),
];

// Public transfers only. An inactive hook and an unpaused pausable mint are
// admitted after inspecting their actual extension bytes. Active hooks and
// confidential account balances still require a distinct backend.
fn extensions(d: &[u8], mint: bool) -> ProgramResult {
    let base = if mint { 82 } else { 165 };
    if d.len() == base {
        return Ok(());
    }
    need(
        d.len() >= 166 && d[165] == if mint { 1 } else { 2 },
        IDENTITY,
    )?;
    if mint {
        need(d[82..165].iter().all(|b| *b == 0), IDENTITY)?;
    }
    let mut p = 166;
    let mut seen = 0u64;
    while p < d.len() {
        if d[p] == 0 && d[p..].iter().all(|b| *b == 0) {
            break;
        }
        need(p + 4 <= d.len(), IDENTITY)?;
        let kind = u16::from_le_bytes([d[p], d[p + 1]]);
        let len = u16::from_le_bytes([d[p + 2], d[p + 3]]) as usize;
        let allowed = if mint {
            [1, 3, 4, 6, 10, 12, 14, 16, 18, 19, 25, 26].contains(&kind)
        } else {
            [2, 7, 15, 27].contains(&kind)
        };
        need(allowed && kind < 64 && seen & (1u64 << kind) == 0, IDENTITY)?;
        seen |= 1u64 << kind;
        let ext = d.get(p + 4..p + 4 + len).ok_or(err(IDENTITY))?;
        match (mint, kind) {
            (true, 14) => need(len == 64 && ext[32..].iter().all(|b| *b == 0), IDENTITY)?,
            // Confidential-transfer fee configuration belongs to the mint and
            // does not turn a public token account into a confidential one.
            // Account extension 5/17 remains unadmitted below, so the graph
            // cannot conceal spendable balance from conservation checks.
            (true, 16) => need(len == 129, IDENTITY)?,
            (true, 26) => need(len == 33 && ext[32] == 0, IDENTITY)?,
            (true, 6) => need(len == 1 && ext[0] == 1, IDENTITY)?,
            (false, 15) => need(len == 1 && ext[0] == 0, IDENTITY)?,
            (false, 27) | (false, 7) => need(len == 0, IDENTITY)?,
            (false, 2) => need(len == 8, IDENTITY)?,
            _ => {}
        }
        p = p.checked_add(4 + len).ok_or(err(BOUNDS))?;
        need(p <= d.len(), IDENTITY)?;
    }
    Ok(())
}
fn balance(a: &AccountInfo) -> Result<u64, ProgramError> {
    let d = a.try_borrow_data()?;
    need(d.len() >= 165 && d[108] == 1, IDENTITY)?;
    ad(dex::u64_at(&d, 64))
}
fn asset(
    a: &[AccountInfo],
    g: &Graph,
    i: usize,
    inspect_mint: bool,
    wallet_index: usize,
) -> Result<u64, ProgramError> {
    let spec = g.assets[i];
    let t = &a[spec.token as usize];
    let m = &a[spec.mint as usize];
    let p = &a[spec.program as usize];
    checked_asset(&a[wallet_index], t, m, p, inspect_mint)
}
pub(super) fn checked_asset(
    wallet: &AccountInfo,
    t: &AccountInfo,
    m: &AccountInfo,
    p: &AccountInfo,
    inspect_mint: bool,
) -> Result<u64, ProgramError> {
    need(
        (p.key == &TOKEN || p.key == &TOKEN_2022)
            && p.executable
            && t.owner == p.key
            && m.owner == p.key
            && t.is_writable
            && !m.is_writable,
        IDENTITY,
    )?;
    let td = t.try_borrow_data()?;
    need(
        td.len() >= 165
            && td[108] == 1
            // A swap authorizes balance movement, never a persistent delegate
            // or replacement close authority on an owner-controlled asset.
            && td[72..76] == [0; 4]
            && td[121..129] == [0; 8]
            && td[129..133] == [0; 4]
            && ad(dex::key(&td, 0))? == m.key.to_bytes()
            && ad(dex::key(&td, 32))? == wallet.key.to_bytes(),
        IDENTITY,
    )?;
    if p.key == &TOKEN {
        need(td.len() == 165, IDENTITY)?;
    } else {
        extensions(&td, false)?;
    }
    // Mints are read-only in the parent instruction and cannot change in any
    // CPI. Inspect their extensions once; mutable token state is checked again.
    if inspect_mint {
        let md = m.try_borrow_data()?;
        need(md.len() >= 82 && md[45] == 1, IDENTITY)?;
        if p.key == &TOKEN {
            need(md.len() == 82, IDENTITY)?;
        } else {
            extensions(&md, true)?;
        }
    }
    ad(dex::u64_at(&td, 64))
}

pub(super) fn settle_graph(program: &Pubkey, a: &[AccountInfo], d: &[u8]) -> ProgramResult {
    settle_graph_for(program, a, d, 0, 1)
}

/// Validate a graph for an arbitrary signer/nonce pair before any FlowCell CPI.
/// Asset and leg indices remain relative to the parent instruction's account
/// list, allowing several owners to execute disjoint residual graphs atomically.
pub(super) fn preflight_graph_for(
    program: &Pubkey,
    a: &[AccountInfo],
    d: &[u8],
    wallet_index: usize,
    nonce_index: usize,
) -> ProgramResult {
    preflight_graph_for_allowing(program, a, d, wallet_index, nonce_index, &[])
}

/// Aggregate instructions may contain sibling graphs with additional wallet
/// token accounts. The parent has already decoded those graphs and supplies
/// their declared asset-token indices here. They are allowed to exist in the
/// account list, but this graph can still touch only indices present in its own
/// typed assets and legs.
pub(super) fn preflight_graph_for_allowing(
    program: &Pubkey,
    a: &[AccountInfo],
    d: &[u8],
    wallet_index: usize,
    nonce_index: usize,
    aggregate_asset_tokens: &[u8],
) -> ProgramResult {
    preflight_graph_for_pending_input(
        program,
        a,
        d,
        wallet_index,
        nonce_index,
        aggregate_asset_tokens,
        0,
    )
}

/// A parent may promise only the verified minimum proceeds of its preflighted
/// funding graph. This credit affects the source balance check only; execution
/// always reads actual balances and cannot spend this promise.
#[allow(clippy::too_many_arguments)]
pub(super) fn preflight_graph_for_pending_input(
    program: &Pubkey,
    a: &[AccountInfo],
    d: &[u8],
    wallet_index: usize,
    nonce_index: usize,
    aggregate_asset_tokens: &[u8],
    pending_input: u64,
) -> ProgramResult {
    let g = ad(Graph::decode(d, a.len()))?;
    #[cfg(feature = "profile-cu")]
    solana_program::msg!("PROFILE preflight_decoded");
    need(
        wallet_index < a.len()
            && nonce_index < a.len()
            && wallet_index != nonce_index
            && a[wallet_index].is_signer
            && a[nonce_index].is_writable
            && a[nonce_index].owner == program,
        IDENTITY,
    )?;
    {
        let n = a[nonce_index].try_borrow_data()?;
        need(
            n.len() == NONCE_LEN
                && &n[..8] == NONCE_TAG
                && n[8..40] == a[wallet_index].key.to_bytes(),
            IDENTITY,
        )?;
        need(ad(dex::u64_at(&n, 40))? == g.sequence, REPLAY)?;
    }
    #[cfg(feature = "profile-cu")]
    solana_program::msg!("PROFILE preflight_nonce");
    let clock = Clock::get()?;
    need(
        clock.slot <= g.deadline && clock.unix_timestamp >= 0,
        EXPIRED,
    )?;
    let mut before = [0u64; MAX_ASSETS];
    for i in 0..g.asset_count {
        #[cfg(feature = "profile-cu")]
        solana_program::msg!("PROFILE preflight_asset {}", i);
        before[i] = asset(a, &g, i, true, wallet_index)?;
        for j in 0..i {
            need(
                a[g.assets[i].token as usize].key != a[g.assets[j].token as usize].key
                    && a[g.assets[i].mint as usize].key != a[g.assets[j].mint as usize].key,
                IDENTITY,
            )?;
        }
    }
    need(
        before[0]
            .checked_add(pending_input)
            .is_some_and(|amount| amount >= g.input),
        BOUNDS,
    )?;
    #[cfg(feature = "profile-cu")]
    solana_program::msg!("PROFILE preflight_assets");
    // Classify shared accounts once. Prefix collisions always get a complete
    // key comparison; the prefix is an acceleration, never an identity proof.
    let mut asset_flags = [0u8; dex::graph::MAX_ACCOUNTS];
    for (index, info) in a.iter().enumerate() {
        for (i, spec) in g.assets[..g.asset_count].iter().enumerate() {
            let key = a[spec.token as usize].key;
            if info.key.as_ref()[..8] == key.as_ref()[..8] && info.key == key {
                asset_flags[index] |= 1 << i;
            }
        }
        if asset_flags[index] == 0
            && info.is_writable
            && (info.owner == &TOKEN || info.owner == &TOKEN_2022)
            && !aggregate_asset_tokens.contains(&(index as u8))
        {
            let data = info.try_borrow_data()?;
            if data.len() >= 165
                && (info.owner == &TOKEN || data.len() == 165 || data.get(165) == Some(&2))
            {
                need(data[32..64] != a[wallet_index].key.to_bytes(), IDENTITY)?;
            }
        }
    }
    #[cfg(feature = "profile-cu")]
    solana_program::msg!("PROFILE preflight_aliases");
    // Validate every leg before the first CPI, including branches later skipped.
    for leg in g.legs[..g.leg_count].iter().flatten() {
        validate(a, &g, leg, &asset_flags, wallet_index, nonce_index)?;
    }
    #[cfg(feature = "profile-cu")]
    solana_program::msg!("PROFILE preflight_legs");
    Ok(())
}

pub(super) fn settle_graph_for(
    program: &Pubkey,
    a: &[AccountInfo],
    d: &[u8],
    wallet_index: usize,
    nonce_index: usize,
) -> ProgramResult {
    // A public graph always has one source (asset zero) and one final sink (the
    // last asset). Assets in between are typed transit balances, not competing
    // outputs: execute_preflighted_graph_for requires every one of them to
    // return to its exact starting balance before checking the sink min_out.
    // Multi-issuer output comparison remains confined to opcode 18 v2, whose
    // economic reflow path converts each terminal product through its policy.
    preflight_graph_for(program, a, d, wallet_index, nonce_index)?;
    let outcome = execute_preflighted_graph_for(a, d, wallet_index)?;
    let next = outcome.sequence.checked_add(1).ok_or(err(BOUNDS))?;
    {
        let mut nonce = a[nonce_index].try_borrow_mut_data()?;
        nonce[40..48].copy_from_slice(&next.to_le_bytes());
        nonce[48..56].copy_from_slice(&outcome.actual_in.to_le_bytes());
        nonce[56..64].copy_from_slice(&outcome.actual_out.to_le_bytes());
    }
    set_return_data(&outcome.receipt[..outcome.receipt_len]);
    Ok(())
}

pub(super) struct GraphOutcome {
    pub sequence: u64,
    pub actual_in: u64,
    pub actual_out: u64,
    receipt: [u8; 224],
    receipt_len: usize,
}

pub(super) struct EconomicGraphOutcome {
    pub sequence: u64,
    pub actual_in: u64,
    pub exposure_q32: u64,
    pub executed: usize,
}

/// Execute a graph after its complete parent instruction has passed every
/// preflight. This deliberately does not advance the owner nonce, allowing the
/// StockMesh exposure instruction to execute several product graphs under one
/// signed economic intent and advance the nonce once after the aggregate
/// exposure postcondition succeeds.
pub(super) fn execute_preflighted_graph_for(
    a: &[AccountInfo],
    d: &[u8],
    wallet_index: usize,
) -> Result<GraphOutcome, ProgramError> {
    let g = ad(Graph::decode(d, a.len()))?;
    let clock = Clock::get()?;
    let mut before = [0u64; MAX_ASSETS];
    for (i, value) in before[..g.asset_count].iter_mut().enumerate() {
        *value = asset(a, &g, i, false, wallet_index)?;
    }
    need(before[0] >= g.input, BOUNDS)?;
    let mut oracle = if g.reflow_calls > 0 {
        Some(super::allocation::Oracle::new(a, &g, &clock)?)
    } else {
        None
    };
    let mut used = [0u64; dex::graph::MAX_CANDIDATES];
    #[cfg(feature = "profile-cu")]
    {
        solana_program::msg!("PROFILE oracle_ready");
        solana_program::log::sol_log_compute_units();
    }
    // SPL input authority must not implicitly authorize an additional native
    // SOL debit. Network fees were already charged before this invocation.
    let wallet_lamports = a[wallet_index].lamports();
    let mut current = before;
    let mut receipts = [0u8; 224]; // Reflow adds candidate index + oracle calls per leg.
    let receipt_stride = if oracle.is_some() { 48 } else { 32 };
    let mut executed = 0u64;
    let mut ix = Instruction {
        program_id: PHOENIX,
        accounts: Vec::with_capacity(MAX_CPI_ACCOUNTS),
        data: Vec::with_capacity(80),
    };
    for iteration in 0..if oracle.is_some() { 4 } else { g.leg_count } {
        let remaining = g
            .input
            .checked_sub(before[0].checked_sub(current[0]).ok_or(err(CONSERVATION))?)
            .ok_or(err(CONSERVATION))?;
        let (index, selected_budget, oracle_calls) = if let Some(o) = &mut oracle {
            if remaining == 0 {
                break;
            }
            if iteration == 0 && g.seed_input != 0 {
                (0, g.seed_input, 0)
            } else {
                o.next(a, &g, &clock, remaining, &used, executed as usize)?
            }
        } else {
            (iteration, u64::MAX, 0)
        };
        let leg = g.legs[index].as_ref().ok_or(err(BOUNDS))?;
        #[cfg(feature = "profile-cu")]
        {
            solana_program::msg!("PROFILE allocated");
            solana_program::log::sol_log_compute_units();
        }
        let available = if leg.source == 0 {
            let spent = before[0].checked_sub(current[0]).ok_or(err(CONSERVATION))?;
            g.input.checked_sub(spent).ok_or(err(CONSERVATION))?
        } else {
            current[leg.source]
                .checked_sub(before[leg.source])
                .ok_or(err(CONSERVATION))?
        };
        let budget = available.min(leg.budget).min(selected_budget);
        if budget == 0 {
            continue;
        }
        let mut bytes = [0u8; 80];
        let len = if leg.venue == Venue::Phoenix {
            let state = a[leg.accounts[2] as usize].try_borrow_data()?;
            let book = ad(PhoenixBook::decode(&state))?;
            let expected = ad(book.quote(
                &a[wallet_index].key.to_bytes(),
                leg.direction,
                budget,
                dex::MAX_MATCHES,
                clock.slot,
                clock.unix_timestamp as u64,
            ))?;
            if expected.input == 0 || expected.output == 0 {
                continue;
            }
            bytes = ad(dex::phoenix_ioc(
                &book.header,
                leg.direction,
                budget,
                dex::MAX_MATCHES,
                g.deadline,
            ))?;
            dex::PHOENIX_IOC_LEN
        } else {
            ad(leg.venue.swap_data(budget, leg.direction, &mut bytes))?
        };
        ix.accounts.clear();
        ix.data.clear();
        ix.program_id = PROGRAMS[leg.venue as usize];
        ix.data.extend_from_slice(&bytes[..len]);
        let (_, signer, _, _) = leg.venue.bindings(leg.direction);
        for (pos, idx) in leg.accounts.iter().enumerate() {
            let info = &a[*idx as usize];
            // Optional Anchor accounts use the executable program as sentinel.
            let writable = leg
                .venue
                .writable_with_owner(pos, info.owner == &ix.program_id)
                && info.key != &ix.program_id;
            ix.accounts.push(if writable {
                AccountMeta::new(*info.key, pos == signer)
            } else {
                AccountMeta::new_readonly(*info.key, pos == signer)
            });
        }
        invoke_checked_accounts(&ix, a)?;
        #[cfg(feature = "profile-cu")]
        {
            solana_program::msg!("PROFILE executed");
            solana_program::log::sol_log_compute_units();
        }
        need(a[wallet_index].lamports() == wallet_lamports, CONSERVATION)?;
        let previous = current;
        for i in 0..g.asset_count {
            current[i] = balance(&a[g.assets[i].token as usize])?;
        }
        let spent = previous[leg.source]
            .checked_sub(current[leg.source])
            .ok_or(err(CONSERVATION))?;
        let received = current[leg.destination]
            .checked_sub(previous[leg.destination])
            .ok_or(err(CONSERVATION))?;
        need(spent > 0 && spent <= budget && received > 0, CONSERVATION)?;
        used[index] = used[index].checked_add(spent).ok_or(err(CONSERVATION))?;
        for i in 0..g.asset_count {
            if i != leg.source && i != leg.destination {
                need(current[i] == previous[i], CONSERVATION)?;
            }
        }
        executed += 1;
        let p = 32 + iteration * receipt_stride;
        for (chunk, value) in
            receipts[p..p + 32]
                .chunks_exact_mut(8)
                .zip([leg.venue as u64, budget, spent, received])
        {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        if oracle.is_some() {
            receipts[p + 32..p + 40].copy_from_slice(&(index as u64).to_le_bytes());
            receipts[p + 40..p + 48].copy_from_slice(&u64::from(oracle_calls).to_le_bytes());
        }
    }
    let actual_in = before[0].checked_sub(current[0]).ok_or(err(CONSERVATION))?;
    let sink = g.asset_count - 1;
    let actual_out = current[sink]
        .checked_sub(before[sink])
        .ok_or(err(CONSERVATION))?;
    need(actual_in == g.input, CONSERVATION)?;
    need(actual_out >= g.min_out, MIN_OUT)?;
    for i in 1..sink {
        need(current[i] == before[i], CONSERVATION)?;
    }
    for (i, value) in current[..g.asset_count].iter().enumerate() {
        need(asset(a, &g, i, false, wallet_index)? == *value, IDENTITY)?;
    }
    for (chunk, value) in receipts[..32]
        .chunks_exact_mut(8)
        .zip([g.sequence, actual_in, actual_out, executed])
    {
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    let receipt_len = 32
        + if oracle.is_some() {
            executed as usize * 48
        } else {
            g.leg_count * 32
        };
    Ok(GraphOutcome {
        sequence: g.sequence,
        actual_in,
        actual_out,
        receipt: receipts,
        receipt_len,
    })
}

#[derive(Clone, Copy, Default)]
struct EconomicPath {
    first: usize,
    second: Option<usize>,
    terminal: usize,
}

impl EconomicPath {
    fn cpis(self) -> usize {
        1 + usize::from(self.second.is_some())
    }
}

/// Derive bounded one- or two-CPI economic paths from the signed graph. A
/// product may also be a transit asset: cash -> SPYx is a direct SPYx
/// candidate, while cash -> SPYx -> SPYon is a distinct SPYon candidate. The
/// wire carries typed venue legs, never caller supplied CPI bytes or a frozen
/// percentage split.
fn economic_paths(
    g: &Graph,
    converters: &[Option<exposure::Converter>; MAX_ASSETS],
) -> Result<([Option<EconomicPath>; dex::graph::MAX_CANDIDATES], usize), ProgramError> {
    let mut paths = [None; dex::graph::MAX_CANDIDATES];
    let mut count = 0usize;
    let mut used_legs = [false; dex::graph::MAX_CANDIDATES];
    let mut reached = [false; MAX_ASSETS];
    for first in 0..g.leg_count {
        let head = g.legs[first].as_ref().ok_or(err(BOUNDS))?;
        if head.source != 0 {
            continue;
        }
        if converters[head.destination].is_some() {
            need(count < paths.len(), BOUNDS)?;
            paths[count] = Some(EconomicPath {
                first,
                second: None,
                terminal: head.destination,
            });
            count += 1;
            used_legs[first] = true;
            reached[head.destination] = true;
        }
        for second in 0..g.leg_count {
            let tail = g.legs[second].as_ref().ok_or(err(BOUNDS))?;
            if tail.source == head.destination
                && tail.destination > tail.source
                && converters[tail.destination].is_some()
            {
                need(count < paths.len(), BOUNDS)?;
                paths[count] = Some(EconomicPath {
                    first,
                    second: Some(second),
                    terminal: tail.destination,
                });
                count += 1;
                used_legs[first] = true;
                used_legs[second] = true;
                reached[tail.destination] = true;
            }
        }
    }
    need(
        count > 0
            && used_legs[..g.leg_count].iter().all(|used| *used)
            && (1..g.asset_count).all(|index| converters[index].is_none() || reached[index]),
        BOUNDS,
    )?;
    Ok((paths, count))
}

#[allow(clippy::too_many_arguments)]
fn quote_economic_path(
    oracle: &super::allocation::Oracle,
    a: &[AccountInfo],
    g: &Graph,
    clock: &Clock,
    path: EconomicPath,
    input: u64,
    used: &[u64; dex::graph::MAX_CANDIDATES],
    exact: bool,
    converters: &[Option<exposure::Converter>; MAX_ASSETS],
) -> Result<u64, ProgramError> {
    let first = g.legs[path.first].as_ref().ok_or(err(BOUNDS))?;
    need(
        input <= first.budget.saturating_sub(used[path.first]),
        BOUNDS,
    )?;
    let intermediate = oracle.quote(a, g, path.first, input, clock, exact)?;
    let raw = if let Some(second_index) = path.second {
        let second = g.legs[second_index].as_ref().ok_or(err(BOUNDS))?;
        need(
            second.source == first.destination
                && second.destination == path.terminal
                && intermediate <= second.budget.saturating_sub(used[second_index]),
            BOUNDS,
        )?;
        // A composed candidate may never strand its temporary issuer balance.
        // Phoenix therefore has to quote the second hop as an exact input even
        // before the final economic round.
        oracle.quote(a, g, second_index, intermediate, clock, true)?
    } else {
        intermediate
    };
    converters[path.terminal]
        .ok_or(err(BOUNDS))?
        .convert(raw)
}

#[allow(clippy::too_many_arguments)]
fn select_economic_path(
    oracle: &mut super::allocation::Oracle,
    a: &[AccountInfo],
    g: &Graph,
    clock: &Clock,
    paths: &[Option<EconomicPath>; dex::graph::MAX_CANDIDATES],
    path_count: usize,
    remaining: u64,
    used: &[u64; dex::graph::MAX_CANDIDATES],
    remaining_cpis: usize,
    converters: &[Option<exposure::Converter>; MAX_ASSETS],
) -> Result<(EconomicPath, u64), ProgramError> {
    need(remaining > 0 && remaining_cpis > 0, BOUNDS)?;
    oracle.refresh(a, g, clock, used)?;
    let mut cache = [(usize::MAX, 0u64, None); 16];
    let mut cursor = 0usize;
    let mut quote = |index: usize, amount: u64, exact: bool| {
        let path = paths[index].ok_or(err(BOUNDS))?;
        need(path.cpis() <= remaining_cpis, BOUNDS)?;
        if let Some((_, _, value)) = cache
            .iter()
            .find(|(candidate, input, _)| *candidate == index && *input == amount)
        {
            return value.ok_or(err(ADAPTER));
        }
        let value = quote_economic_path(
            oracle,
            a,
            g,
            clock,
            path,
            amount,
            used,
            exact,
            converters,
        );
        cache[cursor] = (index, amount, value.as_ref().ok().copied());
        cursor = (cursor + 1) % cache.len();
        value
    };

    let mut best_full = None;
    for index in 0..path_count {
        if let Ok(output) = quote(index, remaining, true) {
            if best_full.is_none_or(|(_, prior)| output > prior) {
                best_full = Some((index, output));
            }
        }
    }
    let (full_index, _) = best_full.ok_or(err(ADAPTER))?;
    if remaining_cpis == 1 {
        return Ok((paths[full_index].ok_or(err(BOUNDS))?, remaining));
    }

    let plan = skew_engine::optimizer::oracle::refine(
        path_count,
        remaining,
        u32::from(g.reflow_calls),
        |index, amount| {
            quote(index, amount, false).map_err(|_| skew_engine::Error::Capacity)
        },
    )
    .map_err(|_| err(ADAPTER))?;
    let selected = (0..path_count)
        .filter(|index| paths[*index].is_some_and(|path| path.cpis() <= remaining_cpis))
        .max_by_key(|index| (plan.inputs[*index], core::cmp::Reverse(*index)))
        .ok_or(err(ADAPTER))?;
    let path = paths[selected].ok_or(err(BOUNDS))?;
    let amount = plan.inputs[selected];
    need(amount > 0, ADAPTER)?;
    // Never start a partial composed path if it would consume the last CPI.
    // The full-order candidate above is already proven executable and becomes
    // the deterministic fallback.
    if amount < remaining && path.cpis() == remaining_cpis {
        Ok((paths[full_index].ok_or(err(BOUNDS))?, remaining))
    } else {
        Ok((path, amount))
    }
}

fn invoke_economic_leg(
    a: &[AccountInfo],
    g: &Graph,
    wallet_index: usize,
    clock: &Clock,
    index: usize,
    budget: u64,
    ix: &mut Instruction,
) -> ProgramResult {
    let leg = g.legs[index].as_ref().ok_or(err(BOUNDS))?;
    let mut bytes = [0u8; 80];
    let len = if leg.venue == Venue::Phoenix {
        let state = a[leg.accounts[2] as usize].try_borrow_data()?;
        let book = ad(PhoenixBook::decode(&state))?;
        let expected = ad(book.quote(
            &a[wallet_index].key.to_bytes(),
            leg.direction,
            budget,
            dex::MAX_MATCHES,
            clock.slot,
            clock.unix_timestamp as u64,
        ))?;
        need(expected.input == budget && expected.output > 0, ADAPTER)?;
        bytes = ad(dex::phoenix_ioc(
            &book.header,
            leg.direction,
            budget,
            dex::MAX_MATCHES,
            g.deadline,
        ))?;
        dex::PHOENIX_IOC_LEN
    } else {
        ad(leg.venue.swap_data(budget, leg.direction, &mut bytes))?
    };
    ix.accounts.clear();
    ix.data.clear();
    ix.program_id = PROGRAMS[leg.venue as usize];
    ix.data.extend_from_slice(&bytes[..len]);
    let (_, signer, _, _) = leg.venue.bindings(leg.direction);
    for (position, account_index) in leg.accounts.iter().enumerate() {
        let info = &a[*account_index as usize];
        let writable = leg
            .venue
            .writable_with_owner(position, info.owner == &ix.program_id)
            && info.key != &ix.program_id;
        ix.accounts.push(if writable {
            AccountMeta::new(*info.key, position == signer)
        } else {
            AccountMeta::new_readonly(*info.key, position == signer)
        });
    }
    invoke_checked_accounts(ix, a)
}

/// Execute a bounded one- or two-CPI, multi-product candidate graph. Candidate
/// paths are re-quoted in conservative underlying-share Q32 after each observed
/// fill. A second hop consumes exactly the balance delta produced by its first
/// hop, so preexisting or previously acquired product inventory cannot leak
/// into another issuer path.
#[inline(never)]
pub(super) fn execute_preflighted_economic_reflow_for(
    a: &[AccountInfo],
    d: &[u8],
    wallet_index: usize,
    converters: &[Option<exposure::Converter>; MAX_ASSETS],
    maximum_executions: usize,
) -> Result<EconomicGraphOutcome, ProgramError> {
    let g = ad(Graph::decode(d, a.len()))?;
    need(
        g.reflow_calls > 0 && (1..=4).contains(&maximum_executions) && g.asset_count >= 2,
        BOUNDS,
    )?;
    let (paths, path_count) = economic_paths(&g, converters)?;
    let clock = Clock::get()?;
    let mut before = [0u64; MAX_ASSETS];
    for (i, value) in before[..g.asset_count].iter_mut().enumerate() {
        *value = asset(a, &g, i, false, wallet_index)?;
    }
    need(before[0] >= g.input, BOUNDS)?;
    let mut oracle = super::allocation::Oracle::new(a, &g, &clock)?;
    let mut used = [0u64; dex::graph::MAX_CANDIDATES];
    let wallet_lamports = a[wallet_index].lamports();
    let mut current = before;
    let mut executed = 0usize;
    let mut ix = Instruction {
        program_id: PHOENIX,
        accounts: Vec::with_capacity(MAX_CPI_ACCOUNTS),
        data: Vec::with_capacity(80),
    };
    for _ in 0..maximum_executions {
        let spent_input = before[0].checked_sub(current[0]).ok_or(err(CONSERVATION))?;
        let remaining = g.input.checked_sub(spent_input).ok_or(err(CONSERVATION))?;
        if remaining == 0 {
            break;
        }
        let (path, selected_budget) = select_economic_path(
            &mut oracle,
            a,
            &g,
            &clock,
            &paths,
            path_count,
            remaining,
            &used,
            maximum_executions
                .checked_sub(executed)
                .ok_or(err(BOUNDS))?,
            converters,
        )?;
        let leg = g.legs[path.first].as_ref().ok_or(err(BOUNDS))?;
        let available = g
            .input
            .checked_sub(before[0].checked_sub(current[0]).ok_or(err(CONSERVATION))?)
            .ok_or(err(CONSERVATION))?;
        let budget = available.min(leg.budget).min(selected_budget);
        need(budget > 0, ADAPTER)?;

        invoke_economic_leg(a, &g, wallet_index, &clock, path.first, budget, &mut ix)?;
        need(a[wallet_index].lamports() == wallet_lamports, CONSERVATION)?;

        let previous = current;
        for i in 0..g.asset_count {
            current[i] = balance(&a[g.assets[i].token as usize])?;
        }
        let spent = previous[0]
            .checked_sub(current[0])
            .ok_or(err(CONSERVATION))?;
        let received = current[leg.destination]
            .checked_sub(previous[leg.destination])
            .ok_or(err(CONSERVATION))?;
        need(spent > 0 && spent <= budget && received > 0, CONSERVATION)?;
        used[path.first] = used[path.first]
            .checked_add(spent)
            .ok_or(err(CONSERVATION))?;
        for i in 1..g.asset_count {
            if i != leg.destination {
                need(current[i] == previous[i], CONSERVATION)?;
            }
        }
        executed = executed.checked_add(1).ok_or(err(BOUNDS))?;

        if let Some(second_index) = path.second {
            let second = g.legs[second_index].as_ref().ok_or(err(BOUNDS))?;
            need(
                second.source == leg.destination
                    && second.destination == path.terminal
                    && executed < maximum_executions,
                BOUNDS,
            )?;
            invoke_economic_leg(
                a,
                &g,
                wallet_index,
                &clock,
                second_index,
                received,
                &mut ix,
            )?;
            need(a[wallet_index].lamports() == wallet_lamports, CONSERVATION)?;
            let after_first = current;
            for i in 0..g.asset_count {
                current[i] = balance(&a[g.assets[i].token as usize])?;
            }
            let second_spent = after_first[second.source]
                .checked_sub(current[second.source])
                .ok_or(err(CONSERVATION))?;
            let second_received = current[second.destination]
                .checked_sub(after_first[second.destination])
                .ok_or(err(CONSERVATION))?;
            need(
                second_spent == received
                    && second_received > 0
                    && current[second.source] == previous[second.source]
                    && current[0] == after_first[0],
                CONSERVATION,
            )?;
            for i in 1..g.asset_count {
                if i != second.source && i != second.destination {
                    need(current[i] == after_first[i], CONSERVATION)?;
                }
            }
            used[second_index] = used[second_index]
                .checked_add(second_spent)
                .ok_or(err(CONSERVATION))?;
            executed = executed.checked_add(1).ok_or(err(BOUNDS))?;
        }
    }

    let actual_in = before[0].checked_sub(current[0]).ok_or(err(CONSERVATION))?;
    need(actual_in == g.input, CONSERVATION)?;
    let mut exposure_q32 = 0u64;
    for i in 1..g.asset_count {
        let raw = current[i].checked_sub(before[i]).ok_or(err(CONSERVATION))?;
        if let Some(converter) = converters[i] {
            if raw > 0 {
                exposure_q32 = exposure_q32
                    .checked_add(converter.convert(raw)?)
                    .ok_or(err(BOUNDS))?;
            }
        } else {
            need(raw == 0 && current[i] == before[i], CONSERVATION)?;
        }
        need(
            asset(a, &g, i, false, wallet_index)? == current[i],
            IDENTITY,
        )?;
    }
    need(exposure_q32 >= g.min_out, MIN_OUT)?;
    Ok(EconomicGraphOutcome {
        sequence: g.sequence,
        actual_in,
        exposure_q32,
        executed,
    })
}

#[inline(never)]
fn validate(
    a: &[AccountInfo],
    g: &Graph,
    leg: &Leg,
    asset_flags: &[u8; dex::graph::MAX_ACCOUNTS],
    wallet_index: usize,
    nonce_index: usize,
) -> ProgramResult {
    let id = &PROGRAMS[leg.venue as usize];
    executable(&a[leg.program as usize], id)?;
    let (pool, signer, src, dst) = leg.venue.bindings(leg.direction);
    let at = |i: usize| &a[leg.accounts[i] as usize];
    need(
        at(pool).owner == id
            && at(signer).key == a[wallet_index].key
            && at(src).key == a[g.assets[leg.source].token as usize].key
            && at(dst).key == a[g.assets[leg.destination].token as usize].key,
        IDENTITY,
    )?;
    for (pos, idx) in leg.accounts.iter().enumerate() {
        let info = &a[*idx as usize];
        need(info.key != a[nonce_index].key, IDENTITY)?;
        if leg.venue.writable_with_owner(pos, info.owner == id) && info.key != id {
            need(info.is_writable, IDENTITY)?;
        }
        if pos != src && pos != dst {
            need(asset_flags[*idx as usize] == 0, IDENTITY)?;
        }
    }
    Ok(())
}
