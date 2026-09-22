//! Strict conversion of an authenticated Pyth price into Stocklana's Q32
//! quote-atoms-per-base-atom interval.
//!
//! Parsing Hermes JSON does not verify the Wormhole/Pyth attestation. Callers
//! must verify the corresponding Solana PriceUpdateV2/price-feed account or
//! place this adapter behind a configured source signer. Multiple Pyth feeds
//! remain one correlation group in the fair-value policy.
use crate::Result;
use serde_json::Value;
use skew_engine::SCALE;

pub const NVDAX_USD_CORE: &str = "4244d07890e4610f46bbde67de8f43a4bf8b569eebe904f136b469f148503b7f";
pub const NVDAX_NVDA_REDEMPTION_RATE_CORE: &str =
    "b675c4e9f46d94afa9174a7df09966b77a2950970bb50a77ec8ad4fcfd8266f4";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub feed_id: [u8; 32],
    pub quote_decimals: u8,
    pub base_decimals: u8,
    pub maximum_age_seconds: u64,
    pub maximum_confidence_bps: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PriceInterval {
    pub publish_time: u64,
    pub low_q32: u64,
    pub high_q32: u64,
}

impl Config {
    pub fn from_hex(
        feed_id: &str,
        quote_decimals: u8,
        base_decimals: u8,
        maximum_age_seconds: u64,
        maximum_confidence_bps: u16,
    ) -> Result<Self> {
        let id = feed_id.strip_prefix("0x").unwrap_or(feed_id);
        if id.len() != 64
            || quote_decimals > 18
            || base_decimals > 18
            || !(1..=60).contains(&maximum_age_seconds)
            || !(1..=2_000).contains(&maximum_confidence_bps)
        {
            return Err("pyth policy bounds".into());
        }
        let mut bytes = [0u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&id[index * 2..index * 2 + 2], 16)
                .map_err(|_| "pyth feed id")?;
        }
        Ok(Self {
            feed_id: bytes,
            quote_decimals,
            base_decimals,
            maximum_age_seconds,
            maximum_confidence_bps,
        })
    }
}

/// Accept the parsed part of a Hermes v2 latest-price response only after the
/// caller has authenticated the update. Unknown response fields are ignored;
/// identity, uniqueness, freshness, confidence and arithmetic are fail-closed.
pub fn parse_hermes(value: &Value, policy: Config, now_seconds: u64) -> Result<PriceInterval> {
    let rows = value["parsed"].as_array().ok_or("pyth parsed prices")?;
    if rows.is_empty() || rows.len() > 64 {
        return Err("pyth parsed price bounds".into());
    }
    let expected = hex(&policy.feed_id);
    let mut selected = None;
    for row in rows {
        let id = row["id"].as_str().ok_or("pyth feed id")?;
        let id = id.strip_prefix("0x").unwrap_or(id);
        if id.eq_ignore_ascii_case(&expected) && selected.replace(row).is_some() {
            return Err("duplicate pyth feed".into());
        }
    }
    let price = &selected.ok_or("required pyth feed missing")?["price"];
    let raw = signed(price.get("price").ok_or("pyth price")?)?;
    let conf = unsigned(price.get("conf").ok_or("pyth confidence")?)?;
    let exponent = signed(price.get("expo").ok_or("pyth exponent")?)?;
    let publish_time = signed(price.get("publish_time").ok_or("pyth publish time")?)?;
    if raw <= 0
        || !(-18..=18).contains(&exponent)
        || publish_time <= 0
        || u64::try_from(publish_time).map_err(|_| "pyth publish time")? > now_seconds + 2
    {
        return Err("pyth price shape".into());
    }
    let raw = u64::try_from(raw).map_err(|_| "pyth price")?;
    let publish_time = u64::try_from(publish_time).map_err(|_| "pyth publish time")?;
    if now_seconds.saturating_sub(publish_time) > policy.maximum_age_seconds {
        return Err("stale pyth price".into());
    }
    if conf >= raw
        || u128::from(conf) * 10_000 > u128::from(raw) * u128::from(policy.maximum_confidence_bps)
    {
        return Err("pyth confidence bound".into());
    }
    let low = q32(
        raw - conf,
        i32::try_from(exponent).map_err(|_| "pyth exponent")?,
        policy.quote_decimals,
        policy.base_decimals,
        false,
    )?;
    let high = q32(
        raw.checked_add(conf).ok_or("pyth confidence overflow")?,
        i32::try_from(exponent).map_err(|_| "pyth exponent")?,
        policy.quote_decimals,
        policy.base_decimals,
        true,
    )?;
    if low == 0 || high < low {
        return Err("pyth interval".into());
    }
    Ok(PriceInterval {
        publish_time,
        low_q32: low,
        high_q32: high,
    })
}

