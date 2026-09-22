//! Native allocation -> pool-derived CPI accounts. Discovery is not admission:
//! fetch the returned dependencies together with wallet/policy/nonce accounts in
//! ONE final bank, then lower, compile and simulate the exact wallet message.
//! No aggregator transaction, floating-point split or supplied privileges enter.
#[cfg(test)]
#[path = "native_wire/captured_inventory_tests.rs"]
pub(crate) mod captured_inventory_tests;
use crate::{
    feed::{Account, Snapshot},
    market::{MarketConfig, Venue},
    swap_wire::{Budget, SwapGraph, SwapLeg, TokenAsset},
    world::NativeSwapProposal,
    Result,
};
use solana_pubkey::{pubkey, Pubkey};
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
};
use stocklana_adapters::graph::Venue as Abi;

const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN22: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const MEMO: Pubkey = pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
const ATA: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// Canonical wallet destination; creation/initialization is a separate, checked
/// envelope operation. Merely deriving this address does not prove it exists.
pub fn wallet_asset(owner: Pubkey, mint: Pubkey, bank: &Snapshot) -> Result<TokenAsset> {
    require(owner != Pubkey::default(), "native graph wallet")?;
    let token_program = pk(&account(bank, &mint)?.owner)?;
    require(
        [TOKEN, TOKEN22].contains(&token_program),
        "native graph mint owner",
    )?;
    Ok(TokenAsset {
        mint,
        token_program,
        token: Pubkey::find_program_address(
            &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
            &ATA,
        )
        .0,
    })
}

/// Lower a single sink (funding or one issuer product) as one atomic DAG. Each
/// intermediate consumer is funded by its producers, never the wallet's old
/// intermediate balance. A multi-issuer parent composes these typed graphs with
/// the product-aware clearing envelope and its aggregate exposure postcondition.
pub fn lower_native_graph(
    mut graph: SwapGraph,
    proposals: &[NativeSwapProposal],
    bank: &Snapshot,
) -> Result<SwapGraph> {
    require(
        graph.legs.is_empty() && (1..=4).contains(&proposals.len()),
        "native graph leg bound",
    )?;
    let mut assets = vec![graph.input];
    assets.extend_from_slice(&graph.intermediates);
    assets.push(graph.output);
    require(
        assets.len() <= stocklana_adapters::graph::MAX_ASSETS,
        "native graph asset bound",
    )?;
    let mut positions = BTreeMap::new();
    for (index, asset) in assets.iter().enumerate() {
        require(
            positions.insert(asset.mint, index).is_none(),
            "native graph duplicate mint",
        )?;
    }
    let mut produced = vec![0u64; assets.len()];
    let mut spent = vec![0u64; assets.len()];
    let mut pools = BTreeSet::new();
    let mut edges = Vec::with_capacity(proposals.len());
    for proposal in proposals {
        let source = *positions
            .get(&pk(&proposal.market.input_mint)?)
            .ok_or("native graph source asset")?;
        let destination = *positions
            .get(&pk(&proposal.market.output_mint)?)
            .ok_or("native graph destination asset")?;
        require(
            source < destination && pools.insert(&proposal.market.pool),
            "native graph topology or repeated pool",
        )?;
        // Stage labels describe economic funding/product groups. Actual token
        // dependencies come from the mint DAG, including interleaved branches.
        require(
            (1..=2).contains(&proposal.stage),
            "native graph stage binding",
        )?;
        spent[source] = spent[source]
            .checked_add(proposal.input_atoms)
            .ok_or("native graph input overflow")?;
        require(
            spent[source]
                <= if source == 0 {
                    graph.input_atoms
                } else {
                    produced[source]
                },
            "native graph consumer before producer",
        )?;
        produced[destination] = produced[destination]
            .checked_add(proposal.expected_output_atoms)
            .ok_or("native graph output overflow")?;
        edges.push((source, destination));
    }
    require(
        spent[0] == graph.input_atoms
            && produced[0] == 0
            && *spent.last().ok_or("native graph empty")? == 0
            && *produced.last().ok_or("native graph empty")? >= graph.minimum_output_atoms,
        "native graph endpoint conservation",
    )?;
    for index in 1..assets.len() - 1 {
        require(
            produced[index] > 0 && produced[index] == spent[index],
            "native graph intermediate conservation",
        )?;
    }
    for (index, (proposal, (source, destination))) in proposals.iter().zip(&edges).enumerate() {
        let budget = if edges[index + 1..].iter().any(|(s, _)| s == source) {
            Budget::Exact(proposal.input_atoms)
        } else {
            Budget::Remaining
        };
        graph.legs.push(lower_native_leg_impl(
            proposal,
            bank,
            graph.owner,
            assets[*source],
            assets[*destination],
            budget,
            true,
        )?);
    }
    Ok(graph)
}

