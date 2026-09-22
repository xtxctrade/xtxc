//! FlowCell v2: fully signed multi-asset crossing followed by one bounded,
//! owner-scoped residual graph for every partially crossed intent.
//!
//! The offchain Flow Folding solver proposes transfers and residual graphs. The
//! program treats them only as a proposal: it validates all owners, nonces,
//! limits, account isolation and graphs before the first CPI, then verifies each
//! owner's exact balance deltas before one atomic commit.
use super::*;
use dex::graph::Graph;

const MAX_INTENTS: usize = 4;
const MAX_TRANSFERS: usize = 8;
const MAX_EXTERNAL_LEGS: usize = 4;
const INTENT_LEN: usize = 32;

#[derive(Clone, Copy, Default)]
struct CellIntent {
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

fn checked(
    a: &[AccountInfo],
    intent: &CellIntent,
    input: bool,
    inspect_mint: bool,
) -> Result<u64, ProgramError> {
    let (token, mint, token_program) = if input {
        (intent.source, intent.input_mint, intent.input_program)
    } else {
        (
            intent.destination,
            intent.output_mint,
            intent.output_program,
        )
    };
    graph::checked_asset(
        &a[intent.owner],
        &a[token],
        &a[mint],
        &a[token_program],
        inspect_mint,
    )
}

/// Opcode 12 layout:
/// `opcode, intent_count, transfer_count, residual_count, deadline`, followed
/// by 32-byte intents, 10-byte transfers and residual records
/// `(intent_index: u8, graph_len: u16, graph_bytes)`.
pub(super) fn clear(program: &Pubkey, a: &[AccountInfo], d: &[u8]) -> ProgramResult {
    need(d.len() >= 12 && d[0] == 12 && a.len() <= 64, BOUNDS)?;
    let intent_count = d[1] as usize;
    let transfer_count = d[2] as usize;
    let residual_count = d[3] as usize;
    need(
        (2..=MAX_INTENTS).contains(&intent_count)
            && (1..=MAX_TRANSFERS).contains(&transfer_count)
            && residual_count <= intent_count,
        BOUNDS,
    )?;
    let deadline = ad(dex::u64_at(d, 4))?;
    need(Clock::get()?.slot <= deadline, EXPIRED)?;
    let transfer_start = 12usize
        .checked_add(intent_count.checked_mul(INTENT_LEN).ok_or(err(BOUNDS))?)
        .ok_or(err(BOUNDS))?;
    let transfer_end = transfer_start
        .checked_add(transfer_count.checked_mul(10).ok_or(err(BOUNDS))?)
        .ok_or(err(BOUNDS))?;
    need(transfer_end <= d.len(), BOUNDS)?;

    let mut intents = [CellIntent::default(); MAX_INTENTS];
    for (position, intent) in intents[..intent_count].iter_mut().enumerate() {
        let start = 12 + position * INTENT_LEN;
        let row = d.get(start..start + INTENT_LEN).ok_or(err(BOUNDS))?;
        need(
            row[..8].iter().all(|index| (*index as usize) < a.len()),
            BOUNDS,
        )?;
        *intent = CellIntent {
            owner: row[0] as usize,
            nonce: row[1] as usize,
            source: row[2] as usize,
            destination: row[3] as usize,
            input_mint: row[4] as usize,
            output_mint: row[5] as usize,
            input_program: row[6] as usize,
            output_program: row[7] as usize,
            sequence: ad(dex::u64_at(row, 8))?,
            input: ad(dex::u64_at(row, 16))?,
            min_out: ad(dex::u64_at(row, 24))?,
            ..CellIntent::default()
        };
        need(
            a[intent.owner].is_signer
                && a[intent.nonce].is_writable
                && a[intent.nonce].owner == program
                && a[intent.input_mint].key != a[intent.output_mint].key
                && intent.input > 0
                && intent.input <= dex::MAX_INPUT
                && intent.min_out > 0
                && intent.sequence < u64::MAX,
            IDENTITY,
        )?;
        let nonce = a[intent.nonce].try_borrow_data()?;
        need(
            nonce.len() == NONCE_LEN
                && &nonce[..8] == NONCE_TAG
                && nonce[8..40] == a[intent.owner].key.to_bytes(),
            IDENTITY,
        )?;
        need(ad(dex::u64_at(&nonce, 40))? == intent.sequence, REPLAY)?;
        intent.before_in = checked(a, intent, true, true)?;
        intent.before_out = checked(a, intent, false, true)?;
        need(intent.before_in >= intent.input, BOUNDS)?;
    }

    // Every signer, nonce and financial token account is isolated. Shared mint
    // and token-program accounts remain read-only and may be deduplicated.
    for i in 0..intent_count {
        let current = [
            intents[i].owner,
            intents[i].nonce,
            intents[i].source,
            intents[i].destination,
        ];
        for left in 0..current.len() {
            for right in 0..left {
                need(a[current[left]].key != a[current[right]].key, IDENTITY)?;
            }
        }
        for prior_intent in intents.iter().take(i) {
            let prior = [
                prior_intent.owner,
                prior_intent.nonce,
                prior_intent.source,
                prior_intent.destination,
            ];
            for left in current {
                for right in prior {
                    need(a[left].key != a[right].key, IDENTITY)?;
                }
            }
        }
    }

    let mut crossed_input = [0u64; MAX_INTENTS];
    let mut crossed_output = [0u64; MAX_INTENTS];
    for transfer in d[transfer_start..transfer_end].chunks_exact(10) {
        let from = transfer[0] as usize;
        let to = transfer[1] as usize;
        let amount = ad(dex::u64_at(transfer, 2))?;
        need(
            from < intent_count && to < intent_count && from != to && amount > 0,
            BOUNDS,
        )?;
        let source = intents[from];
        let destination = intents[to];
        need(
            a[source.input_mint].key == a[destination.output_mint].key
                && a[source.input_program].key == a[destination.output_program].key,
            IDENTITY,
        )?;
        crossed_input[from] = crossed_input[from].checked_add(amount).ok_or(err(BOUNDS))?;
        crossed_output[to] = crossed_output[to].checked_add(amount).ok_or(err(BOUNDS))?;
        need(crossed_input[from] <= source.input, BOUNDS)?;
    }

    let mut residuals: [Option<&[u8]>; MAX_INTENTS] = [None; MAX_INTENTS];
    let mut cursor = transfer_end;
    let mut maximum_external_legs = 0usize;
    for _ in 0..residual_count {
        let header = d.get(cursor..cursor + 3).ok_or(err(BOUNDS))?;
        let owner_slot = header[0] as usize;
        let graph_len = u16::from_le_bytes([header[1], header[2]]) as usize;
        cursor = cursor.checked_add(3).ok_or(err(BOUNDS))?;
        let bytes = d.get(cursor..cursor + graph_len).ok_or(err(BOUNDS))?;
        cursor = cursor.checked_add(graph_len).ok_or(err(BOUNDS))?;
        need(
            owner_slot < intent_count && residuals[owner_slot].is_none(),
            BOUNDS,
        )?;
        let intent = intents[owner_slot];
        let residual_input = intent
            .input
            .checked_sub(crossed_input[owner_slot])
            .ok_or(err(CONSERVATION))?;
        need(residual_input > 0, CONSERVATION)?;
        let graph = ad(Graph::decode(bytes, a.len()))?;
        let final_asset = graph.asset_count - 1;
        need(
            graph.sequence == intent.sequence
                && graph.input == residual_input
                && graph.deadline <= deadline
                && a[graph.assets[0].token as usize].key == a[intent.source].key
                && a[graph.assets[0].mint as usize].key == a[intent.input_mint].key
                && a[graph.assets[0].program as usize].key == a[intent.input_program].key
                && a[graph.assets[final_asset].token as usize].key == a[intent.destination].key
                && a[graph.assets[final_asset].mint as usize].key == a[intent.output_mint].key
                && a[graph.assets[final_asset].program as usize].key
                    == a[intent.output_program].key
                && graph.min_out
                    >= intent
                        .min_out
                        .saturating_sub(crossed_output[owner_slot])
                        .max(1),
            IDENTITY,
        )?;
        maximum_external_legs = maximum_external_legs
            .checked_add(if graph.reflow_calls > 0 {
                MAX_EXTERNAL_LEGS
            } else {
                graph.leg_count
            })
            .ok_or(err(BOUNDS))?;
        need(maximum_external_legs <= MAX_EXTERNAL_LEGS, BOUNDS)?;

        // A residual graph may use only this owner's financial state. It may
        // share read-only mints, programs and oracle accounts with another graph.
        for other in intents[..intent_count]
            .iter()
            .filter(|other| other.owner != intent.owner)
        {
            for asset in &graph.assets[..graph.asset_count] {
                need(
                    [asset.token, asset.mint, asset.program]
                        .iter()
                        .all(|index| {
                            let key = a[*index as usize].key;
                            key != a[other.owner].key
                                && key != a[other.nonce].key
                                && key != a[other.source].key
                                && key != a[other.destination].key
                        }),
                    IDENTITY,
                )?;
            }
            for leg in graph.legs[..graph.leg_count].iter().flatten() {
                need(
                    core::iter::once(&leg.program)
                        .chain(leg.accounts.iter())
                        .all(|index| {
                            let key = a[*index as usize].key;
                            key != a[other.owner].key
                                && key != a[other.nonce].key
                                && key != a[other.source].key
                                && key != a[other.destination].key
                        }),
                    IDENTITY,
                )?;
            }
        }
        graph::preflight_graph_for(program, a, bytes, intent.owner, intent.nonce)?;
        residuals[owner_slot] = Some(bytes);
    }
    need(cursor == d.len(), BOUNDS)?;

    for i in 0..intent_count {
        need(
            (crossed_input[i] < intents[i].input) == residuals[i].is_some(),
            CONSERVATION,
        )?;
    }
    // Residual graphs cannot share any writable account. This keeps their state
    // transitions independent and prevents an earlier graph from invalidating a
    // later graph's signed quote inside the same cell.
    for i in 0..intent_count {
        let Some(left_bytes) = residuals[i] else {
            continue;
        };
        let left = ad(Graph::decode(left_bytes, a.len()))?;
        for right_bytes in residuals[..i].iter().flatten() {
            let right = ad(Graph::decode(right_bytes, a.len()))?;
            for left_leg in left.legs[..left.leg_count].iter().flatten() {
                for left_index in left_leg.accounts {
                    if !a[*left_index as usize].is_writable {
                        continue;
                    }
                    for right_leg in right.legs[..right.leg_count].iter().flatten() {
                        for right_index in right_leg.accounts {
                            if a[*right_index as usize].is_writable {
                                need(
                                    a[*left_index as usize].key != a[*right_index as usize].key,
                                    IDENTITY,
                                )?;
                            }
                        }
                    }
                }
            }
        }
    }

    // All graphs and transfers are now validated. From this point any failure
    // aborts the Solana instruction and rolls back every earlier CPI.
    let mut transfer_ix = Instruction {
        program_id: TOKEN,
        accounts: Vec::with_capacity(4),
        data: vec![12, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    };
    for transfer in d[transfer_start..transfer_end].chunks_exact(10) {
        let source = intents[transfer[0] as usize];
        let destination = intents[transfer[1] as usize];
        let amount = ad(dex::u64_at(transfer, 2))?;
        transfer_ix.program_id = *a[source.input_program].key;
        transfer_ix.accounts.clear();
        transfer_ix.accounts.extend_from_slice(&[
            AccountMeta::new(*a[source.source].key, false),
            AccountMeta::new_readonly(*a[source.input_mint].key, false),
            AccountMeta::new(*a[destination.destination].key, false),
            AccountMeta::new_readonly(*a[source.owner].key, true),
        ]);
        transfer_ix.data[1..9].copy_from_slice(&amount.to_le_bytes());
        transfer_ix.data[9] = a[source.input_mint].try_borrow_data()?[44];
        invoke_checked_accounts(&transfer_ix, a)?;
    }
    for (index, residual) in residuals[..intent_count].iter().enumerate() {
        if let Some(bytes) = residual {
            graph::settle_graph_for(
                program,
                a,
                bytes,
                intents[index].owner,
                intents[index].nonce,
            )?;
        }
    }

    let mut receipt = [0u8; 16 + MAX_INTENTS * 24];
    receipt[..8].copy_from_slice(b"SKEWCEL2");
    receipt[8..16].copy_from_slice(&(intent_count as u64).to_le_bytes());
    for (index, intent) in intents[..intent_count].iter().enumerate() {
        let actual_input = intent
            .before_in
            .checked_sub(checked(a, intent, true, false)?)
            .ok_or(err(CONSERVATION))?;
        let actual_output = checked(a, intent, false, false)?
            .checked_sub(intent.before_out)
            .ok_or(err(CONSERVATION))?;
        need(actual_input == intent.input, CONSERVATION)?;
        need(actual_output >= intent.min_out, MIN_OUT)?;
        let mut nonce = a[intent.nonce].try_borrow_mut_data()?;
        let next_sequence = intent.sequence + 1;
        if residuals[index].is_some() {
            need(ad(dex::u64_at(&nonce, 40))? == next_sequence, REPLAY)?;
        } else {
            nonce[40..48].copy_from_slice(&next_sequence.to_le_bytes());
        }
        nonce[48..56].copy_from_slice(&actual_input.to_le_bytes());
        nonce[56..64].copy_from_slice(&actual_output.to_le_bytes());
        for (chunk, value) in receipt[16 + index * 24..16 + (index + 1) * 24]
            .chunks_exact_mut(8)
            .zip([intent.sequence, actual_input, actual_output])
        {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
    }
    set_return_data(&receipt[..16 + intent_count * 24]);
    Ok(())
}
