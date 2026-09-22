//! One common funding graph before product-aware clearing. All graph accounts
//! are declared up front; neither funding nor child graphs advance the nonce
//! until the entire economic postcondition succeeds.
use super::*;
use dex::graph::{Asset, Graph, MAX_ASSETS};

#[derive(Clone, Copy)]
pub(super) struct Funding<'a> {
    pub bytes: &'a [u8],
    pub surplus_product: usize,
    /// Version two routes all post-funding cash through one multi-output
    /// economic graph instead of assigning surplus to a preselected product.
    pub global_reflow: bool,
}

#[derive(Clone, Copy)]
pub(super) struct State {
    pub source: Asset,
    pub before_input: u64,
    pub input_atoms: u64,
    pub legs: usize,
    pub tokens: [u8; MAX_ASSETS],
    pub token_count: usize,
}

fn references(g: &Graph, index: usize) -> bool {
    g.assets[..g.asset_count]
        .iter()
        .any(|asset| [asset.token, asset.mint, asset.program].contains(&(index as u8)))
        || g.legs[..g.leg_count]
            .iter()
            .flatten()
            .any(|leg| leg.program as usize == index || leg.accounts.contains(&(index as u8)))
}

impl Funding<'_> {
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    pub fn inspect(
        &self,
        a: &[AccountInfo],
        buyer: usize,
        sequence: u64,
        cash_source: usize,
        cash_mint: usize,
        cash_program: usize,
        deadline: u64,
        minimum_cash: u64,
    ) -> Result<State, ProgramError> {
        let g = ad(Graph::decode(self.bytes, a.len()))?;
        let sink = g.assets[g.asset_count - 1];
        need(
            self.bytes[0] == 2
                && g.leg_count <= 3
                && g.sequence == sequence
                && g.deadline <= deadline
                && g.min_out >= minimum_cash
                && a[sink.token as usize].key == a[cash_source].key
                && a[sink.mint as usize].key == a[cash_mint].key
                && a[sink.program as usize].key == a[cash_program].key,
            IDENTITY,
        )?;
        let source = g.assets[0];
        need(
            a[source.token as usize].key != a[cash_source].key
                && a[source.mint as usize].key != a[cash_mint].key,
            IDENTITY,
        )?;
        let before_input = graph::checked_asset(
            &a[buyer],
            &a[source.token as usize],
            &a[source.mint as usize],
            &a[source.program as usize],
            true,
        )?;
        need(before_input >= g.input, BOUNDS)?;
        let mut tokens = [u8::MAX; MAX_ASSETS];
        for (index, asset) in g.assets[..g.asset_count].iter().enumerate() {
            tokens[index] = asset.token;
        }
        Ok(State {
            source,
            before_input,
            input_atoms: g.input,
            legs: g.leg_count,
            tokens,
            token_count: g.asset_count,
        })
    }

    pub fn exclude(&self, a: &[AccountInfo], indices: &[usize]) -> ProgramResult {
        let g = ad(Graph::decode(self.bytes, a.len()))?;
        for index in indices {
            need(!references(&g, *index), IDENTITY)?;
        }
        Ok(())
    }

    pub fn separate(
        &self,
        a: &[AccountInfo],
        other: &Graph,
        buyer: usize,
        cash: usize,
    ) -> ProgramResult {
        let g = ad(Graph::decode(self.bytes, a.len()))?;
        for (index, account) in a.iter().enumerate() {
            if account.is_writable && index != buyer && index != cash {
                need(
                    !(references(&g, index) && references(other, index)),
                    IDENTITY,
                )?;
            }
        }
        Ok(())
    }
}

/// Opcode 18: `[18, version, selector, reserved, funding_len, cell_len]`, then a
/// fixed funding graph and an opcode-14 cell. Version one uses `selector` as a
/// fixed surplus product. Version two requires selector zero and lets the cell's
/// sentinel residual graph re-solve the entire observed cash balance across
/// issuer products after every bounded CPI.
#[inline(never)]
pub(super) fn execute(program: &Pubkey, a: &[AccountInfo], d: &[u8]) -> ProgramResult {
    need(
        (8..=1024).contains(&d.len())
            && d[0] == 18
            && matches!(d[1], 1 | 2)
            && (d[1] == 1 || d[2] == 0)
            && d[3] == 0
            && a.len() <= 64,
        BOUNDS,
    )?;
    for (index, account) in a.iter().enumerate() {
        for prior in &a[..index] {
            if account.key.as_ref()[..8] == prior.key.as_ref()[..8] {
                need(account.key != prior.key, IDENTITY)?;
            }
        }
    }
    let funding_len = usize::from(u16::from_le_bytes([d[4], d[5]]));
    let cell_len = usize::from(u16::from_le_bytes([d[6], d[7]]));
    let end = 8usize.checked_add(funding_len).ok_or(err(BOUNDS))?;
    need(end.checked_add(cell_len) == Some(d.len()), BOUNDS)?;
    let bytes = d.get(8..end).ok_or(err(BOUNDS))?;
    let cell = d.get(end..).ok_or(err(BOUNDS))?;
    meshcell::clear_with_funding(
        program,
        a,
        cell,
        Funding {
            bytes,
            surplus_product: d[2] as usize,
            global_reflow: d[1] == 2,
        },
    )
}