/// Lower direct cash->issuer candidates plus bounded issuer-conversion paths
/// without freezing the optimizer's discovery-time split. A noncash source
/// must itself have a direct cash producer, which admits cash->SPYx->SPYon but
/// rejects deeper or cyclic routes. Opcode 18 v2 chooses integer path inputs
/// from the observed post-funding cash.
pub fn lower_economic_reflow_graph(
    mut graph: SwapGraph,
    proposals: &[NativeSwapProposal],
    bank: &Snapshot,
) -> Result<SwapGraph> {
    require(
        graph.legs.is_empty()
            && (1..=stocklana_adapters::graph::MAX_CANDIDATES).contains(&proposals.len()),
        "economic native candidate bound",
    )?;
    // Opcode 14 carries stage-one product candidates; opcode 18 passes its
    // stage-two residual after funding. Both lower to the same economic graph.
    // Do not confuse route-world depth with proposal stage or accept a mixture.
    let product_stage = proposals[0].stage;
    require(matches!(product_stage, 1 | 2)
        && proposals.iter().all(|proposal| proposal.stage == product_stage),
        "economic native product stage")?;
    let mut assets = vec![graph.input];
    assets.extend_from_slice(&graph.intermediates);
    assets.push(graph.output);
    require(
        assets.len() <= stocklana_adapters::graph::MAX_ASSETS,
        "economic native asset bound",
    )?;
    let positions = assets
        .iter()
        .enumerate()
        .map(|(index, asset)| (asset.mint, index))
        .collect::<BTreeMap<_, _>>();
    require(
        positions.len() == assets.len(),
        "economic native duplicate mint",
    )?;
    let mut pools = BTreeSet::new();
    let mut produced = BTreeSet::new();
    let mut direct = BTreeSet::new();
    let mut topology = Vec::with_capacity(proposals.len());
    for proposal in proposals {
        let source = positions
            .get(&pk(&proposal.market.input_mint)?)
            .copied()
            .ok_or("economic native source asset")?;
        let destination = positions
            .get(&pk(&proposal.market.output_mint)?)
            .copied()
            .ok_or("economic native destination asset")?;
        require(
            proposal.stage == product_stage
                && proposal.product_id.is_some()
                && source != destination
                && destination > 0
                && pools.insert(proposal.market.pool.as_str()),
            "economic native proposal binding",
        )?;
        if source == 0 {
            direct.insert(destination);
        }
        produced.insert(destination);
        topology.push((source, destination));
        graph.legs.push(lower_native_leg(
            proposal,
            bank,
            graph.owner,
            assets[source],
            assets[destination],
            Budget::Remaining,
        )?);
    }
    require(
        (1..assets.len()).all(|index| produced.contains(&index))
            && topology
                .iter()
                .all(|(source, _)| *source == 0 || direct.contains(source)),
        "economic native missing product candidate",
    )?;
    Ok(graph)
}

