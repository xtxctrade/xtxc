//! Pure, bounded CLMM/DLMM array-address derivation for trusted capture jobs.
//! It reads pool bytes from stdin and never contacts RPC, signs, or submits.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::json;
use skew_execution_host::market::{MarketConfig, select_contiguous_array_horizon};
use std::collections::BTreeSet;
use solana_pubkey::Pubkey;
use std::str::FromStr;
use std::io::{self, Read};

const MAX_INPUT: u64 = 1024 * 1024;
const MAX_MARKETS: usize = 32;
const MAX_POOL_BYTES: usize = 128 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    markets: Vec<InputMarket>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputMarket {
    market: MarketConfig,
    pool_data_base64: String,
    /// Addresses observed with the expected owner by the bounded capture job.
    /// Selection is a discovery hint, never execution admission.
    #[serde(default)]
    initialized_arrays: Option<Vec<String>>,
}

fn run() -> Result<(), String> {
    let mut input = Vec::new();
    io::stdin()
        .take(MAX_INPUT + 1)
        .read_to_end(&mut input)
        .map_err(|error| error.to_string())?;
    if input.is_empty() || input.len() as u64 > MAX_INPUT {
        return Err("array horizon input bound".into());
    }
    let request: Request = serde_json::from_slice(&input).map_err(|error| error.to_string())?;
    if request.markets.is_empty() || request.markets.len() > MAX_MARKETS {
        return Err("array horizon market bound".into());
    }
    let mut horizons = Vec::with_capacity(request.markets.len());
    for row in request.markets {
        let pool = STANDARD
            .decode(row.pool_data_base64)
            .map_err(|error| error.to_string())?;
        if pool.is_empty() || pool.len() > MAX_POOL_BYTES {
            return Err("array horizon pool byte bound".into());
        }
        let active = row.market.active_array_ordinal(&pool)?;
        let candidates = row.market.dynamic_array_candidates(&pool)?;
        if candidates.is_empty() || candidates.len() > 8 {
            return Err("array horizon candidate bound".into());
        }
        let selected = row.initialized_arrays.as_ref().map(|present| {
            if present.len() > 8 || present.iter().collect::<BTreeSet<_>>().len() != present.len()
                || present.iter().any(|key| !candidates.iter().any(|(_, candidate)| candidate == key))
            { return Err("array observation bounds".into()); }
            select_contiguous_array_horizon(&candidates, active,
                &present.iter().map(String::as_str).collect(),
                row.market.array_capacity.map_or(row.market.tick_arrays.len(), usize::from))
        }).transpose()?;
        let config = if row.market.venue == skew_execution_host::market::Venue::OrcaWhirlpool {
            Pubkey::find_program_address(&[b"oracle", Pubkey::from_str(&row.market.pool).map_err(|_| "pool key")?.as_ref()],
                &Pubkey::from_str(&row.market.program).map_err(|_| "program key")?).0.to_string()
        } else { row.market.config.clone() };
        horizons.push(json!({
            "pool": row.market.pool,
            "config": config,
            "activeOrdinal": active,
            "selectedArrays": selected,
            "candidates": candidates.into_iter().map(|(ordinal, address)| json!({
                "ordinal": ordinal,
                "address": address,
            })).collect::<Vec<_>>(),
        }));
    }
    println!(
        "{}",
        json!({
            "schema": "skew.stockmesh.array-horizon/v1",
            "horizons": horizons,
        })
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
