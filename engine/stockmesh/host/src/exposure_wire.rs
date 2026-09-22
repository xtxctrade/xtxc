//! Native integer allocations -> one product-aware residual execution.
//!
//! This compiler is deliberately narrower than the optimizer. It accepts only
//! a one-stage direct input-to-product allocation from one coherent execution
//! bank. Shared SOL funding and post-funding residual Reflow use opcode 18 and
//! are compiled by a separate path; silently turning them into sequential swaps
//! would weaken the economic intent.
use crate::{
    feed::{Feed, Snapshot},
    native_wire,
    onebook_wire::{self, ExposureAllocation, ExposureFillSpec, MeshProduct},
    swap_wire::{self, AccountView, SwapGraph, TokenAsset},
    world::NativeSwapProposal,
    Result,
};
use solana_instruction::Instruction;
use solana_pubkey::{pubkey, Pubkey};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

#[derive(Clone)]
pub struct DirectProduct {
    /// Manifest product ID carried by the solver proposal. The on-chain product
    /// identity is independently bound by `MeshProduct` and the policy account.
    pub product_id: String,
    pub product: MeshProduct,
    pub minimum_output_atoms: u64,
}

pub struct DirectExposureSpec {
    pub buyer: Pubkey,
    pub buyer_nonce: Pubkey,
    pub input: TokenAsset,
    pub buyer_sequence: u64,
    pub input_atoms: u64,
    pub minimum_exposure_q32: u64,
    pub deadline_slot: u64,
    pub maximum_policy_age: u64,
    pub allow_underlying_closed: bool,
    pub products: Vec<DirectProduct>,
}

pub struct FundedExposureSpec {
    pub buyer: Pubkey,
    pub buyer_nonce: Pubkey,
    pub input: TokenAsset,
    pub cash: TokenAsset,
    pub buyer_sequence: u64,
    pub input_atoms: u64,
    pub minimum_cash_atoms: u64,
    pub minimum_exposure_q32: u64,
    pub reflow_oracle_calls: u8,
    pub deadline_slot: u64,
    pub maximum_policy_age: u64,
    pub allow_underlying_closed: bool,
    pub products: Vec<DirectProduct>,
}

