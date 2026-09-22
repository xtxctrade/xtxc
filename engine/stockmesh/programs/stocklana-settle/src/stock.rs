//! A user-selected stock policy PDA is a trust root, not proof of issuer affiliation.
//! Its address commits the authority, instrument, exact cash/output mint pair
//! and rights document hash. Rights or settlement-asset changes therefore
//! create a different PDA; operational state changes advance the version and
//! abort stale transactions before CPI.
use super::*;
use dex::graph::{Graph, MAX_ASSETS};
const TAG: &[u8; 8] = b"SKEWSTK2";
const LEN: usize = 225;
const SECONDARY: u8 = 1;
const HALTED: u8 = 8;
const UNDERLYING_OPEN: u8 = 16;

/// Opcode 19: authority, policy PDA, system program. Identity and rights fields
/// are immutable; version advances by exactly one. A changed rights hash or cash
/// mint has a new address and cannot reuse an already signed policy account.
pub(super) fn publish(program: &Pubkey, a: &[AccountInfo], d: &[u8]) -> ProgramResult {
    need(a.len() == 3 && d.len() == 178 && d[0] == 19, BOUNDS)?;
    need(
        a[0].is_signer && a[0].is_writable && a[1].is_writable && *a[2].key == system_program::id(),
        IDENTITY,
    )?;
    let instrument = ad(dex::key(d, 1))?;
    let issuer = ad(dex::key(d, 33))?;
    let input = ad(dex::key(d, 65))?;
    let output = ad(dex::key(d, 97))?;
    let rights = ad(dex::key(d, 129))?;
    let version = ad(dex::u64_at(d, 161))?;
    let expires = ad(dex::u64_at(d, 169))?;
    let flags = d[177];
    let slot = Clock::get()?.slot;
    need(
        version > 0
            && flags & !31 == 0
            && input != output
            && instrument != [0; 32]
            && issuer != [0; 32]
            && rights != [0; 32]
            && expires >= slot
            && expires <= slot.checked_add(3600).ok_or(err(BOUNDS))?,
        BOUNDS,
    )?;
    let (pda, bump) = Pubkey::find_program_address(
        &[
            b"stock2",
            a[0].key.as_ref(),
            &instrument,
            &input,
            &output,
            &rights,
        ],
        program,
    );
    need(*a[1].key == pda, IDENTITY)?;
    if a[1].owner == program {
        let old = a[1].try_borrow_data()?;
        need(
            old.len() == LEN
                && &old[..8] == TAG
                && old[8..40] == a[0].key.to_bytes()
                && old[40..72] == instrument
                && old[72..104] == issuer
                && old[104..136] == input
                && old[136..168] == output
                && old[168..200] == rights,
            IDENTITY,
        )?;
        need(
            ad(dex::u64_at(&old, 200))?.checked_add(1) == Some(version),
            REPLAY,
        )?;
    } else {
        need(
            a[1].data_is_empty() && *a[1].owner == system_program::id() && version == 1,
            IDENTITY,
        )?;
        let rent = Rent::get()?.minimum_balance(LEN);
        let seeds: &[&[u8]] = &[
            b"stock2",
            a[0].key.as_ref(),
            &instrument,
            &input,
            &output,
            &rights,
            &[bump],
        ];
        if a[1].lamports() < rent {
            invoke(
                &system_instruction::transfer(a[0].key, a[1].key, rent - a[1].lamports()),
                a,
            )?;
        }
        invoke_signed(
            &system_instruction::allocate(a[1].key, LEN as u64),
            a,
            &[seeds],
        )?;
        invoke_signed(&system_instruction::assign(a[1].key, program), a, &[seeds])?;
    }
    let mut state = a[1].try_borrow_mut_data()?;
    state[..8].copy_from_slice(TAG);
    state[8..40].copy_from_slice(a[0].key.as_ref());
    state[40..168].copy_from_slice(&d[1..129]);
    state[168..200].copy_from_slice(&rights);
    state[200..208].copy_from_slice(&version.to_le_bytes());
    state[208..216].copy_from_slice(&slot.to_le_bytes());
    state[216..224].copy_from_slice(&expires.to_le_bytes());
    state[224] = flags;
    Ok(())
}

