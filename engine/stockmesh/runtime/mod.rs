pub mod admission;
pub mod fixture;

use crate::compiler::Snapshot;
use crate::{Key, Result};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Balances {
    pub input: u64,
    pub output: u64,
}

/// The SVM adapter must validate SPL token ownership/mints and signer authority,
/// dispatch only pinned programs, reload balances after CPI, and return errors.
/// A runtime error is fatal: transactional rollback belongs to the host.
pub trait ExecutionHost {
    fn slot(&self) -> u64;
    fn snapshot(&self, index: usize) -> Result<Snapshot>;
    fn balances(&self) -> Result<Balances>;
    fn invoke(&mut self, index: usize, input: u64) -> Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgePin {
    pub market: Key,
    pub program: Key,
    pub liquidity_id: Key,
    pub writable_resources: u64,
    pub min_generation: u64,
}

impl From<Snapshot> for EdgePin {
    fn from(s: Snapshot) -> Self {
        Self {
            market: s.market,
            program: s.program,
            liquidity_id: s.liquidity_id,
            writable_resources: s.writable_resources,
            min_generation: s.generation,
        }
    }
}