/// Compile SOL (or another funding mint) into cash, then route the actual cash
/// result through the opcode-18 v2 economic graph.  Discovery allocations are
/// used only to admit candidate pools and a conservative funding floor; the
/// on-chain residual split is recomputed after funding and after every CPI.
pub fn compile_funded_exposure_reflow(
    settlement_program: Pubkey,
    spec: FundedExposureSpec,
    proposals: &[NativeSwapProposal],
    bank: &Snapshot,
) -> Result<Instruction> {
    if settlement_program == Pubkey::default()
        || spec.buyer == Pubkey::default()
        || spec.input == spec.cash
        || spec.input_atoms == 0
        || spec.minimum_cash_atoms == 0
        || spec.minimum_exposure_q32 == 0
        || !(8..=64).contains(&spec.reflow_oracle_calls)
        || !(1..=4).contains(&spec.products.len())
        || proposals.len() < 2
        || proposals.len() > stocklana_adapters::graph::MAX_CANDIDATES
    {
        return Err("funded exposure bounds".into());
    }
    if native_wire::wallet_asset(spec.buyer, spec.input.mint, bank)? != spec.input
        || native_wire::wallet_asset(spec.buyer, spec.cash.mint, bank)? != spec.cash
    {
        return Err("funded exposure canonical assets".into());
    }
    let mut products = BTreeMap::new();
    let mut product_mints = BTreeMap::new();
    for product in &spec.products {
        let destination = native_wire::wallet_asset(spec.buyer, product.product.mint, bank)?;
        if product.product_id.is_empty()
            || product.product.claim.is_some()
            || destination.token != product.product.destination
            || destination.token_program != product.product.token_program
            || products
                .insert(product.product_id.as_str(), product)
                .is_some()
            || product_mints
                .insert(product.product.mint, product.product_id.as_str())
                .is_some()
        {
            return Err("funded exposure product identity".into());
        }
    }
    let mut funding = Vec::new();
    let mut residual = Vec::new();
    let mut funding_input = 0u64;
    let mut funding_output = 0u64;
    let mut residual_products = BTreeSet::new();
    let mut direct_products = BTreeSet::new();
    let mut residual_topology = Vec::new();
    for proposal in proposals {
        if proposal.stage == 1 {
            if proposal.product_id.is_some()
                || proposal.market.input_mint != spec.input.mint.to_string()
                || proposal.market.output_mint != spec.cash.mint.to_string()
            {
                return Err("funded exposure funding binding".into());
            }
            funding_input = funding_input
                .checked_add(proposal.input_atoms)
                .ok_or("funded exposure input overflow")?;
            funding_output = funding_output
                .checked_add(proposal.expected_output_atoms)
                .ok_or("funded exposure cash overflow")?;
            funding.push(proposal.clone());
        } else if proposal.stage == 2 {
            let product_id = proposal
                .product_id
                .as_deref()
                .ok_or("funded exposure product candidate")?;
            let source_mint: Pubkey = proposal
                .market
                .input_mint
                .parse()
                .map_err(|_| "funded exposure residual source")?;
            let output_mint: Pubkey = proposal
                .market
                .output_mint
                .parse()
                .map_err(|_| "funded exposure residual output")?;
            let expected_product = product_mints
                .get(&output_mint)
                .ok_or("funded exposure unadmitted output")?;
            if product_id != *expected_product
                || (source_mint != spec.cash.mint && !product_mints.contains_key(&source_mint))
                || source_mint == output_mint
            {
                return Err("funded exposure residual binding".into());
            }
            if source_mint == spec.cash.mint {
                direct_products.insert(output_mint);
            }
            residual_products.insert(product_id);
            residual_topology.push((source_mint, output_mint));
            residual.push(proposal.clone());
        } else {
            return Err("funded exposure stage".into());
        }
    }
    if funding.is_empty()
        || funding.len() > 3
        || funding_input != spec.input_atoms
        || funding_output < spec.minimum_cash_atoms
        || residual_products.len() != spec.products.len()
        || residual_topology
            .iter()
            .any(|(source, _)| *source != spec.cash.mint && !direct_products.contains(source))
        || funding.len().checked_add(1).is_none_or(|legs| legs > 4)
    {
        return Err("funded exposure coverage/conservation".into());
    }
    let funding_graph = native_wire::lower_native_graph(
        SwapGraph {
            owner: spec.buyer,
            sequence: spec.buyer_sequence,
            input_atoms: spec.input_atoms,
            minimum_output_atoms: spec.minimum_cash_atoms,
            deadline_slot: spec.deadline_slot,
            input: spec.input,
            output: spec.cash,
            intermediates: Vec::new(),
            legs: Vec::new(),
        },
        &funding,
        bank,
    )?;
    let funding_instruction =
        swap_wire::compile_swap_graph(settlement_program, &funding_graph, |address| {
            read(bank, address)
        })?;

    // The deployed v2 allocator predates Raydium dynamic fees. For one direct
    // product there is nothing to reallocate: deployed opcode18 v1 consumes the
    // *observed* funding cash (including surplus) with a remainder child graph.
    // Retain issuer policy, aggregate exposure floor and atomic rollback. This
    // is not a generic fallback after CPI failure or an invented program ABI.
    if spec.products.len()==1 && residual.len()==1
        && residual[0].market.input_mint==spec.cash.mint.to_string()
        && residual[0].market.venue==crate::market::Venue::RaydiumClmm
        && bank.accounts.iter().find(|a|a.key==residual[0].market.pool)
            .is_some_and(|a|a.owner==residual[0].market.program && a.data.get(1096..1176).is_some_and(|v|v.iter().any(|x|*x!=0)))
    {
        let product=&spec.products[0];
        let output=TokenAsset{token:product.product.destination,mint:product.product.mint,token_program:product.product.token_program};
        let mut edge=residual[0].clone();edge.input_atoms=spec.minimum_cash_atoms;
        edge.expected_output_atoms=edge.market.compile_exact_pair(bank,&edge.market.input_mint,&edge.market.output_mint)?.quote(edge.input_atoms)?;
        let graph=native_wire::lower_native_graph(SwapGraph{owner:spec.buyer,sequence:spec.buyer_sequence,
            input_atoms:spec.minimum_cash_atoms,minimum_output_atoms:product.minimum_output_atoms,
            deadline_slot:spec.deadline_slot,input:spec.cash,output,intermediates:vec![],legs:vec![]},&[edge],bank)?;
        let graph=swap_wire::compile_swap_graph(settlement_program,&graph,|address|read(bank,address))?;
        return onebook_wire::compile_funded_mesh_fill(settlement_program,funding_instruction,
            onebook_wire::MeshFillSpec{buyer:spec.buyer,buyer_nonce:spec.buyer_nonce,buyer_cash_source:spec.cash.token,
                cash_mint:spec.cash.mint,cash_token_program:spec.cash.token_program,buyer_sequence:spec.buyer_sequence,
                buyer_input_atoms:spec.minimum_cash_atoms,minimum_exposure_q32:spec.minimum_exposure_q32,
                deadline_slot:spec.deadline_slot,maximum_policy_age:spec.maximum_policy_age,
                allow_underlying_closed:spec.allow_underlying_closed,
                products:spec.products.into_iter().map(|p|p.product).collect(),sellers:vec![],
                residuals:vec![onebook_wire::MeshResidual{product_index:0,graph}]},0);
    }

    let mut output_assets = spec
        .products
        .iter()
        .map(|product| TokenAsset {
            token: product.product.destination,
            mint: product.product.mint,
            token_program: product.product.token_program,
        })
        .collect::<Vec<_>>();
    output_assets.sort_by_key(|asset| {
        (
            !direct_products.contains(&asset.mint),
            asset.mint.to_bytes(),
        )
    });
    let output = *output_assets.last().ok_or("funded exposure output")?;
    let economic_graph = native_wire::lower_economic_reflow_graph(
        SwapGraph {
            owner: spec.buyer,
            sequence: spec.buyer_sequence,
            input_atoms: spec.minimum_cash_atoms,
            // This is an external-exposure guard; the cell's signed aggregate
            // Q32 floor remains the authoritative stock-level postcondition.
            minimum_output_atoms: 1,
            deadline_slot: spec.deadline_slot,
            input: spec.cash,
            output,
            intermediates: output_assets[..output_assets.len() - 1].to_vec(),
            legs: Vec::new(),
        },
        &residual,
        bank,
    )?;
    let economic_instruction = swap_wire::compile_economic_reflow_graph(
        settlement_program,
        &economic_graph,
        spec.reflow_oracle_calls,
        |address| read(bank, address),
    )?;
    onebook_wire::compile_funded_mesh_reflow(
        settlement_program,
        funding_instruction,
        onebook_wire::MeshFillSpec {
            buyer: spec.buyer,
            buyer_nonce: spec.buyer_nonce,
            buyer_cash_source: spec.cash.token,
            cash_mint: spec.cash.mint,
            cash_token_program: spec.cash.token_program,
            buyer_sequence: spec.buyer_sequence,
            buyer_input_atoms: spec.minimum_cash_atoms,
            minimum_exposure_q32: spec.minimum_exposure_q32,
            deadline_slot: spec.deadline_slot,
            maximum_policy_age: spec.maximum_policy_age,
            allow_underlying_closed: spec.allow_underlying_closed,
            products: spec
                .products
                .into_iter()
                .map(|product| product.product)
                .collect(),
            sellers: Vec::new(),
            residuals: Vec::new(),
        },
        economic_instruction,
    )
}

