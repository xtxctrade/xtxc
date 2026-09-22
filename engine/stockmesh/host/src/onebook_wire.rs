//! Exact host compiler for the continuous OneBook on-chain ABI.
//!
//! These builders create unsigned instructions only. Deployment configuration
//! supplies the settlement program and authorities; this module owns no key and
//! never sends a transaction.
use crate::Result;
use sha2::{Digest, Sha256};
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::{v0, AddressLookupTableAccount, VersionedMessage};
use solana_pubkey::Pubkey;
use stocklana_adapters::{self as dex, graph::Graph};

pub const FIXED_RATIONAL: u8 = 0;
pub const TOKEN_2022_SCALED_UI: u8 = 1;
pub const STOCK_POLICY_V2_TAG: &[u8; 8] = b"SKEWSTK2";
pub const STOCK_POLICY_V2_LEN: usize = 225;

#[derive(Clone, Copy)]
pub struct StockPolicyV2Spec {
    pub authority: Pubkey,
    pub instrument: [u8; 32],
    pub issuer: [u8; 32],
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub rights_hash: [u8; 32],
    pub version: u64,
    pub expires_slot: u64,
    pub flags: u8,
}

/// The address itself commits the rights hash. Updating operational state uses
/// a monotonically increasing version; changing legal/economic rights creates
/// a different policy account.
pub fn stock_policy_v2_address(
    settlement_program: &Pubkey,
    authority: &Pubkey,
    instrument: &[u8; 32],
    input_mint: &Pubkey,
    output_mint: &Pubkey,
    rights_hash: &[u8; 32],
) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"stock2",
            authority.as_ref(),
            instrument,
            input_mint.as_ref(),
            output_mint.as_ref(),
            rights_hash,
        ],
        settlement_program,
    )
    .0
}

/// Compile ProductPolicy v2 publication. This function owns no authority key
/// and returns only the exact instruction the authority must independently sign.
pub fn compile_stock_policy_v2(
    settlement_program: Pubkey,
    spec: StockPolicyV2Spec,
) -> Result<Instruction> {
    if spec.authority == Pubkey::default()
        || spec.instrument == [0; 32]
        || spec.issuer == [0; 32]
        || spec.input_mint == spec.output_mint
        || spec.rights_hash == [0; 32]
        || spec.version == 0
        || spec.expires_slot == 0
        || spec.flags & !31 != 0
    {
        return Err("ProductPolicy v2 bounds".into());
    }
    let policy = stock_policy_v2_address(
        &settlement_program,
        &spec.authority,
        &spec.instrument,
        &spec.input_mint,
        &spec.output_mint,
        &spec.rights_hash,
    );
    let mut data = vec![19];
    data.extend_from_slice(&spec.instrument);
    data.extend_from_slice(&spec.issuer);
    data.extend_from_slice(spec.input_mint.as_ref());
    data.extend_from_slice(spec.output_mint.as_ref());
    data.extend_from_slice(&spec.rights_hash);
    data.extend_from_slice(&spec.version.to_le_bytes());
    data.extend_from_slice(&spec.expires_slot.to_le_bytes());
    data.push(spec.flags);
    debug_assert_eq!(data.len(), 178);
    Ok(Instruction {
        program_id: settlement_program,
        accounts: vec![
            AccountMeta::new(spec.authority, true),
            AccountMeta::new(policy, false),
            AccountMeta::new_readonly(Pubkey::default(), false),
        ],
        data,
    })
}

/// Wrap a typed product->cash graph with the inverse check against the same
/// ProductPolicy v2 account used for cash->product execution. Account indices
/// inside the graph stay stable because the policy is appended read-only.
pub fn compile_reverse_stock_fill(
    settlement_program: Pubkey,
    policy: Pubkey,
    policy_version: u64,
    maximum_policy_age: u64,
    allow_underlying_closed: bool,
    graph: Instruction,
) -> Result<Instruction> {
    if settlement_program == Pubkey::default()
        || policy == Pubkey::default()
        || graph.program_id != settlement_program
        || policy_version == 0
        || !(1..=150).contains(&maximum_policy_age)
        || graph
            .data
            .first()
            .is_none_or(|opcode| ![2, 4, 9].contains(opcode))
        || graph.accounts.len() >= 255
        || graph.accounts.iter().any(|meta| meta.pubkey == policy)
    {
        return Err("reverse stock fill bounds".into());
    }
    let policy_index = u8::try_from(graph.accounts.len()).map_err(|_| "reverse policy index")?;
    let mut accounts = graph.accounts;
    accounts.push(AccountMeta::new_readonly(policy, false));
    let mut data = vec![20, policy_index, u8::from(allow_underlying_closed), 0];
    data.extend_from_slice(&policy_version.to_le_bytes());
    data.extend_from_slice(&maximum_policy_age.to_le_bytes());
    data.extend_from_slice(&graph.data);
    Ok(Instruction {
        program_id: settlement_program,
        accounts,
        data,
    })
}

fn bounded_conversion(model: u8, conservative_bps: u16, numerator: u64, denominator: u64) -> bool {
    matches!(model, FIXED_RATIONAL | TOKEN_2022_SCALED_UI)
        && (1..=10_000).contains(&conservative_bps)
        && numerator > 0
        && denominator > 0
}

fn domain_id(domain: &[u8], value: &str) -> Result<[u8; 32]> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| {
            byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b':' || byte == b'-' || byte == b'.'
        })
    {
        return Err("OneBook economic identity".into());
    }
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update(value.as_bytes());
    Ok(hash.finalize().into())
}

pub fn instrument_id(symbol: &str) -> Result<[u8; 32]> {
    domain_id(b"SKEW_INSTRUMENT_V1\0", symbol)
}

#[cfg(test)]
mod stock_share_class_identity_tests {
    use super::*;
    #[test]
    fn dotted_stock_class_is_preserved_without_changing_legacy_hashes() {
        let dotted = instrument_id("BRK.B").unwrap();
        assert_ne!(dotted, instrument_id("BRKB").unwrap());
        assert_ne!(dotted, instrument_id("BRK-B").unwrap());
        assert_ne!(dotted, instrument_id("BRK.A").unwrap());
        let mut prior = Sha256::new();
        prior.update(b"SKEW_INSTRUMENT_V1\0");
        prior.update(b"NVDA");
        assert_eq!(instrument_id("NVDA").unwrap(), <[u8; 32]>::from(prior.finalize()));
        assert!(instrument_id("brk.b").is_err());
        assert!(instrument_id("BRK/B").is_err());
    }
}

pub fn issuer_id(issuer: &str) -> Result<[u8; 32]> {
    if issuer.is_empty()
        || issuer.len() > 64
        || issuer.trim() != issuer
        || !issuer
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
        return Err("OneBook issuer identity".into());
    }
    let mut hash = Sha256::new();
    hash.update(b"SKEW_ISSUER_V1\0");
    hash.update(issuer.as_bytes());
    Ok(hash.finalize().into())
}

#[derive(Clone, Copy)]
pub struct ProductClaimSpec {
    pub authority: Pubkey,
    pub policy: Pubkey,
    pub cash_mint: Pubkey,
    pub product_mint: Pubkey,
    pub product_token_program: Pubkey,
    pub instrument: [u8; 32],
    pub issuer: [u8; 32],
    pub model: u8,
    pub conservative_bps: u16,
    pub policy_version: u64,
    pub maximum_policy_age: u64,
    pub claim_version: u64,
    pub numerator: u64,
    pub denominator: u64,
    pub expires_slot: u64,
}

pub fn product_claim_address(
    settlement_program: &Pubkey,
    authority: &Pubkey,
    instrument: &[u8; 32],
    product_mint: &Pubkey,
) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"claim",
            authority.as_ref(),
            instrument,
            product_mint.as_ref(),
        ],
        settlement_program,
    )
    .0
}