/// Opcode 7: policy index, allow-underlying-closed, reserved; expected version,
/// maximum policy age in slots; then a typed graph (opcode 2 or 4). The user signs
/// the policy address, version, exact mint pair, budget, min-out and deadline.
pub(super) fn execute(program: &Pubkey, a: &[AccountInfo], d: &[u8]) -> ProgramResult {
    need(d.len() > 20 && d[0] == 7 && d[2] <= 1 && d[3] == 0, BOUNDS)?;
    let index = d[1] as usize;
    need(index < a.len() && index > 1, BOUNDS)?;
    let version = ad(dex::u64_at(d, 4))?;
    let max_age = ad(dex::u64_at(d, 12))?;
    need((1..=150).contains(&max_age), BOUNDS)?;
    let graph = ad(Graph::decode(&d[20..], a.len()))?;
    let clock = Clock::get()?;
    validate_policy_for_graph(
        program,
        a,
        index,
        version,
        max_age,
        d[2] == 1,
        &graph,
        &clock,
    )?;
    // Read-only policy cannot change during the admitted graph's CPI execution.
    super::graph::settle_graph(program, a, &d[20..])?;
    let (id, body) = solana_program::program::get_return_data().ok_or(err(CONSERVATION))?;
    need(id == *program && body.len() <= 224, CONSERVATION)?;
    let mut receipt = [0u8; 240];
    receipt[..8].copy_from_slice(TAG);
    receipt[8..16].copy_from_slice(&version.to_le_bytes());
    receipt[16..16 + body.len()].copy_from_slice(&body);
    set_return_data(&receipt[..16 + body.len()]);
    Ok(())
}