/// Compile a direct cash Economic Reflow graph. This is the USDC analogue of
/// opcode-18 v2: candidate products and bounded issuer conversions compete in
/// one opcode-14 settlement, with no fabricated funding leg.
pub fn compile_direct_exposure_reflow(
    settlement_program: Pubkey,
    spec: DirectExposureSpec,
    proposals: &[NativeSwapProposal],
    bank: &Snapshot,
) -> Result<Instruction> {
    if settlement_program == Pubkey::default()
        || spec.buyer == Pubkey::default()
        || spec.input_atoms == 0
        || spec.minimum_exposure_q32 == 0
        || !(1..=4).contains(&spec.products.len())
        || proposals.is_empty()
        || proposals.len() > stocklana_adapters::graph::MAX_CANDIDATES
    {
        return Err("direct economic reflow bounds".into());
    }
    if native_wire::wallet_asset(spec.buyer, spec.input.mint, bank)? != spec.input {
        return Err("direct economic reflow canonical input".into());
    }
    let mut product_mints = BTreeMap::new();
    let mut output_assets = Vec::with_capacity(spec.products.len());
    for product in &spec.products {
        let destination = native_wire::wallet_asset(spec.buyer, product.product.mint, bank)?;
        if product.product_id.is_empty()
            || product.product.claim.is_some()
            || destination.token != product.product.destination
            || destination.token_program != product.product.token_program
            || product_mints
                .insert(product.product.mint, product.product_id.as_str())
                .is_some()
        {
            return Err("direct economic reflow product identity".into());
        }
        output_assets.push(destination);
    }
    let mut direct = BTreeSet::new();
    let mut topology = Vec::new();
    let mut covered = BTreeSet::new();
    let mut pools = BTreeSet::new();
    for proposal in proposals {
        let source: Pubkey = proposal
            .market
            .input_mint
            .parse()
            .map_err(|_| "direct economic reflow source")?;
        let output: Pubkey = proposal
            .market
            .output_mint
            .parse()
            .map_err(|_| "direct economic reflow output")?;
        let expected = product_mints
            .get(&output)
            .ok_or("direct economic reflow unadmitted output")?;
        if proposal.stage != 1
            || proposal.product_id.as_deref() != Some(*expected)
            || source == output
            || (source != spec.input.mint && !product_mints.contains_key(&source))
            || !pools.insert(proposal.market.pool.as_str())
        {
            return Err("direct economic reflow proposal binding".into());
        }
        if source == spec.input.mint {
            direct.insert(output);
        }
        topology.push((source, output));
        covered.insert(output);
    }
    if covered.len() != spec.products.len()
        || topology
            .iter()
            .any(|(source, _)| *source != spec.input.mint && !direct.contains(source))
    {
        return Err("direct economic reflow coverage".into());
    }
    output_assets.sort_by_key(|asset| (!direct.contains(&asset.mint), asset.mint.to_bytes()));
    let output = *output_assets
        .last()
        .ok_or("direct economic reflow output")?;
    let graph = native_wire::lower_economic_reflow_graph(
        SwapGraph {
            owner: spec.buyer,
            sequence: spec.buyer_sequence,
            input_atoms: spec.input_atoms,
            minimum_output_atoms: 1,
            deadline_slot: spec.deadline_slot,
            input: spec.input,
            output,
            intermediates: output_assets[..output_assets.len() - 1].to_vec(),
            legs: Vec::new(),
        },
        proposals,
        bank,
    )?;
    let economic =
        swap_wire::compile_economic_reflow_graph(settlement_program, &graph, 16, |address| {
            read(bank, address)
        })?;
    onebook_wire::compile_direct_mesh_reflow(
        settlement_program,
        onebook_wire::MeshFillSpec {
            buyer: spec.buyer,
            buyer_nonce: spec.buyer_nonce,
            buyer_cash_source: spec.input.token,
            cash_mint: spec.input.mint,
            cash_token_program: spec.input.token_program,
            buyer_sequence: spec.buyer_sequence,
            buyer_input_atoms: spec.input_atoms,
            minimum_exposure_q32: spec.minimum_exposure_q32,
            deadline_slot: spec.deadline_slot,
            maximum_policy_age: spec.maximum_policy_age,
            allow_underlying_closed: spec.allow_underlying_closed,
            products: spec
                .products
                .into_iter()
                .map(|product| product.product)
                .collect(),
            sellers: Vec::new(),
            residuals: Vec::new(),
        },
        economic,
    )
}