pub fn compile_product_claim(
    settlement_program: Pubkey,
    spec: ProductClaimSpec,
) -> Result<Instruction> {
    if spec.authority == Pubkey::default()
        || spec.policy == Pubkey::default()
        || spec.cash_mint == spec.product_mint
        || spec.product_token_program == Pubkey::default()
        || spec.instrument == [0; 32]
        || spec.issuer == [0; 32]
        || spec.policy_version == 0
        || spec.claim_version == 0
        || !(1..=150).contains(&spec.maximum_policy_age)
        || spec.expires_slot == 0
        || !bounded_conversion(
            spec.model,
            spec.conservative_bps,
            spec.numerator,
            spec.denominator,
        )
    {
        return Err("ProductClaim bounds".into());
    }
    let claim = product_claim_address(
        &settlement_program,
        &spec.authority,
        &spec.instrument,
        &spec.product_mint,
    );
    let mut data = vec![17, spec.model, 1, 0];
    data.extend_from_slice(&spec.conservative_bps.to_le_bytes());
    data.extend_from_slice(&[0, 0]);
    data.extend_from_slice(&spec.instrument);
    data.extend_from_slice(&spec.issuer);
    for value in [
        spec.policy_version,
        spec.maximum_policy_age,
        spec.claim_version,
        spec.numerator,
        spec.denominator,
        spec.expires_slot,
    ] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    debug_assert_eq!(data.len(), 120);
    Ok(Instruction {
        program_id: settlement_program,
        accounts: vec![
            AccountMeta::new(spec.authority, true),
            AccountMeta::new(claim, false),
            AccountMeta::new_readonly(spec.policy, false),
            AccountMeta::new_readonly(spec.cash_mint, false),
            AccountMeta::new_readonly(spec.product_mint, false),
            AccountMeta::new_readonly(spec.product_token_program, false),
            AccountMeta::new_readonly(Pubkey::default(), false),
        ],
        data,
    })
}

#[derive(Clone, Copy)]
pub struct AskSpec {
    pub owner: Pubkey,
    pub source: Pubkey,
    pub escrow: Pubkey,
    pub cash_destination: Pubkey,
    pub cash_mint: Pubkey,
    pub product_mint: Pubkey,
    pub product_token_program: Pubkey,
    pub cash_token_program: Pubkey,
    pub policy: Pubkey,
    pub claim_authority: Pubkey,
    pub order_nonce: u64,
    pub stock_atoms: u64,
    pub minimum_cash_atoms_per_share: u64,
    pub expires_slot: u64,
    pub policy_version: u64,
    pub maximum_policy_age: u64,
    pub instrument: [u8; 32],
    pub model: u8,
    pub conservative_bps: u16,
    pub numerator: u64,
    pub denominator: u64,
    pub allow_underlying_closed: bool,
}

pub fn exposure_order_address(settlement_program: &Pubkey, owner: &Pubkey, nonce: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[b"onebook", owner.as_ref(), &nonce.to_le_bytes()],
        settlement_program,
    )
    .0
}

pub fn compile_ask(settlement_program: Pubkey, spec: AskSpec) -> Result<Instruction> {
    if spec.owner == Pubkey::default()
        || spec.cash_mint == spec.product_mint
        || [spec.source, spec.escrow, spec.cash_destination]
            .iter()
            .any(|key| *key == Pubkey::default())
        || spec.source == spec.escrow
        || spec.stock_atoms == 0
        || spec.minimum_cash_atoms_per_share == 0
        || spec.expires_slot == 0
        || spec.policy_version == 0
        || !(1..=150).contains(&spec.maximum_policy_age)
        || spec.instrument == [0; 32]
        || !bounded_conversion(
            spec.model,
            spec.conservative_bps,
            spec.numerator,
            spec.denominator,
        )
    {
        return Err("ClaimCell ask bounds".into());
    }
    let order = exposure_order_address(&settlement_program, &spec.owner, spec.order_nonce);
    let claim = product_claim_address(
        &settlement_program,
        &spec.claim_authority,
        &spec.instrument,
        &spec.product_mint,
    );
    let mut data = vec![15, u8::from(spec.allow_underlying_closed), spec.model, 0];
    data.extend_from_slice(&spec.conservative_bps.to_le_bytes());
    data.extend_from_slice(&[0, 0]);
    for value in [
        spec.order_nonce,
        spec.stock_atoms,
        spec.minimum_cash_atoms_per_share,
        spec.expires_slot,
        spec.policy_version,
        spec.maximum_policy_age,
        spec.numerator,
        spec.denominator,
    ] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    data.extend_from_slice(&spec.instrument);
    debug_assert_eq!(data.len(), 104);
    Ok(Instruction {
        program_id: settlement_program,
        accounts: vec![
            AccountMeta::new(spec.owner, true),
            AccountMeta::new(order, false),
            AccountMeta::new(spec.source, false),
            AccountMeta::new(spec.escrow, false),
            AccountMeta::new(spec.cash_destination, false),
            AccountMeta::new_readonly(spec.cash_mint, false),
            AccountMeta::new_readonly(spec.product_mint, false),
            AccountMeta::new_readonly(spec.product_token_program, false),
            AccountMeta::new_readonly(spec.cash_token_program, false),
            AccountMeta::new_readonly(spec.policy, false),
            AccountMeta::new_readonly(claim, false),
            AccountMeta::new_readonly(Pubkey::default(), false),
        ],
        data,
    })
}

pub fn compile_cancel(
    settlement_program: Pubkey,
    owner: Pubkey,
    order_nonce: u64,
    escrow: Pubkey,
    destination: Pubkey,
    product_mint: Pubkey,
    product_token_program: Pubkey,
) -> Result<Instruction> {
    if owner == Pubkey::default()
        || [escrow, destination, product_mint, product_token_program].contains(&Pubkey::default())
        || escrow == destination
    {
        return Err("ClaimCell cancel bounds".into());
    }
    let order = exposure_order_address(&settlement_program, &owner, order_nonce);
    let mut data = vec![16];
    data.extend_from_slice(&[0; 7]);
    data.extend_from_slice(&order_nonce.to_le_bytes());
    Ok(Instruction {
        program_id: settlement_program,
        accounts: vec![
            AccountMeta::new(owner, true),
            AccountMeta::new(order, false),
            AccountMeta::new(escrow, false),
            AccountMeta::new(destination, false),
            AccountMeta::new_readonly(product_mint, false),
            AccountMeta::new_readonly(product_token_program, false),
        ],
        data,
    })
}

#[derive(Clone, Copy)]
pub struct MeshProduct {
    pub policy: Pubkey,
    pub claim: Option<Pubkey>,
    pub destination: Pubkey,
    pub mint: Pubkey,
    pub token_program: Pubkey,
    pub model: u8,
    pub conservative_bps: u16,
    pub policy_version: u64,
    pub numerator: u64,
    pub denominator: u64,
}

#[derive(Clone, Copy)]
pub enum MeshSellerAuthority {
    Signed { nonce: Pubkey },
    ClaimCell { order: Pubkey },
}

#[derive(Clone, Copy)]
pub struct MeshSeller {
    pub product_index: usize,
    pub owner: Pubkey,
    pub authority: MeshSellerAuthority,
    pub stock_source: Pubkey,
    pub cash_destination: Pubkey,
    pub sequence_or_revision: u64,
    pub stock_atoms: u64,
    pub cash_atoms: u64,
    pub minimum_cash_atoms: u64,
}

#[derive(Clone)]
pub struct MeshResidual {
    pub product_index: usize,
    pub graph: Instruction,
}

#[derive(Clone)]
pub struct MeshFillSpec {
    pub buyer: Pubkey,
    pub buyer_nonce: Pubkey,
    pub buyer_cash_source: Pubkey,
    pub cash_mint: Pubkey,
    pub cash_token_program: Pubkey,
    pub buyer_sequence: u64,
    pub buyer_input_atoms: u64,
    pub minimum_exposure_q32: u64,
    pub deadline_slot: u64,
    pub maximum_policy_age: u64,
    pub allow_underlying_closed: bool,
    pub products: Vec<MeshProduct>,
    pub sellers: Vec<MeshSeller>,
    pub residuals: Vec<MeshResidual>,
}

/// One issuer's executable allocation, with the same conversion descriptor
/// used by internal clearing. Its graph contains typed venue bindings only.
pub struct ExposureAllocation {
    pub product: MeshProduct,
    pub graph: Instruction,
}

pub struct ExposureFillSpec {
    pub buyer: Pubkey,
    pub buyer_nonce: Pubkey,
    pub buyer_source: Pubkey,
    pub input_mint: Pubkey,
    pub input_token_program: Pubkey,
    pub buyer_sequence: u64,
    pub input_atoms: u64,
    pub minimum_exposure_q32: u64,
    pub deadline_slot: u64,
    pub maximum_policy_age: u64,
    pub allow_underlying_closed: bool,
    pub allocations: Vec<ExposureAllocation>,
}

