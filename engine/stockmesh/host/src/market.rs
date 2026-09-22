//! Stock-specific native curves. One immutable bank supplies all dependencies.
use crate::{feed::Snapshot, Result};
use serde::{Deserialize, Serialize};
use skew_native::{bytes, clmm::ClmmCurve, dlmm::DlmmCurve, u64_at, TransferFee};
use solana_pubkey::Pubkey;
use std::collections::BTreeSet;
use std::str::FromStr;

/// Select the initialized connected component containing the active price.
/// Discovery and runtime rotation must not select the nearest N arrays across
/// a missing interval: the native curve cannot quote through that hole.
pub fn select_contiguous_array_horizon(
    candidates: &[(i64, String)],
    active: i64,
    present: &BTreeSet<&str>,
    capacity: usize,
) -> Result<Vec<String>> {
    if candidates.is_empty() || candidates.len() > 8 || !(1..=8).contains(&capacity)
        || candidates.windows(2).any(|pair| pair[0].0.checked_add(1) != Some(pair[1].0))
        || candidates.iter().map(|(_, key)| key).collect::<BTreeSet<_>>().len() != candidates.len()
    {
        return Err("array selection bounds".into());
    }
    let center = candidates.iter().position(|(ordinal, _)| *ordinal == active)
        .ok_or("array selection active candidate")?;
    if !present.contains(candidates[center].1.as_str()) {
        return Err("active tick/bin array is not initialized".into());
    }
    let mut left = center;
    while left > 0 && present.contains(candidates[left - 1].1.as_str()) { left -= 1; }
    let mut right = center;
    while right + 1 < candidates.len() && present.contains(candidates[right + 1].1.as_str()) { right += 1; }
    let target = capacity.min(right - left + 1);
    let mut selected_left = center.saturating_sub(target / 2).max(left);
    let selected_right = (selected_left + target - 1).min(right);
    selected_left = selected_right + 1 - target;
    Ok(candidates[selected_left..=selected_right].iter().map(|(_, key)| key.clone()).collect())
}

pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
pub const STOCK_PRODUCTS: [(&str, &str); 17] = [
    ("NVDA", "Xsc9qvGR1efVDFGLrVsmkzv3qi45LTBjeUKSPmx9qEh"),
    ("TSLA", "XsDoVfqeBukxuZHWhdvWHBhgEHjGNst4MLodqsJHzoB"),
    ("SPY", "XsoCS1TfEyfFhfvj8EtZ528L3CaKBDBRqRapnBbDF2W"),
    ("QQQ", "Xs8S1uUs1zvS2p7iwtsG3b6fkhpvmwz4GYU3gWAmWHZ"),
    ("COIN", "Xs7ZdzSHLU9ftNJsii5fCeJhoRWSC32SQGzGQtePxNu"),
    ("AAPL", "XsbEhLAtcf6HdfpFZ5xEMdqW8nfAvcsP5bdudRLJzJp"),
    ("MSFT", "XspzcW1PRtgf6Wj92HCiZdjzKCyFekVD8P5Ueh3dRMX"),
    ("GOOGL", "XsCPL9dNWBMvFtTmwcCA5v3xWPSMEBCszbQdiLLq6aN"),
    ("AMZN", "Xs3eBt7uRfJX8QUs4suhyU8p2M6DoUDrJyWBa8LLZsg"),
    ("META", "Xsa62P5mvPszXL1krVUnU5ar38bBSVcWAB6fmPCo5Zu"),
    ("MSTR", "XsP7xzNPvEHS1m6qfanPUGjNmdnmsLKEoNAnHjdxxyZ"),
    ("NFLX", "XsEH7wWfJJu2ZT3UCFeVfALnVA6CP5ur7Ee11KmzVpL"),
    ("MU", "XsQLZycSZ7QnBBdBXQaTbQdiUcbRqjNJgyBGAMzhHav"),
    ("HOOD", "XsvNBAYkrDRNhA7wPHQfX3ZUXZyZLdnCQDfHZ56bzpg"),
    ("SPACEX", "PreANxuXjsy2pvisWWMNB6YaJNzr7681wJJr2rHsfTh"),
    ("OPENAI", "PreweJYECqtQwBtpxHL171nL2K6umo692gTm7Q3rpgF"),
    ("ANTHROPIC", "Pren1FvFX6J3E4kXhJuCiAD5aDmGEb7qJRncwA8Lkhw"),
];