fn require(ok: bool, error: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(error.into())
    }
}
fn pk(key: &str) -> Result<Pubkey> {
    Pubkey::from_str(key).map_err(|_| "native wire public key".into())
}
fn bytes<const N: usize>(data: &[u8], offset: usize) -> Result<[u8; N]> {
    data.get(offset..offset.checked_add(N).ok_or("native wire offset")?)
        .ok_or("native wire account layout")?
        .try_into()
        .map_err(|_| "native wire bytes".into())
}
fn key(data: &[u8], offset: usize) -> Result<Pubkey> {
    Ok(Pubkey::new_from_array(bytes(data, offset)?))
}
fn account<'a>(bank: &'a Snapshot, address: &Pubkey) -> Result<&'a Account> {
    let name = address.to_string();
    let mut rows = bank.accounts.iter().filter(|a| a.key == name);
    let row = rows
        .next()
        .ok_or_else(|| format!("native wire bank missing {address}"))?;
    require(rows.next().is_none(), "native wire ambiguous bank account")?;
    Ok(row)
}
fn owned<'a>(bank: &'a Snapshot, address: &Pubkey, owner: Pubkey) -> Result<&'a [u8]> {
    let row = account(bank, address)?;
    require(
        pk(&row.owner)? == owner && !row.executable,
        "native wire account owner",
    )?;
    Ok(&row.data)
}

struct Recipe {
    abi: Abi,
    program: Pubkey,
    pool: Pubkey,
    config: Option<Pubkey>,
    mints: [Pubkey; 2],
    programs: [Pubkey; 2],
    vaults: [Pubkey; 2],
    oracle: Pubkey,
    arrays: Vec<Pubkey>,
    extension: Option<Pubkey>,
    direction: bool,
}

/// Accounts needed in the final execution bank, derived from a discovery bank.
/// The caller must refetch these AND all market/wallet/policy dependencies in a
/// single response; discovery bytes are never copied into the final bank.
pub fn execution_dependencies(config: &MarketConfig, discovery: &Snapshot) -> Result<Vec<Pubkey>> {
    let r = recipe(config, discovery)?;
    let mut keys: BTreeSet<_> = config.keys().iter().map(|s| pk(s)).collect::<Result<_>>()?;
    keys.extend([r.program, r.pool, r.oracle, MEMO]);
    keys.extend(r.config);
    keys.extend(r.mints);
    keys.extend(r.programs);
    keys.extend(r.vaults);
    keys.extend(r.arrays);
    keys.extend(r.extension);
    match r.abi {
        Abi::RaydiumClmm | Abi::ByrealClmm => keys.extend([TOKEN, TOKEN22]),
        Abi::MeteoraDlmm => {
            keys.insert(Pubkey::find_program_address(&[b"__event_authority"], &r.program).0);
        }
        _ => {}
    }
    Ok(keys.into_iter().collect())
}

/// Preserve the solver's exact integer allocation and re-quote against the same
/// complete execution bank used for account derivation. Economic eligibility,
/// bank currentness, signatures and final resource admission are separate gates.
/// `Remaining` is valid only when the enclosing compiler admits a last consumer.
pub fn lower_native_leg(
    proposal: &NativeSwapProposal,
    bank: &Snapshot,
    owner: Pubkey,
    source: TokenAsset,
    destination: TokenAsset,
    budget: Budget,
) -> Result<SwapLeg> {
    lower_native_leg_impl(proposal,bank,owner,source,destination,budget,false)
}