/// Compile residual-only multi-issuer execution using the production opcode-13
/// ABI. No harness-specific byte builder or single-output-mint substitution.
pub fn compile_exposure_fill(program: Pubkey, spec: ExposureFillSpec) -> Result<Instruction> {
    if spec.buyer == Pubkey::default()
        || spec.buyer_sequence == u64::MAX
        || spec.input_atoms == 0
        || spec.input_atoms > dex::MAX_INPUT
        || spec.minimum_exposure_q32 == 0
        || spec.deadline_slot == 0
        || !(1..=150).contains(&spec.maximum_policy_age)
        || !(1..=4).contains(&spec.allocations.len())
        || spec.buyer_nonce
            != Pubkey::find_program_address(&[b"stocklana", spec.buyer.as_ref()], &program).0
    {
        return Err("ExposureFill bounds/nonce".into());
    }
    let mut accounts = vec![
        AccountMeta::new_readonly(spec.buyer, true),
        AccountMeta::new(spec.buyer_nonce, false),
    ];
    let mut data = vec![
        13,
        spec.allocations.len() as u8,
        u8::from(spec.allow_underlying_closed),
        0,
    ];
    for value in [
        spec.deadline_slot,
        spec.minimum_exposure_q32,
        spec.maximum_policy_age,
    ] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    let mut input = 0u64;
    let mut legs = 0usize;
    let mut products = std::collections::BTreeSet::new();
    for allocation in &spec.allocations {
        let product = &allocation.product;
        let graph = &allocation.graph;
        if graph.program_id != program
            || graph.accounts.len() < 2
            || graph.accounts[0].pubkey != spec.buyer
            || !graph.accounts[0].is_signer
            || graph.accounts[1].pubkey != spec.buyer_nonce
            || !graph.accounts[1].is_writable
            || product.claim.is_some()
            || product.policy_version == 0
            || product.policy == Pubkey::default()
            || product.mint == spec.input_mint
            || !products.insert(product.mint)
            || !bounded_conversion(
                product.model,
                product.conservative_bps,
                product.numerator,
                product.denominator,
            )
        {
            return Err("ExposureFill product/authority".into());
        }
        let decoded = Graph::decode(&graph.data, graph.accounts.len())
            .map_err(|_| "ExposureFill typed graph")?;
        let source = decoded.assets[0];
        let sink = decoded.assets[decoded.asset_count - 1];
        for (index, expected) in [
            (source.token, spec.buyer_source),
            (source.mint, spec.input_mint),
            (source.program, spec.input_token_program),
            (sink.token, product.destination),
            (sink.mint, product.mint),
            (sink.program, product.token_program),
        ] {
            if graph.accounts[usize::from(index)].pubkey != expected {
                return Err("ExposureFill source/sink".into());
            }
        }
        if decoded.sequence != spec.buyer_sequence
            || decoded.deadline > spec.deadline_slot
            || decoded.min_out == 0
        {
            return Err("ExposureFill signed graph constraints".into());
        }
        input = input
            .checked_add(decoded.input)
            .ok_or("ExposureFill input overflow")?;
        legs = legs
            .checked_add(if decoded.reflow_calls > 0 {
                4
            } else {
                decoded.leg_count
            })
            .ok_or("ExposureFill leg overflow")?;
        if legs > 4 {
            return Err("ExposureFill leg bound".into());
        }
        let bytes = remap_graph(graph, &mut accounts)?;
        let policy = meta(&mut accounts, product.policy, false, false)?;
        data.extend_from_slice(&[policy, product.model]);
        data.extend_from_slice(&product.conservative_bps.to_le_bytes());
        for value in [
            product.policy_version,
            product.numerator,
            product.denominator,
        ] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.extend_from_slice(
            &u16::try_from(bytes.len())
                .map_err(|_| "ExposureFill graph size")?
                .to_le_bytes(),
        );
        data.extend_from_slice(&[0, 0]);
        data.extend_from_slice(&bytes);
    }
    if input != spec.input_atoms {
        return Err("ExposureFill input conservation".into());
    }
    for allocation in &spec.allocations {
        for readonly in [
            allocation.product.policy,
            allocation.product.mint,
            allocation.product.token_program,
            spec.input_mint,
            spec.input_token_program,
        ] {
            if accounts
                .iter()
                .any(|a| a.pubkey == readonly && (a.is_writable || a.is_signer))
            {
                return Err("ExposureFill readonly identity alias".into());
            }
        }
    }
    Ok(Instruction {
        program_id: program,
        accounts,
        data,
    })
}

fn meta(accounts: &mut Vec<AccountMeta>, key: Pubkey, writable: bool, signer: bool) -> Result<u8> {
    if let Some((index, prior)) = accounts
        .iter_mut()
        .enumerate()
        .find(|(_, prior)| prior.pubkey == key)
    {
        prior.is_writable |= writable;
        prior.is_signer |= signer;
        return u8::try_from(index).map_err(|_| "MeshFill account index".into());
    }
    if accounts.len() >= 64 {
        return Err("MeshFill account bound".into());
    }
    let index = u8::try_from(accounts.len()).map_err(|_| "MeshFill account index")?;
    accounts.push(if writable {
        AccountMeta::new(key, signer)
    } else {
        AccountMeta::new_readonly(key, signer)
    });
    Ok(index)
}

fn remap_graph(graph: &Instruction, accounts: &mut Vec<AccountMeta>) -> Result<Vec<u8>> {
    let decoded =
        Graph::decode(&graph.data, graph.accounts.len()).map_err(|_| "MeshFill residual graph")?;
    let mut remap = Vec::with_capacity(graph.accounts.len());
    for account in &graph.accounts {
        remap.push(meta(
            accounts,
            account.pubkey,
            account.is_writable,
            account.is_signer,
        )?);
    }
    let mut data = graph.data.clone();
    for asset in 0..decoded.asset_count {
        for offset in 0..3 {
            let cursor = 36 + asset * 3 + offset;
            data[cursor] = *remap
                .get(data[cursor] as usize)
                .ok_or("MeshFill residual asset index")?;
        }
    }
    let mut cursor = 36 + decoded.asset_count * 3;
    if data[0] == 9 {
        cursor += 8;
    }
    for _ in 0..decoded.leg_count {
        data[cursor + 12] = *remap
            .get(data[cursor + 12] as usize)
            .ok_or("MeshFill residual program index")?;
        let count = data[cursor + 13] as usize;
        for account_index in data.iter_mut().skip(cursor + 14).take(count) {
            *account_index = *remap
                .get(*account_index as usize)
                .ok_or("MeshFill residual CPI index")?;
        }
        cursor = cursor
            .checked_add(14 + count)
            .ok_or("MeshFill residual length")?;
    }
    if cursor != data.len() {
        return Err("MeshFill residual trailing data".into());
    }
    Ok(data)
}

/// Economic Reflow admits direct cash->product candidates and bounded
/// cash->product->product compositions. Every noncash source must have a direct
/// cash producer in the same signed graph, so no third hop or hidden wallet
/// inventory can become executable.
fn bounded_economic_paths(graph: &Graph<'_>) -> bool {
    graph.legs[..graph.leg_count].iter().flatten().all(|leg| {
        leg.destination > 0
            && (leg.source == 0
                || graph.legs[..graph.leg_count]
                    .iter()
                    .flatten()
                    .any(|head| head.source == 0 && head.destination == leg.source))
    }) && (1..graph.asset_count).all(|asset| {
        graph.legs[..graph.leg_count]
            .iter()
            .flatten()
            .any(|leg| leg.destination == asset)
    })
}