pub fn admitted_instrument(mint: &str) -> Option<&'static str> {
    STOCK_PRODUCTS
        .iter()
        .find_map(|(instrument, product)| (*product == mint).then_some(*instrument))
}

pub fn admitted_pair(input_mint: &str, output_mint: &str) -> bool {
    ((input_mint == WSOL_MINT && output_mint == USDC_MINT)
        || (input_mint == USDC_MINT && output_mint == WSOL_MINT))
        || ([USDC_MINT, WSOL_MINT].contains(&input_mint)
            && admitted_instrument(output_mint).is_some())
        || (admitted_instrument(input_mint).is_some()
            && [USDC_MINT, WSOL_MINT].contains(&output_mint))
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    #[default]
    RaydiumClmm,
    ByrealClmm,
    MeteoraDlmm,
    OrcaWhirlpool,
}
impl Venue {
    pub fn program(self) -> &'static str {
        match self {
            Self::OrcaWhirlpool => "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
            Self::RaydiumClmm => "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK",
            Self::ByrealClmm => "REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2",
            Self::MeteoraDlmm => "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct MarketConfig {
    #[serde(default)]
    pub venue: Venue,
    pub program: String,
    pub pool: String,
    #[serde(default)]
    pub config: String,
    pub input_mint: String,
    pub output_mint: String,
    pub tick_arrays: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub array_capacity: Option<u8>,
    pub clock: String,
}
impl MarketConfig {
    /// Reverse only the typed mint direction. Pool identity, program identity,
    /// oracle and bounded tick/bin horizon remain the same coherent market.
    pub fn reversed(&self) -> Self {
        let mut reversed = self.clone();
        std::mem::swap(&mut reversed.input_mint, &mut reversed.output_mint);
        reversed
    }

    pub fn keys(&self) -> Vec<String> {
        let mut k = vec![self.pool.clone()];
        if !self.config.is_empty() {
            k.push(self.config.clone());
        }
        k.extend([
            self.input_mint.clone(),
            self.output_mint.clone(),
            self.clock.clone(),
        ]);
        k.extend(self.tick_arrays.clone());
        k
    }

