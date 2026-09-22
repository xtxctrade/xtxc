//! A bounded, on-chain progress commitment for large stock intents.
//!
//! Each capsule is still one owner-signed Stocklana graph and is atomic inside
//! its transaction. The plan PDA enforces exact sequencing, a fixed input size,
//! and a conservative cumulative minimum-output floor across transactions.
//! This makes a large intent resumable; it does not make many transactions
//! collectively atomic.
use super::*;
use dex::graph::Graph;

const TAG: &[u8; 8] = b"SKEWCAP1";
const LEN: usize = 232;
const MAX_PLAN_INPUT: u64 = 10_000_000_000 * 1_000_000;
const MAX_CAPSULE_INPUT: u64 = 500_000 * 1_000_000;
const MAX_CAPSULES: u64 = 20_000;
const MAX_LIFETIME_SLOTS: u64 = 1_512_000;

const OWNER: core::ops::Range<usize> = 8..40;
const ID: core::ops::Range<usize> = 40..72;
const POLICY: core::ops::Range<usize> = 72..104;
const INPUT_MINT: core::ops::Range<usize> = 104..136;
const OUTPUT_MINT: core::ops::Range<usize> = 136..168;
const TOTAL_INPUT: usize = 168;
const TOTAL_MINIMUM_OUTPUT: usize = 176;
const MAXIMUM_CAPSULE_INPUT: usize = 184;
const SETTLED_INPUT: usize = 192;
const SETTLED_OUTPUT: usize = 200;
const NEXT_SEQUENCE: usize = 208;
const EXPIRES_SLOT: usize = 216;
const CAPSULE_COUNT: usize = 224;

/// Opcode 10 data: tag, plan id, total input, total minimum output, maximum
/// capsule input, first capsule sequence, expiry slot. Accounts are owner, plan
/// PDA, stock policy, input mint, output mint, system program.
pub(super) fn initialize(program: &Pubkey, a: &[AccountInfo], d: &[u8]) -> ProgramResult {
    need(a.len() == 6 && d.len() == 73 && d[0] == 10, BOUNDS)?;
    need(
        a[0].is_signer
            && a[0].is_writable
            && a[1].is_writable
            && !a[2].is_writable
            && !a[3].is_writable
            && !a[4].is_writable
            && a[2].owner == program
            && *a[5].key == system_program::id()
            && a[1].key != a[2].key
            && a[3].key != a[4].key,
        IDENTITY,
    )?;
    let id = ad(dex::key(d, 1))?;
    let total_input = ad(dex::u64_at(d, 33))?;
    let total_minimum_output = ad(dex::u64_at(d, 41))?;
    let maximum_capsule_input = ad(dex::u64_at(d, 49))?;
    let first_sequence = ad(dex::u64_at(d, 57))?;
    let expires_slot = ad(dex::u64_at(d, 65))?;
    let slot = Clock::get()?.slot;
    need(
        id != [0; 32]
            && total_input > 0
            && total_input <= MAX_PLAN_INPUT
            && total_minimum_output > 0
            && maximum_capsule_input > 0
            && maximum_capsule_input <= MAX_CAPSULE_INPUT
            && total_input.div_ceil(maximum_capsule_input) <= MAX_CAPSULES
            && first_sequence > 0
            && expires_slot >= slot
            && expires_slot <= slot.checked_add(MAX_LIFETIME_SLOTS).ok_or(err(BOUNDS))?,
        BOUNDS,
    )?;
    let (pda, bump) = Pubkey::find_program_address(&[b"capsule", a[0].key.as_ref(), &id], program);
    need(
        *a[1].key == pda && a[1].data_is_empty() && *a[1].owner == system_program::id(),
        IDENTITY,
    )?;
    let rent = Rent::get()?.minimum_balance(LEN);
    let seeds: &[&[u8]] = &[b"capsule", a[0].key.as_ref(), &id, &[bump]];
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
    let mut state = a[1].try_borrow_mut_data()?;
    state.fill(0);
    state[..8].copy_from_slice(TAG);
    state[OWNER].copy_from_slice(a[0].key.as_ref());
    state[ID].copy_from_slice(&id);
    state[POLICY].copy_from_slice(a[2].key.as_ref());
    state[INPUT_MINT].copy_from_slice(a[3].key.as_ref());
    state[OUTPUT_MINT].copy_from_slice(a[4].key.as_ref());
    put(&mut state, TOTAL_INPUT, total_input);
    put(&mut state, TOTAL_MINIMUM_OUTPUT, total_minimum_output);
    put(&mut state, MAXIMUM_CAPSULE_INPUT, maximum_capsule_input);
    put(&mut state, NEXT_SEQUENCE, first_sequence);
    put(&mut state, EXPIRES_SLOT, expires_slot);
    Ok(())
}

