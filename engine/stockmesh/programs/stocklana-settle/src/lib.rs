//! Single-wallet, two-venue atomic exact-input settlement. The signed instruction
//! authorizes a bounded Phoenix IOC budget and sends the observed residual to
//! Raydium CPMM. No arbitrary CPI bytes and no caller-selected program IDs.
use solana_program::{
    account_info::AccountInfo,
    clock::Clock,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::{invoke, invoke_signed, set_return_data},
    program_error::ProgramError,
    pubkey,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};
use solana_sdk_ids::system_program;
use solana_system_interface::instruction as system_instruction;
use stocklana_adapters::{self as dex, PhoenixBook, RaydiumPool};

mod allocation;
mod capsule;
mod claim;
mod crossing;
mod exposure;
mod flowcell;
mod funding;
mod graph;
mod meshcell;
mod order;
mod stock;

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint_no_alloc!(process_instruction);

pub const PHOENIX: Pubkey = pubkey!("PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY");
pub const PHOENIX_LOG: Pubkey = pubkey!("7aDTsspkQNGKmrexAN7FLx9oxU3iPczSSvHNggyuqYkR");
pub const RAYDIUM: Pubkey = pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
pub const RAYDIUM_AUTH: Pubkey = pubkey!("GpMZbSM2GgvTKHJirzeGfMFoaZ8UR2X7F4v8vHTvxFbL");
pub const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const NONCE_LEN: usize = 64;
pub const NONCE_TAG: &[u8; 8] = b"SKEWSEQ1";
// Stable custom errors, recorded in adversarial SBF tests.
pub const IDENTITY: u32 = 1;
pub const REPLAY: u32 = 2;
pub const EXPIRED: u32 = 3;
pub const BOUNDS: u32 = 4;
pub const MIN_OUT: u32 = 5;
pub const CONSERVATION: u32 = 6;
pub const ADAPTER: u32 = 7;
fn err(code: u32) -> ProgramError {
    ProgramError::Custom(code)
}
fn need(ok: bool, code: u32) -> ProgramResult {
    if ok {
        Ok(())
    } else {
        Err(err(code))
    }
}
fn ad<T>(value: dex::Result<T>) -> Result<T, ProgramError> {
    value.map_err(|_| err(ADAPTER))
}