    /// Derive the bounded account horizon around the pool's current price.
    /// These addresses are discovery candidates only. The caller must probe
    /// account existence and then fetch every selected dependency again in one
    /// final coherent RPC bank before compiling or quoting.
    pub fn dynamic_array_candidates(&self, pool: &[u8]) -> Result<Vec<(i64, String)>> {
        let pool_key = Pubkey::from_str(&self.pool).map_err(|error| error.to_string())?;
        let program = Pubkey::from_str(&self.program).map_err(|error| error.to_string())?;
        let (current, minimum, maximum, count) = match self.venue {
            Venue::RaydiumClmm | Venue::ByrealClmm => {
                let spacing = i64::from(u16::from_le_bytes(market_bytes(pool, 235)?));
                let current_tick = i64::from(i32::from_le_bytes(market_bytes(pool, 269)?));
                if spacing == 0 || !(-443_636..=443_636).contains(&current_tick) {
                    return Err("CLMM tick or spacing bounds".into());
                }
                let span = 60i64.checked_mul(spacing).ok_or("CLMM span")?;
                (
                    current_tick.div_euclid(span),
                    // Both venue contracts admit partially overlapping edge
                    // arrays. The lower start can be below the minimum tick.
                    i64::from(-443_636i32).div_euclid(span),
                    i64::from(443_636i32).div_euclid(span),
                    8usize,
                )
            }
            Venue::OrcaWhirlpool => {
                let spacing = i64::from(u16::from_le_bytes(market_bytes(pool, 41)?));
                let current_tick = i64::from(i32::from_le_bytes(market_bytes(pool, 81)?));
                if spacing == 0 || !(-443_636..=443_636).contains(&current_tick) {
                    return Err("Whirlpool tick or spacing bounds".into());
                }
                let span = 88i64.checked_mul(spacing).ok_or("Whirlpool span")?;
                (
                    current_tick.div_euclid(span),
                    i64::from(-443_636i32).div_euclid(span),
                    i64::from(443_636i32).div_euclid(span),
                    6usize,
                )
            }
            Venue::MeteoraDlmm => {
                let active = i64::from(i32::from_le_bytes(market_bytes(pool, 76)?));
                let minimum_bin = i64::from(i32::from_le_bytes(market_bytes(pool, 24)?));
                let maximum_bin = i64::from(i32::from_le_bytes(market_bytes(pool, 28)?));
                (
                    active.div_euclid(70),
                    minimum_bin.div_euclid(70),
                    maximum_bin.div_euclid(70),
                    6usize,
                )
            }
        };
        if current < minimum || current > maximum {
            return Err("active array outside pool bounds".into());
        }
        let before = i64::try_from(count / 2).map_err(|_| "array radius")?;
        let after = i64::try_from(count - count / 2 - 1).map_err(|_| "array radius")?;
        let low = current.saturating_sub(before).max(minimum);
        let high = current.saturating_add(after).min(maximum);
        let mut candidates = Vec::with_capacity(count);
        for ordinal in low..=high {
            let (address, _) = match self.venue {
                Venue::RaydiumClmm | Venue::ByrealClmm => {
                    let spacing = i64::from(u16::from_le_bytes(market_bytes(pool, 235)?));
                    let start = ordinal
                        .checked_mul(60)
                        .and_then(|value| value.checked_mul(spacing))
                        .and_then(|value| i32::try_from(value).ok())
                        .ok_or("CLMM array start")?;
                    Pubkey::find_program_address(
                        &[b"tick_array", pool_key.as_ref(), &start.to_be_bytes()],
                        &program,
                    )
                }
                Venue::OrcaWhirlpool => {
                    let spacing = i64::from(u16::from_le_bytes(market_bytes(pool, 41)?));
                    let start = ordinal
                        .checked_mul(88)
                        .and_then(|value| value.checked_mul(spacing))
                        .and_then(|value| i32::try_from(value).ok())
                        .ok_or("Whirlpool array start")?;
                    let start = start.to_string();
                    Pubkey::find_program_address(
                        &[b"tick_array", pool_key.as_ref(), start.as_bytes()],
                        &program,
                    )
                }
                Venue::MeteoraDlmm => Pubkey::find_program_address(
                    &[b"bin_array", pool_key.as_ref(), &ordinal.to_le_bytes()],
                    &program,
                ),
            };
            candidates.push((ordinal, address.to_string()));
        }
        if candidates.is_empty() || candidates.len() > count {
            return Err("dynamic array candidate bound".into());
        }
        Ok(candidates)
    }

    pub(crate) fn array_horizon_needs_rotation(&self, pool: &[u8]) -> Result<bool> {
        let candidates = self.dynamic_array_candidates(pool)?;
        let active = active_ordinal(self.venue, pool)?;
        let active_key = candidates
            .iter()
            .find_map(|(ordinal, key)| (*ordinal == active).then_some(key))
            .ok_or("missing active array candidate")?;
        Ok(!self.tick_arrays.contains(active_key))
    }

    pub fn active_array_ordinal(&self, pool: &[u8]) -> Result<i64> {
        active_ordinal(self.venue, pool)
    }
    pub fn compile(&self, s: &Snapshot) -> Result<Market> {
        if self.program != self.venue.program()
            || !admitted_pair(&self.input_mint, &self.output_mint)
        {
            return Err("outside admitted StockMesh input/product lane".into());
        }
        self.compile_exact_pair(s, &self.input_mint, &self.output_mint)
    }