/// Opcode 20 applies an existing cash->product ProductPolicy to the inverse
/// secondary-liquidity graph. The signed graph must begin with the policy's
/// exact product mint and its first destination must be the policy cash mint.
/// A later cash->SOL leg may follow inside the same atomic graph.
pub(super) fn execute_reverse(program: &Pubkey, a: &[AccountInfo], d: &[u8]) -> ProgramResult {
    need(d.len() > 20 && d[0] == 20 && d[2] <= 1 && d[3] == 0, BOUNDS)?;
    let index = d[1] as usize;
    need(index < a.len() && index > 1, BOUNDS)?;
    let version = ad(dex::u64_at(d, 4))?;
    let max_age = ad(dex::u64_at(d, 12))?;
    need((1..=150).contains(&max_age), BOUNDS)?;
    let graph = ad(Graph::decode(&d[20..], a.len()))?;
    need(graph.asset_count >= 2, IDENTITY)?;
    let clock = Clock::get()?;
    // ProductPolicy v2 stores cash as input and the issuer product as output.
    // Reverse settlement binds those identities in the opposite graph order.
    validate_policy_for_reverse_graph(
        program,
        a,
        index,
        version,
        max_age,
        d[2] == 1,
        &graph,
        &clock,
    )?;
    super::graph::settle_graph(program, a, &d[20..])?;
    let (id, body) = solana_program::program::get_return_data().ok_or(err(CONSERVATION))?;
    need(id == *program && body.len() <= 224, CONSERVATION)?;
    let mut receipt = [0u8; 240];
    receipt[..8].copy_from_slice(TAG);
    receipt[8..16].copy_from_slice(&version.to_le_bytes());
    receipt[16..16 + body.len()].copy_from_slice(&body);
    set_return_data(&receipt[..16 + body.len()]);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_policy_for_reverse_graph(
    program: &Pubkey,
    a: &[AccountInfo],
    index: usize,
    version: u64,
    max_age: u64,
    allow_underlying_closed: bool,
    graph: &Graph,
    clock: &Clock,
) -> ProgramResult {
    need(
        index < a.len() && index > 1 && (1..=150).contains(&max_age),
        BOUNDS,
    )?;
    let policy = &a[index];
    need(
        policy.owner == program && !policy.is_writable && !policy.executable,
        IDENTITY,
    )?;
    let p = policy.try_borrow_data()?;
    need(p.len() == LEN && &p[..8] == TAG, IDENTITY)?;
    let authority = Pubkey::new_from_array(ad(dex::key(&p, 8))?);
    let instrument = ad(dex::key(&p, 40))?;
    let cash_mint = ad(dex::key(&p, 104))?;
    let product_mint = ad(dex::key(&p, 136))?;
    let rights = ad(dex::key(&p, 168))?;
    let (expected, _) = Pubkey::find_program_address(
        &[
            b"stock2",
            authority.as_ref(),
            &instrument,
            &cash_mint,
            &product_mint,
            &rights,
        ],
        program,
    );
    need(
        policy.key == &expected && ad(dex::u64_at(&p, 200))? == version,
        IDENTITY,
    )?;
    let updated = ad(dex::u64_at(&p, 208))?;
    let expires = ad(dex::u64_at(&p, 216))?;
    need(
        updated <= clock.slot
            && clock.slot - updated <= max_age
            && clock.slot <= expires
            && graph.deadline <= expires,
        EXPIRED,
    )?;
    need(
        p[224] & SECONDARY != 0
            && p[224] & HALTED == 0
            && (p[224] & UNDERLYING_OPEN != 0 || allow_underlying_closed),
        EXPIRED,
    )?;

    let product_index = graph.assets[0].mint as usize;
    need(
        product_index < a.len() && a[product_index].key.to_bytes() == product_mint,
        IDENTITY,
    )?;
    let mut reachable = [false; MAX_ASSETS];
    reachable[0] = true;
    for _ in 0..graph.asset_count {
        for leg in graph.legs[..graph.leg_count].iter().flatten() {
            let source = leg.source;
            let destination = leg.destination;
            if source < graph.asset_count && destination < graph.asset_count && reachable[source] {
                reachable[destination] = true;
            }
        }
    }
    let mut cash_reachable = false;
    for (asset_index, reached) in reachable.iter().enumerate().take(graph.asset_count) {
        let mint_index = graph.assets[asset_index].mint as usize;
        need(mint_index < a.len(), BOUNDS)?;
        cash_reachable |= *reached && a[mint_index].key.to_bytes() == cash_mint;
    }
    need(cash_reachable, IDENTITY)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn validate_policy_for_graph(
    program: &Pubkey,
    a: &[AccountInfo],
    index: usize,
    version: u64,
    max_age: u64,
    allow_underlying_closed: bool,
    graph: &Graph,
    clock: &Clock,
) -> ProgramResult {
    validate_policy_for_pair(
        program,
        a,
        index,
        version,
        max_age,
        allow_underlying_closed,
        graph.assets[0].mint as usize,
        graph.assets[graph.asset_count - 1].mint as usize,
        graph.deadline,
        clock,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn validate_policy_for_pair(
    program: &Pubkey,
    a: &[AccountInfo],
    index: usize,
    version: u64,
    max_age: u64,
    allow_underlying_closed: bool,
    input_mint_index: usize,
    output_mint_index: usize,
    deadline: u64,
    clock: &Clock,
) -> ProgramResult {
    need(
        index < a.len() && index > 1 && (1..=150).contains(&max_age),
        BOUNDS,
    )?;
    need(
        input_mint_index < a.len() && output_mint_index < a.len(),
        BOUNDS,
    )?;
    let policy = &a[index];
    need(
        policy.owner == program && !policy.is_writable && !policy.executable,
        IDENTITY,
    )?;
    {
        let p = policy.try_borrow_data()?;
        need(p.len() == LEN && &p[..8] == TAG, IDENTITY)?;
        let authority = Pubkey::new_from_array(ad(dex::key(&p, 8))?);
        let instrument = ad(dex::key(&p, 40))?;
        let input = ad(dex::key(&p, 104))?;
        let output = ad(dex::key(&p, 136))?;
        let rights = ad(dex::key(&p, 168))?;
        let (expected, _) = Pubkey::find_program_address(
            &[
                b"stock2",
                authority.as_ref(),
                &instrument,
                &input,
                &output,
                &rights,
            ],
            program,
        );
        need(
            policy.key == &expected && ad(dex::u64_at(&p, 200))? == version,
            IDENTITY,
        )?;
        let slot = clock.slot;
        let updated = ad(dex::u64_at(&p, 208))?;
        let expires = ad(dex::u64_at(&p, 216))?;
        need(
            updated <= slot && slot - updated <= max_age && slot <= expires && deadline <= expires,
            EXPIRED,
        )?;
        need(
            p[224] & SECONDARY != 0
                && p[224] & HALTED == 0
                && (p[224] & UNDERLYING_OPEN != 0 || allow_underlying_closed),
            EXPIRED,
        )?;
        need(
            a[input_mint_index].key.to_bytes() == input
                && a[output_mint_index].key.to_bytes() == output,
            IDENTITY,
        )?;
    }
    Ok(())
}