/// One common funding execution followed by an opcode-14 clearing cell. The
/// cell's signed cash budget is a minimum funding requirement. Any additional
/// observed proceeds increase only the chosen product's final source consumer;
/// the onchain program preserves the buyer's preexisting cash balance exactly.
/// This compiler does not claim that this fixed surplus allocation is a global
/// execution-time optimum. Exact funded simulation is still required.
pub fn compile_funded_mesh(
    program: Pubkey,
    funding: Instruction,
    cell: Instruction,
    surplus_product: usize,
) -> Result<Instruction> {
    let read = |data: &[u8], offset| dex::u64_at(data, offset).map_err(|_| "funded cell header");
    if funding.program_id != program
        || cell.program_id != program
        || cell.data.len() < 56
        || cell.data[0] != 14
        || cell.data[4] > 1
        || cell.data[5..8] != [0, 0, 0]
        || !(1..=4).contains(&cell.data[1])
        || cell.data[2] > 3
        || cell.data[3] == 0
        || cell.data[3] > cell.data[1]
        || cell.data[48] != 0
        || cell.data[49] != 1
        || cell.accounts.len() < 5
        || funding.accounts.len() < 2
        || funding.data.first() != Some(&2)
        || !cell.accounts[0].is_signer
        || !cell.accounts[1].is_writable
        || funding.accounts[0].pubkey != cell.accounts[0].pubkey
        || funding.accounts[1].pubkey != cell.accounts[1].pubkey
        || funding.accounts.iter().skip(1).any(|meta| meta.is_signer)
        || surplus_product >= usize::from(cell.data[1])
    {
        return Err("funded mesh identity/header".into());
    }
    let mut keys = std::collections::BTreeSet::new();
    if cell.accounts.iter().any(|meta| !keys.insert(meta.pubkey)) {
        return Err("funded mesh duplicate account".into());
    }
    let mut accounts = cell.accounts.clone();
    let funding_bytes = remap_graph(&funding, &mut accounts)?;
    let g = Graph::decode(&funding_bytes, accounts.len()).map_err(|_| "funding graph")?;
    let buyer_cash = usize::from(cell.data[50]);
    let sink = g.assets[g.asset_count - 1];
    if g.sequence != read(&cell.data, 24)?
        || g.deadline > read(&cell.data, 8)?
        || g.min_out < read(&cell.data, 32)?
        || g.leg_count > 3
        || sink.token != cell.data[50]
        || sink.mint != cell.data[51]
        || sink.program != cell.data[52]
        || g.assets[0].token == sink.token
        || g.assets[0].mint == sink.mint
    {
        return Err("funding graph cash/intent binding".into());
    }
    let refs = |graph: &Graph, i: usize| {
        graph.assets[..graph.asset_count]
            .iter()
            .any(|a| [a.token, a.mint, a.program].contains(&(i as u8)))
            || graph.legs[..graph.leg_count]
                .iter()
                .flatten()
                .any(|leg| leg.program as usize == i || leg.accounts.contains(&(i as u8)))
    };
    let account = |i: u8| {
        accounts
            .get(usize::from(i))
            .ok_or("funded mesh account index")
    };
    for i in [cell.data[51], cell.data[52]] {
        if account(i)?.is_writable || account(i)?.is_signer {
            return Err("funded mesh cash identity".into());
        }
    }
    for asset in &g.assets[..g.asset_count] {
        for i in [asset.mint, asset.program] {
            if account(i)?.is_writable || account(i)?.is_signer {
                return Err("funding protected asset".into());
            }
        }
    }
    let mut cursor = 56usize;
    for _ in 0..cell.data[1] {
        let row = cell
            .data
            .get(cursor..cursor + 32)
            .ok_or("funded product descriptor")?;
        for i in [row[0], row[1], row[2]] {
            if refs(&g, usize::from(i)) {
                return Err("funding touches product".into());
            }
        }
        for i in [row[0], row[2], row[3]] {
            if account(i)?.is_writable || account(i)?.is_signer {
                return Err("funded product privilege".into());
            }
        }
        if row[5] != 0
            && (refs(&g, usize::from(row[5]))
                || account(row[5])?.is_writable
                || account(row[5])?.is_signer)
        {
            return Err("funding claim alias".into());
        }
        cursor += 32;
    }
    for _ in 0..cell.data[2] {
        let row = cell
            .data
            .get(cursor..cursor + 40)
            .ok_or("funded seller descriptor")?;
        for i in &row[1..5] {
            account(*i)?;
            if refs(&g, usize::from(*i)) {
                return Err("funding touches seller".into());
            }
        }
        cursor += 40;
    }
    let mut legs = g.leg_count;
    let mut found = false;
    for _ in 0..cell.data[3] {
        let row = cell
            .data
            .get(cursor..cursor + 4)
            .ok_or("funded residual descriptor")?;
        let len = usize::from(u16::from_le_bytes([row[1], row[2]]));
        cursor += 4;
        let bytes = cell
            .data
            .get(cursor..cursor + len)
            .ok_or("funded residual length")?;
        let child = Graph::decode(bytes, accounts.len()).map_err(|_| "funded residual graph")?;
        legs += if child.reflow_calls > 0 {
            4
        } else {
            child.leg_count
        };
        if legs > 4 {
            return Err("funded mesh total leg bound".into());
        }
        for (i, meta) in accounts.iter().enumerate() {
            if meta.is_writable && i != 0 && i != buyer_cash && refs(&g, i) && refs(&child, i) {
                return Err("funded mesh shared writable resource".into());
            }
        }
        if usize::from(row[0]) == surplus_product {
            if found
                || bytes[0] != 2
                || child.legs[..child.leg_count]
                    .iter()
                    .flatten()
                    .rfind(|leg| leg.source == 0)
                    .is_none_or(|leg| leg.budget != u64::MAX)
            {
                return Err("funded surplus must drain residual input".into());
            }
            found = true;
        }
        cursor += len;
    }
    if cursor != cell.data.len() || !found {
        return Err("funded residual layout".into());
    }
    let mut data = vec![18, 1, surplus_product as u8, 0];
    data.extend_from_slice(
        &u16::try_from(funding_bytes.len())
            .map_err(|_| "funding length")?
            .to_le_bytes(),
    );
    data.extend_from_slice(
        &u16::try_from(cell.data.len())
            .map_err(|_| "funded cell length")?
            .to_le_bytes(),
    );
    data.extend_from_slice(&funding_bytes);
    data.extend_from_slice(&cell.data);
    if data.len() > 1024 {
        return Err("funded instruction byte bound".into());
    }
    Ok(Instruction {
        program_id: program,
        accounts,
        data,
    })
}

