//! Continuous, escrow-backed StockMesh asks.
//!
//! Every order is an independent PDA keyed by `(owner, nonce)`. There is no
//! mutable market account, epoch, crank cursor or privileged sequencer. A
//! product-aware FlowCell may consume several of these cells and route the
//! buyer's residual through deployed venues in the same atomic instruction.
use super::*;

const TAG: &[u8; 8] = b"SKEWORD1";
const LEN: usize = 336;
const OPEN: u8 = 1;
const CANCELLED: u8 = 2;

const OWNER: usize = 8;
const INSTRUMENT: usize = 40;
const POLICY: usize = 72;
const CASH_MINT: usize = 104;
const PRODUCT_MINT: usize = 136;
const ESCROW: usize = 168;
const CASH_DESTINATION: usize = 200;
const ORDER_NONCE: usize = 232;
const TOTAL_STOCK: usize = 240;
const REMAINING_STOCK: usize = 248;
const MIN_CASH_PER_SHARE: usize = 256;
const EXPIRES_SLOT: usize = 264;
const REVISION: usize = 272;
const FILLED_CASH: usize = 280;
const CREATED_SLOT: usize = 288;
const INITIAL_POLICY_VERSION: usize = 296;
const MODEL: usize = 304;
const CONSERVATIVE_BPS: usize = 305;
const NUMERATOR: usize = 312;
const DENOMINATOR: usize = 320;
const STATUS: usize = 328;

fn transfer_checked(
    a: &[AccountInfo],
    source: usize,
    mint: usize,
    destination: usize,
    authority: usize,
    token_program: usize,
    amount: u64,
) -> ProgramResult {
    let mut data = [0u8; 10];
    data[0] = 12;
    data[1..9].copy_from_slice(&amount.to_le_bytes());
    data[9] = a[mint].try_borrow_data()?[44];
    let instruction = Instruction {
        program_id: *a[token_program].key,
        accounts: vec![
            AccountMeta::new(*a[source].key, false),
            AccountMeta::new_readonly(*a[mint].key, false),
            AccountMeta::new(*a[destination].key, false),
            AccountMeta::new_readonly(*a[authority].key, true),
        ],
        data: data.to_vec(),
    };
    invoke_checked_accounts(&instruction, a)
}

#[allow(clippy::too_many_arguments)]
fn transfer_checked_signed(
    program: &Pubkey,
    a: &[AccountInfo],
    source: usize,
    mint: usize,
    destination: usize,
    order: usize,
    token_program: usize,
    owner: usize,
    nonce: u64,
    amount: u64,
) -> ProgramResult {
    let mut data = [0u8; 10];
    data[0] = 12;
    data[1..9].copy_from_slice(&amount.to_le_bytes());
    data[9] = a[mint].try_borrow_data()?[44];
    let instruction = Instruction {
        program_id: *a[token_program].key,
        accounts: vec![
            AccountMeta::new(*a[source].key, false),
            AccountMeta::new_readonly(*a[mint].key, false),
            AccountMeta::new(*a[destination].key, false),
            AccountMeta::new_readonly(*a[order].key, true),
        ],
        data: data.to_vec(),
    };
    for account in a {
        if account.is_writable {
            drop(account.try_borrow_mut_lamports()?);
            drop(account.try_borrow_mut_data()?);
        } else {
            drop(account.try_borrow_lamports()?);
            drop(account.try_borrow_data()?);
        }
    }
    let nonce_bytes = nonce.to_le_bytes();
    let (expected, bump) =
        Pubkey::find_program_address(&[b"onebook", a[owner].key.as_ref(), &nonce_bytes], program);
    need(a[order].key == &expected, IDENTITY)?;
    let bump_seed = [bump];
    let seeds: &[&[u8]] = &[b"onebook", a[owner].key.as_ref(), &nonce_bytes, &bump_seed];
    invoke_signed(&instruction, a, &[seeds])
}

fn ceil_q32(value: u64, price: u64) -> Result<u64, ProgramError> {
    let product = u128::from(value)
        .checked_mul(u128::from(price))
        .ok_or(err(BOUNDS))?;
    let result = product.checked_add((1u128 << 32) - 1).ok_or(err(BOUNDS))? >> 32;
    u64::try_from(result).map_err(|_| err(BOUNDS))
}