    pub(crate) fn compile_exact_pair(
        &self,
        s: &Snapshot,
        admitted_input_mint: &str,
        admitted_output_mint: &str,
    ) -> Result<Market> {
        if self.program != self.venue.program()
            || self.input_mint != admitted_input_mint
            || self.output_mint != admitted_output_mint
            || self.input_mint == self.output_mint
        {
            return Err("market differs from admitted product pair".into());
        }
        let _: [u8; 32] = bs58::decode(admitted_input_mint)
            .into_vec()
            .map_err(|e| e.to_string())?
            .try_into()
            .map_err(|_| "input mint length")?;
        let _: [u8; 32] = bs58::decode(admitted_output_mint)
            .into_vec()
            .map_err(|e| e.to_string())?
            .try_into()
            .map_err(|_| "output mint length")?;
        let account = |key: &str| {
            s.accounts
                .iter()
                .find(|a| a.key == key)
                .ok_or_else(|| format!("missing {key}"))
        };
        let pool = account(&self.pool)?;
        if pool.owner != self.program || pool.executable {
            return Err("pool owner".into());
        }
        let key = |s: &str| -> Result<[u8; 32]> {
            bs58::decode(s)
                .into_vec()
                .map_err(|e| e.to_string())?
                .try_into()
                .map_err(|_| "key length".into())
        };
        let config = if self.venue == Venue::OrcaWhirlpool {
            let oracle = account(&self.config)?;
            let expected = Pubkey::find_program_address(
                &[b"oracle", &key(&self.pool)?],
                &Pubkey::new_from_array(key(&self.program)?),
            )
            .0;
            let fixed_fee =
                market_bytes::<2>(&pool.data, 41)? == market_bytes::<2>(&pool.data, 43)?;
            // Fixed-fee Whirlpool predates the adaptive oracle account. Its
            // canonical PDA may be uninitialized; adaptive pools never qualify.
            let uninitialized = fixed_fee
                && oracle.owner == "11111111111111111111111111111111"
                && oracle.data.is_empty();
            let initialized = oracle.owner == self.program
                && bytes::<32>(&oracle.data, 8).ok() == Some(key(&self.pool)?);
            if self.config != expected.to_string()
                || oracle.executable
                || !(uninitialized || initialized)
            {
                return Err("oracle pool binding".into());
            }
            Some(oracle)
        } else if self.venue != Venue::MeteoraDlmm {
            let config = account(&self.config)?;
            if config.owner != self.program
                || config.executable
                || bytes::<32>(&pool.data, 9).map_err(|e| format!("{e:?}"))? != key(&self.config)?
            {
                return Err("config binding".into());
            }
            Some(config)
        } else {
            if !self.config.is_empty() {
                return Err("DLMM has no config account".into());
            }
            None
        };
        let im = account(&self.input_mint)?;
        let om = account(&self.output_mint)?;
        for mint in [im, om] {
            if ![
                "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
            ]
            .contains(&mint.owner.as_str())
            {
                return Err("mint owner".into());
            }
        }
        let offsets = if self.venue == Venue::OrcaWhirlpool {
            [101, 181]
        } else if self.venue == Venue::MeteoraDlmm {
            [88, 120]
        } else {
            [73, 105]
        };
        let ma = bytes::<32>(&pool.data, offsets[0]).map_err(|e| format!("{e:?}"))?;
        let mb = bytes::<32>(&pool.data, offsets[1]).map_err(|e| format!("{e:?}"))?;
        let zero = key(&self.input_mint)? == ma;
        if (if zero { mb } else { ma }) != key(&self.output_mint)?
            || (!zero && mb != key(&self.input_mint)?)
        {
            return Err("mint pair binding".into());
        }
        if self.clock != "SysvarC1ock11111111111111111111111111111111" {
            return Err("clock identity".into());
        }
        let clock = account(&self.clock)?;
        if clock.owner != "Sysvar1111111111111111111111111111111111111"
            || clock.data.len() != 40
            || u64_at(&clock.data, 0).map_err(|e| format!("{e:?}"))? != s.slot
        {
            return Err("clock bank binding".into());
        }
        let epoch = u64_at(&clock.data, 16).map_err(|e| format!("{e:?}"))?;
        let time = u64_at(&clock.data, 32).map_err(|e| format!("{e:?}"))?;
        let inf = TransferFee::decode(&im.data, epoch).map_err(|e| format!("{e:?}"))?;
        let outf = TransferFee::decode(&om.data, epoch).map_err(|e| format!("{e:?}"))?;
        let mut arrays = Vec::new();
        for k in &self.tick_arrays {
            let a = account(k)?;
            if a.owner != self.program {
                return Err("tick owner".into());
            }
            arrays.push(a.data.as_slice());
        }
        let curve = match self.venue {
            Venue::OrcaWhirlpool => skew_native::orca::OrcaCurve::decode(
                key(&self.pool)?,
                &pool.data,
                &arrays,
                Some(&config.ok_or("oracle missing")?.data),
                [inf, outf],
                time,
                zero,
            )
            .map(NativeCurve::Orca),
            Venue::RaydiumClmm => ClmmCurve::decode_at(
                key(&self.pool)?,
                &pool.data,
                &config.ok_or("missing config")?.data,
                &arrays,
                [inf, outf],
                time,
            )
            .map(NativeCurve::Clmm),
            Venue::ByrealClmm => ClmmCurve::decode_byreal(
                key(&self.pool)?,
                &pool.data,
                &config.ok_or("missing config")?.data,
                &arrays,
                [inf, outf],
                time,
            )
            .map(NativeCurve::Clmm),
            Venue::MeteoraDlmm => DlmmCurve::decode(
                key(&self.pool)?,
                &pool.data,
                &arrays,
                [inf, outf],
                s.slot,
                time,
            )
            .map(NativeCurve::Dlmm),
        }
        .map_err(|e| format!("native state rejected: {e:?}"))?;
        Ok(Market {
            curve,
            zero,
            slot: s.slot,
            generation: s.generation,
        })
    }
}

fn active_ordinal(venue: Venue, pool: &[u8]) -> Result<i64> {
    Ok(match venue {
        Venue::RaydiumClmm | Venue::ByrealClmm => {
            let spacing = i64::from(u16::from_le_bytes(market_bytes(pool, 235)?));
            let tick = i64::from(i32::from_le_bytes(market_bytes(pool, 269)?));
            if spacing == 0 {
                return Err("zero CLMM tick spacing".into());
            }
            tick.div_euclid(60 * spacing)
        }
        Venue::OrcaWhirlpool => {
            let spacing = i64::from(u16::from_le_bytes(market_bytes(pool, 41)?));
            let tick = i64::from(i32::from_le_bytes(market_bytes(pool, 81)?));
            if spacing == 0 {
                return Err("zero Whirlpool tick spacing".into());
            }
            tick.div_euclid(88 * spacing)
        }
        Venue::MeteoraDlmm => i64::from(i32::from_le_bytes(market_bytes(pool, 76)?)).div_euclid(70),
    })
}

fn market_bytes<const N: usize>(data: &[u8], offset: usize) -> Result<[u8; N]> {
    bytes(data, offset).map_err(|error| format!("{error:?}"))
}

enum NativeCurve {
    Orca(skew_native::orca::OrcaCurve),
    Clmm(ClmmCurve),
    Dlmm(DlmmCurve),
}
pub struct Market {
    curve: NativeCurve,
    zero: bool,
    pub slot: u64,
    pub generation: u64,
}
impl Market {
    pub fn quote(&self, input: u64) -> Result<u64> {
        if input == 0 || input > 500_000_000_000 {
            return Err("input bound".into());
        }
        match &self.curve {
            NativeCurve::Orca(c) => c.quote(input),
            NativeCurve::Clmm(c) => c.quote(input, self.zero),
            NativeCurve::Dlmm(c) => c.quote(input, self.zero),
        }
        .map_err(|e| format!("{e:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contiguous_selection_never_jumps_missing_array() {
        let candidates = (-3..=4).map(|i| (i, format!("array-{i}"))).collect::<Vec<_>>();
        let present = candidates.iter().filter(|(i, _)| *i != -1 && *i != 3)
            .map(|(_, key)| key.as_str()).collect();
        assert_eq!(select_contiguous_array_horizon(&candidates, 0, &present, 6).unwrap(),
            ["array-0", "array-1", "array-2"]);
        assert_eq!(select_contiguous_array_horizon(&candidates, 0, &present, 1).unwrap(), ["array-0"]);
        assert!(select_contiguous_array_horizon(&candidates, -1, &present, 6).is_err());
    }

    #[test]
    fn contiguous_selection_exhaustive_presence_and_capacity() {
        let candidates = (-4..4).map(|i| (i, format!("array-{i}"))).collect::<Vec<_>>();
        for mask in 0u16..256 {
            let present = candidates.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, (_, key))| key.as_str()).collect();
            for capacity in 1..=8 {
                for active in -4..4 {
                    let selected = select_contiguous_array_horizon(&candidates, active, &present, capacity);
                    if mask & (1 << (active + 4)) == 0 { assert!(selected.is_err()); continue; }
                    let selected = selected.unwrap();
                    assert!(selected.len() <= capacity && selected.contains(&format!("array-{active}")));
                    let indexes = selected.iter().map(|key| candidates.iter().position(|(_, k)| k == key).unwrap()).collect::<Vec<_>>();
                    assert!(indexes.windows(2).all(|p| p[1] == p[0] + 1));
                    assert!(selected.iter().all(|key| present.contains(key.as_str())));
                }
            }
        }
        let all = candidates.iter().map(|(_, key)| key.as_str()).collect();
        assert!(select_contiguous_array_horizon(&candidates, 0, &all, 0).is_err());
        let mut invalid = candidates.clone(); invalid[2].0 = invalid[1].0;
        assert!(select_contiguous_array_horizon(&invalid, 0, &all, 4).is_err());
        invalid = candidates.clone(); invalid[2].1 = invalid[1].1.clone();
        assert!(select_contiguous_array_horizon(&invalid, 0, &all, 4).is_err());
    }

