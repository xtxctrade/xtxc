//! Native quote proposals. Decode at snapshot publication, never at every quote.
//! The caller pins program/owner/account identities; final SBF execution gates admission.
pub mod clmm;
mod clmm_dynamic;
pub mod damm;
pub mod dlmm;
pub mod memo;
pub mod orca;
pub mod surface;

pub type Result<T> = core::result::Result<T, Error>;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Layout,
    Unsupported,
    Arithmetic,
    Capacity,
    Stale,
}

pub fn bytes<const N: usize>(d: &[u8], o: usize) -> Result<[u8; N]> {
    d.get(o..o.checked_add(N).ok_or(Error::Layout)?)
        .ok_or(Error::Layout)?
        .try_into()
        .map_err(|_| Error::Layout)
}
pub fn u64_at(d: &[u8], o: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(bytes(d, o)?))
}
pub fn u128_at(d: &[u8], o: usize) -> Result<u128> {
    Ok(u128::from_le_bytes(bytes(d, o)?))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TransferFee {
    pub bps: u16,
    pub maximum: u64,
}

/// Exact, conservative view of Token-2022's ScaledUiAmount extension.
///
/// The wire format stores the multiplier as IEEE-754 bytes.  Economic code
/// never converts those bytes to a host float: the decoder maps the positive,
/// finite value directly to Q32 and rounds down.  This makes raw-token to
/// underlying-exposure conversion deterministic across machines.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScaledUiAmount {
    pub decimals: u8,
    pub multiplier_q32: u64,
    pub next_multiplier_effective_timestamp: i64,
    pub next_multiplier_q32: u64,
}

impl ScaledUiAmount {
    pub fn decode(mint: &[u8], unix_timestamp: i64) -> Result<Self> {
        if mint.len() < 166 || mint[45] != 1 || mint[165] != 1 || mint[82..165].iter().any(|x| *x != 0) {
            return Err(Error::Layout);
        }
        let decimals = mint[44];
        let mut position = 166usize;
        let mut scaled = None;
        let mut seen = 0u64;
        while position < mint.len() {
            if mint[position..].iter().all(|byte| *byte == 0) {
                break;
            }
            let kind = u16::from_le_bytes(bytes(mint, position)?);
            let length = usize::from(u16::from_le_bytes(bytes(mint, position + 2)?));
            if kind >= 64 || seen & (1u64 << kind) != 0 {
                return Err(Error::Unsupported);
            }
            seen |= 1u64 << kind;
            let value = mint
                .get(position + 4..position + 4 + length)
                .ok_or(Error::Layout)?;
            if kind == 25 {
                if length != 56 {
                    return Err(Error::Layout);
                }
                let current = positive_f64_q32(bytes(value, 32)?)?;
                let effective = i64::from_le_bytes(bytes(value, 40)?);
                let next = positive_f64_q32(bytes(value, 48)?)?;
                scaled = Some((current, effective, next));
            }
            position = position
                .checked_add(4)
                .and_then(|value| value.checked_add(length))
                .ok_or(Error::Arithmetic)?;
        }
        let (current, effective, next) = scaled.ok_or(Error::Unsupported)?;
        let multiplier_q32 = if unix_timestamp >= effective { next } else { current };
        Ok(Self {
            decimals,
            multiplier_q32,
            next_multiplier_effective_timestamp: effective,
            next_multiplier_q32: next,
        })
    }

    pub fn exposure_q32(self, raw_atoms: u64, numerator: u64, denominator: u64, conservative_bps: u16) -> Result<u64> {
        if raw_atoms == 0 || numerator == 0 || denominator == 0 || conservative_bps == 0 || conservative_bps > 10_000 {
            return Err(Error::Arithmetic);
        }
        let scale = 10u128.checked_pow(u32::from(self.decimals)).ok_or(Error::Arithmetic)?;
        let denominator = scale
            .checked_mul(u128::from(denominator))
            .and_then(|value| value.checked_mul(10_000))
            .ok_or(Error::Arithmetic)?;
        let value = u128::from(raw_atoms)
            .checked_mul(u128::from(self.multiplier_q32))
            .and_then(|value| value.checked_mul(u128::from(numerator)))
            .and_then(|value| value.checked_mul(u128::from(conservative_bps)))
            .ok_or(Error::Arithmetic)?
            / denominator;
        u64::try_from(value).map_err(|_| Error::Arithmetic)
    }
}

