//! StockMesh FlowCell: product-aware internal crossing followed by aggregate
//! economic-exposure residual execution.
//!
//! One buyer signs a total cash debit and an underlying-share Q32 floor. Up to
//! three sellers contribute exact issuer-product atoms in exchange for exact
//! cash, and the buyer's remaining cash is routed through up to four typed DEX
//! graphs. The program validates every product policy, owner, nonce, account
//! partition and residual graph before the first transfer or DEX CPI. It then
//! checks the buyer's aggregate conservative exposure across issuer products.
use super::*;
use dex::graph::Graph;

const MAX_PRODUCTS: usize = exposure::MAX_PRODUCTS;
const MAX_SELLERS: usize = 3;
const HEADER_LEN: usize = 56;
const PRODUCT_LEN: usize = 32;
const SELLER_LEN: usize = 40;

#[derive(Clone, Copy, Default)]
struct Product {
    policy: usize,
    claim: usize,
    destination: usize,
    mint: usize,
    token_program: usize,
    model: u8,
    conservative_bps: u16,
    policy_version: u64,
    numerator: u64,
    denominator: u64,
    before_output: u64,
    internal_raw: u64,
}

#[derive(Clone, Copy, Default)]
struct Seller {
    product: usize,
    owner: usize,
    nonce: usize,
    source: usize,
    cash_destination: usize,
    sequence: u64,
    stock_input: u64,
    cash_output: u64,
    minimum_cash_output: u64,
    before_stock: u64,
    before_cash: u64,
    resting: bool,
    order_nonce: u64,
}

fn references(graph: &Graph, index: usize) -> bool {
    graph.assets[..graph.asset_count]
        .iter()
        .any(|asset| [asset.token, asset.mint, asset.program].contains(&(index as u8)))
        || graph.legs[..graph.leg_count]
            .iter()
            .flatten()
            .any(|leg| leg.program as usize == index || leg.accounts.contains(&(index as u8)))
}

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

/// Opcode 14 layout:
///
/// - 56-byte header: opcode, product/seller/residual counts, closed-session
///   flag, reserved bytes, deadline, policy age, buyer sequence, total buyer
///   cash input, minimum aggregate exposure, and buyer account indices.
/// - one 32-byte descriptor per issuer product;
/// - one 40-byte seller record per internal stock-for-cash crossing;
/// - residual records `(product_index, graph_len, reserved, graph_bytes)`.
#[inline(never)]
pub(super) fn clear(program: &Pubkey, a: &[AccountInfo], d: &[u8]) -> ProgramResult {
    clear_inner(program, a, d, None)
}

pub(super) fn clear_with_funding(
    program: &Pubkey,
    a: &[AccountInfo],
    d: &[u8],
    funding: funding::Funding,
) -> ProgramResult {
    clear_inner(program, a, d, Some(funding))
}