pub struct DirectExecutionBank {
    /// Deterministic complete `getMultipleAccounts` request. A successful
    /// response to this exact list becomes the sole lowering/simulation bank.
    pub keys: Vec<String>,
    /// Only these canonical wallet PDAs may decode a returned JSON `null` as an
    /// explicitly absent System account. No market/policy miss is optional.
    pub optional_wallet_accounts: Vec<String>,
    pub assets: Vec<TokenAsset>,
    pub nonce: Pubkey,
}

impl DirectExecutionBank {
    pub fn feed(&self, max_age: Duration, max_bytes: usize) -> Result<Feed> {
        Feed::new_execution_bank(
            self.keys.clone(),
            self.optional_wallet_accounts.iter().cloned().collect(),
            max_age,
            max_bytes,
        )
    }
}

const SYSTEM: Pubkey = Pubkey::new_from_array([0; 32]);
const ATA: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// Expand a proposal bank into the final coherent execution-bank request.
/// Dependencies are derived from admitted pool bytes; no external API supplies
/// CPI metas or writable privileges.
pub fn plan_direct_execution_bank(
    settlement_program: Pubkey,
    lookup_table: Pubkey,
    buyer: Pubkey,
    input_mint: Pubkey,
    products: &[DirectProduct],
    proposals: &[NativeSwapProposal],
    discovery: &Snapshot,
) -> Result<DirectExecutionBank> {
    if settlement_program == SYSTEM
        || lookup_table == SYSTEM
        || buyer == SYSTEM
        || !(1..=4).contains(&products.len())
        || proposals.is_empty()
        || proposals.len() > stocklana_adapters::graph::MAX_CANDIDATES
    {
        return Err("direct execution bank bounds".into());
    }
    let mut keys = BTreeSet::new();
    let mut optional = BTreeSet::new();
    keys.extend([settlement_program, lookup_table, buyer, SYSTEM, ATA]);
    let input = native_wire::wallet_asset(buyer, input_mint, discovery)?;
    let mut assets = vec![input];
    keys.extend([input.token, input.mint, input.token_program]);
    optional.insert(input.token);
    let mut product_ids = BTreeMap::new();
    let mut product_by_mint = BTreeMap::new();
    for product in products {
        if product.product_id.is_empty()
            || product.product_id.len() > 64
            || product.product.policy == SYSTEM
            || product.product.claim.is_some()
            || product_ids
                .insert(product.product_id.as_str(), product)
                .is_some()
            || product_by_mint
                .insert(product.product.mint, product.product_id.as_str())
                .is_some()
        {
            return Err("direct execution bank product".into());
        }
        let asset = native_wire::wallet_asset(buyer, product.product.mint, discovery)?;
        if asset.token != product.product.destination
            || asset.token_program != product.product.token_program
            || asset.mint == input_mint
        {
            return Err("direct execution bank product ATA".into());
        }
        keys.extend([
            asset.token,
            asset.mint,
            asset.token_program,
            product.product.policy,
        ]);
        optional.insert(asset.token);
        assets.push(asset);
    }
    let mut seen_pools = BTreeSet::new();
    let mut allocated_products = BTreeSet::new();
    let mut direct_products = BTreeSet::new();
    let mut topology = Vec::new();
    for proposal in proposals {
        let product_id = proposal
            .product_id
            .as_deref()
            .ok_or("direct execution bank proposal product")?;
        let source: Pubkey = proposal
            .market
            .input_mint
            .parse()
            .map_err(|_| "direct execution bank proposal source")?;
        let output: Pubkey = proposal
            .market
            .output_mint
            .parse()
            .map_err(|_| "direct execution bank proposal output")?;
        let expected = product_by_mint
            .get(&output)
            .ok_or("direct execution bank unadmitted output")?;
        if proposal.stage != 1
            || product_id != *expected
            || source == output
            || (source != input_mint && !product_by_mint.contains_key(&source))
            || !seen_pools.insert(proposal.market.pool.as_str())
        {
            return Err("direct execution bank proposal binding".into());
        }
        if source == input_mint {
            direct_products.insert(output);
        }
        topology.push((source, output));
        allocated_products.insert(product_id);
        keys.extend(native_wire::execution_dependencies(
            &proposal.market,
            discovery,
        )?);
    }
    if allocated_products.len() != products.len()
        || topology
            .iter()
            .any(|(source, _)| *source != input_mint && !direct_products.contains(source))
    {
        return Err("direct execution bank missing product".into());
    }
    let nonce =
        Pubkey::find_program_address(&[b"stocklana", buyer.as_ref()], &settlement_program).0;
    keys.insert(nonce);
    optional.insert(nonce);
    if keys.len() > 100 || optional.len() > 6 {
        return Err("direct execution bank account bound".into());
    }
    Ok(DirectExecutionBank {
        keys: keys.into_iter().map(|key| key.to_string()).collect(),
        optional_wallet_accounts: optional.into_iter().map(|key| key.to_string()).collect(),
        assets,
        nonce,
    })
}

