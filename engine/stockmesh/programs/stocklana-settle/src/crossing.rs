//! Fully funded, fully signed internal crossing. No oracle, custody or auction
//! trust: each signer authorizes exact input and minimum output. Partial netting
//! Opcode 5 combines partial crossing and the first owner's residual DEX graph.
use super::*;
const MAX_INTENTS: usize = 4;
const MAX_TRANSFERS: usize = 8;
const INTENT_LEN: usize = 32;

#[derive(Clone, Copy, Default)]
struct Intent {
    owner: usize,
    nonce: usize,
    source: usize,
    destination: usize,
    input_mint: usize,
    output_mint: usize,
    input_program: usize,
    output_program: usize,
    sequence: u64,
    input: u64,
    min_out: u64,
    before_in: u64,
    before_out: u64,
}
fn checked(a: &[AccountInfo], i: &Intent, input: bool, mint: bool) -> Result<u64, ProgramError> {
    let (t, m, p) = if input {
        (i.source, i.input_mint, i.input_program)
    } else {
        (i.destination, i.output_mint, i.output_program)
    };
    graph::checked_asset(&a[i.owner], &a[t], &a[m], &a[p], mint)
}
pub(super) fn clear(program: &Pubkey, a: &[AccountInfo], d: &[u8]) -> ProgramResult {
    need(
        d.len() >= 12 && [3, 5].contains(&d[0]) && d[3] == 0 && a.len() <= 64,
        BOUNDS,
    )?;
    let n = d[1] as usize;
    let transfers = d[2] as usize;
    let external = d[0] == 5;
    let transfer_end = 12 + n * INTENT_LEN + transfers * 10;
    need(
        (2..=MAX_INTENTS).contains(&n)
            && (2..=MAX_TRANSFERS).contains(&transfers)
            && if external {
                d.len() > transfer_end + 2
            } else {
                d.len() == transfer_end
            },
        BOUNDS,
    )?;
    need(Clock::get()?.slot <= ad(dex::u64_at(d, 4))?, EXPIRED)?;
    let mut intents = [Intent::default(); MAX_INTENTS];
    for (j, i) in intents[..n].iter_mut().enumerate() {
        let p = 12 + j * INTENT_LEN;
        let s = &d[p..p + INTENT_LEN];
        need(s[..8].iter().all(|x| (*x as usize) < a.len()), BOUNDS)?;
        *i = Intent {
            owner: s[0] as usize,
            nonce: s[1] as usize,
            source: s[2] as usize,
            destination: s[3] as usize,
            input_mint: s[4] as usize,
            output_mint: s[5] as usize,
            input_program: s[6] as usize,
            output_program: s[7] as usize,
            sequence: ad(dex::u64_at(s, 8))?,
            input: ad(dex::u64_at(s, 16))?,
            min_out: ad(dex::u64_at(s, 24))?,
            ..Intent::default()
        };
        need(
            a[i.owner].is_signer
                && a[i.nonce].is_writable
                && a[i.nonce].owner == program
                && a[i.input_mint].key != a[i.output_mint].key
                && i.input > 0
                && i.input <= dex::MAX_INPUT
                && i.min_out > 0
                && i.sequence < u64::MAX,
            IDENTITY,
        )?;
        let nonce = a[i.nonce].try_borrow_data()?;
        need(
            nonce.len() == NONCE_LEN
                && &nonce[..8] == NONCE_TAG
                && nonce[8..40] == a[i.owner].key.to_bytes(),
            IDENTITY,
        )?;
        need(ad(dex::u64_at(&nonce, 40))? == i.sequence, REPLAY)?;
        i.before_in = checked(a, i, true, true)?;
        i.before_out = checked(a, i, false, true)?;
        need(i.before_in >= i.input, BOUNDS)?;
    }
    // No balance or nonce may represent two participants. Each participant's
    // debit/credit is consequently independent of aliases in the account list.
    for i in 0..n {
        for j in 0..i {
            need(
                a[intents[i].owner].key != a[intents[j].owner].key
                    && a[intents[i].nonce].key != a[intents[j].nonce].key,
                IDENTITY,
            )?;
        }
        for (j, other) in intents[..n].iter().enumerate() {
            if i != j {
                need(
                    a[intents[i].source].key != a[other.source].key
                        && a[intents[i].destination].key != a[other.destination].key,
                    IDENTITY,
                )?;
            }
            need(
                a[intents[i].source].key != a[other.destination].key,
                IDENTITY,
            )?;
        }
    }
    let start = 12 + n * INTENT_LEN;
    let mut debits = [0u64; MAX_INTENTS];
    for t in d[start..transfer_end].chunks_exact(10) {
        let from = t[0] as usize;
        let to = t[1] as usize;
        let amount = ad(dex::u64_at(t, 2))?;
        need(from < n && to < n && from != to && amount > 0, BOUNDS)?;
        let src = intents[from];
        let dst = intents[to];
        need(
            a[src.input_mint].key == a[dst.output_mint].key
                && a[src.input_program].key == a[dst.output_program].key,
            IDENTITY,
        )?;
        debits[from] = debits[from].checked_add(amount).ok_or(err(BOUNDS))?;
        need(debits[from] <= src.input, BOUNDS)?;
    }
    for i in 0..n {
        need(
            if external && i == 0 {
                debits[i] < intents[i].input
            } else {
                debits[i] == intents[i].input
            },
            CONSERVATION,
        )?;
    }
    let residual = if external {
        let len = u16::from_le_bytes([d[transfer_end], d[transfer_end + 1]]) as usize;
        need(d.len() == transfer_end + 2 + len, BOUNDS)?;
        let data = &d[transfer_end + 2..];
        let g = ad(dex::graph::Graph::decode(data, a.len()))?;
        let first = intents[0];
        need(
            first.owner == 0
                && first.nonce == 1
                && g.asset_count == 2
                && g.sequence == first.sequence
                && g.input == first.input - debits[0]
                && g.deadline <= ad(dex::u64_at(d, 4))?
                && a[g.assets[0].token as usize].key == a[first.source].key
                && a[g.assets[1].token as usize].key == a[first.destination].key
                && a[g.assets[0].mint as usize].key == a[first.input_mint].key
                && a[g.assets[1].mint as usize].key == a[first.output_mint].key,
            IDENTITY,
        )?;
        // No external leg can access another signer's financial state. Their
        // signatures authorize crossing only; the first owner authorizes DEX CPI.
        for leg in g.legs[..g.leg_count].iter().flatten() {
            for idx in leg.accounts {
                for other in &intents[1..n] {
                    need(
                        [other.owner, other.nonce, other.source, other.destination]
                            .iter()
                            .all(|i| a[*i].key != a[*idx as usize].key),
                        IDENTITY,
                    )?;
                }
            }
        }
        Some(data)
    } else {
        None
    };
    let mut ix = Instruction {
        program_id: TOKEN,
        accounts: Vec::with_capacity(4),
        data: vec![12, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    };
    for t in d[start..transfer_end].chunks_exact(10) {
        let src = intents[t[0] as usize];
        let dst = intents[t[1] as usize];
        let amount = ad(dex::u64_at(t, 2))?;
        ix.program_id = *a[src.input_program].key;
        ix.accounts.clear();
        ix.accounts.extend_from_slice(&[
            AccountMeta::new(*a[src.source].key, false),
            AccountMeta::new_readonly(*a[src.input_mint].key, false),
            AccountMeta::new(*a[dst.destination].key, false),
            AccountMeta::new_readonly(*a[src.owner].key, true),
        ]);
        ix.data[1..9].copy_from_slice(&amount.to_le_bytes());
        ix.data[9] = a[src.input_mint].try_borrow_data()?[44];
        invoke_checked_accounts(&ix, a)?;
    }
    if let Some(data) = residual {
        // Direct Rust invocation retains this instruction's signatures and
        // privilege checks; all transfers and venue CPIs share one rollback.
        graph::settle_graph(program, a, data)?;
    }
    let mut receipt = [0u8; 16 + MAX_INTENTS * 24];
    receipt[..8].copy_from_slice(if external { b"SKEWNET1" } else { b"SKEWCOW1" });
    receipt[8..16].copy_from_slice(&(n as u64).to_le_bytes());
    for (j, i) in intents[..n].iter().enumerate() {
        let input = i
            .before_in
            .checked_sub(checked(a, i, true, false)?)
            .ok_or(err(CONSERVATION))?;
        let output = checked(a, i, false, false)?
            .checked_sub(i.before_out)
            .ok_or(err(CONSERVATION))?;
        need(input == i.input, CONSERVATION)?;
        need(output >= i.min_out, MIN_OUT)?;
        let mut nonce = a[i.nonce].try_borrow_mut_data()?;
        nonce[40..48].copy_from_slice(&(i.sequence + 1).to_le_bytes());
        nonce[48..56].copy_from_slice(&input.to_le_bytes());
        nonce[56..64].copy_from_slice(&output.to_le_bytes());
        for (chunk, val) in receipt[16 + j * 24..16 + (j + 1) * 24]
            .chunks_exact_mut(8)
            .zip([i.sequence, input, output])
        {
            chunk.copy_from_slice(&val.to_le_bytes());
        }
    }
    set_return_data(&receipt[..16 + n * 24]);
    Ok(())
}
