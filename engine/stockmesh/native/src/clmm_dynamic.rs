//! Raydium dynamic-fee state (pool_fee.rs / SwapState at ed7c84a54ced59c55981780546adb0b4583dcf85).
//! Apache-2.0 upstream; checked integers, snapshot Clock only, no host clock.
use crate::{bytes, u64_at, Error, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DynamicFee {
    filter: u16,
    decay: u16,
    reduction: u16,
    control: u32,
    maximum: u32,
    reference_index: i32,
    reference: u32,
    accumulator: u32,
    timestamp: u64,
}
impl DynamicFee {
    pub(crate) fn decode(pool: &[u8], spacing: i32, current: i32, timestamp: u64) -> Result<Option<Self>> {
        let d = pool.get(1096..1176).ok_or(Error::Layout)?;
        if d.iter().all(|v| *v == 0) { return Ok(None); }
        let mut fee = Self {
            filter: u16::from_le_bytes(bytes(d, 0)?),
            decay: u16::from_le_bytes(bytes(d, 2)?),
            reduction: u16::from_le_bytes(bytes(d, 4)?),
            control: u32::from_le_bytes(bytes(d, 6)?),
            maximum: u32::from_le_bytes(bytes(d, 10)?),
            reference_index: i32::from_le_bytes(bytes(d, 14)?),
            reference: u32::from_le_bytes(bytes(d, 18)?),
            accumulator: u32::from_le_bytes(bytes(d, 22)?),
            timestamp: u64_at(d, 26)?,
        };
        if spacing <= 0 || fee.filter == 0 || fee.decay <= fee.filter
            || fee.reduction == 0 || fee.reduction >= 10_000
            || fee.control == 0 || fee.control >= 100_000
            || u64::from(fee.maximum) * spacing as u64 > u64::from(u32::MAX)
            || fee.reference > fee.maximum || fee.accumulator > fee.maximum
            || d[34..].iter().any(|v| *v != 0)
        { return Err(Error::Unsupported); }
        // decode() without a coherent Clock must never price a dynamic pool.
        if timestamp == 0 || timestamp < fee.timestamp { return Err(Error::Stale); }
        let elapsed = timestamp - fee.timestamp;
        if elapsed >= u64::from(fee.filter) {
            fee.reference_index = current.div_euclid(spacing);
            fee.reference = if elapsed < u64::from(fee.decay) {
                (u64::from(fee.accumulator) * u64::from(fee.reduction) / 10_000) as u32
            } else { 0 };
            fee.timestamp = timestamp;
        }
        Ok(Some(fee))
    }
    pub(crate) fn update(&mut self, index: i32) -> Result<()> {
        let distance = (i64::from(self.reference_index) - i64::from(index)).unsigned_abs();
        let value = u64::from(self.reference).checked_add(distance.checked_mul(10_000).ok_or(Error::Arithmetic)?).ok_or(Error::Arithmetic)?;
        self.accumulator = value.min(u64::from(self.maximum)) as u32;
        Ok(())
    }
    pub(crate) fn capped(&self) -> bool { self.accumulator == self.maximum }
    pub(crate) fn total(&self, base: u32, spacing: i32) -> Result<u32> {
        let crossed = u128::from(self.accumulator).checked_mul(spacing as u128).ok_or(Error::Arithmetic)?;
        let numerator = crossed.checked_mul(crossed).and_then(|v| v.checked_mul(u128::from(self.control))).ok_or(Error::Arithmetic)?;
        let variable = numerator.div_ceil(100_000u128 * 10_000 * 10_000).min(100_000) as u32;
        Ok(base.checked_add(variable).ok_or(Error::Arithmetic)?.min(100_000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pool() -> Vec<u8> {
        let mut p=vec![0;1544]; let d=&mut p[1096..1176];
        d[0..2].copy_from_slice(&60u16.to_le_bytes());
        d[2..4].copy_from_slice(&600u16.to_le_bytes());
        d[4..6].copy_from_slice(&5000u16.to_le_bytes());
        d[6..10].copy_from_slice(&15000u32.to_le_bytes());
        d[10..14].copy_from_slice(&150000u32.to_le_bytes());
        d[14..18].copy_from_slice(&(-1i32).to_le_bytes());
        d[18..22].copy_from_slice(&10000u32.to_le_bytes());
        d[22..26].copy_from_slice(&40000u32.to_le_bytes());
        d[26..34].copy_from_slice(&1000u64.to_le_bytes()); p
    }
    #[test] fn decay_negative_floor_and_integer_fee() {
        let p=pool();
        let mut f=DynamicFee::decode(&p,60,-1,1059).unwrap().unwrap();
        f.update(-1).unwrap();assert_eq!(f.accumulator,10000);assert_eq!(f.total(3000,60).unwrap(),3540);
        let mut f=DynamicFee::decode(&p,60,-61,1060).unwrap().unwrap();
        assert_eq!(f.reference_index,-2);assert_eq!(f.reference,20000);
        f.update(-3).unwrap();assert_eq!(f.total(3000,60).unwrap(),7860);
        let mut f=DynamicFee::decode(&p,60,-61,1600).unwrap().unwrap();
        f.update(-2).unwrap();assert_eq!(f.total(3000,60).unwrap(),3000);
        f.update(i32::MAX).unwrap();assert!(f.capped());assert_eq!(f.total(3000,60).unwrap(),100000);
    }
    #[test] fn rejects_unknown_layout_bad_clock_and_overflow() {
        let mut p=pool();assert_eq!(DynamicFee::decode(&p,60,0,999),Err(Error::Stale));
        p[1130]=1;assert_eq!(DynamicFee::decode(&p,60,0,1600),Err(Error::Unsupported));
        p[1130]=0;p[1106..1110].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(DynamicFee::decode(&p,60,0,1600),Err(Error::Unsupported));
        assert_eq!(DynamicFee::decode(&vec![0;1544],60,0,0).unwrap(),None);
    }
}