// Only fixed-amount flow graphs may trim a quoted horizon. Economic Reflow
// candidates keep their full capacity: their allocation changes on-chain.
fn lower_native_leg_impl(
    proposal:&NativeSwapProposal, bank:&Snapshot, owner:Pubkey,
    source:TokenAsset, destination:TokenAsset, budget:Budget, compact:bool,
) -> Result<SwapLeg> {
    let c = &proposal.market;
    require(
        owner != Pubkey::default() && source.token != destination.token,
        "native wire wallet identity",
    )?;
    require(
        source.mint == pk(&c.input_mint)? && destination.mint == pk(&c.output_mint)?,
        "native wire proposal asset binding",
    )?;
    if let Budget::Exact(value) = budget {
        require(
            value == proposal.input_atoms,
            "native wire integer allocation changed",
        )?;
    }
    require(
        proposal.input_atoms > 0 && proposal.expected_output_atoms > 0,
        "native wire empty allocation",
    )?;
    let quote = |config: &MarketConfig| {
        config
            .compile_exact_pair(bank, &c.input_mint, &c.output_mint)?
            .quote(proposal.input_atoms)
    };
    require(
        quote(c)? == proposal.expected_output_atoms,
        "native wire stale native quote",
    )?;
    let mut r = recipe(c, bank)?;
    for asset in [source, destination] {
        let mint = account(bank, &asset.mint)?;
        let token = owned(bank, &asset.token, asset.token_program)?;
        require(
            pk(&mint.owner)? == asset.token_program
                && key(token, 0)? == asset.mint
                && key(token, 32)? == owner,
            "native wire wallet token binding",
        )?;
    }
    // Orca's bounded ABI has exactly three tick-array slots. Do not quote more
    // liquidity than can be lowered into this transaction's declared horizon.
    let mut selected = c.clone();
    selected.tick_arrays = r.arrays.iter().map(ToString::to_string).collect();
    require(
        quote(&selected)? == proposal.expected_output_atoms,
        "native wire execution horizon differs from quote",
    )?;
    if compact {
        let count=exact_prefix_len(r.arrays.len(),proposal.expected_output_atoms,|count| {
            let mut prefix=selected.clone();prefix.tick_arrays.truncate(count);quote(&prefix)
        })?;
        r.arrays.truncate(count);
    }
    for i in 0..2 {
        let vault = owned(bank, &r.vaults[i], r.programs[i])?;
        require(
            key(vault, 0)? == r.mints[i] && key(vault, 32)? == r.pool,
            "native wire pool vault binding",
        )?;
    }
    let oracle = account(bank, &r.oracle)?;
    // The native market compiler above already binds the PDA and permits this
    // empty state only for a fixed-fee Whirlpool, never an adaptive oracle.
    if !(r.abi == Abi::OrcaWhirlpool
        && oracle.owner == Pubkey::default().to_string()
        && oracle.data.is_empty()
        && !oracle.executable)
    {
        owned(bank, &r.oracle, r.program)?;
    }
    if let Some(extension) = r.extension {
        let data = owned(bank, &extension, r.program)?;
        require(key(data, 8)? == r.pool, "native wire bitmap pool binding")?;
    }
    let (a, b) = if r.direction {
        (source.token, destination.token)
    } else {
        (destination.token, source.token)
    };
    let accounts = match r.abi {
        Abi::RaydiumClmm | Abi::ByrealClmm => {
            let vaults = if r.direction {
                r.vaults
            } else {
                [r.vaults[1], r.vaults[0]]
            };
            let mut accounts = vec![
                owner,
                r.config.ok_or("native CLMM config")?,
                r.pool,
                source.token,
                destination.token,
                vaults[0],
                vaults[1],
                r.oracle,
                TOKEN,
                TOKEN22,
                MEMO,
                source.mint,
                destination.mint,
            ];
            accounts.extend_from_slice(&r.arrays);
            accounts.extend(r.extension);
            accounts
        }
        Abi::OrcaWhirlpool => {
            let mut arrays = r.arrays.clone();
            let last = *arrays.last().ok_or("native Whirlpool empty horizon")?;
            arrays.resize(3, last);
            vec![
                r.programs[0],
                r.programs[1],
                MEMO,
                owner,
                r.pool,
                r.mints[0],
                r.mints[1],
                a,
                r.vaults[0],
                b,
                r.vaults[1],
                arrays[0],
                arrays[1],
                arrays[2],
                r.oracle,
            ]
        }
        Abi::MeteoraDlmm => {
            let event = Pubkey::find_program_address(&[b"__event_authority"], &r.program).0;
            // Anchor optional accounts use the callee's executable program ID.
            let mut accounts = vec![
                r.pool,
                r.extension.unwrap_or(r.program),
                r.vaults[0],
                r.vaults[1],
                source.token,
                destination.token,
                r.mints[0],
                r.mints[1],
                r.oracle,
                r.program,
                owner,
                r.programs[0],
                r.programs[1],
                MEMO,
                event,
                r.program,
            ];
            accounts.extend_from_slice(&r.arrays);
            accounts
        }
        _ => return Err("native wire venue not implemented".into()),
    };
    let (low, high) = r.abi.account_bounds();
    require(
        (low..=high).contains(&accounts.len()),
        "native wire CPI account bound",
    )?;
    for address in &accounts {
        account(bank, address)?;
    }
    Ok(SwapLeg {
        venue: r.abi,
        direction: r.direction,
        source: source.token,
        destination: destination.token,
        budget,
        accounts,
    })
}