fn positive_f64_q32(bytes: [u8; 8]) -> Result<u64> {
    let bits = u64::from_le_bytes(bytes);
    if bits >> 63 != 0 {
        return Err(Error::Unsupported);
    }
    let exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1u64 << 52) - 1);
    if exponent == 0 || exponent == 0x7ff {
        return Err(Error::Unsupported);
    }
    let significand = u128::from((1u64 << 52) | fraction);
    // value * 2^32 = significand * 2^(exponent - 1023 - 52 + 32)
    let shift = exponent - 1043;
    let fixed = if shift >= 0 {
        significand.checked_shl(shift as u32).ok_or(Error::Arithmetic)?
    } else {
        significand.checked_shr((-shift) as u32).unwrap_or(0)
    };
    if fixed == 0 {
        return Err(Error::Unsupported);
    }
    u64::try_from(fixed).map_err(|_| Error::Arithmetic)
}
impl TransferFee {
    pub fn decode(mint: &[u8], epoch: u64) -> Result<Self> {
        if mint.len() < 82 || mint[45] != 1 {
            return Err(Error::Layout);
        }
        if mint.len() == 82 {
            return Ok(Self::default());
        }
        if mint.len() < 166 || mint[165] != 1 || mint[82..165].iter().any(|x| *x != 0) {
            return Err(Error::Layout);
        }
        let mut p = 166;
        let mut seen = 0u64;
        let mut fee = Self::default();
        while p < mint.len() {
            if mint[p..].iter().all(|b| *b == 0) {
                break;
            }
            let kind = u16::from_le_bytes(bytes(mint, p)?);
            let len = u16::from_le_bytes(bytes(mint, p + 2)?) as usize;
            if ![1, 3, 4, 6, 10, 12, 14, 16, 18, 19, 25, 26].contains(&kind)
                || seen & (1u64 << kind) != 0
            {
                return Err(Error::Unsupported);
            }
            seen |= 1u64 << kind;
            let ext = mint.get(p + 4..p + 4 + len).ok_or(Error::Layout)?;
            match kind {
                1 => {
                    if len != 108 {
                        return Err(Error::Layout);
                    }
                    let o = if epoch >= u64_at(ext, 90)? { 90 } else { 72 };
                    fee = Self {
                        maximum: u64_at(ext, o + 8)?,
                        bps: u16::from_le_bytes(bytes(ext, o + 16)?),
                    };
                    if fee.bps > 10_000 {
                        return Err(Error::Layout);
                    }
                }
                14 if len != 64 || ext[32..].iter().any(|x| *x != 0) => {
                    return Err(Error::Unsupported)
                }
                // This is mint configuration for fees on confidential
                // transfers. Public transfer-fee calculation still comes only
                // from extension 1; malformed layouts remain fail-closed.
                16 if len != 129 => return Err(Error::Unsupported),
                26 if len != 33 || ext[32] != 0 => return Err(Error::Unsupported),
                6 if len != 1 || ext[0] != 1 => return Err(Error::Unsupported),
                _ => {}
            }
            p += 4 + len;
        }
        Ok(fee)
    }
    pub fn net(self, amount: u64) -> Result<u64> {
        let fee = ((u128::from(amount) * u128::from(self.bps)).div_ceil(10_000))
            .min(u128::from(self.maximum)) as u64;
        amount.checked_sub(fee).ok_or(Error::Arithmetic)
    }
}