/// Opcode 11 data: tag, plan account index, policy account index,
/// allow-underlying-closed, plan id, capsule sequence, stock-policy version,
/// max policy age, then a graph. Wallet and nonce retain graph indices 0 and 1.
pub(super) fn execute(program: &Pubkey, a: &[AccountInfo], d: &[u8]) -> ProgramResult {
    need(
        d.len() > 60 && d[0] == 11 && d[3] <= 1 && a.len() >= 6,
        BOUNDS,
    )?;
    let plan_index = d[1] as usize;
    let policy_index = d[2] as usize;
    need(
        plan_index > 1
            && policy_index > 1
            && plan_index < a.len()
            && policy_index < a.len()
            && plan_index != policy_index,
        BOUNDS,
    )?;
    let plan_account = &a[plan_index];
    let policy_account = &a[policy_index];
    need(
        a[0].is_signer
            && a[1].is_writable
            && plan_account.is_writable
            && plan_account.owner == program
            && !policy_account.is_writable
            && policy_account.owner == program
            && a[0].key != a[1].key
            && a[1].key != plan_account.key
            && plan_account.key != policy_account.key,
        IDENTITY,
    )?;
    let id = ad(dex::key(d, 4))?;
    let sequence = ad(dex::u64_at(d, 36))?;
    let version = ad(dex::u64_at(d, 44))?;
    let max_age = ad(dex::u64_at(d, 52))?;
    let graph_bytes = &d[60..];
    let graph = ad(Graph::decode(graph_bytes, a.len()))?;
    // Plan and policy state must never be smuggled into a venue CPI or treated
    // as graph assets. They are writable/read-only control state respectively.
    for asset in &graph.assets[..graph.asset_count] {
        need(
            ![plan_index, policy_index].contains(&(asset.token as usize))
                && ![plan_index, policy_index].contains(&(asset.mint as usize))
                && ![plan_index, policy_index].contains(&(asset.program as usize)),
            IDENTITY,
        )?;
    }
    for leg in graph.legs[..graph.leg_count].iter().flatten() {
        need(
            ![plan_index, policy_index].contains(&(leg.program as usize))
                && !leg
                    .accounts
                    .iter()
                    .any(|index| [plan_index, policy_index].contains(&(*index as usize))),
            IDENTITY,
        )?;
    }

    let slot = Clock::get()?.slot;
    let (
        total_input,
        total_minimum_output,
        maximum_capsule_input,
        settled_input,
        settled_output,
        expires_slot,
        capsule_count,
    ) = {
        let state = plan_account.try_borrow_data()?;
        let (expected_plan, _) =
            Pubkey::find_program_address(&[b"capsule", a[0].key.as_ref(), &id], program);
        need(
            plan_account.key == &expected_plan
                && state.len() == LEN
                && &state[..8] == TAG
                && state[OWNER] == a[0].key.to_bytes()
                && state[ID] == id
                && state[POLICY] == policy_account.key.to_bytes()
                && state[INPUT_MINT] == a[graph.assets[0].mint as usize].key.to_bytes()
                && state[OUTPUT_MINT]
                    == a[graph.assets[graph.asset_count - 1].mint as usize]
                        .key
                        .to_bytes()
                && ad(dex::u64_at(&state, NEXT_SEQUENCE))? == sequence,
            IDENTITY,
        )?;
        (
            ad(dex::u64_at(&state, TOTAL_INPUT))?,
            ad(dex::u64_at(&state, TOTAL_MINIMUM_OUTPUT))?,
            ad(dex::u64_at(&state, MAXIMUM_CAPSULE_INPUT))?,
            ad(dex::u64_at(&state, SETTLED_INPUT))?,
            ad(dex::u64_at(&state, SETTLED_OUTPUT))?,
            ad(dex::u64_at(&state, EXPIRES_SLOT))?,
            ad(dex::u64_at(&state, CAPSULE_COUNT))?,
        )
    };
    need(
        total_input > 0
            && total_input <= MAX_PLAN_INPUT
            && total_minimum_output > 0
            && maximum_capsule_input > 0
            && maximum_capsule_input <= MAX_CAPSULE_INPUT
            && settled_input < total_input
            && capsule_count < MAX_CAPSULES
            && slot <= expires_slot
            && graph.deadline <= expires_slot,
        EXPIRED,
    )?;
    let remaining = total_input
        .checked_sub(settled_input)
        .ok_or(err(CONSERVATION))?;
    let expected_input = remaining.min(maximum_capsule_input);
    need(graph.input == expected_input, BOUNDS)?;
    let next_settled_input = settled_input
        .checked_add(graph.input)
        .ok_or(err(CONSERVATION))?;
    let required_cumulative_output = ceil_div(
        u128::from(next_settled_input)
            .checked_mul(u128::from(total_minimum_output))
            .ok_or(err(BOUNDS))?,
        u128::from(total_input),
    )?;
    let required_cumulative_output =
        u64::try_from(required_cumulative_output).map_err(|_| err(BOUNDS))?;
    let required_capsule_output = required_cumulative_output
        .saturating_sub(settled_output)
        .max(1);
    need(graph.min_out >= required_capsule_output, MIN_OUT)?;

    let mut inner = Vec::with_capacity(d.len() - 40);
    inner.extend_from_slice(&[7, policy_index as u8, d[3], 0]);
    inner.extend_from_slice(&version.to_le_bytes());
    inner.extend_from_slice(&max_age.to_le_bytes());
    inner.extend_from_slice(graph_bytes);
    stock::execute(program, a, &inner)?;
    let (receipt_program, receipt) =
        solana_program::program::get_return_data().ok_or(err(CONSERVATION))?;
    need(
        receipt_program == *program
            && receipt.len() >= 48
            && &receipt[..8] == b"SKEWSTK2"
            && ad(dex::u64_at(&receipt, 8))? == version,
        CONSERVATION,
    )?;
    let actual_input = ad(dex::u64_at(&receipt, 24))?;
    let actual_output = ad(dex::u64_at(&receipt, 32))?;
    need(actual_input == graph.input, CONSERVATION)?;
    let next_settled_output = settled_output
        .checked_add(actual_output)
        .ok_or(err(CONSERVATION))?;
    need(
        next_settled_output >= required_cumulative_output && next_settled_input <= total_input,
        MIN_OUT,
    )?;
    let next_sequence = sequence.checked_add(1).ok_or(err(BOUNDS))?;
    let next_capsule_count = capsule_count.checked_add(1).ok_or(err(BOUNDS))?;
    {
        let mut state = plan_account.try_borrow_mut_data()?;
        put(&mut state, SETTLED_INPUT, next_settled_input);
        put(&mut state, SETTLED_OUTPUT, next_settled_output);
        put(&mut state, NEXT_SEQUENCE, next_sequence);
        put(&mut state, CAPSULE_COUNT, next_capsule_count);
    }

    let mut out = [0u8; 96];
    out[..8].copy_from_slice(TAG);
    out[8..40].copy_from_slice(&id);
    for (chunk, value) in out[40..88].chunks_exact_mut(8).zip([
        sequence,
        actual_input,
        actual_output,
        next_settled_input,
        next_settled_output,
        u64::from(next_settled_input == total_input),
    ]) {
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    out[88..96].copy_from_slice(&required_cumulative_output.to_le_bytes());
    set_return_data(&out);
    Ok(())
}

#[allow(clippy::manual_is_multiple_of)]
fn ceil_div(numerator: u128, denominator: u128) -> Result<u128, ProgramError> {
    need(denominator != 0, BOUNDS)?;
    Ok(numerator / denominator + u128::from(numerator % denominator != 0))
}

fn put(state: &mut [u8], offset: usize, value: u64) {
    state[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