/// Complete bank for SOL->cash funding plus cash->issuer candidates.  Every
/// dependency is derived from the same native market manifests as the direct
/// path; no aggregator transaction or API credential enters the account list.
#[allow(clippy::too_many_arguments)]
pub fn plan_funded_execution_bank(
    settlement_program: Pubkey,
    lookup_table: Pubkey,
    buyer: Pubkey,
    input_mint: Pubkey,
    cash_mint: Pubkey,
    products: &[DirectProduct],
    proposals: &[NativeSwapProposal],
    discovery: &Snapshot,
) -> Result<DirectExecutionBank> {
    if input_mint == cash_mint
        || settlement_program == SYSTEM
        || lookup_table == SYSTEM
        || buyer == SYSTEM
        || !(1..=4).contains(&products.len())
        || proposals.len() < 2
        || proposals.len() > stocklana_adapters::graph::MAX_CANDIDATES
    {
        return Err("funded execution bank bounds".into());
    }
    let input = native_wire::wallet_asset(buyer, input_mint, discovery)?;
    let cash = native_wire::wallet_asset(buyer, cash_mint, discovery)?;
    let mut keys = BTreeSet::from([settlement_program, lookup_table, buyer, SYSTEM, ATA]);
    let mut optional = BTreeSet::new();
    let mut assets = vec![input, cash];
    for asset in [input, cash] {
        keys.extend([asset.token, asset.mint, asset.token_program]);
        optional.insert(asset.token);
    }
    let mut admitted = BTreeMap::new();
    let mut admitted_by_mint = BTreeMap::new();
    for product in products {
        let asset = native_wire::wallet_asset(buyer, product.product.mint, discovery)?;
        if product.product_id.is_empty()
            || product.product.claim.is_some()
            || asset.token != product.product.destination
            || asset.token_program != product.product.token_program
            || admitted
                .insert(product.product_id.as_str(), product.product.mint)
                .is_some()
            || admitted_by_mint
                .insert(product.product.mint, product.product_id.as_str())
                .is_some()
        {
            return Err("funded execution bank product".into());
        }
        keys.extend([
            asset.token,
            asset.mint,
            asset.token_program,
            product.product.policy,
        ]);
        optional.insert(asset.token);
        assets.push(asset);
    }
    let mut funding = 0usize;
    let mut product_candidates = BTreeSet::new();
    let mut direct_products = BTreeSet::new();
    let mut product_topology = Vec::new();
    let mut pools = BTreeSet::new();
    for proposal in proposals {
        let valid = if proposal.stage == 1 {
            funding += 1;
            proposal.product_id.is_none()
                && proposal.market.input_mint == input_mint.to_string()
                && proposal.market.output_mint == cash_mint.to_string()
        } else if proposal.stage == 2 {
            let product_id = proposal
                .product_id
                .as_deref()
                .ok_or("funded execution bank candidate product")?;
            let input: Pubkey = proposal
                .market
                .input_mint
                .parse()
                .map_err(|_| "funded execution bank candidate source")?;
            let output: Pubkey = proposal
                .market
                .output_mint
                .parse()
                .map_err(|_| "funded execution bank candidate output")?;
            let expected = admitted_by_mint
                .get(&output)
                .ok_or("funded execution bank unadmitted product")?;
            product_candidates.insert(product_id);
            if input == cash_mint {
                direct_products.insert(output);
            }
            product_topology.push((input, output));
            product_id == *expected
                && input != output
                && (input == cash_mint || admitted_by_mint.contains_key(&input))
        } else {
            false
        };
        if !valid || !pools.insert(proposal.market.pool.as_str()) {
            return Err("funded execution bank proposal binding".into());
        }
        keys.extend(native_wire::execution_dependencies(
            &proposal.market,
            discovery,
        )?);
    }
    if funding == 0
        || funding > 3
        || product_candidates.len() != products.len()
        || product_topology
            .iter()
            .any(|(source, _)| *source != cash_mint && !direct_products.contains(source))
    {
        return Err("funded execution bank candidate coverage".into());
    }
    let nonce =
        Pubkey::find_program_address(&[b"stocklana", buyer.as_ref()], &settlement_program).0;
    keys.insert(nonce);
    optional.insert(nonce);
    if keys.len() > 100 || optional.len() > 7 {
        return Err("funded execution bank account bound".into());
    }
    Ok(DirectExecutionBank {
        keys: keys.into_iter().map(|key| key.to_string()).collect(),
        optional_wallet_accounts: optional.into_iter().map(|key| key.to_string()).collect(),
        assets,
        nonce,
    })
}