fn q32(raw: u64, exponent: i32, quote_decimals: u8, base_decimals: u8, up: bool) -> Result<u64> {
    let power = exponent
        .checked_add(i32::from(quote_decimals))
        .and_then(|value| value.checked_sub(i32::from(base_decimals)))
        .ok_or("pyth decimal scale")?;
    let mut numerator = u128::from(raw)
        .checked_mul(SCALE)
        .ok_or("pyth q32 overflow")?;
    let mut denominator = 1u128;
    if power >= 0 {
        numerator = numerator
            .checked_mul(pow10(power as u32)?)
            .ok_or("pyth q32 overflow")?;
    } else {
        denominator = pow10(power.unsigned_abs())?;
    }
    let value = numerator / denominator;
    let value = if up && numerator % denominator != 0 {
        value.checked_add(1).ok_or("pyth q32 overflow")?
    } else {
        value
    };
    u64::try_from(value).map_err(|_| "pyth q32 range".into())
}

fn pow10(power: u32) -> Result<u128> {
    if power > 30 {
        return Err("pyth decimal scale".into());
    }
    10u128.checked_pow(power).ok_or("pyth decimal scale".into())
}

fn signed(value: &Value) -> Result<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .ok_or_else(|| "pyth signed integer".into())
}

fn unsigned(value: &Value) -> Result<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .ok_or_else(|| "pyth unsigned integer".into())
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> Config {
        Config::from_hex(NVDAX_USD_CORE, 6, 6, 30, 20).unwrap()
    }
    fn payload() -> Value {
        json!({"parsed":[{"id":NVDAX_USD_CORE,"price":{"price":"18741000000","conf":"5000000","expo":-8,"publish_time":1_000}}],"binary":{"data":["not-trusted-here"]}})
    }

    #[test]
    fn exact_feed_becomes_a_conservative_atom_interval() {
        let interval = parse_hermes(&payload(), config(), 1_010).unwrap();
        assert_eq!(interval.publish_time, 1_000);
        assert!(interval.low_q32 < 187_410u64 * SCALE as u64 / 1_000);
        assert!(interval.high_q32 > 187_410u64 * SCALE as u64 / 1_000);
        assert!(interval.low_q32 > 187u64 * SCALE as u64);
        assert!(interval.high_q32 < 188u64 * SCALE as u64);
    }

    #[test]
    fn identity_freshness_confidence_and_shape_fail_closed() {
        let mut wrong = payload();
        wrong["parsed"][0]["id"] = json!(NVDAX_NVDA_REDEMPTION_RATE_CORE);
        assert!(parse_hermes(&wrong, config(), 1_010).is_err());
        assert!(parse_hermes(&payload(), config(), 1_031).is_err());
        let mut future = payload();
        future["parsed"][0]["price"]["publish_time"] = json!(1_013);
        assert!(parse_hermes(&future, config(), 1_010).is_err());
        let mut wide = payload();
        wide["parsed"][0]["price"]["conf"] = json!("100000000");
        assert!(parse_hermes(&wide, config(), 1_010).is_err());
        let mut duplicate = payload();
        let row = duplicate["parsed"][0].clone();
        duplicate["parsed"].as_array_mut().unwrap().push(row);
        assert!(parse_hermes(&duplicate, config(), 1_010).is_err());
        let mut negative = payload();
        negative["parsed"][0]["price"]["price"] = json!("-1");
        assert!(parse_hermes(&negative, config(), 1_010).is_err());
    }
}