/// Opcode 15 creates one funded ask cell. The escrow token account is created
/// outside this program with the order PDA as authority and must be empty.
/// Layout: `opcode, allow_closed, model, reserved, conservative_bps, reserved,
/// order_nonce, stock_atoms, minimum_cash_atoms_per_share, expires_slot,
/// policy_version, maximum_policy_age, numerator, denominator, instrument`.
pub(super) fn place(program: &Pubkey, a: &[AccountInfo], d: &[u8]) -> ProgramResult {
    need(
        d.len() == 104
            && d[0] == 15
            && d[1] <= 1
            && matches!(
                d[2],
                exposure::FIXED_RATIONAL | exposure::TOKEN_2022_SCALED_UI
            )
            && d[3] == 0
            && d[6..8] == [0, 0]
            && a.len() == 12,
        BOUNDS,
    )?;
    need(
        a[0].is_signer
            && a[0].is_writable
            && a[1].is_writable
            && a[2].is_writable
            && a[3].is_writable
            && a[4].is_writable
            && *a[11].key == system_program::id(),
        IDENTITY,
    )?;
    let conservative_bps = u16::from_le_bytes([d[4], d[5]]);
    let order_nonce = ad(dex::u64_at(d, 8))?;
    let stock_atoms = ad(dex::u64_at(d, 16))?;
    let minimum_cash_per_share = ad(dex::u64_at(d, 24))?;
    let expires_slot = ad(dex::u64_at(d, 32))?;
    let policy_version = ad(dex::u64_at(d, 40))?;
    let maximum_policy_age = ad(dex::u64_at(d, 48))?;
    let numerator = ad(dex::u64_at(d, 56))?;
    let denominator = ad(dex::u64_at(d, 64))?;
    let instrument = ad(dex::key(d, 72))?;
    need(
        order_nonce < u64::MAX
            && stock_atoms > 0
            && stock_atoms <= dex::MAX_INPUT
            && minimum_cash_per_share > 0
            && policy_version > 0
            && (1..=150).contains(&maximum_policy_age)
            && (1..=10_000).contains(&conservative_bps)
            && numerator > 0
            && denominator > 0
            && instrument != [0; 32],
        BOUNDS,
    )?;
    let clock = Clock::get()?;
    need(
        clock.unix_timestamp >= 0
            && expires_slot > clock.slot
            && expires_slot <= clock.slot.checked_add(1_000_000).ok_or(err(BOUNDS))?,
        EXPIRED,
    )?;
    let nonce_bytes = order_nonce.to_le_bytes();
    let (order, bump) =
        Pubkey::find_program_address(&[b"onebook", a[0].key.as_ref(), &nonce_bytes], program);
    need(
        a[1].key == &order && a[1].data_is_empty() && a[1].owner == &system_program::id(),
        IDENTITY,
    )?;
    claim::validate(
        program,
        a,
        10,
        9,
        5,
        6,
        7,
        policy_version,
        maximum_policy_age,
        d[1] == 1,
        d[2],
        conservative_bps,
        numerator,
        denominator,
        clock.slot,
        &clock,
    )?;
    {
        let policy = a[9].try_borrow_data()?;
        need(policy[40..72] == instrument, IDENTITY)?;
    }
    let source_before = graph::checked_asset(&a[0], &a[2], &a[6], &a[7], true)?;
    let escrow_before = graph::checked_asset(&a[1], &a[3], &a[6], &a[7], false)?;
    let _cash_before = graph::checked_asset(&a[0], &a[4], &a[5], &a[8], true)?;
    need(source_before >= stock_atoms && escrow_before == 0, BOUNDS)?;
    let exposure = exposure::convert(
        a,
        6,
        d[2],
        conservative_bps,
        numerator,
        denominator,
        stock_atoms,
        clock.unix_timestamp,
    )?;
    need(
        exposure > 0 && ceil_q32(exposure, minimum_cash_per_share)? > 0,
        BOUNDS,
    )?;

    let rent = Rent::get()?.minimum_balance(LEN);
    let bump_seed = [bump];
    let seeds: &[&[u8]] = &[b"onebook", a[0].key.as_ref(), &nonce_bytes, &bump_seed];
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
    transfer_checked(a, 2, 6, 3, 0, 7, stock_atoms)?;
    let source_after = graph::checked_asset(&a[0], &a[2], &a[6], &a[7], false)?;
    let escrow_after = graph::checked_asset(&a[1], &a[3], &a[6], &a[7], false)?;
    need(
        source_before.checked_sub(source_after) == Some(stock_atoms)
            && escrow_after.checked_sub(escrow_before) == Some(stock_atoms),
        CONSERVATION,
    )?;
    let mut state = a[1].try_borrow_mut_data()?;
    state.fill(0);
    state[..8].copy_from_slice(TAG);
    state[OWNER..OWNER + 32].copy_from_slice(a[0].key.as_ref());
    state[INSTRUMENT..INSTRUMENT + 32].copy_from_slice(&instrument);
    state[POLICY..POLICY + 32].copy_from_slice(a[10].key.as_ref());
    state[CASH_MINT..CASH_MINT + 32].copy_from_slice(a[5].key.as_ref());
    state[PRODUCT_MINT..PRODUCT_MINT + 32].copy_from_slice(a[6].key.as_ref());
    state[ESCROW..ESCROW + 32].copy_from_slice(a[3].key.as_ref());
    state[CASH_DESTINATION..CASH_DESTINATION + 32].copy_from_slice(a[4].key.as_ref());
    for (offset, value) in [
        (ORDER_NONCE, order_nonce),
        (TOTAL_STOCK, stock_atoms),
        (REMAINING_STOCK, stock_atoms),
        (MIN_CASH_PER_SHARE, minimum_cash_per_share),
        (EXPIRES_SLOT, expires_slot),
        (REVISION, 0),
        (FILLED_CASH, 0),
        (CREATED_SLOT, clock.slot),
        (INITIAL_POLICY_VERSION, policy_version),
        (NUMERATOR, numerator),
        (DENOMINATOR, denominator),
    ] {
        state[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    state[MODEL] = d[2];
    state[CONSERVATIVE_BPS..CONSERVATIVE_BPS + 2].copy_from_slice(&conservative_bps.to_le_bytes());
    state[STATUS] = OPEN;
    set_return_data(&state[..LEN]);
    Ok(())
}

/// Validate a resting ask against the current policy and product conversion.
/// The returned nonce is used only to derive the escrow authority PDA.
#[allow(clippy::too_many_arguments)]
pub(super) fn validate_fill(
    program: &Pubkey,
    a: &[AccountInfo],
    order: usize,
    owner: usize,
    escrow: usize,
    cash_destination: usize,
    policy: usize,
    cash_mint: usize,
    product_mint: usize,
    product_program: usize,
    expected_revision: u64,
    stock_atoms: u64,
    cash_atoms: u64,
    model: u8,
    conservative_bps: u16,
    numerator: u64,
    denominator: u64,
    clock: &Clock,
) -> Result<u64, ProgramError> {
    need(
        [
            order,
            owner,
            escrow,
            cash_destination,
            policy,
            cash_mint,
            product_mint,
            product_program,
        ]
        .iter()
        .all(|index| *index < a.len())
            && a[order].is_writable
            && a[order].owner == program
            && !a[owner].is_writable
            && stock_atoms > 0
            && cash_atoms > 0,
        IDENTITY,
    )?;
    let state = a[order].try_borrow_data()?;
    need(
        state.len() == LEN
            && &state[..8] == TAG
            && state[OWNER..OWNER + 32] == a[owner].key.to_bytes()
            && state[POLICY..POLICY + 32] == a[policy].key.to_bytes()
            && state[CASH_MINT..CASH_MINT + 32] == a[cash_mint].key.to_bytes()
            && state[PRODUCT_MINT..PRODUCT_MINT + 32] == a[product_mint].key.to_bytes()
            && state[ESCROW..ESCROW + 32] == a[escrow].key.to_bytes()
            && state[CASH_DESTINATION..CASH_DESTINATION + 32] == a[cash_destination].key.to_bytes(),
        IDENTITY,
    )?;
    need(state[STATUS] == OPEN, REPLAY)?;
    need(
        ad(dex::u64_at(&state, REVISION))? == expected_revision,
        REPLAY,
    )?;
    need(
        clock.slot <= ad(dex::u64_at(&state, EXPIRES_SLOT))?,
        EXPIRED,
    )?;
    let nonce = ad(dex::u64_at(&state, ORDER_NONCE))?;
    let nonce_bytes = nonce.to_le_bytes();
    let (expected, _) =
        Pubkey::find_program_address(&[b"onebook", a[owner].key.as_ref(), &nonce_bytes], program);
    need(a[order].key == &expected, IDENTITY)?;
    let remaining = ad(dex::u64_at(&state, REMAINING_STOCK))?;
    let minimum_cash_per_share = ad(dex::u64_at(&state, MIN_CASH_PER_SHARE))?;
    need(stock_atoms <= remaining, BOUNDS)?;
    drop(state);
    let escrow_balance = graph::checked_asset(
        &a[order],
        &a[escrow],
        &a[product_mint],
        &a[product_program],
        false,
    )?;
    need(escrow_balance >= remaining, CONSERVATION)?;
    let exposure = exposure::convert(
        a,
        product_mint,
        model,
        conservative_bps,
        numerator,
        denominator,
        stock_atoms,
        clock.unix_timestamp,
    )?;
    need(
        exposure > 0 && cash_atoms >= ceil_q32(exposure, minimum_cash_per_share)?,
        MIN_OUT,
    )?;
    Ok(nonce)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn transfer_fill(
    program: &Pubkey,
    a: &[AccountInfo],
    source: usize,
    mint: usize,
    destination: usize,
    order: usize,
    token_program: usize,
    owner: usize,
    nonce: u64,
    amount: u64,
) -> ProgramResult {
    transfer_checked_signed(
        program,
        a,
        source,
        mint,
        destination,
        order,
        token_program,
        owner,
        nonce,
        amount,
    )
}

pub(super) fn record_fill(
    a: &[AccountInfo],
    order: usize,
    expected_revision: u64,
    stock_atoms: u64,
    cash_atoms: u64,
) -> ProgramResult {
    let mut state = a[order].try_borrow_mut_data()?;
    need(
        state.len() == LEN
            && &state[..8] == TAG
            && state[STATUS] == OPEN
            && ad(dex::u64_at(&state, REVISION))? == expected_revision,
        REPLAY,
    )?;
    let remaining = ad(dex::u64_at(&state, REMAINING_STOCK))?
        .checked_sub(stock_atoms)
        .ok_or(err(CONSERVATION))?;
    let filled_cash = ad(dex::u64_at(&state, FILLED_CASH))?
        .checked_add(cash_atoms)
        .ok_or(err(CONSERVATION))?;
    state[REMAINING_STOCK..REMAINING_STOCK + 8].copy_from_slice(&remaining.to_le_bytes());
    state[REVISION..REVISION + 8].copy_from_slice(
        &expected_revision
            .checked_add(1)
            .ok_or(err(BOUNDS))?
            .to_le_bytes(),
    );
    state[FILLED_CASH..FILLED_CASH + 8].copy_from_slice(&filled_cash.to_le_bytes());
    Ok(())
}

/// Opcode 16 releases all remaining escrowed stock back to the owner. The
/// tombstone is retained so the same `(owner, nonce)` can never be replayed.
pub(super) fn cancel(program: &Pubkey, a: &[AccountInfo], d: &[u8]) -> ProgramResult {
    need(
        d.len() == 16 && d[0] == 16 && d[1..8] == [0; 7] && a.len() == 6,
        BOUNDS,
    )?;
    need(
        a[0].is_signer
            && a[0].is_writable
            && a[1].is_writable
            && a[1].owner == program
            && a[2].is_writable
            && a[3].is_writable,
        IDENTITY,
    )?;
    let nonce = ad(dex::u64_at(d, 8))?;
    let nonce_bytes = nonce.to_le_bytes();
    let (expected, _) =
        Pubkey::find_program_address(&[b"onebook", a[0].key.as_ref(), &nonce_bytes], program);
    need(a[1].key == &expected, IDENTITY)?;
    let (remaining, revision) = {
        let state = a[1].try_borrow_data()?;
        need(
            state.len() == LEN
                && &state[..8] == TAG
                && state[STATUS] == OPEN
                && state[OWNER..OWNER + 32] == a[0].key.to_bytes()
                && state[PRODUCT_MINT..PRODUCT_MINT + 32] == a[4].key.to_bytes()
                && state[ESCROW..ESCROW + 32] == a[2].key.to_bytes(),
            IDENTITY,
        )?;
        (
            ad(dex::u64_at(&state, REMAINING_STOCK))?,
            ad(dex::u64_at(&state, REVISION))?,
        )
    };
    let before = graph::checked_asset(&a[1], &a[2], &a[4], &a[5], true)?;
    let destination_before = graph::checked_asset(&a[0], &a[3], &a[4], &a[5], false)?;
    need(before >= remaining, CONSERVATION)?;
    if remaining > 0 {
        transfer_checked_signed(program, a, 2, 4, 3, 1, 5, 0, nonce, remaining)?;
    }
    let after = graph::checked_asset(&a[1], &a[2], &a[4], &a[5], false)?;
    let destination_after = graph::checked_asset(&a[0], &a[3], &a[4], &a[5], false)?;
    need(
        before.checked_sub(after) == Some(remaining)
            && destination_after.checked_sub(destination_before) == Some(remaining),
        CONSERVATION,
    )?;
    let mut state = a[1].try_borrow_mut_data()?;
    state[REMAINING_STOCK..REMAINING_STOCK + 8].fill(0);
    state[REVISION..REVISION + 8]
        .copy_from_slice(&revision.checked_add(1).ok_or(err(BOUNDS))?.to_le_bytes());
    state[STATUS] = CANCELLED;
    let mut receipt = [0u8; 32];
    receipt[..8].copy_from_slice(TAG);
    receipt[8..16].copy_from_slice(&nonce.to_le_bytes());
    receipt[16..24].copy_from_slice(&remaining.to_le_bytes());
    receipt[24..32].copy_from_slice(&(revision + 1).to_le_bytes());
    set_return_data(&receipt);
    Ok(())
}