fn read<'a>(bank: &'a Snapshot, address: &Pubkey) -> Result<AccountView<'a>> {
    let name = address.to_string();
    let mut rows = bank.accounts.iter().filter(|value| value.key == name);
    let value = rows.next().ok_or("direct exposure account missing")?;
    if rows.next().is_some() {
        return Err("direct exposure ambiguous account".into());
    }
    Ok(AccountView {
        owner: value
            .owner
            .parse()
            .map_err(|_| "direct exposure account owner")?,
        executable: value.executable,
        data: &value.data,
    })
}

/// Compile the solver's exact integer split without converting it to display
/// percentages or re-quoting products independently. Native lowering still
/// re-quotes every venue against this same bank and rejects stale curves.
pub fn compile_direct_exposure(
    settlement_program: Pubkey,
    spec: DirectExposureSpec,
    proposals: &[NativeSwapProposal],
    bank: &Snapshot,
) -> Result<Instruction> {
    if settlement_program == Pubkey::default()
        || spec.buyer == Pubkey::default()
        || !(1..=4).contains(&spec.products.len())
        || proposals.is_empty()
        || proposals.len() > 4
        || spec.input_atoms == 0
    {
        return Err("direct exposure bounds".into());
    }
    let canonical_input = native_wire::wallet_asset(spec.buyer, spec.input.mint, bank)?;
    if canonical_input != spec.input {
        return Err("direct exposure input ATA".into());
    }
    let mut products = BTreeMap::new();
    for product in &spec.products {
        if product.product_id.is_empty()
            || product.product_id.len() > 64
            || product.minimum_output_atoms == 0
            || product.product.claim.is_some()
            || products
                .insert(product.product_id.as_str(), product)
                .is_some()
        {
            return Err("direct exposure product identity".into());
        }
        let canonical = native_wire::wallet_asset(spec.buyer, product.product.mint, bank)?;
        if canonical.token != product.product.destination
            || canonical.token_program != product.product.token_program
            || product.product.mint == spec.input.mint
        {
            return Err("direct exposure product ATA".into());
        }
    }
    let mut grouped = BTreeMap::<&str, Vec<NativeSwapProposal>>::new();
    let mut input = 0u64;
    let mut pools = BTreeSet::new();
    for proposal in proposals {
        let product_id = proposal
            .product_id
            .as_deref()
            .ok_or("direct exposure proposal product")?;
        let product = products
            .get(product_id)
            .ok_or("direct exposure unadmitted product")?;
        if proposal.stage != 1
            || proposal.market.input_mint != spec.input.mint.to_string()
            || proposal.market.output_mint != product.product.mint.to_string()
            || !pools.insert(proposal.market.pool.as_str())
        {
            return Err("direct exposure proposal binding".into());
        }
        input = input
            .checked_add(proposal.input_atoms)
            .ok_or("direct exposure input overflow")?;
        grouped
            .entry(product_id)
            .or_default()
            .push(proposal.clone());
    }
    if input != spec.input_atoms || grouped.len() != products.len() {
        return Err("direct exposure allocation conservation".into());
    }
    let mut allocations = Vec::with_capacity(spec.products.len());
    for product in &spec.products {
        let legs = grouped
            .remove(product.product_id.as_str())
            .ok_or("direct exposure missing product allocation")?;
        let product_input = legs.iter().try_fold(0u64, |sum, proposal| {
            sum.checked_add(proposal.input_atoms)
                .ok_or("direct exposure product input overflow")
        })?;
        let quoted_output = legs.iter().try_fold(0u64, |sum, proposal| {
            sum.checked_add(proposal.expected_output_atoms)
                .ok_or("direct exposure product output overflow")
        })?;
        if quoted_output < product.minimum_output_atoms {
            return Err("direct exposure product floor".into());
        }
        let output = TokenAsset {
            token: product.product.destination,
            mint: product.product.mint,
            token_program: product.product.token_program,
        };
        let graph = native_wire::lower_native_graph(
            SwapGraph {
                owner: spec.buyer,
                sequence: spec.buyer_sequence,
                input_atoms: product_input,
                minimum_output_atoms: product.minimum_output_atoms,
                deadline_slot: spec.deadline_slot,
                input: spec.input,
                output,
                intermediates: Vec::new(),
                legs: Vec::new(),
            },
            &legs,
            bank,
        )?;
        allocations.push(ExposureAllocation {
            product: product.product,
            graph: swap_wire::compile_swap_graph(settlement_program, &graph, |address| {
                read(bank, address)
            })?,
        });
    }
    onebook_wire::compile_exposure_fill(
        settlement_program,
        ExposureFillSpec {
            buyer: spec.buyer,
            buyer_nonce: spec.buyer_nonce,
            buyer_source: spec.input.token,
            input_mint: spec.input.mint,
            input_token_program: spec.input.token_program,
            buyer_sequence: spec.buyer_sequence,
            input_atoms: spec.input_atoms,
            minimum_exposure_q32: spec.minimum_exposure_q32,
            deadline_slot: spec.deadline_slot,
            maximum_policy_age: spec.maximum_policy_age,
            allow_underlying_closed: spec.allow_underlying_closed,
            allocations,
        },
    )
}
