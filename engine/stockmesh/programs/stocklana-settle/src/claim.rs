//! On-chain binding between an economic instrument, an issuer product mint and
//! the conservative exposure conversion used by OneBook price limits.
use super::*;

const TAG: &[u8; 8] = b"SKEWCLM1";
const LEN: usize = 288;
const AUTHORITY: usize = 8;
const INSTRUMENT: usize = 40;
const ISSUER: usize = 72;
const CASH_MINT: usize = 104;
const PRODUCT_MINT: usize = 136;
const POLICY: usize = 168;
const TOKEN_PROGRAM: usize = 200;
const POLICY_VERSION: usize = 232;
const CLAIM_VERSION: usize = 240;
const NUMERATOR: usize = 248;
const DENOMINATOR: usize = 256;
const UPDATED_SLOT: usize = 264;
const EXPIRES_SLOT: usize = 272;
const CONSERVATIVE_BPS: usize = 280;
const MODEL: usize = 282;
const FLAGS: usize = 283;
const ACTIVE: u8 = 1;

/// Opcode 17 publishes or advances a ProductClaim. Accounts are authority,
/// claim PDA, ProductPolicy, cash mint, product mint, product token program and
/// system program. Conversion values are versioned on chain and cannot be
/// supplied ad hoc by a taker.
///
/// Data: `opcode, model, flags, reserved, conservative_bps, reserved,
/// instrument, issuer, policy_version, maximum_policy_age, claim_version,
/// numerator, denominator, expires_slot`.
pub(super) fn publish(program: &Pubkey, a: &[AccountInfo], d: &[u8]) -> ProgramResult {
    need(
        d.len() == 120
            && d[0] == 17
            && matches!(
                d[1],
                exposure::FIXED_RATIONAL | exposure::TOKEN_2022_SCALED_UI
            )
            && d[2] == ACTIVE
            && d[3] == 0
            && d[6..8] == [0, 0]
            && a.len() == 7,
        BOUNDS,
    )?;
    need(
        a[0].is_signer
            && a[0].is_writable
            && a[1].is_writable
            && !a[2].is_writable
            && !a[3].is_writable
            && !a[4].is_writable
            && !a[5].is_writable
            && a[5].executable
            && a[4].owner == a[5].key
            && [TOKEN, graph::TOKEN_2022].contains(a[5].key)
            && [TOKEN, graph::TOKEN_2022].contains(a[3].owner)
            && *a[6].key == system_program::id(),
        IDENTITY,
    )?;
    let conservative_bps = u16::from_le_bytes([d[4], d[5]]);
    let instrument = ad(dex::key(d, 8))?;
    let issuer = ad(dex::key(d, 40))?;
    let policy_version = ad(dex::u64_at(d, 72))?;
    let maximum_policy_age = ad(dex::u64_at(d, 80))?;
    let claim_version = ad(dex::u64_at(d, 88))?;
    let numerator = ad(dex::u64_at(d, 96))?;
    let denominator = ad(dex::u64_at(d, 104))?;
    let expires_slot = ad(dex::u64_at(d, 112))?;
    need(
        instrument != [0; 32]
            && issuer != [0; 32]
            && policy_version > 0
            && claim_version > 0
            && (1..=150).contains(&maximum_policy_age)
            && (1..=10_000).contains(&conservative_bps)
            && numerator > 0
            && denominator > 0,
        BOUNDS,
    )?;
    let clock = Clock::get()?;
    need(
        expires_slot >= clock.slot
            && expires_slot <= clock.slot.checked_add(1_000_000).ok_or(err(BOUNDS))?,
        EXPIRED,
    )?;
    stock::validate_policy_for_pair(
        program,
        a,
        2,
        policy_version,
        maximum_policy_age,
        false,
        3,
        4,
        clock.slot,
        &clock,
    )?;
    {
        let policy = a[2].try_borrow_data()?;
        need(
            policy[40..72] == instrument && policy[72..104] == issuer,
            IDENTITY,
        )?;
    }
    let (claim, bump) = Pubkey::find_program_address(
        &[b"claim", a[0].key.as_ref(), &instrument, a[4].key.as_ref()],
        program,
    );
    need(a[1].key == &claim, IDENTITY)?;
    if a[1].owner == program {
        let state = a[1].try_borrow_data()?;
        need(
            state.len() == LEN
                && &state[..8] == TAG
                && state[AUTHORITY..AUTHORITY + 32] == a[0].key.to_bytes()
                && state[INSTRUMENT..INSTRUMENT + 32] == instrument
                && state[ISSUER..ISSUER + 32] == issuer
                && state[CASH_MINT..CASH_MINT + 32] == a[3].key.to_bytes()
                && state[PRODUCT_MINT..PRODUCT_MINT + 32] == a[4].key.to_bytes()
                && state[POLICY..POLICY + 32] == a[2].key.to_bytes()
                && state[TOKEN_PROGRAM..TOKEN_PROGRAM + 32] == a[5].key.to_bytes()
                && ad(dex::u64_at(&state, CLAIM_VERSION))?.checked_add(1) == Some(claim_version),
            IDENTITY,
        )?;
    } else {
        need(
            a[1].data_is_empty() && a[1].owner == &system_program::id() && claim_version == 1,
            IDENTITY,
        )?;
        let rent = Rent::get()?.minimum_balance(LEN);
        let bump_seed = [bump];
        let seeds: &[&[u8]] = &[
            b"claim",
            a[0].key.as_ref(),
            &instrument,
            a[4].key.as_ref(),
            &bump_seed,
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
    state.fill(0);
    state[..8].copy_from_slice(TAG);
    state[AUTHORITY..AUTHORITY + 32].copy_from_slice(a[0].key.as_ref());
    state[INSTRUMENT..INSTRUMENT + 32].copy_from_slice(&instrument);
    state[ISSUER..ISSUER + 32].copy_from_slice(&issuer);
    state[CASH_MINT..CASH_MINT + 32].copy_from_slice(a[3].key.as_ref());
    state[PRODUCT_MINT..PRODUCT_MINT + 32].copy_from_slice(a[4].key.as_ref());
    state[POLICY..POLICY + 32].copy_from_slice(a[2].key.as_ref());
    state[TOKEN_PROGRAM..TOKEN_PROGRAM + 32].copy_from_slice(a[5].key.as_ref());
    for (offset, value) in [
        (POLICY_VERSION, policy_version),
        (CLAIM_VERSION, claim_version),
        (NUMERATOR, numerator),
        (DENOMINATOR, denominator),
        (UPDATED_SLOT, clock.slot),
        (EXPIRES_SLOT, expires_slot),
    ] {
        state[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    state[CONSERVATIVE_BPS..CONSERVATIVE_BPS + 2].copy_from_slice(&conservative_bps.to_le_bytes());
    state[MODEL] = d[1];
    state[FLAGS] = d[2];
    set_return_data(&state[..LEN]);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn validate(
    program: &Pubkey,
    a: &[AccountInfo],
    claim: usize,
    policy: usize,
    cash_mint: usize,
    product_mint: usize,
    token_program: usize,
    policy_version: u64,
    maximum_policy_age: u64,
    allow_underlying_closed: bool,
    model: u8,
    conservative_bps: u16,
    numerator: u64,
    denominator: u64,
    deadline: u64,
    clock: &Clock,
) -> ProgramResult {
    need(
        [claim, policy, cash_mint, product_mint, token_program]
            .iter()
            .all(|index| *index < a.len()),
        BOUNDS,
    )?;
    let state = a[claim].try_borrow_data()?;
    need(
        a[claim].owner == program
            && !a[claim].is_writable
            && !a[claim].executable
            && state.len() == LEN
            && &state[..8] == TAG
            && state[FLAGS] == ACTIVE
            && state[POLICY..POLICY + 32] == a[policy].key.to_bytes()
            && state[CASH_MINT..CASH_MINT + 32] == a[cash_mint].key.to_bytes()
            && state[PRODUCT_MINT..PRODUCT_MINT + 32] == a[product_mint].key.to_bytes()
            && state[TOKEN_PROGRAM..TOKEN_PROGRAM + 32] == a[token_program].key.to_bytes()
            && ad(dex::u64_at(&state, POLICY_VERSION))? == policy_version
            && ad(dex::u64_at(&state, NUMERATOR))? == numerator
            && ad(dex::u64_at(&state, DENOMINATOR))? == denominator
            && u16::from_le_bytes([state[CONSERVATIVE_BPS], state[CONSERVATIVE_BPS + 1]])
                == conservative_bps
            && state[MODEL] == model,
        IDENTITY,
    )?;
    let authority = Pubkey::new_from_array(ad(dex::key(&state, AUTHORITY))?);
    let instrument = ad(dex::key(&state, INSTRUMENT))?;
    let (expected, _) = Pubkey::find_program_address(
        &[
            b"claim",
            authority.as_ref(),
            &instrument,
            a[product_mint].key.as_ref(),
        ],
        program,
    );
    need(a[claim].key == &expected, IDENTITY)?;
    need(
        clock.slot <= ad(dex::u64_at(&state, EXPIRES_SLOT))?
            && deadline <= ad(dex::u64_at(&state, EXPIRES_SLOT))?,
        EXPIRED,
    )?;
    drop(state);
    stock::validate_policy_for_pair(
        program,
        a,
        policy,
        policy_version,
        maximum_policy_age,
        allow_underlying_closed,
        cash_mint,
        product_mint,
        deadline,
        clock,
    )
}