#[inline(never)]
pub fn process_instruction(program: &Pubkey, a: &[AccountInfo], data: &[u8]) -> ProgramResult {
    match data.first() {
        Some(0) if data.len() == 1 => initialize(program, a),
        Some(1) if data.len() == 44 => settle(program, a, data),
        Some(2) => graph::settle_graph(program, a, data),
        Some(3) => crossing::clear(program, a, data),
        Some(4 | 9) => graph::settle_graph(program, a, data),
        Some(5) => crossing::clear(program, a, data),
        // Opcode 6 was the pre-rights-commitment ProductPolicy v1 publisher.
        // It is deliberately retired: settlement accepts only v2 policies
        // whose PDA address commits the exact rights document hash.
        Some(19) => stock::publish(program, a, data),
        Some(7) => stock::execute(program, a, data),
        Some(20) => stock::execute_reverse(program, a, data),
        Some(10) => capsule::initialize(program, a, data),
        Some(11) => capsule::execute(program, a, data),
        Some(12) => flowcell::clear(program, a, data),
        Some(13) => exposure::execute(program, a, data),
        Some(14) => meshcell::clear(program, a, data),
        Some(15) => order::place(program, a, data),
        Some(16) => order::cancel(program, a, data),
        Some(17) => claim::publish(program, a, data),
        Some(18) => funding::execute(program, a, data),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}
fn initialize(program: &Pubkey, a: &[AccountInfo]) -> ProgramResult {
    need(a.len() == 3, BOUNDS)?;
    need(
        a[0].is_signer && a[0].is_writable && a[1].is_writable,
        IDENTITY,
    )?;
    need(*a[2].key == system_program::id(), IDENTITY)?;
    let (pda, bump) = Pubkey::find_program_address(&[b"stocklana", a[0].key.as_ref()], program);
    need(
        *a[1].key == pda && a[1].data_is_empty() && *a[1].owner == system_program::id(),
        IDENTITY,
    )?;
    let required = Rent::get()?.minimum_balance(NONCE_LEN);
    let seeds: &[&[u8]] = &[b"stocklana", a[0].key.as_ref(), &[bump]];
    // Accept harmless prefunding of the PDA without allowing reinitialization.
    if a[1].lamports() < required {
        invoke(
            &system_instruction::transfer(a[0].key, a[1].key, required - a[1].lamports()),
            a,
        )?;
    }
    invoke_signed(
        &system_instruction::allocate(a[1].key, NONCE_LEN as u64),
        a,
        &[seeds],
    )?;
    invoke_signed(&system_instruction::assign(a[1].key, program), a, &[seeds])?;
    let mut state = a[1].try_borrow_mut_data()?;
    state[..8].copy_from_slice(NONCE_TAG);
    state[8..40].copy_from_slice(a[0].key.as_ref());
    Ok(())
}
fn token(account: &AccountInfo, mint: &Pubkey, authority: &Pubkey) -> Result<u64, ProgramError> {
    need(*account.owner == TOKEN, IDENTITY)?;
    let data = account.try_borrow_data()?;
    need(
        ad(dex::key(&data, 0))? == mint.to_bytes()
            && ad(dex::key(&data, 32))? == authority.to_bytes(),
        IDENTITY,
    )?;
    ad(dex::token_amount(&data))
}
fn amount(account: &AccountInfo) -> Result<u64, ProgramError> {
    ad(dex::token_amount(&account.try_borrow_data()?))
}
fn executable(account: &AccountInfo, id: &Pubkey) -> ProgramResult {
    need(account.key == id && account.executable, IDENTITY)
}

/// Preserve SDK borrow safety without its metadata x AccountInfo key search.
/// Every account supplied to the syscall is checked, using the caller's stronger
/// writable privileges. All guards are dropped before CPI. This also protects
/// later edits from accidentally carrying a Ref across the syscall.
#[inline(never)]
fn invoke_checked_accounts(ix: &Instruction, accounts: &[AccountInfo]) -> ProgramResult {
    for account in accounts {
        if account.is_writable {
            drop(account.try_borrow_mut_lamports()?);
            drop(account.try_borrow_mut_data()?);
        } else {
            drop(account.try_borrow_lamports()?);
            drop(account.try_borrow_data()?);
        }
    }
    // SDK's invoke_unchecked is missing an unsafe marker. The necessary borrow
    // checks are performed above; Solana still enforces CPI privilege/ownership.
    solana_program::program::invoke_unchecked(ix, accounts)
}

fn settle(program: &Pubkey, a: &[AccountInfo], d: &[u8]) -> ProgramResult {
    need(a.len() == 19, BOUNDS)?;
    let mode = d[1];
    let sell = d[2] == 1;
    let matches = u64::from(d[3]);
    let sequence = ad(dex::u64_at(d, 4))?;
    let input = ad(dex::u64_at(d, 12))?;
    let min_out = ad(dex::u64_at(d, 20))?;
    let deadline = ad(dex::u64_at(d, 28))?;
    let phoenix_budget = ad(dex::u64_at(d, 36))?;
    need(
        (1..=3).contains(&mode)
            && d[2] <= 1
            && input > 0
            && input <= dex::MAX_INPUT
            && min_out > 0
            && phoenix_budget <= input
            && (1..=dex::MAX_MATCHES).contains(&matches),
        BOUNDS,
    )?;
    need(
        a[0].is_signer && a[0].is_writable && a[1].is_writable && *a[1].owner == *program,
        IDENTITY,
    )?;
    {
        let nonce = a[1].try_borrow_data()?;
        need(
            nonce.len() == NONCE_LEN
                && &nonce[..8] == NONCE_TAG
                && nonce[8..40] == a[0].key.to_bytes(),
            IDENTITY,
        )?;
        need(ad(dex::u64_at(&nonce, 40))? == sequence, REPLAY)?;
    }
    let next_sequence = sequence.checked_add(1).ok_or(err(BOUNDS))?;
    let clock = Clock::get()?;
    need(clock.slot <= deadline && clock.unix_timestamp >= 0, EXPIRED)?;
    executable(&a[4], &TOKEN)?;
    need(
        *a[5].owner == TOKEN && *a[6].owner == TOKEN && a[5].key != a[6].key,
        IDENTITY,
    )?;
    // No aliases among writable financial state, including unused venue accounts.
    const WRITABLE: [usize; 10] = [1, 2, 3, 9, 10, 11, 15, 16, 17, 18];
    let mut prefixes = [0u64; 10];
    for (i, index) in WRITABLE.iter().enumerate() {
        need(a[*index].is_writable, IDENTITY)?;
        prefixes[i] = ad(dex::u64_at(a[*index].key.as_ref(), 0))?;
        for j in 0..i {
            // A collision triggers the full comparison; no probabilistic check.
            if prefixes[i] == prefixes[j] {
                need(a[*index].key != a[WRITABLE[j]].key, IDENTITY)?;
            }
        }
    }
    let before_base = token(&a[2], a[5].key, a[0].key)?;
    let before_quote = token(&a[3], a[6].key, a[0].key)?;
    let (src, dst, before_in, before_out) = if sell {
        (2, 3, before_base, before_quote)
    } else {
        (3, 2, before_quote, before_base)
    };
    need(before_in >= input, BOUNDS)?;
    let mut legs = 0u64;
    if mode & 1 != 0
        && phoenix_budget > 0
        && phoenix(a, sell, phoenix_budget, matches, deadline, &clock)?
    {
        legs += 1;
    }
    let after_phoenix = amount(&a[src])?;
    let spent = before_in
        .checked_sub(after_phoenix)
        .ok_or(err(CONSERVATION))?;
    need(spent <= input && spent <= phoenix_budget, CONSERVATION)?;
    let residual = input - spent;
    if mode & 2 != 0 && residual > 0 {
        raydium(a, sell, residual, clock.unix_timestamp as u64)?;
        legs += 1;
    }
    let actual_in = before_in
        .checked_sub(amount(&a[src])?)
        .ok_or(err(CONSERVATION))?;
    let actual_out = amount(&a[dst])?
        .checked_sub(before_out)
        .ok_or(err(CONSERVATION))?;
    need(actual_in == input, CONSERVATION)?;
    need(actual_out >= min_out, MIN_OUT)?;
    {
        let mut nonce = a[1].try_borrow_mut_data()?;
        nonce[40..48].copy_from_slice(&next_sequence.to_le_bytes());
        nonce[48..56].copy_from_slice(&actual_in.to_le_bytes());
        nonce[56..64].copy_from_slice(&actual_out.to_le_bytes());
    }
    // Fixed binary receipt, no formatted hot-path logs.
    let mut receipt = [0u8; 40];
    for (chunk, value) in receipt
        .chunks_exact_mut(8)
        .zip([sequence, actual_in, actual_out, spent, legs])
    {
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    set_return_data(&receipt);
    Ok(())
}

#[inline(never)]
fn phoenix(
    a: &[AccountInfo],
    sell: bool,
    input: u64,
    matches: u64,
    deadline: u64,
    clock: &Clock,
) -> Result<bool, ProgramError> {
    executable(&a[7], &PHOENIX)?;
    need(*a[8].key == PHOENIX_LOG && *a[9].owner == PHOENIX, IDENTITY)?;
    let (header, expected) = {
        let state = a[9].try_borrow_data()?;
        let book = ad(PhoenixBook::decode(&state))?;
        let expected = ad(book.quote(
            &a[0].key.to_bytes(),
            sell,
            input,
            matches,
            clock.slot,
            clock.unix_timestamp as u64,
        ))?;
        (book.header, expected)
    };
    need(
        header.base_mint == a[5].key.to_bytes()
            && header.quote_mint == a[6].key.to_bytes()
            && header.base_vault == a[10].key.to_bytes()
            && header.quote_vault == a[11].key.to_bytes(),
        IDENTITY,
    )?;
    need(*a[10].owner == TOKEN && *a[11].owner == TOKEN, IDENTITY)?;
    // Skip a provable zero-output CPI. Expired makers still consumed the bounded
    // quote walk; cleaning their book entries is not needed to settle this intent.
    if expected.input == 0
        || expected.output == 0
        || input
            < if sell {
                header.base_lot
            } else {
                header.quote_lot
            }
    {
        return Ok(false);
    }
    let bytes = ad(dex::phoenix_ioc(&header, sell, input, matches, deadline))?;
    let indices = [7, 8, 9, 0, 2, 3, 10, 11, 4];
    let accounts = indices
        .iter()
        .enumerate()
        .map(|(i, j)| {
            if [2, 4, 5, 6, 7].contains(&i) {
                AccountMeta::new(*a[*j].key, false)
            } else {
                AccountMeta::new_readonly(*a[*j].key, i == 3)
            }
        })
        .collect();
    let (src, dst) = if sell { (2, 3) } else { (3, 2) };
    let before_in = amount(&a[src])?;
    let before_out = amount(&a[dst])?;
    invoke_checked_accounts(
        &Instruction {
            program_id: PHOENIX,
            accounts,
            data: bytes[..dex::PHOENIX_IOC_LEN].to_vec(),
        },
        a,
    )?;
    need(
        before_in.checked_sub(amount(&a[src])?) == Some(expected.input)
            && amount(&a[dst])?.checked_sub(before_out) == Some(expected.output),
        CONSERVATION,
    )?;
    Ok(true)
}

#[inline(never)]
fn raydium(a: &[AccountInfo], sell: bool, input: u64, now: u64) -> ProgramResult {
    executable(&a[12], &RAYDIUM)?;
    need(
        *a[13].key == RAYDIUM_AUTH && *a[14].owner == RAYDIUM && *a[15].owner == RAYDIUM,
        IDENTITY,
    )?;
    let pool = ad(RaydiumPool::decode(&a[15].try_borrow_data()?, now))?;
    need(
        pool.config == a[14].key.to_bytes()
            && pool.observation == a[18].key.to_bytes()
            && pool.mints == [a[5].key.to_bytes(), a[6].key.to_bytes()]
            && pool.vaults == [a[16].key.to_bytes(), a[17].key.to_bytes()]
            && pool.token_programs == [TOKEN.to_bytes(); 2],
        IDENTITY,
    )?;
    let balances = [
        token(&a[16], a[5].key, &RAYDIUM_AUTH)?,
        token(&a[17], a[6].key, &RAYDIUM_AUTH)?,
    ];
    let exact_out = ad(pool.quote(&a[14].try_borrow_data()?, balances, sell, input))?;
    let (src, dst, iv, ov, im, om) = if sell {
        (2, 3, 16, 17, 5, 6)
    } else {
        (3, 2, 17, 16, 6, 5)
    };
    let before = amount(&a[dst])?;
    let indices = [0, 13, 14, 15, src, dst, iv, ov, 4, 4, im, om, 18];
    let accounts = indices
        .iter()
        .enumerate()
        .map(|(i, j)| {
            if [0, 3, 4, 5, 6, 7, 12].contains(&i) {
                AccountMeta::new(*a[*j].key, i == 0)
            } else {
                AccountMeta::new_readonly(*a[*j].key, false)
            }
        })
        .collect();
    invoke_checked_accounts(
        &Instruction {
            program_id: RAYDIUM,
            accounts,
            data: dex::raydium_swap(input, exact_out).to_vec(),
        },
        a,
    )?;
    need(
        amount(&a[dst])?.checked_sub(before) == Some(exact_out),
        CONSERVATION,
    )
}
