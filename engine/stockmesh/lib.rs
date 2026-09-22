//! Bounded, allocation-free single-pair liquidity compiler and execution kernel.
//! The core deliberately has no network, signing, clock, allocator or floats.
#![no_std]

#[cfg(not(feature = "onchain-oracle"))]
pub mod clearing;
#[cfg(not(feature = "onchain-oracle"))]
pub mod compiler;
#[cfg(not(feature = "onchain-oracle"))]
pub mod fair;
#[cfg(not(feature = "onchain-oracle"))]
pub mod onebook;
#[cfg(not(feature = "onchain-oracle"))]
pub mod optimizer;
#[cfg(feature = "onchain-oracle")]
pub mod optimizer {
    pub mod oracle;
}
#[cfg(not(feature = "onchain-oracle"))]
pub mod reflow;
#[cfg(not(feature = "onchain-oracle"))]
pub mod runtime;

pub const MAX_EDGES: usize = 8;
pub const MAX_SEGMENTS: usize = 16;
pub const MAX_LEGS: usize = 4;
pub const MAX_REFLOWS: usize = 3;
pub const SCALE: u128 = 1u128 << 32;
/// Domain bound makes every intermediate multiplication fit u128.
pub const MAX_ATOMS: u64 = 100_000_000_000_000;
pub const MAX_RATE: u64 = 1u64 << 48;

pub type Key = [u8; 32];
pub type Result<T> = core::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidAmount,
    InvalidCurve,
    Arithmetic,
    Capacity,
    Bounds,
    WorkLimit,
    Stale,
    Expired,
    Identity,
    AliasedLiquidity,
    Unsupported,
    VenueUnavailable,
    FatalCpi,
    BalanceInvariant,
    MinOut,
    QueueFull,
    Duplicate,
    OwnerLimit,
    InvalidTicket,
}

/// A deterministic operation budget. This is NOT a Solana compute-unit meter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkMeter {
    pub used: u32,
    pub limit: u32,
}

impl WorkMeter {
    pub const fn new(limit: u32) -> Self {
        Self { used: 0, limit }
    }
    pub fn charge(&mut self, units: u32) -> Result<()> {
        let next = self.used.checked_add(units).ok_or(Error::WorkLimit)?;
        if next > self.limit {
            return Err(Error::WorkLimit);
        }
        self.used = next;
        Ok(())
    }
}

#[cfg(not(feature = "onchain-oracle"))]
pub(crate) fn ceil_div(n: u128, d: u128) -> Result<u128> {
    if d == 0 {
        return Err(Error::Arithmetic);
    }
    Ok(n / d + u128::from(n % d != 0))
}

#[cfg(not(feature = "onchain-oracle"))]
pub(crate) fn as_u64(n: u128) -> Result<u64> {
    n.try_into().map_err(|_| Error::Arithmetic)
}