/// Version-two funding envelope.  The child cell has exactly one sentinel
/// residual record (product index 255) containing a multi-output opcode-4
/// graph.  Funding and market graphs share only the buyer and cash asset; all
/// other writable state remains disjoint.
fn compile_funded_mesh_reflow_envelope(
    program: Pubkey,
    funding: Instruction,
    cell: Instruction,
) -> Result<Instruction> {
    let read = |data: &[u8], offset| dex::u64_at(data, offset).map_err(|_| "funded v2 header");
    if funding.program_id != program
        || cell.program_id != program
        || cell.data.len() < 56
        || cell.data[0] != 14
        || cell.data[4] > 1
        || cell.data[5..8] != [0, 0, 0]
        || !(1..=4).contains(&cell.data[1])
        || cell.data[2] > 3
        || cell.data[3] != 1
        || cell.data[48] != 0
        || cell.data[49] != 1
        || cell.accounts.len() < 5
        || funding.accounts.len() < 2
        || funding.data.first() != Some(&2)
        || !cell.accounts[0].is_signer
        || !cell.accounts[1].is_writable
        || funding.accounts[0].pubkey != cell.accounts[0].pubkey
        || funding.accounts[1].pubkey != cell.accounts[1].pubkey
        || funding.accounts.iter().skip(1).any(|meta| meta.is_signer)
    {
        return Err("funded v2 identity/header".into());
    }
    let mut unique = std::collections::BTreeSet::new();
    if cell.accounts.iter().any(|meta| !unique.insert(meta.pubkey)) {
        return Err("funded v2 duplicate account".into());
    }
    let mut accounts = cell.accounts.clone();
    let funding_bytes = remap_graph(&funding, &mut accounts)?;
    let funding_graph =
        Graph::decode(&funding_bytes, accounts.len()).map_err(|_| "funded v2 funding graph")?;
    let cash_source = usize::from(cell.data[50]);
    let cash_mint = cell.data[51];
    let cash_program = cell.data[52];
    let funding_sink = funding_graph.assets[funding_graph.asset_count - 1];
    if funding_graph.sequence != read(&cell.data, 24)?
        || funding_graph.deadline > read(&cell.data, 8)?
        || funding_graph.min_out < read(&cell.data, 32)?
        || funding_graph.leg_count == 0
        || funding_graph.leg_count >= 4
        || funding_sink.token != cell.data[50]
        || funding_sink.mint != cash_mint
        || funding_sink.program != cash_program
        || funding_graph.assets[0].token == funding_sink.token
        || funding_graph.assets[0].mint == funding_sink.mint
    {
        return Err("funded v2 cash/intent binding".into());
    }
    let references = |graph: &Graph, index: usize| {
        graph.assets[..graph.asset_count]
            .iter()
            .any(|asset| [asset.token, asset.mint, asset.program].contains(&(index as u8)))
            || graph.legs[..graph.leg_count]
                .iter()
                .flatten()
                .any(|leg| leg.program as usize == index || leg.accounts.contains(&(index as u8)))
    };
    let account = |index: u8| {
        accounts
            .get(usize::from(index))
            .ok_or("funded v2 account index")
    };
    if account(cash_mint)?.is_writable
        || account(cash_mint)?.is_signer
        || account(cash_program)?.is_writable
        || account(cash_program)?.is_signer
    {
        return Err("funded v2 cash privilege".into());
    }

    let mut cursor = 56usize;
    let mut product_accounts = Vec::with_capacity(usize::from(cell.data[1]));
    for _ in 0..cell.data[1] {
        let row = cell
            .data
            .get(cursor..cursor + 32)
            .ok_or("funded v2 product descriptor")?;
        for index in [row[0], row[1], row[2]] {
            if references(&funding_graph, usize::from(index)) {
                return Err("funded v2 funding touches product".into());
            }
        }
        for index in [row[0], row[2], row[3]] {
            if account(index)?.is_writable || account(index)?.is_signer {
                return Err("funded v2 product privilege".into());
            }
        }
        if row[5] != 0
            && (references(&funding_graph, usize::from(row[5]))
                || account(row[5])?.is_writable
                || account(row[5])?.is_signer)
        {
            return Err("funded v2 claim alias".into());
        }
        product_accounts.push([row[1], row[2], row[3]]);
        cursor += 32;
    }
    let mut internal_cash = 0u64;
    for _ in 0..cell.data[2] {
        let row = cell
            .data
            .get(cursor..cursor + 40)
            .ok_or("funded v2 seller descriptor")?;
        for index in &row[1..5] {
            account(*index)?;
            if references(&funding_graph, usize::from(*index)) {
                return Err("funded v2 funding touches seller".into());
            }
        }
        internal_cash = internal_cash
            .checked_add(read(row, 24)?)
            .ok_or("funded v2 internal cash")?;
        cursor += 40;
    }
    let row = cell
        .data
        .get(cursor..cursor + 4)
        .ok_or("funded v2 residual descriptor")?;
    if row[0] != u8::MAX || row[3] != 0 {
        return Err("funded v2 residual sentinel".into());
    }
    let graph_len = usize::from(u16::from_le_bytes([row[1], row[2]]));
    cursor += 4;
    let economic_bytes = cell
        .data
        .get(cursor..cursor + graph_len)
        .ok_or("funded v2 residual length")?;
    cursor += graph_len;
    let economic =
        Graph::decode(economic_bytes, accounts.len()).map_err(|_| "funded v2 economic graph")?;
    if cursor != cell.data.len()
        || economic_bytes[0] != 4
        || economic.reflow_calls == 0
        || economic.sequence != funding_graph.sequence
        || economic.deadline > read(&cell.data, 8)?
        || economic.assets[0].token as usize != cash_source
        || economic.assets[0].mint != cash_mint
        || economic.assets[0].program != cash_program
        || internal_cash.checked_add(economic.input) != Some(read(&cell.data, 32)?)
        || !bounded_economic_paths(&economic)
    {
        return Err("funded v2 economic binding".into());
    }
    let mut seen_products = std::collections::BTreeSet::new();
    for asset in &economic.assets[1..economic.asset_count] {
        let identity = [asset.token, asset.mint, asset.program];
        let matches = product_accounts
            .iter()
            .enumerate()
            .filter(|(_, product)| **product == identity)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if matches.len() != 1 || !seen_products.insert(matches[0]) {
            return Err("funded v2 economic product substitution".into());
        }
    }
    for (index, meta) in accounts.iter().enumerate() {
        if meta.is_writable
            && index != 0
            && index != cash_source
            && references(&funding_graph, index)
            && references(&economic, index)
        {
            return Err("funded v2 shared writable resource".into());
        }
    }

    let mut data = vec![18, 2, 0, 0];
    data.extend_from_slice(
        &u16::try_from(funding_bytes.len())
            .map_err(|_| "funded v2 funding length")?
            .to_le_bytes(),
    );
    data.extend_from_slice(
        &u16::try_from(cell.data.len())
            .map_err(|_| "funded v2 cell length")?
            .to_le_bytes(),
    );
    data.extend_from_slice(&funding_bytes);
    data.extend_from_slice(&cell.data);
    if data.len() > 1024 {
        return Err("funded v2 instruction byte bound".into());
    }
    Ok(Instruction {
        program_id: program,
        accounts,
        data,
    })
}

/// Compile one continuous product-aware fill. It can consume escrow-backed
/// ClaimCells and live signed sellers, then execute product-specific residual
/// graphs before enforcing the buyer's aggregate exposure floor.
pub fn compile_mesh_fill(settlement_program: Pubkey, spec: MeshFillSpec) -> Result<Instruction> {
    compile_mesh_fill_inner(settlement_program, spec, false, None)
}

/// Direct-cash Economic Reflow uses opcode 14 with a signed mode bit. It has no
/// fake funding graph and needs no counterparty: the one/two-hop candidate
/// graph consumes the buyer's cash directly and enforces one aggregate stock
/// exposure floor.
pub fn compile_direct_mesh_reflow(
    program: Pubkey,
    spec: MeshFillSpec,
    economic_graph: Instruction,
) -> Result<Instruction> {
    let mut cell = compile_mesh_fill_inner(program, spec, true, Some(&economic_graph))?;
    if cell.data.len() < 56 || cell.data[0] != 14 || cell.data[5..8] != [0, 0, 0] {
        return Err("direct economic reflow header".into());
    }
    cell.data[5] = 1;
    Ok(cell)
}

/// Funding also works when there is no internal match: the same product cell
/// executes the entire residual with no seller signature or fake counterparty.
pub fn compile_funded_mesh_fill(
    program: Pubkey,
    funding: Instruction,
    spec: MeshFillSpec,
    surplus_product: usize,
) -> Result<Instruction> {
    let cell = compile_mesh_fill_inner(program, spec, true, None)?;
    compile_funded_mesh(program, funding, cell, surplus_product)
}

/// Compile shared funding followed by on-chain economic residual Reflow.  The
/// global graph can target several issuer mints, but every leg spends the same
/// cash account and every output must match one declared product descriptor.
pub fn compile_funded_mesh_reflow(
    program: Pubkey,
    funding: Instruction,
    spec: MeshFillSpec,
    economic_graph: Instruction,
) -> Result<Instruction> {
    let cell = compile_mesh_fill_inner(program, spec, true, Some(&economic_graph))?;
    compile_funded_mesh_reflow_envelope(program, funding, cell)
}

