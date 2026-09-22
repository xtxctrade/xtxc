//! Operator-pinned lookup selection; never selected by a browser or quote URL.
//! Each instrument can share a cohort ALT, without requiring the entire stock
//! universe to fit one 256-address table. Exact frozen-table admission remains
//! in the final coherent execution bank, after this configuration selection.
use crate::{onebook_wire::instrument_id, Result};
use solana_pubkey::Pubkey;
use std::{collections::BTreeMap, str::FromStr};

#[derive(Clone)]
pub struct LookupRegistry {
    default: Pubkey,
    instruments: BTreeMap<String, Pubkey>,
}

impl LookupRegistry {
    pub fn parse(default: &str, entries: &BTreeMap<String, String>, forbidden: &[Pubkey]) -> Result<Self> {
        if entries.len() > 128 { return Err("instrument lookup table bound".into()); }
        let parse = |value: &str| -> Result<Pubkey> {
            let key = Pubkey::from_str(value).map_err(|_| "lookup table public key")?;
            if key == Pubkey::default() || forbidden.contains(&key)
                || matches!(value, "AddressLookupTab1e1111111111111111111111111" | "SysvarC1ock11111111111111111111111111111111"
                    | "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA" | "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
                    | "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL" | "ComputeBudget111111111111111111111111111111") {
                return Err("lookup table identity collides with authority or program".into());
            }
            Ok(key)
        };
        let default = parse(default)?;
        let mut instruments = BTreeMap::new();
        for (instrument, table) in entries {
            instrument_id(instrument)?;
            instruments.insert(instrument.clone(), parse(table)?);
        }
        Ok(Self { default, instruments })
    }

    pub fn for_instrument(&self, instrument: &str) -> Result<Pubkey> {
        instrument_id(instrument)?;
        if self.instruments.is_empty() { return Ok(self.default); }
        // Once an explicit registry is present, a missing ticker is not an
        // instruction to silently use the legacy/global table.
        self.instruments.get(instrument).copied().ok_or_else(|| "instrument lookup table missing".into())
    }

    pub fn validate_coverage<'a>(&self, instruments: impl IntoIterator<Item = &'a str>) -> Result<()> {
        let expected = instruments.into_iter().collect::<std::collections::BTreeSet<_>>();
        for instrument in &expected { self.for_instrument(instrument)?; }
        if !self.instruments.is_empty()
            && self.instruments.keys().map(String::as_str).collect::<std::collections::BTreeSet<_>>() != expected {
            return Err("instrument lookup table coverage mismatch".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_and_explicit_cohorts_preserve_exact_tickers() {
        let default = Pubkey::new_unique(); let a = Pubkey::new_unique(); let b = Pubkey::new_unique();
        let legacy = LookupRegistry::parse(&default.to_string(), &BTreeMap::new(), &[]).unwrap();
        assert_eq!(legacy.for_instrument("BRK.B").unwrap(), default);
        let map = BTreeMap::from([("BRK.B".into(), a.to_string()), ("NVDA".into(), b.to_string()), ("TSLA".into(), b.to_string())]);
        let registry = LookupRegistry::parse(&default.to_string(), &map, &[]).unwrap();
        assert_eq!(registry.for_instrument("BRK.B").unwrap(), a);
        assert_eq!(registry.for_instrument("TSLA").unwrap(), b);
        assert!(registry.for_instrument("BRKB").is_err());
        assert!(registry.for_instrument("nvda").is_err());
        assert!(registry.for_instrument("UNKNOWN").is_err());
        assert!(registry.validate_coverage(["BRK.B", "NVDA", "TSLA"]).is_ok());
        assert!(registry.validate_coverage(["NVDA"]).is_err());
        assert!(registry.validate_coverage(["NVDA", "SPY"]).is_err());
    }
    #[test]
    fn invalid_or_authority_aliases_are_rejected() {
        let default = Pubkey::new_unique(); let authority = Pubkey::new_unique();
        for value in ["bad".into(), Pubkey::default().to_string(), authority.to_string(), "AddressLookupTab1e1111111111111111111111111".into()] {
            assert!(LookupRegistry::parse(&default.to_string(), &BTreeMap::from([("NVDA".into(), value)]), &[authority]).is_err());
        }
        assert!(LookupRegistry::parse(&authority.to_string(), &BTreeMap::new(), &[authority]).is_err());
    }
}