fn exact_prefix_len(count:usize,expected:u64,mut quote:impl FnMut(usize)->Result<u64>)->Result<usize> {
    require((1..=8).contains(&count) && expected>0,"native prefix bounds")?;
    for n in 1..count {
        if quote(n).is_ok_and(|amount|amount==expected) {return Ok(n);}
    }
    // The full horizon was already re-quoted against the exact execution bank.
    Ok(count)
}

#[cfg(test)]
mod prefix_tests {
    use super::*;
    #[test]
    fn compact_horizon_requires_exact_integer_quote_not_partial_fill() {
        let mut calls=Vec::new();
        assert_eq!(exact_prefix_len(4,100,|n|{calls.push(n);match n{1=>Err("capacity".into()),2=>Ok(99),_=>Ok(100)}}).unwrap(),3);
        assert_eq!(calls,vec![1,2,3]);
        assert_eq!(exact_prefix_len(3,100,|_|Ok(101)).unwrap(),3);
        assert_eq!(exact_prefix_len(1,100,|_|panic!("full horizon is already checked")).unwrap(),1);
        assert!(exact_prefix_len(9,100,|_|Ok(100)).is_err());
        assert!(exact_prefix_len(3,0,|_|Ok(0)).is_err());
    }
}

fn recipe(c: &MarketConfig, bank: &Snapshot) -> Result<Recipe> {
    require(
        c.program == c.venue.program() && !c.tick_arrays.is_empty() && c.tick_arrays.len() <= 8,
        "native wire market admission",
    )?;
    let program = pk(&c.program)?;
    let pool = pk(&c.pool)?;
    let data = owned(bank, &pool, program)?;
    let (abi, offsets, vault_offsets, oracle, config, span, current) = match c.venue {
        Venue::RaydiumClmm | Venue::ByrealClmm => {
            require(data.len() == 1544, "native CLMM pool layout")?;
            let config = pk(&c.config)?;
            require(key(data, 9)? == config, "native CLMM config binding")?;
            (
                if c.venue == Venue::RaydiumClmm {
                    Abi::RaydiumClmm
                } else {
                    Abi::ByrealClmm
                },
                [73, 105],
                [137, 169],
                key(data, 201)?,
                Some(config),
                60 * i64::from(u16::from_le_bytes(bytes(data, 235)?)),
                i64::from(i32::from_le_bytes(bytes(data, 269)?)),
            )
        }
        Venue::OrcaWhirlpool => {
            require(data.len() == 653, "native Whirlpool pool layout")?;
            let oracle = Pubkey::find_program_address(&[b"oracle", pool.as_ref()], &program).0;
            require(pk(&c.config)? == oracle, "native Whirlpool oracle PDA")?;
            (
                Abi::OrcaWhirlpool,
                [101, 181],
                [133, 213],
                oracle,
                None,
                88 * i64::from(u16::from_le_bytes(bytes(data, 41)?)),
                i64::from(i32::from_le_bytes(bytes(data, 81)?)),
            )
        }
        Venue::MeteoraDlmm => {
            require(
                data.len() == 904 && c.config.is_empty(),
                "native DLMM pool layout",
            )?;
            (
                Abi::MeteoraDlmm,
                [88, 120],
                [152, 184],
                key(data, 552)?,
                None,
                70,
                i64::from(i32::from_le_bytes(bytes(data, 76)?)),
            )
        }
    };
    require(span > 0, "native wire zero array span")?;
    let mints = [key(data, offsets[0])?, key(data, offsets[1])?];
    let input = pk(&c.input_mint)?;
    let output = pk(&c.output_mint)?;
    let direction = input == mints[0];
    require(
        input != output
            && (if direction {
                [input, output]
            } else {
                [output, input]
            }) == mints,
        "native wire market mint pair",
    )?;
    let programs = [
        pk(&account(bank, &mints[0])?.owner)?,
        pk(&account(bank, &mints[1])?.owner)?,
    ];
    require(
        programs.iter().all(|p| [TOKEN, TOKEN22].contains(p)),
        "native wire token program",
    )?;
    let vaults = [key(data, vault_offsets[0])?, key(data, vault_offsets[1])?];
    let mut arrays = Vec::new();
    let mut seen = BTreeSet::new();
    for name in &c.tick_arrays {
        let address = pk(name)?;
        require(seen.insert(address), "native wire duplicate array")?;
        let d = owned(bank, &address, program)?;
        let start = match abi {
            Abi::MeteoraDlmm => i64::from_le_bytes(bytes(d, 8)?)
                .checked_mul(70)
                .ok_or("native bin overflow")?,
            Abi::OrcaWhirlpool => i64::from(i32::from_le_bytes(bytes(d, 8)?)),
            _ => i64::from(i32::from_le_bytes(bytes(d, 40)?)),
        };
        require(start.rem_euclid(span) == 0, "native wire array alignment")?;
        let expected = match abi {
            Abi::MeteoraDlmm => {
                Pubkey::find_program_address(
                    &[b"bin_array", pool.as_ref(), &(start / 70).to_le_bytes()],
                    &program,
                )
                .0
            }
            Abi::OrcaWhirlpool => {
                Pubkey::find_program_address(
                    &[b"tick_array", pool.as_ref(), start.to_string().as_bytes()],
                    &program,
                )
                .0
            }
            _ => {
                Pubkey::find_program_address(
                    &[
                        b"tick_array",
                        pool.as_ref(),
                        &i32::try_from(start)
                            .map_err(|_| "native tick overflow")?
                            .to_be_bytes(),
                    ],
                    &program,
                )
                .0
            }
        };
        require(address == expected, "native wire array PDA")?;
        let active = current.div_euclid(span) * span;
        if (direction && start <= active) || (!direction && start >= active) {
            arrays.push((start, address));
        }
    }
    arrays.sort_unstable_by_key(|(start, _)| if direction { -*start } else { *start });
    if abi == Abi::OrcaWhirlpool {
        arrays.truncate(3);
    }
    require(
        !arrays.is_empty(),
        "native wire missing directional horizon",
    )?;
    let extension = if abi != Abi::OrcaWhirlpool
        && arrays
            .iter()
            .any(|(start, _)| !(-512..512).contains(&(start / span)))
    {
        let seed: &[u8] = if abi == Abi::MeteoraDlmm {
            b"bitmap"
        } else {
            b"pool_tick_array_bitmap_extension"
        };
        Some(Pubkey::find_program_address(&[seed, pool.as_ref()], &program).0)
    } else {
        None
    };
    Ok(Recipe {
        abi,
        program,
        pool,
        config,
        mints,
        programs,
        vaults,
        oracle,
        arrays: arrays.into_iter().map(|(_, key)| key).collect(),
        extension,
        direction,
    })
}