#[inline(never)]
fn clear_inner(
    program: &Pubkey,
    a: &[AccountInfo],
    d: &[u8],
    funding: Option<funding::Funding>,
) -> ProgramResult {
    let direct_global_reflow = d.get(5) == Some(&1);
    let global_reflow = funding.is_some_and(|value| value.global_reflow) || direct_global_reflow;
    need(
        d.len() >= HEADER_LEN
            && d[0] == 14
            && d[4] <= 1
            && d[5] <= 1
            && d[6..8] == [0, 0]
            && !(funding.is_some() && direct_global_reflow)
            && a.len() <= 64,
        BOUNDS,
    )?;
    let product_count = d[1] as usize;
    let seller_count = d[2] as usize;
    let residual_count = d[3] as usize;
    need(
        (1..=MAX_PRODUCTS).contains(&product_count)
            && (seller_count <= MAX_SELLERS
                && (seller_count > 0 || funding.is_some() || direct_global_reflow))
            && if global_reflow {
                residual_count == 1
            } else {
                residual_count <= product_count
            },
        BOUNDS,
    )?;
    #[cfg(feature = "profile-cu")]
    solana_program::msg!("PROFILE mesh_header");
    let deadline = ad(dex::u64_at(d, 8))?;
    let maximum_policy_age = ad(dex::u64_at(d, 16))?;
    let buyer_sequence = ad(dex::u64_at(d, 24))?;
    let mut buyer_input = ad(dex::u64_at(d, 32))?;
    let minimum_exposure = ad(dex::u64_at(d, 40))?;
    need(
        (1..=150).contains(&maximum_policy_age)
            && buyer_sequence < u64::MAX
            && buyer_input > 0
            && buyer_input <= dex::MAX_INPUT
            && minimum_exposure > 0,
        BOUNDS,
    )?;
    let buyer = d[48] as usize;
    let buyer_nonce = d[49] as usize;
    let buyer_source = d[50] as usize;
    let cash_mint = d[51] as usize;
    let cash_program = d[52] as usize;
    need(
        d[53..56] == [0, 0, 0]
            && [buyer, buyer_nonce, buyer_source, cash_mint, cash_program]
                .iter()
                .all(|index| *index < a.len())
            && a[buyer].is_signer
            && a[buyer_nonce].is_writable
            && a[buyer_nonce].owner == program,
        IDENTITY,
    )?;
    {
        let nonce = a[buyer_nonce].try_borrow_data()?;
        need(
            nonce.len() == NONCE_LEN
                && &nonce[..8] == NONCE_TAG
                && nonce[8..40] == a[buyer].key.to_bytes(),
            IDENTITY,
        )?;
        need(ad(dex::u64_at(&nonce, 40))? == buyer_sequence, REPLAY)?;
    }
    let before_buyer_cash = graph::checked_asset(
        &a[buyer],
        &a[buyer_source],
        &a[cash_mint],
        &a[cash_program],
        true,
    )?;
    let funding_state = match funding {
        Some(funding) => Some(funding.inspect(
            a,
            buyer,
            buyer_sequence,
            buyer_source,
            cash_mint,
            cash_program,
            deadline,
            buyer_input,
        )?),
        None => {
            need(before_buyer_cash >= buyer_input, BOUNDS)?;
            None
        }
    };
    let clock = Clock::get()?;
    need(clock.slot <= deadline && clock.unix_timestamp >= 0, EXPIRED)?;

    let product_start = HEADER_LEN;
    let seller_start = product_start
        .checked_add(product_count.checked_mul(PRODUCT_LEN).ok_or(err(BOUNDS))?)
        .ok_or(err(BOUNDS))?;
    let residual_start = seller_start
        .checked_add(seller_count.checked_mul(SELLER_LEN).ok_or(err(BOUNDS))?)
        .ok_or(err(BOUNDS))?;
    need(residual_start <= d.len(), BOUNDS)?;

    let mut products = [Product::default(); MAX_PRODUCTS];
    // A MeshFill is one economic-stock intent. Each issuer policy is valid in
    // its own namespace, so pair validation alone cannot prevent a caller from
    // summing unrelated instruments into one aggregate exposure floor.
    let mut economic_instrument = [0u8; 32];
    for position in 0..product_count {
        let start = product_start + position * PRODUCT_LEN;
        let row = d.get(start..start + PRODUCT_LEN).ok_or(err(BOUNDS))?;
        need(
            row[..4].iter().all(|index| (*index as usize) < a.len())
                && (row[5] == 0 || (row[5] as usize) < a.len()),
            BOUNDS,
        )?;
        let mut product = Product {
            policy: row[0] as usize,
            claim: row[5] as usize,
            destination: row[1] as usize,
            mint: row[2] as usize,
            token_program: row[3] as usize,
            model: row[4],
            conservative_bps: u16::from_le_bytes([row[6], row[7]]),
            policy_version: ad(dex::u64_at(row, 8))?,
            numerator: ad(dex::u64_at(row, 16))?,
            denominator: ad(dex::u64_at(row, 24))?,
            ..Product::default()
        };
        need(
            product.policy_version > 0
                && product.numerator > 0
                && product.denominator > 0
                && (1..=10_000).contains(&product.conservative_bps)
                && matches!(
                    product.model,
                    exposure::FIXED_RATIONAL | exposure::TOKEN_2022_SCALED_UI
                )
                && a[product.mint].key != a[cash_mint].key,
            BOUNDS,
        )?;
        if product.claim != 0 {
            need(
                !a[product.claim].is_writable && !a[product.claim].is_signer,
                IDENTITY,
            )?;
        }
        if let Some(funding) = funding {
            funding.exclude(a, &[product.policy, product.destination, product.mint])?;
            if product.claim != 0 {
                funding.exclude(a, &[product.claim])?;
            }
        }
        for prior in &products[..position] {
            need(
                a[prior.mint].key != a[product.mint].key
                    && a[prior.destination].key != a[product.destination].key
                    && a[prior.policy].key != a[product.policy].key
                    && (prior.claim == 0
                        || product.claim == 0
                        || a[prior.claim].key != a[product.claim].key),
                IDENTITY,
            )?;
        }
        product.before_output = graph::checked_asset(
            &a[buyer],
            &a[product.destination],
            &a[product.mint],
            &a[product.token_program],
            true,
        )?;
        if product.claim == 0 {
            stock::validate_policy_for_pair(
                program,
                a,
                product.policy,
                product.policy_version,
                maximum_policy_age,
                d[4] == 1,
                cash_mint,
                product.mint,
                deadline,
                &clock,
            )?;
        } else {
            claim::validate(
                program,
                a,
                product.claim,
                product.policy,
                cash_mint,
                product.mint,
                product.token_program,
                product.policy_version,
                maximum_policy_age,
                d[4] == 1,
                product.model,
                product.conservative_bps,
                product.numerator,
                product.denominator,
                deadline,
                &clock,
            )?;
        }
        {
            let policy = a[product.policy].try_borrow_data()?;
            let instrument = ad(dex::key(&policy, 40))?;
            if position == 0 {
                economic_instrument = instrument;
            } else {
                need(instrument == economic_instrument, IDENTITY)?;
            }
        }
        products[position] = product;
    }
    #[cfg(feature = "profile-cu")]
    solana_program::msg!("PROFILE mesh_products");

    let mut sellers = [Seller::default(); MAX_SELLERS];
    let mut internal_cash = 0u64;
    for position in 0..seller_count {
        let start = seller_start + position * SELLER_LEN;
        let row = d.get(start..start + SELLER_LEN).ok_or(err(BOUNDS))?;
        need(row[5] <= 1 && row[6..8] == [0, 0], BOUNDS)?;
        let mut seller = Seller {
            product: row[0] as usize,
            owner: row[1] as usize,
            nonce: row[2] as usize,
            source: row[3] as usize,
            cash_destination: row[4] as usize,
            sequence: ad(dex::u64_at(row, 8))?,
            stock_input: ad(dex::u64_at(row, 16))?,
            cash_output: ad(dex::u64_at(row, 24))?,
            minimum_cash_output: ad(dex::u64_at(row, 32))?,
            resting: row[5] == 1,
            ..Seller::default()
        };
        need(
            seller.product < product_count
                && [
                    seller.owner,
                    seller.nonce,
                    seller.source,
                    seller.cash_destination,
                ]
                .iter()
                .all(|index| *index < a.len())
                && seller.owner != buyer
                && a[seller.nonce].is_writable
                && a[seller.nonce].owner == program
                && seller.sequence < u64::MAX
                && seller.stock_input > 0
                && seller.stock_input <= dex::MAX_INPUT
                && seller.minimum_cash_output > 0
                && seller.cash_output >= seller.minimum_cash_output,
            IDENTITY,
        )?;
        need(a[seller.owner].is_signer || seller.resting, IDENTITY)?;
        need(!seller.resting || !a[seller.owner].is_writable, IDENTITY)?;
        let financial = [
            seller.owner,
            seller.nonce,
            seller.source,
            seller.cash_destination,
        ];
        if let Some(funding) = funding {
            funding.exclude(a, &financial)?;
        }
        for left in 0..financial.len() {
            for right in 0..left {
                need(a[financial[left]].key != a[financial[right]].key, IDENTITY)?;
            }
            need(
                a[financial[left]].key != a[buyer].key
                    && a[financial[left]].key != a[buyer_nonce].key
                    && a[financial[left]].key != a[buyer_source].key,
                IDENTITY,
            )?;
            for product in &products[..product_count] {
                need(
                    a[financial[left]].key != a[product.destination].key,
                    IDENTITY,
                )?;
            }
            for prior in &sellers[..position] {
                need(
                    [
                        prior.owner,
                        prior.nonce,
                        prior.source,
                        prior.cash_destination,
                    ]
                    .iter()
                    .all(|index| a[financial[left]].key != a[*index].key),
                    IDENTITY,
                )?;
            }
        }
        if !seller.resting {
            let nonce = a[seller.nonce].try_borrow_data()?;
            need(
                nonce.len() == NONCE_LEN
                    && &nonce[..8] == NONCE_TAG
                    && nonce[8..40] == a[seller.owner].key.to_bytes(),
                IDENTITY,
            )?;
            need(ad(dex::u64_at(&nonce, 40))? == seller.sequence, REPLAY)?;
        }
        let product = &mut products[seller.product];
        if seller.resting {
            need(product.claim != 0, IDENTITY)?;
            seller.order_nonce = order::validate_fill(
                program,
                a,
                seller.nonce,
                seller.owner,
                seller.source,
                seller.cash_destination,
                product.claim,
                cash_mint,
                product.mint,
                product.token_program,
                seller.sequence,
                seller.stock_input,
                seller.cash_output,
                product.model,
                product.conservative_bps,
                product.numerator,
                product.denominator,
                &clock,
            )?;
        }
        let stock_authority = if seller.resting {
            seller.nonce
        } else {
            seller.owner
        };
        seller.before_stock = graph::checked_asset(
            &a[stock_authority],
            &a[seller.source],
            &a[product.mint],
            &a[product.token_program],
            true,
        )?;
        seller.before_cash = graph::checked_asset(
            &a[seller.owner],
            &a[seller.cash_destination],
            &a[cash_mint],
            &a[cash_program],
            true,
        )?;
        need(seller.before_stock >= seller.stock_input, BOUNDS)?;
        product.internal_raw = product
            .internal_raw
            .checked_add(seller.stock_input)
            .ok_or(err(BOUNDS))?;
        internal_cash = internal_cash
            .checked_add(seller.cash_output)
            .ok_or(err(BOUNDS))?;
        need(internal_cash <= buyer_input, BOUNDS)?;
        sellers[position] = seller;
    }

    let mut residuals: [Option<&[u8]>; MAX_PRODUCTS] = [None; MAX_PRODUCTS];
    let mut global_residual = None;
    let mut global_products = [false; MAX_PRODUCTS];
    let mut residual_input = 0u64;
    let mut external_legs = funding_state.map_or(0, |funding| funding.legs);
    let mut cursor = residual_start;
    for _ in 0..residual_count {
        let header = d.get(cursor..cursor + 4).ok_or(err(BOUNDS))?;
        let product_index = header[0] as usize;
        let graph_len = usize::from(u16::from_le_bytes([header[1], header[2]]));
        need(header[3] == 0, BOUNDS)?;
        cursor = cursor.checked_add(4).ok_or(err(BOUNDS))?;
        let bytes = d
            .get(cursor..cursor.checked_add(graph_len).ok_or(err(BOUNDS))?)
            .ok_or(err(BOUNDS))?;
        cursor = cursor.checked_add(graph_len).ok_or(err(BOUNDS))?;
        let graph = ad(Graph::decode(bytes, a.len()))?;
        need(
            graph.sequence == buyer_sequence
                && graph.deadline <= deadline
                && a[graph.assets[0].token as usize].key == a[buyer_source].key
                && a[graph.assets[0].mint as usize].key == a[cash_mint].key
                && a[graph.assets[0].program as usize].key == a[cash_program].key,
            IDENTITY,
        )?;
        if global_reflow {
            need(
                product_index == usize::from(u8::MAX)
                    && global_residual.is_none()
                    && bytes[0] == 4
                    && graph.reflow_calls > 0
                    && graph.asset_count <= product_count + 1
                    && external_legs < 4
                    && graph.legs[..graph.leg_count].iter().flatten().all(|leg| {
                        leg.destination > 0
                            && (leg.source == 0
                                || graph.legs[..graph.leg_count]
                                    .iter()
                                    .flatten()
                                    .any(|head| head.source == 0 && head.destination == leg.source))
                    }),
                BOUNDS,
            )?;
            for asset_index in 1..graph.asset_count {
                let asset = graph.assets[asset_index];
                let mut matched = None;
                for (position, product) in products[..product_count].iter().enumerate() {
                    if a[asset.token as usize].key == a[product.destination].key
                        && a[asset.mint as usize].key == a[product.mint].key
                        && a[asset.program as usize].key == a[product.token_program].key
                    {
                        need(matched.is_none() && !global_products[position], IDENTITY)?;
                        matched = Some(position);
                    }
                }
                let position = matched.ok_or(err(IDENTITY))?;
                need(
                    graph.legs[..graph.leg_count].iter().flatten().any(|leg| {
                        leg.destination == asset_index
                            && (leg.source == 0
                                || graph.legs[..graph.leg_count]
                                    .iter()
                                    .flatten()
                                    .any(|head| head.source == 0 && head.destination == leg.source))
                    }),
                    CONSERVATION,
                )?;
                global_products[position] = true;
            }
            residual_input = graph.input;
            global_residual = Some(bytes);
        } else {
            need(
                product_index < product_count && residuals[product_index].is_none(),
                BOUNDS,
            )?;
            let product = products[product_index];
            let sink = graph.assets[graph.asset_count - 1];
            need(
                a[sink.token as usize].key == a[product.destination].key
                    && a[sink.mint as usize].key == a[product.mint].key
                    && a[sink.program as usize].key == a[product.token_program].key,
                IDENTITY,
            )?;
            residual_input = residual_input.checked_add(graph.input).ok_or(err(BOUNDS))?;
            external_legs = external_legs
                .checked_add(if graph.reflow_calls > 0 {
                    4
                } else {
                    graph.leg_count
                })
                .ok_or(err(BOUNDS))?;
            need(external_legs <= 4, BOUNDS)?;
            residuals[product_index] = Some(bytes);
        }
        if let Some(funding) = funding {
            funding.separate(a, &graph, buyer, buyer_source)?;
        }
        for seller in &sellers[..seller_count] {
            for index in [
                seller.owner,
                seller.nonce,
                seller.source,
                seller.cash_destination,
            ] {
                need(!references(&graph, index), IDENTITY)?;
            }
        }
    }
    need(
        cursor == d.len() && internal_cash.checked_add(residual_input) == Some(buyer_input),
        CONSERVATION,
    )?;
    #[cfg(feature = "profile-cu")]
    solana_program::msg!("PROFILE mesh_residuals");
    for position in 0..product_count {
        need(
            products[position].internal_raw > 0
                || residuals[position].is_some()
                || global_products[position],
            CONSERVATION,
        )?;
        let Some(bytes) = residuals[position] else {
            continue;
        };
        let graph = ad(Graph::decode(bytes, a.len()))?;
        for prior_bytes in residuals[..position].iter().flatten() {
            let prior = ad(Graph::decode(prior_bytes, a.len()))?;
            for (index, account) in a.iter().enumerate() {
                if account.is_writable
                    && index != buyer_source
                    && references(&prior, index)
                    && references(&graph, index)
                {
                    return Err(err(IDENTITY));
                }
            }
        }
    }

    let mut aggregate_asset_tokens =
        [u8::MAX; MAX_PRODUCTS * dex::graph::MAX_ASSETS + MAX_PRODUCTS + dex::graph::MAX_ASSETS];
    let mut aggregate_asset_count = 0usize;
    for product in &products[..product_count] {
        aggregate_asset_tokens[aggregate_asset_count] = product.destination as u8;
        aggregate_asset_count += 1;
    }
    for bytes in residuals[..product_count].iter().flatten() {
        let graph = ad(Graph::decode(bytes, a.len()))?;
        for asset in &graph.assets[..graph.asset_count] {
            if !aggregate_asset_tokens[..aggregate_asset_count].contains(&asset.token) {
                aggregate_asset_tokens[aggregate_asset_count] = asset.token;
                aggregate_asset_count += 1;
            }
        }
    }
    if let Some(bytes) = global_residual {
        let graph = ad(Graph::decode(bytes, a.len()))?;
        for asset in &graph.assets[..graph.asset_count] {
            if !aggregate_asset_tokens[..aggregate_asset_count].contains(&asset.token) {
                aggregate_asset_tokens[aggregate_asset_count] = asset.token;
                aggregate_asset_count += 1;
            }
        }
    }
    if let (Some(funding), Some(state)) = (funding, funding_state) {
        if funding.global_reflow {
            need(global_residual.is_some(), BOUNDS)?;
        } else {
            need(funding.surplus_product < product_count, BOUNDS)?;
            let bytes = residuals[funding.surplus_product].ok_or(err(BOUNDS))?;
            let graph = ad(Graph::decode(bytes, a.len()))?;
            need(
                bytes[0] == 2
                    && graph.legs[..graph.leg_count]
                        .iter()
                        .flatten()
                        .rfind(|leg| leg.source == 0)
                        .is_some_and(|leg| leg.budget == u64::MAX),
                BOUNDS,
            )?;
        }
        for token in &state.tokens[..state.token_count] {
            if !aggregate_asset_tokens[..aggregate_asset_count].contains(token) {
                aggregate_asset_tokens[aggregate_asset_count] = *token;
                aggregate_asset_count += 1;
            }
        }
        graph::preflight_graph_for_allowing(
            program,
            a,
            funding.bytes,
            buyer,
            buyer_nonce,
            &aggregate_asset_tokens[..aggregate_asset_count],
        )?;
    }
    for bytes in residuals[..product_count].iter().flatten() {
        graph::preflight_graph_for_pending_input(
            program,
            a,
            bytes,
            buyer,
            buyer_nonce,
            &aggregate_asset_tokens[..aggregate_asset_count],
            if funding.is_some() { buyer_input } else { 0 },
        )?;
    }
    if let Some(bytes) = global_residual {
        graph::preflight_graph_for_pending_input(
            program,
            a,
            bytes,
            buyer,
            buyer_nonce,
            &aggregate_asset_tokens[..aggregate_asset_count],
            buyer_input,
        )?;
    }
    #[cfg(feature = "profile-cu")]
    solana_program::msg!("PROFILE mesh_preflight");

    // Every signer, policy, graph and account partition is fixed. Any transfer
    // or DEX failure from here aborts the enclosing Solana instruction.
    let mut funded_cash = 0u64;
    let mut surplus_graph = Vec::new();
    let mut observed_global_graph = Vec::new();
    if let Some(funding) = funding {
        let outcome = graph::execute_preflighted_graph_for(a, funding.bytes, buyer)?;
        funded_cash = outcome.actual_out;
        need(
            outcome.sequence == buyer_sequence
                && funded_cash >= buyer_input
                && funded_cash <= dex::MAX_INPUT,
            CONSERVATION,
        )?;
        if funding.global_reflow {
            let external_cash = funded_cash
                .checked_sub(internal_cash)
                .ok_or(err(CONSERVATION))?;
            need(external_cash > 0, CONSERVATION)?;
            observed_global_graph.extend_from_slice(global_residual.ok_or(err(BOUNDS))?);
            observed_global_graph[12..20].copy_from_slice(&external_cash.to_le_bytes());
            graph::preflight_graph_for_allowing(
                program,
                a,
                &observed_global_graph,
                buyer,
                buyer_nonce,
                &aggregate_asset_tokens[..aggregate_asset_count],
            )?;
            global_residual = Some(&observed_global_graph);
        } else {
            let surplus = funded_cash
                .checked_sub(buyer_input)
                .ok_or(err(CONSERVATION))?;
            let bytes = residuals[funding.surplus_product].ok_or(err(BOUNDS))?;
            let previous = ad(dex::u64_at(bytes, 12))?;
            let increased = previous.checked_add(surplus).ok_or(err(BOUNDS))?;
            surplus_graph.extend_from_slice(bytes);
            surplus_graph[12..20].copy_from_slice(&increased.to_le_bytes());
            graph::preflight_graph_for_allowing(
                program,
                a,
                &surplus_graph,
                buyer,
                buyer_nonce,
                &aggregate_asset_tokens[..aggregate_asset_count],
            )?;
            residuals[funding.surplus_product] = Some(&surplus_graph);
        }
        buyer_input = funded_cash;
    }
    for seller in &sellers[..seller_count] {
        let product = products[seller.product];
        transfer_checked(
            a,
            buyer_source,
            cash_mint,
            seller.cash_destination,
            buyer,
            cash_program,
            seller.cash_output,
        )?;
        if seller.resting {
            order::transfer_fill(
                program,
                a,
                seller.source,
                product.mint,
                product.destination,
                seller.nonce,
                product.token_program,
                seller.owner,
                seller.order_nonce,
                seller.stock_input,
            )?;
        } else {
            transfer_checked(
                a,
                seller.source,
                product.mint,
                product.destination,
                seller.owner,
                product.token_program,
                seller.stock_input,
            )?;
        }
    }
    for bytes in residuals[..product_count].iter().flatten() {
        let outcome = graph::execute_preflighted_graph_for(a, bytes, buyer)?;
        need(outcome.sequence == buyer_sequence, REPLAY)?;
    }
    if let Some(bytes) = global_residual {
        let residual_graph = ad(Graph::decode(bytes, a.len()))?;
        let mut converters = [None; dex::graph::MAX_ASSETS];
        for (asset_index, converter) in converters
            .iter_mut()
            .enumerate()
            .take(residual_graph.asset_count)
            .skip(1)
        {
            let asset = residual_graph.assets[asset_index];
            let product = products[..product_count]
                .iter()
                .find(|product| {
                    a[asset.token as usize].key == a[product.destination].key
                        && a[asset.mint as usize].key == a[product.mint].key
                        && a[asset.program as usize].key == a[product.token_program].key
                })
                .ok_or(err(IDENTITY))?;
            *converter = Some(exposure::Converter::decode(
                a,
                product.mint,
                product.model,
                product.conservative_bps,
                product.numerator,
                product.denominator,
                clock.unix_timestamp,
            )?);
        }
        #[cfg(feature = "profile-cu")]
        solana_program::msg!("PROFILE mesh_converters");
        let outcome = graph::execute_preflighted_economic_reflow_for(
            a,
            bytes,
            buyer,
            &converters,
            4usize.checked_sub(external_legs).ok_or(err(BOUNDS))?,
        )?;
        #[cfg(feature = "profile-cu")]
        solana_program::msg!("PROFILE mesh_economic_executed");
        need(
            outcome.sequence == buyer_sequence
                && outcome.actual_in
                    == buyer_input
                        .checked_sub(internal_cash)
                        .ok_or(err(CONSERVATION))?
                && outcome.exposure_q32 > 0
                && outcome.executed > 0,
            CONSERVATION,
        )?;
    }

    let after_buyer_cash = graph::checked_asset(
        &a[buyer],
        &a[buyer_source],
        &a[cash_mint],
        &a[cash_program],
        false,
    )?;
    need(
        before_buyer_cash
            .checked_add(funded_cash)
            .and_then(|cash| cash.checked_sub(after_buyer_cash))
            == Some(buyer_input),
        CONSERVATION,
    )?;
    let authorized_input = if let Some(state) = funding_state {
        let after = graph::checked_asset(
            &a[buyer],
            &a[state.source.token as usize],
            &a[state.source.mint as usize],
            &a[state.source.program as usize],
            false,
        )?;
        need(
            state.before_input.checked_sub(after) == Some(state.input_atoms),
            CONSERVATION,
        )?;
        state.input_atoms
    } else {
        buyer_input
    };
    let mut product_raw = [0u64; MAX_PRODUCTS];
    let mut product_exposure = [0u64; MAX_PRODUCTS];
    let mut total_exposure = 0u64;
    for position in 0..product_count {
        let product = products[position];
        let after = graph::checked_asset(
            &a[buyer],
            &a[product.destination],
            &a[product.mint],
            &a[product.token_program],
            false,
        )?;
        product_raw[position] = after
            .checked_sub(product.before_output)
            .ok_or(err(CONSERVATION))?;
        need(product_raw[position] >= product.internal_raw, CONSERVATION)?;
        product_exposure[position] = if product_raw[position] == 0 {
            0
        } else {
            exposure::convert(
                a,
                product.mint,
                product.model,
                product.conservative_bps,
                product.numerator,
                product.denominator,
                product_raw[position],
                clock.unix_timestamp,
            )?
        };
        total_exposure = total_exposure
            .checked_add(product_exposure[position])
            .ok_or(err(BOUNDS))?;
    }
    need(total_exposure >= minimum_exposure, MIN_OUT)?;
    for seller in &sellers[..seller_count] {
        let product = products[seller.product];
        let stock_authority = if seller.resting {
            seller.nonce
        } else {
            seller.owner
        };
        let stock = graph::checked_asset(
            &a[stock_authority],
            &a[seller.source],
            &a[product.mint],
            &a[product.token_program],
            false,
        )?;
        let cash = graph::checked_asset(
            &a[seller.owner],
            &a[seller.cash_destination],
            &a[cash_mint],
            &a[cash_program],
            false,
        )?;
        need(
            seller.before_stock.checked_sub(stock) == Some(seller.stock_input)
                && cash.checked_sub(seller.before_cash) == Some(seller.cash_output)
                && seller.cash_output >= seller.minimum_cash_output,
            CONSERVATION,
        )?;
    }

    {
        let mut nonce = a[buyer_nonce].try_borrow_mut_data()?;
        nonce[40..48].copy_from_slice(&(buyer_sequence + 1).to_le_bytes());
        nonce[48..56].copy_from_slice(&authorized_input.to_le_bytes());
        nonce[56..64].copy_from_slice(&total_exposure.to_le_bytes());
    }
    for seller in &sellers[..seller_count] {
        if seller.resting {
            order::record_fill(
                a,
                seller.nonce,
                seller.sequence,
                seller.stock_input,
                seller.cash_output,
            )?;
        } else {
            let mut nonce = a[seller.nonce].try_borrow_mut_data()?;
            nonce[40..48].copy_from_slice(&(seller.sequence + 1).to_le_bytes());
            nonce[48..56].copy_from_slice(&seller.stock_input.to_le_bytes());
            nonce[56..64].copy_from_slice(&seller.cash_output.to_le_bytes());
        }
    }
    let mut receipt = [0u8; 56 + MAX_PRODUCTS * 16 + MAX_SELLERS * 24];
    receipt[..8].copy_from_slice(if global_reflow && funding.is_some() {
        b"SKEWMSF2"
    } else if direct_global_reflow {
        b"SKEWMSR1"
    } else if funding.is_some() {
        b"SKEWMSF1"
    } else {
        b"SKEWMSH1"
    });
    for (chunk, value) in receipt[8..48].chunks_exact_mut(8).zip([
        buyer_sequence,
        authorized_input,
        total_exposure,
        product_count as u64,
        seller_count as u64,
    ]) {
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    let mut receipt_len = 48;
    if funding.is_some() {
        receipt[48..56].copy_from_slice(&funded_cash.to_le_bytes());
        receipt_len = 56;
    }
    for position in 0..product_count {
        receipt[receipt_len..receipt_len + 8].copy_from_slice(&product_raw[position].to_le_bytes());
        receipt[receipt_len + 8..receipt_len + 16]
            .copy_from_slice(&product_exposure[position].to_le_bytes());
        receipt_len += 16;
    }
    for seller in &sellers[..seller_count] {
        for (chunk, value) in receipt[receipt_len..receipt_len + 24]
            .chunks_exact_mut(8)
            .zip([seller.sequence, seller.stock_input, seller.cash_output])
        {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        receipt_len += 24;
    }
    set_return_data(&receipt[..receipt_len]);
    Ok(())
}