fn compile_mesh_fill_inner(
    settlement_program: Pubkey,
    spec: MeshFillSpec,
    allow_empty_sellers: bool,
    economic_graph: Option<&Instruction>,
) -> Result<Instruction> {
    if spec.buyer == Pubkey::default()
        || spec.buyer_sequence == u64::MAX
        || spec.buyer_input_atoms == 0
        || spec.buyer_input_atoms > dex::MAX_INPUT
        || spec.minimum_exposure_q32 == 0
        || spec.deadline_slot == 0
        || !(1..=150).contains(&spec.maximum_policy_age)
        || !(1..=4).contains(&spec.products.len())
        || spec.sellers.len() > 3
        || (spec.sellers.is_empty() && !allow_empty_sellers)
        || if economic_graph.is_some() {
            !spec.residuals.is_empty()
        } else {
            spec.residuals.len() > spec.products.len()
        }
    {
        return Err("MeshFill bounds".into());
    }
    let mut accounts = Vec::new();
    let buyer = meta(&mut accounts, spec.buyer, false, true)?;
    let buyer_nonce = meta(&mut accounts, spec.buyer_nonce, true, false)?;
    let buyer_source = meta(&mut accounts, spec.buyer_cash_source, true, false)?;
    let cash_mint = meta(&mut accounts, spec.cash_mint, false, false)?;
    let cash_program = meta(&mut accounts, spec.cash_token_program, false, false)?;

    let mut product_rows = Vec::with_capacity(spec.products.len());
    for (position, product) in spec.products.iter().enumerate() {
        if product.policy == Pubkey::default()
            || product.destination == Pubkey::default()
            || product.mint == spec.cash_mint
            || product.policy_version == 0
            || !bounded_conversion(
                product.model,
                product.conservative_bps,
                product.numerator,
                product.denominator,
            )
            || spec.products[..position]
                .iter()
                .any(|prior| prior.mint == product.mint || prior.destination == product.destination)
        {
            return Err("MeshFill product".into());
        }
        let mut row = [0u8; 32];
        row[0] = meta(&mut accounts, product.policy, false, false)?;
        row[1] = meta(&mut accounts, product.destination, true, false)?;
        row[2] = meta(&mut accounts, product.mint, false, false)?;
        row[3] = meta(&mut accounts, product.token_program, false, false)?;
        row[4] = product.model;
        row[5] = match product.claim {
            Some(claim) => meta(&mut accounts, claim, false, false)?,
            None => 0,
        };
        row[6..8].copy_from_slice(&product.conservative_bps.to_le_bytes());
        row[8..16].copy_from_slice(&product.policy_version.to_le_bytes());
        row[16..24].copy_from_slice(&product.numerator.to_le_bytes());
        row[24..32].copy_from_slice(&product.denominator.to_le_bytes());
        product_rows.push(row);
    }

    let mut seller_rows = Vec::with_capacity(spec.sellers.len());
    let mut internal_cash = 0u64;
    for seller in &spec.sellers {
        let product = spec
            .products
            .get(seller.product_index)
            .ok_or("MeshFill seller product")?;
        let resting = matches!(seller.authority, MeshSellerAuthority::ClaimCell { .. });
        if seller.owner == spec.buyer
            || seller.stock_atoms == 0
            || seller.stock_atoms > dex::MAX_INPUT
            || seller.cash_atoms == 0
            || seller.cash_atoms < seller.minimum_cash_atoms
            || (resting && product.claim.is_none())
        {
            return Err("MeshFill seller".into());
        }
        internal_cash = internal_cash
            .checked_add(seller.cash_atoms)
            .ok_or("MeshFill internal cash")?;
        let mut row = [0u8; 40];
        row[0] = u8::try_from(seller.product_index).map_err(|_| "MeshFill product index")?;
        row[1] = meta(&mut accounts, seller.owner, false, !resting)?;
        row[2] = match seller.authority {
            MeshSellerAuthority::Signed { nonce } => meta(&mut accounts, nonce, true, false)?,
            MeshSellerAuthority::ClaimCell { order } => meta(&mut accounts, order, true, false)?,
        };
        row[3] = meta(&mut accounts, seller.stock_source, true, false)?;
        row[4] = meta(&mut accounts, seller.cash_destination, true, false)?;
        row[5] = u8::from(resting);
        row[8..16].copy_from_slice(&seller.sequence_or_revision.to_le_bytes());
        row[16..24].copy_from_slice(&seller.stock_atoms.to_le_bytes());
        row[24..32].copy_from_slice(&seller.cash_atoms.to_le_bytes());
        row[32..40].copy_from_slice(&seller.minimum_cash_atoms.to_le_bytes());
        seller_rows.push(row);
    }

    let mut residual_rows = Vec::with_capacity(spec.residuals.len().max(1));
    let mut residual_input = 0u64;
    let mut residual_products = std::collections::BTreeSet::new();
    if let Some(economic) = economic_graph {
        if economic.program_id != settlement_program
            || economic.accounts.len() < 2
            || economic.accounts[0].pubkey != spec.buyer
            || !economic.accounts[0].is_signer
            || economic.accounts[1].pubkey != spec.buyer_nonce
            || !economic.accounts[1].is_writable
        {
            return Err("MeshFill economic graph authority".into());
        }
        let decoded = Graph::decode(&economic.data, economic.accounts.len())
            .map_err(|_| "MeshFill economic graph")?;
        let source = decoded.assets[0];
        if economic.data[0] != 4
            || decoded.reflow_calls == 0
            || decoded.sequence != spec.buyer_sequence
            || decoded.deadline > spec.deadline_slot
            || economic.accounts[usize::from(source.token)].pubkey != spec.buyer_cash_source
            || economic.accounts[usize::from(source.mint)].pubkey != spec.cash_mint
            || economic.accounts[usize::from(source.program)].pubkey != spec.cash_token_program
            || !bounded_economic_paths(&decoded)
        {
            return Err("MeshFill economic graph binding".into());
        }
        for asset in &decoded.assets[1..decoded.asset_count] {
            let matches = spec
                .products
                .iter()
                .enumerate()
                .filter(|(_, product)| {
                    economic.accounts[usize::from(asset.token)].pubkey == product.destination
                        && economic.accounts[usize::from(asset.mint)].pubkey == product.mint
                        && economic.accounts[usize::from(asset.program)].pubkey
                            == product.token_program
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            if matches.len() != 1 || !residual_products.insert(matches[0]) {
                return Err("MeshFill economic product identity".into());
            }
        }
        residual_input = decoded.input;
        let data = remap_graph(economic, &mut accounts)?;
        if data.len() > u16::MAX as usize {
            return Err("MeshFill economic wire bound".into());
        }
        residual_rows.push((usize::from(u8::MAX), data));
    } else {
        let mut maximum_legs = 0usize;
        for residual in &spec.residuals {
            if residual.graph.program_id != settlement_program
                || residual.product_index >= spec.products.len()
                || !residual_products.insert(residual.product_index)
            {
                return Err("MeshFill residual identity".into());
            }
            let decoded = Graph::decode(&residual.graph.data, residual.graph.accounts.len())
                .map_err(|_| "MeshFill residual graph")?;
            if decoded.sequence != spec.buyer_sequence || decoded.deadline > spec.deadline_slot {
                return Err("MeshFill residual authorization".into());
            }
            residual_input = residual_input
                .checked_add(decoded.input)
                .ok_or("MeshFill residual input")?;
            maximum_legs = maximum_legs
                .checked_add(if decoded.reflow_calls > 0 {
                    4
                } else {
                    decoded.leg_count
                })
                .ok_or("MeshFill residual legs")?;
            if maximum_legs > 4 {
                return Err("MeshFill residual leg bound".into());
            }
            let data = remap_graph(&residual.graph, &mut accounts)?;
            if data.len() > u16::MAX as usize {
                return Err("MeshFill residual wire bound".into());
            }
            residual_rows.push((residual.product_index, data));
        }
    }
    if internal_cash.checked_add(residual_input) != Some(spec.buyer_input_atoms) {
        return Err("MeshFill cash conservation".into());
    }
    for (index, _) in spec.products.iter().enumerate() {
        if !spec
            .sellers
            .iter()
            .any(|seller| seller.product_index == index)
            && !residual_products.contains(&index)
        {
            return Err("MeshFill unused product".into());
        }
    }

    let mut data = vec![
        14,
        u8::try_from(spec.products.len()).map_err(|_| "MeshFill product count")?,
        u8::try_from(spec.sellers.len()).map_err(|_| "MeshFill seller count")?,
        u8::try_from(residual_rows.len()).map_err(|_| "MeshFill residual count")?,
        u8::from(spec.allow_underlying_closed),
        0,
        0,
        0,
    ];
    for value in [
        spec.deadline_slot,
        spec.maximum_policy_age,
        spec.buyer_sequence,
        spec.buyer_input_atoms,
        spec.minimum_exposure_q32,
    ] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    data.extend_from_slice(&[
        buyer,
        buyer_nonce,
        buyer_source,
        cash_mint,
        cash_program,
        0,
        0,
        0,
    ]);
    for row in product_rows {
        data.extend_from_slice(&row);
    }
    for row in seller_rows {
        data.extend_from_slice(&row);
    }
    for (product, graph) in residual_rows {
        data.push(u8::try_from(product).map_err(|_| "MeshFill residual product")?);
        data.extend_from_slice(
            &u16::try_from(graph.len())
                .map_err(|_| "MeshFill residual length")?
                .to_le_bytes(),
        );
        data.push(0);
        data.extend_from_slice(&graph);
    }
    Ok(Instruction {
        program_id: settlement_program,
        accounts,
        data,
    })
}

/// Compile the exact unsigned v0 message that a wallet will sign.
///
/// The returned bytes are message bytes rather than a transaction envelope.
/// We still include the compact signature count and 64 bytes per required
/// signature when enforcing Solana's 1,232-byte packet limit.
pub fn compile_unsigned_v0(
    payer: Pubkey,
    instructions: &[Instruction],
    lookup_tables: &[AddressLookupTableAccount],
    recent_blockhash: [u8; 32],
) -> Result<Vec<u8>> {
    if payer == Pubkey::default()
        || recent_blockhash == [0; 32]
        || instructions.is_empty()
        || instructions.len() > crate::wallet_wire::MAX_STOCK_TRANSACTION_INSTRUCTIONS
        || lookup_tables.len() > 4
        || lookup_tables.iter().any(|table| {
            table.key == Pubkey::default()
                || table.addresses.is_empty()
                || table.addresses.len() > 256
        })
    {
        return Err("v0 message bounds".into());
    }
    let compiled = v0::Message::try_compile(
        &payer,
        instructions,
        lookup_tables,
        Hash::new_from_array(recent_blockhash),
    )
    .map_err(|_| "v0 message compile")?;
    let account_locks = compiled.account_keys.len()
        + compiled
            .address_table_lookups
            .iter()
            .map(|lookup| lookup.writable_indexes.len() + lookup.readonly_indexes.len())
            .sum::<usize>();
    if account_locks > 64 {
        return Err("v0 account lock bound".into());
    }
    let required_signatures = usize::from(compiled.header.num_required_signatures);
    let message = VersionedMessage::V0(compiled).serialize();
    let signature_prefix = if required_signatures < 128 { 1 } else { 2 };
    let packet_bytes = signature_prefix
        + required_signatures
            .checked_mul(64)
            .ok_or("v0 signature size")?
        + message.len();
    if packet_bytes > 1_232 {
        return Err("v0 transaction packet bound".into());
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swap_wire::TokenAsset;
    use stocklana_adapters::graph::Venue;

    fn key(byte: u8) -> Pubkey {
        Pubkey::new_from_array([byte; 32])
    }

    #[allow(clippy::too_many_arguments)]
    fn candidate_graph(
        program: Pubkey,
        owner: Pubkey,
        nonce: Pubkey,
        opcode: u8,
        sequence: u64,
        input: u64,
        minimum: u64,
        assets: &[TokenAsset],
    ) -> Instruction {
        let venue = Venue::RaydiumAmmV4;
        let venue_program: Pubkey = venue.program().parse().unwrap();
        let mut accounts = vec![
            AccountMeta::new_readonly(owner, true),
            AccountMeta::new(nonce, false),
        ];
        let mut index = |address: Pubkey, writable: bool| -> u8 {
            if let Some(position) = accounts.iter().position(|meta| meta.pubkey == address) {
                accounts[position].is_writable |= writable;
                position as u8
            } else {
                let position = accounts.len() as u8;
                accounts.push(if writable {
                    AccountMeta::new(address, false)
                } else {
                    AccountMeta::new_readonly(address, false)
                });
                position
            }
        };
        let mut data = vec![opcode, assets.len() as u8, (assets.len() - 1) as u8, 0];
        if opcode == 4 {
            data[3] = 16;
        }
        for value in [sequence, input, minimum, 500] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        for asset in assets {
            data.extend_from_slice(&[
                index(asset.token, true),
                index(asset.mint, false),
                index(asset.token_program, false),
            ]);
        }
        for destination in 1..assets.len() {
            let base = if opcode == 2 { 100 } else { 30 } + destination as u8 * 10;
            let mut cpi = (0..8).map(|offset| key(base + offset)).collect::<Vec<_>>();
            cpi[5] = assets[0].token;
            cpi[6] = assets[destination].token;
            cpi[7] = owner;
            data.extend_from_slice(&[venue as u8, 0, destination as u8, 1]);
            data.extend_from_slice(&u64::MAX.to_le_bytes());
            data.extend_from_slice(&[index(venue_program, false), cpi.len() as u8]);
            for (position, address) in cpi.iter().enumerate() {
                data.push(index(*address, venue.writable(position)));
            }
        }
        Graph::decode(&data, accounts.len()).unwrap();
        Instruction {
            program_id: program,
            accounts,
            data,
        }
    }

    #[test]
    fn reverse_stock_fill_wraps_exact_graph_and_rejects_policy_aliases() {
        let program = key(90);
        let token_program = solana_pubkey::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
        let graph = candidate_graph(
            program,
            key(1),
            key(2),
            2,
            3,
            100_000_000,
            1,
            &[
                TokenAsset {
                    token: key(3),
                    mint: key(4),
                    token_program,
                },
                TokenAsset {
                    token: key(5),
                    mint: key(6),
                    token_program,
                },
            ],
        );
        let graph_accounts = graph.accounts.len();
        let graph_data = graph.data.clone();
        let wrapped = compile_reverse_stock_fill(program, key(7), 4, 150, false, graph).unwrap();
        assert_eq!(&wrapped.data[..4], &[20, graph_accounts as u8, 0, 0]);
        assert_eq!(&wrapped.data[4..12], &4u64.to_le_bytes());
        assert_eq!(&wrapped.data[12..20], &150u64.to_le_bytes());
        assert_eq!(&wrapped.data[20..], graph_data);
        assert_eq!(wrapped.accounts.len(), graph_accounts + 1);
        assert_eq!(wrapped.accounts.last().unwrap().pubkey, key(7));
        assert!(!wrapped.accounts.last().unwrap().is_writable);

        let aliased = candidate_graph(
            program,
            key(1),
            key(2),
            2,
            3,
            100_000_000,
            1,
            &[
                TokenAsset {
                    token: key(7),
                    mint: key(4),
                    token_program,
                },
                TokenAsset {
                    token: key(5),
                    mint: key(6),
                    token_program,
                },
            ],
        );
        assert!(compile_reverse_stock_fill(program, key(7), 4, 150, false, aliased).is_err());
    }

    #[test]
    fn compiles_stable_claim_and_ask_abi() {
        let program = key(90);
        let instrument = instrument_id("NVDA").unwrap();
        let issuer = issuer_id("XSTOCKS").unwrap();
        let claim = compile_product_claim(
            program,
            ProductClaimSpec {
                authority: key(1),
                policy: key(2),
                cash_mint: key(3),
                product_mint: key(4),
                product_token_program: key(5),
                instrument,
                issuer,
                model: FIXED_RATIONAL,
                conservative_bps: 9_980,
                policy_version: 7,
                maximum_policy_age: 150,
                claim_version: 2,
                numerator: 1,
                denominator: 1,
                expires_slot: 500,
            },
        )
        .unwrap();
        assert_eq!(claim.data.len(), 120);
        assert_eq!(claim.data[0], 17);
        assert_eq!(claim.accounts.len(), 7);
        assert_eq!(
            claim.accounts[1].pubkey,
            product_claim_address(&program, &key(1), &instrument, &key(4))
        );

        let ask = compile_ask(
            program,
            AskSpec {
                owner: key(6),
                source: key(7),
                escrow: key(8),
                cash_destination: key(9),
                cash_mint: key(3),
                product_mint: key(4),
                product_token_program: key(5),
                cash_token_program: key(10),
                policy: key(2),
                claim_authority: key(1),
                order_nonce: 11,
                stock_atoms: 12,
                minimum_cash_atoms_per_share: 13,
                expires_slot: 500,
                policy_version: 7,
                maximum_policy_age: 150,
                instrument,
                model: FIXED_RATIONAL,
                conservative_bps: 9_980,
                numerator: 1,
                denominator: 1,
                allow_underlying_closed: false,
            },
        )
        .unwrap();
        assert_eq!(ask.data.len(), 104);
        assert_eq!(ask.data[0], 15);
        assert_eq!(ask.accounts.len(), 12);
        assert_eq!(
            ask.accounts[1].pubkey,
            exposure_order_address(&program, &key(6), 11)
        );
        assert_eq!(ask.accounts[10].pubkey, claim.accounts[1].pubkey);
    }

    #[test]
    fn policy_v2_address_and_wire_commit_rights() {
        let program = key(90);
        let base = StockPolicyV2Spec {
            authority: key(1),
            instrument: instrument_id("NVDA").unwrap(),
            issuer: issuer_id("XSTOCKS").unwrap(),
            input_mint: key(2),
            output_mint: key(3),
            rights_hash: [4; 32],
            version: 1,
            expires_slot: 500,
            flags: 17,
        };
        let first = compile_stock_policy_v2(program, base).unwrap();
        let mut changed = base;
        changed.rights_hash = [5; 32];
        let second = compile_stock_policy_v2(program, changed).unwrap();
        assert_eq!(first.data.len(), 178);
        assert_eq!(first.data[0], 19);
        assert_eq!(&first.data[129..161], &[4; 32]);
        assert_eq!(first.accounts.len(), 3);
        assert_eq!(
            first.accounts[1].pubkey,
            stock_policy_v2_address(
                &program,
                &base.authority,
                &base.instrument,
                &base.input_mint,
                &base.output_mint,
                &base.rights_hash,
            )
        );
        changed = base;
        changed.input_mint = key(6);
        let different_cash = compile_stock_policy_v2(program, changed).unwrap();
        assert_ne!(first.accounts[1].pubkey, second.accounts[1].pubkey);
        assert_ne!(first.accounts[1].pubkey, different_cash.accounts[1].pubkey);
    }

    #[test]
    fn rejects_unbounded_conversion_and_aliases() {
        let result = compile_ask(
            key(90),
            AskSpec {
                owner: key(6),
                source: key(7),
                escrow: key(7),
                cash_destination: key(9),
                cash_mint: key(3),
                product_mint: key(4),
                product_token_program: key(5),
                cash_token_program: key(10),
                policy: key(2),
                claim_authority: key(1),
                order_nonce: 11,
                stock_atoms: 12,
                minimum_cash_atoms_per_share: 13,
                expires_slot: 500,
                policy_version: 7,
                maximum_policy_age: 150,
                instrument: instrument_id("NVDA").unwrap(),
                model: 9,
                conservative_bps: 10_001,
                numerator: 0,
                denominator: 0,
                allow_underlying_closed: false,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn compiles_claimcell_and_live_flow_into_one_fill() {
        let program = key(90);
        let fill = compile_mesh_fill(
            program,
            MeshFillSpec {
                buyer: key(1),
                buyer_nonce: key(2),
                buyer_cash_source: key(3),
                cash_mint: key(4),
                cash_token_program: key(5),
                buyer_sequence: 9,
                buyer_input_atoms: 1_000_000,
                minimum_exposure_q32: 1,
                deadline_slot: 500,
                maximum_policy_age: 150,
                allow_underlying_closed: false,
                products: vec![MeshProduct {
                    policy: key(6),
                    claim: Some(key(7)),
                    destination: key(8),
                    mint: key(9),
                    token_program: key(10),
                    model: FIXED_RATIONAL,
                    conservative_bps: 9_980,
                    policy_version: 2,
                    numerator: 1,
                    denominator: 1,
                }],
                sellers: vec![MeshSeller {
                    product_index: 0,
                    owner: key(11),
                    authority: MeshSellerAuthority::ClaimCell { order: key(12) },
                    stock_source: key(13),
                    cash_destination: key(14),
                    sequence_or_revision: 3,
                    stock_atoms: 100_000,
                    cash_atoms: 1_000_000,
                    minimum_cash_atoms: 990_000,
                }],
                residuals: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(fill.data.len(), 56 + 32 + 40);
        assert_eq!(&fill.data[..8], &[14, 1, 1, 0, 0, 0, 0, 0]);
        assert_eq!(fill.data[56 + 5], 9);
        assert_eq!(fill.data[56 + 32 + 5], 1);
        let owner = fill
            .accounts
            .iter()
            .find(|account| account.pubkey == key(11))
            .unwrap();
        assert!(!owner.is_signer && !owner.is_writable);
        assert!(fill.accounts[0].is_signer);
    }

    #[test]
    fn funded_v2_binds_one_cash_source_to_multiple_issuer_products() {
        let program = key(90);
        let buyer = key(1);
        let nonce = key(2);
        let token_program = solana_pubkey::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
        let cash = TokenAsset {
            token: key(3),
            mint: key(4),
            token_program,
        };
        let source = TokenAsset {
            token: key(20),
            mint: key(21),
            token_program,
        };
        let first = TokenAsset {
            token: key(8),
            mint: key(9),
            token_program,
        };
        let second = TokenAsset {
            token: key(18),
            mint: key(19),
            token_program,
        };
        let funding = candidate_graph(
            program,
            buyer,
            nonce,
            2,
            9,
            10_000_000,
            1_000_000,
            &[source, cash],
        );
        let mut economic = candidate_graph(
            program,
            buyer,
            nonce,
            4,
            9,
            1_000_000,
            1,
            &[cash, first, second],
        );
        // The second issuer is reached through the first issuer product. The
        // runtime must consume only the SPYx delta produced by the first hop.
        let second_leg = 36 + 3 * 3 + (14 + 8);
        economic.data[second_leg + 1] = 1;
        economic.data[second_leg + 14 + 5] = economic.data[39];
        let products = vec![
            MeshProduct {
                policy: key(6),
                claim: None,
                destination: first.token,
                mint: first.mint,
                token_program,
                model: FIXED_RATIONAL,
                conservative_bps: 9_980,
                policy_version: 2,
                numerator: 1,
                denominator: 1,
            },
            MeshProduct {
                policy: key(16),
                claim: None,
                destination: second.token,
                mint: second.mint,
                token_program,
                model: FIXED_RATIONAL,
                conservative_bps: 9_970,
                policy_version: 3,
                numerator: 1,
                denominator: 1,
            },
        ];
        let spec = MeshFillSpec {
            buyer,
            buyer_nonce: nonce,
            buyer_cash_source: cash.token,
            cash_mint: cash.mint,
            cash_token_program: token_program,
            buyer_sequence: 9,
            buyer_input_atoms: 1_000_000,
            minimum_exposure_q32: 1,
            deadline_slot: 500,
            maximum_policy_age: 150,
            allow_underlying_closed: false,
            products,
            sellers: Vec::new(),
            residuals: Vec::new(),
        };
        let decoded = Graph::decode(&economic.data, economic.accounts.len()).unwrap();
        let mut substituted = economic.clone();
        substituted.accounts[usize::from(decoded.assets[1].mint)].pubkey = key(99);
        assert!(
            compile_funded_mesh_reflow(program, funding.clone(), spec.clone(), substituted)
                .unwrap_err()
                .contains("product identity")
        );
        let instruction = compile_funded_mesh_reflow(program, funding, spec, economic).unwrap();
        assert_eq!(&instruction.data[..4], &[18, 2, 0, 0]);
        let funding_len = usize::from(u16::from_le_bytes([
            instruction.data[4],
            instruction.data[5],
        ]));
        let cell = &instruction.data[8 + funding_len..];
        assert_eq!(&cell[..4], &[14, 2, 0, 1]);
        assert_eq!(cell[56 + 2 * 32], u8::MAX);
        let global = Graph::decode(&cell[56 + 2 * 32 + 4..], instruction.accounts.len()).unwrap();
        assert_eq!(global.asset_count, 3);
        assert_eq!(global.legs[0].unwrap().source, 0);
        assert_eq!(global.legs[1].unwrap().source, 1);
        assert_eq!(global.legs[1].unwrap().destination, 2);
    }

    #[test]
    fn compiles_packet_bounded_v0_message_with_lookup_table() {
        let payer = key(1);
        let looked_up: Vec<Pubkey> = (10..50).map(key).collect();
        let instruction = Instruction {
            program_id: key(2),
            accounts: looked_up
                .iter()
                .copied()
                .map(|pubkey| AccountMeta::new_readonly(pubkey, false))
                .collect(),
            data: vec![14, 1, 0, 0],
        };
        let bytes = compile_unsigned_v0(
            payer,
            &[instruction],
            &[AddressLookupTableAccount {
                key: key(3),
                addresses: looked_up,
            }],
            [9; 32],
        )
        .unwrap();
        let message: VersionedMessage = bincode::deserialize(&bytes).unwrap();
        let VersionedMessage::V0(message) = message else {
            panic!("expected v0 message")
        };
        assert_eq!(message.header.num_required_signatures, 1);
        assert_eq!(message.address_table_lookups.len(), 1);
        assert_eq!(message.address_table_lookups[0].readonly_indexes.len(), 40);
        assert!(1 + 64 + bytes.len() <= 1_232);

        let too_many_accounts: Vec<Pubkey> = (10..73).map(key).collect();
        let instruction = Instruction {
            program_id: key(2),
            accounts: too_many_accounts
                .iter()
                .copied()
                .map(|pubkey| AccountMeta::new_readonly(pubkey, false))
                .collect(),
            data: vec![14],
        };
        assert!(compile_unsigned_v0(
            payer,
            &[instruction],
            &[AddressLookupTableAccount {
                key: key(3),
                addresses: too_many_accounts,
            }],
            [9; 32],
        )
        .is_err());
    }

    #[test]
    fn rotated_array_not_in_frozen_table_stays_inline_without_substitution() {
        let old_array=key(10);let new_array=key(11);let shared=key(12);
        let table=AddressLookupTableAccount{key:key(3),addresses:vec![old_array,shared]};
        let instruction=Instruction{program_id:key(2),accounts:vec![AccountMeta::new(new_array,false),AccountMeta::new_readonly(shared,false)],data:vec![14,7,9]};
        let bytes=compile_unsigned_v0(key(1),std::slice::from_ref(&instruction),std::slice::from_ref(&table),[9;32]).unwrap();
        let VersionedMessage::V0(message)=bincode::deserialize(&bytes).unwrap() else {panic!("v0");};
        assert!(message.account_keys.contains(&new_array));
        assert!(!message.account_keys.contains(&old_array));
        assert_eq!(message.address_table_lookups[0].readonly_indexes,[1]);
        assert!(message.address_table_lookups[0].writable_indexes.is_empty());
        let mut resolved=message.account_keys.clone();resolved.push(shared);
        assert_eq!(resolved[message.instructions[0].accounts[0] as usize],new_array);
        assert_eq!(resolved[message.instructions[0].accounts[1] as usize],shared);
        assert_eq!(message.instructions[0].data,instruction.data);
        assert!(message.is_maybe_writable(message.instructions[0].accounts[0] as usize,None));
    }
}