    #[test]
    fn initial_product_registry_is_exact_and_closed() {
        let expected = [
            "NVDA",
            "TSLA",
            "SPY",
            "QQQ",
            "COIN",
            "AAPL",
            "MSFT",
            "GOOGL",
            "AMZN",
            "META",
            "MSTR",
            "NFLX",
            "MU",
            "HOOD",
            "SPACEX",
            "OPENAI",
            "ANTHROPIC",
        ];
        assert_eq!(
            STOCK_PRODUCTS
                .iter()
                .map(|(_, mint)| admitted_instrument(mint).unwrap())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(admitted_instrument(USDC_MINT), None);
        assert_eq!(admitted_instrument(WSOL_MINT), None);
        assert_eq!(
            admitted_instrument("11111111111111111111111111111111"),
            None
        );
        assert!(admitted_pair(WSOL_MINT, USDC_MINT));
        assert!(admitted_pair(USDC_MINT, WSOL_MINT));
    }

    #[test]
    fn bounded_array_horizon_tracks_current_clmm_tick() {
        let market = MarketConfig {
            venue: Venue::RaydiumClmm,
            program: Venue::RaydiumClmm.program().to_string(),
            pool: "11111111111111111111111111111111".to_string(),
            config: String::new(),
            input_mint: USDC_MINT.to_string(),
            output_mint: WSOL_MINT.to_string(),
            tick_arrays: Vec::new(),
            array_capacity: Some(5),
            clock: "SysvarC1ock11111111111111111111111111111111".to_string(),
        };
        let mut pool = vec![0u8; 300];
        pool[235..237].copy_from_slice(&4u16.to_le_bytes());
        pool[269..273].copy_from_slice(&481i32.to_le_bytes());
        let active = market.active_array_ordinal(&pool).unwrap();
        let candidates = market.dynamic_array_candidates(&pool).unwrap();
        assert_eq!(active, 2);
        assert!(candidates.len() <= 8);
        assert!(candidates.iter().any(|(ordinal, _)| *ordinal == active));
        assert!(candidates.iter().all(|(_, address)| bs58::decode(address)
            .into_vec()
            .unwrap()
            .len()
            == 32));
    }

    #[test]
    fn venue_edge_arrays_include_partial_span_without_admitting_invalid_ticks() {
        for venue in [Venue::RaydiumClmm, Venue::OrcaWhirlpool] {
            let market = MarketConfig {
                venue, program: venue.program().to_string(),
                pool: "11111111111111111111111111111111".to_string(),
                config: String::new(), input_mint: USDC_MINT.into(), output_mint: WSOL_MINT.into(),
                tick_arrays: vec![], array_capacity: Some(6),
                clock: "SysvarC1ock11111111111111111111111111111111".into(),
            };
            let (spacing_at,tick_at,width) = if venue == Venue::OrcaWhirlpool {(41,81,88)} else {(235,269,60)};
            for spacing in [1u16,2,128,32_768,u16::MAX] {
                let span=i64::from(spacing)*width;
                for tick in [-443_636i32,-443_635,-1,0,443_635,443_636] {
                    let mut pool=vec![0u8;300];
                    pool[spacing_at..spacing_at+2].copy_from_slice(&spacing.to_le_bytes());
                    pool[tick_at..tick_at+4].copy_from_slice(&tick.to_le_bytes());
                    let candidates=market.dynamic_array_candidates(&pool).unwrap();
                    assert!(candidates.iter().any(|(n,_)|*n==i64::from(tick).div_euclid(span)));
                    assert!(candidates.iter().all(|(n,_)|*n>=(-443_636i64).div_euclid(span) && *n<=443_636i64.div_euclid(span)));
                    assert!(candidates.len()<=8);
                }
                for tick in [-443_637i32,443_637] {
                    let mut pool=vec![0u8;300];
                    pool[spacing_at..spacing_at+2].copy_from_slice(&spacing.to_le_bytes());
                    pool[tick_at..tick_at+4].copy_from_slice(&tick.to_le_bytes());
                    assert!(market.dynamic_array_candidates(&pool).is_err());
                }
            }
        }
    }
}
