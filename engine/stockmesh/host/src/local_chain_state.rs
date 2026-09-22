//! Capability boundary for an external-RPC-independent StockMesh process.
//!
//! Implementations are backed by the pinned replay process. They must not hide
//! JSON-RPC, PubSub, hosted Geyser, or quote API calls behind this interface.
//! A capability that is not wired fails closed instead of returning cached or
//! synthetic success.

use crate::Result;
use std::{collections::BTreeSet, sync::Arc};

pub const MAINNET_GENESIS_BYTES: [u8; 32] = [
    0x45, 0x29, 0x69, 0x98, 0xa6, 0xf8, 0xe2, 0xa7, 0x84, 0xdb, 0x5d, 0x9f, 0x95, 0xe1, 0x8f, 0xc2,
    0x3f, 0x70, 0x44, 0x1a, 0x10, 0x39, 0x44, 0x68, 0x01, 0x08, 0x98, 0x79, 0xb0, 0x8c, 0x7e, 0xf0,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Commitment {
    Processed,
    Confirmed,
    Finalized,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BankIdentity {
    pub genesis: [u8; 32],
    pub slot: u64,
    pub parent_slot: u64,
    pub root_slot: u64,
    pub bank_hash: [u8; 32],
    pub parent_bank_hash: [u8; 32],
    pub source_epoch: u64,
    pub runtime_digest: [u8; 32],
    pub feature_digest: [u8; 32],
    pub recent_blockhash: [u8; 32],
    pub last_valid_block_height: u64,
    pub commitment: Commitment,
}

impl BankIdentity {
    pub fn validate(&self) -> Result<()> {
        if self.genesis != MAINNET_GENESIS_BYTES
            || self.slot == 0
            || self.parent_slot >= self.slot
            || self.root_slot > self.slot
            || self.bank_hash == [0; 32]
            || self.parent_bank_hash == [0; 32]
            || self.runtime_digest == [0; 32]
            || self.feature_digest == [0; 32]
            || self.recent_blockhash == [0; 32]
            || self.last_valid_block_height == 0
        {
            return Err("local bank identity bounds".into());
        }
        if self.commitment == Commitment::Finalized && self.root_slot < self.slot {
            return Err("finalized bank is not rooted".into());
        }
        Ok(())
    }
}

/// Opaque, process-local lease. The generation prevents use after replay has
/// replaced or released the underlying Bank. Raw runtime pointers never cross
/// this boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BankLease {
    pub handle: u64,
    pub generation: u64,
    pub identity: BankIdentity,
}

impl BankLease {
    pub fn validate(&self) -> Result<()> {
        if self.handle == 0 || self.generation == 0 {
            return Err("local bank lease bounds".into());
        }
        self.identity.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalAccount {
    pub key: [u8; 32],
    pub owner: [u8; 32],
    pub lamports: u64,
    pub data: Arc<[u8]>,
    pub executable: bool,
    pub rent_epoch: u64,
    pub write_version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAccounts {
    pub bank: BankIdentity,
    pub accounts: Vec<LocalAccount>,
}

impl ResolvedAccounts {
    pub fn validate_exact(&self, lease: &BankLease, requested: &[[u8; 32]]) -> Result<()> {
        lease.validate()?;
        self.bank.validate()?;
        if self.bank != lease.identity || requested.is_empty() || requested.len() > 256 {
            return Err("local account resolution bank or bounds".into());
        }
        let expected = requested.iter().copied().collect::<BTreeSet<_>>();
        let actual = self
            .accounts
            .iter()
            .map(|account| account.key)
            .collect::<BTreeSet<_>>();
        if expected.len() != requested.len()
            || actual.len() != self.accounts.len()
            || expected != actual
        {
            return Err("local account resolution is not exact".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExactSimulation {
    pub bank: BankIdentity,
    pub units_consumed: u64,
    pub return_data: Option<Vec<u8>>,
    pub post_accounts: Vec<LocalAccount>,
    pub program_error: Option<String>,
}

impl ExactSimulation {
    pub fn validate_lineage(&self, lease: &BankLease) -> Result<()> {
        lease.validate()?;
        self.bank.validate()?;
        if self.bank != lease.identity || self.units_consumed == 0 {
            return Err("local simulation bank or compute units".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureState {
    Unknown,
    Processed,
    Confirmed,
    Finalized,
    Rejected,
    ExpiredNotLanded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureObservation {
    pub signature: [u8; 64],
    pub state: SignatureState,
    pub observed_slot: Option<u64>,
    pub rooted_slot: Option<u64>,
    pub effect_digest: Option<[u8; 32]>,
}

pub trait LocalBankSource: Send + Sync {
    fn acquire_bank(&self, commitment: Commitment) -> Result<BankLease>;
    fn resolve_accounts(&self, lease: &BankLease, keys: &[[u8; 32]]) -> Result<ResolvedAccounts>;
    fn release_bank(&self, lease: BankLease) -> Result<()>;
}

pub trait LocalExecutionRuntime: Send + Sync {
    fn simulate_exact(
        &self,
        lease: &BankLease,
        message: &[u8],
        returned_accounts: &[[u8; 32]],
    ) -> Result<ExactSimulation>;
}

pub trait LocalTransactionPlane: Send + Sync {
    fn submit_exact_wire(&self, wire: &[u8]) -> Result<[u8; 64]>;
    fn observe_signature(&self, signature: [u8; 64]) -> Result<SignatureObservation>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bank() -> BankIdentity {
        BankIdentity {
            genesis: MAINNET_GENESIS_BYTES,
            slot: 100,
            parent_slot: 99,
            root_slot: 98,
            bank_hash: [1; 32],
            parent_bank_hash: [2; 32],
            source_epoch: 7,
            runtime_digest: [3; 32],
            feature_digest: [4; 32],
            recent_blockhash: [5; 32],
            last_valid_block_height: 110,
            commitment: Commitment::Confirmed,
        }
    }

    #[test]
    fn account_resolution_rejects_partial_or_mixed_bank_data() {
        let identity = bank();
        let lease = BankLease {
            handle: 1,
            generation: 2,
            identity: identity.clone(),
        };
        let rows = ResolvedAccounts {
            bank: identity,
            accounts: vec![LocalAccount {
                key: [8; 32],
                owner: [9; 32],
                lamports: 1,
                data: Arc::from([10u8]),
                executable: false,
                rent_epoch: 0,
                write_version: 1,
            }],
        };
        assert!(rows.validate_exact(&lease, &[[8; 32]]).is_ok());
        assert!(rows.validate_exact(&lease, &[[8; 32], [11; 32]]).is_err());
        let mut other = rows.clone();
        other.bank.slot = 101;
        assert!(other.validate_exact(&lease, &[[8; 32]]).is_err());
    }

    #[test]
    fn finalized_bank_must_be_rooted() {
        let mut identity = bank();
        identity.commitment = Commitment::Finalized;
        identity.root_slot = identity.slot - 1;
        assert!(identity.validate().is_err());
        identity.root_slot = identity.slot;
        assert!(identity.validate().is_ok());
    }
}
