mod address_set;
mod address_table;
mod arbos_state;
mod batch_poster_table;
mod block_hashes;
mod bytes;
mod chain_config;
mod features;
mod l1_pricing;
mod l2_pricing;
mod offsets;
pub mod programs;
mod queue;
mod retryables;
mod send_merkle;
mod slot;

use revm::{
    context_interface::{context::SStoreResult, journaled_state::StateLoad},
    primitives::{Address, Bytes, FixedBytes, StorageValue, U256, keccak256},
};

use crate::arb_journal::ArbJournal;
use crate::constants::ARBOS_STATE_ADDRESS;

pub use address_set::AddressSet;
pub use address_table::AddressTable;
pub use arbos_state::{ArbBlockHeaderInfo, ArbosState};
pub use batch_poster_table::{BatchPosterState, BatchPosterTable};
pub use block_hashes::BlockHashes;
pub use bytes::StorageBytes;
pub use chain_config::ChainConfig;
pub use features::{ArbFeatures, FEATURE_INCREASED_CALLDATA_PRICE};
pub use l1_pricing::L1Pricing;
pub use l2_pricing::L2Pricing;
pub use offsets::{ArbosMetadataOffset, L1PricingOffset, L2PricingOffset, Subspace};
pub use programs::{ArbosPrograms, ProgramDataPricer, pack_uint, stylus_param_layout, unpack_uint};
pub use queue::StorageQueue;
pub use retryables::{
    RETRYABLE_LIFETIME_SECONDS, RETRYABLE_REAP_PRICE, RetryableRecord, Retryables,
};
pub use send_merkle::{SendMerkle, SendMerkleUpdateEvent};
pub use slot::{StorageBacked, StorageSlot};

/// ArbOS storage namespace rooted at a specific account and subspace key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StorageSpace {
    key: Bytes,
    account: Address,
}

impl Default for StorageSpace {
    fn default() -> Self {
        Self::arbos()
    }
}

impl StorageSpace {
    /// Creates a new namespace rooted at `account`.
    pub fn new(account: Address) -> Self {
        Self {
            key: Bytes::new(),
            account,
        }
    }

    /// Returns the canonical ArbOS root namespace.
    pub fn arbos() -> Self {
        Self::new(ARBOS_STATE_ADDRESS)
    }

    /// Returns the backing account for this namespace.
    pub fn account(&self) -> Address {
        self.account
    }

    /// Opens a nested subspace using ArbOS's keyed keccak derivation.
    pub fn open_subspace(&self, id: Bytes) -> Self {
        let derived = keccak256(&[self.key.as_ref(), id.as_ref()].concat());
        Self {
            key: Bytes::from(derived.to_vec()),
            account: self.account,
        }
    }

    /// Opens a one-byte keyed subspace.
    pub fn open_subspace_with_key(&self, id: u8) -> Self {
        self.open_subspace(Bytes::from(vec![id]))
    }

    /// Maps an ArbOS logical key into the concrete storage slot used on-chain.
    pub fn slot_for_hash(&self, hash: FixedBytes<32>) -> FixedBytes<32> {
        let hash_bytes = hash.as_slice();
        let derived = keccak256(&[self.key.as_ref(), &hash_bytes[..31]].concat());
        let mut bytes = [0_u8; 32];
        bytes[..31].copy_from_slice(&derived[..31]);
        bytes[31] = hash_bytes[31];
        FixedBytes::from(bytes)
    }

    /// Convenience helper for integer logical keys.
    pub fn slot_for_u256(&self, key: U256) -> FixedBytes<32> {
        self.slot_for_hash(FixedBytes::from(key.to_be_bytes()))
    }

    /// Convenience helper for byte-sized offsets.
    pub fn slot_for_offset(&self, offset: u8) -> FixedBytes<32> {
        let mut bytes = [0_u8; 32];
        bytes[31] = offset;
        self.slot_for_hash(FixedBytes::from(bytes))
    }

    /// Loads a storage value through a journal.
    pub fn get<J: ArbJournal>(
        &self,
        hash: FixedBytes<32>,
        journal: &mut J,
    ) -> Result<StateLoad<StorageValue>, J::Error> {
        journal.read_slot(self.account, self.slot_for_hash(hash).into())
    }

    /// Loads a storage value by integer key.
    pub fn get_u256<J: ArbJournal>(
        &self,
        key: U256,
        journal: &mut J,
    ) -> Result<StateLoad<StorageValue>, J::Error> {
        self.get(FixedBytes::from(key.to_be_bytes()), journal)
    }

    /// Stores a storage value through a journal.
    pub fn set<J: ArbJournal>(
        &self,
        hash: FixedBytes<32>,
        value: StorageValue,
        journal: &mut J,
    ) -> Result<StateLoad<SStoreResult>, J::Error> {
        // `write_slot` warms the account, stores the slot, and touches the account so the write
        // survives commit (revm's `DatabaseCommit` skips untouched accounts).
        journal.write_slot(self.account, self.slot_for_hash(hash).into(), value)
    }

    /// Creates an untyped storage slot accessor.
    pub fn slot(&self, offset: u8) -> StorageSlot {
        StorageSlot::new(self.account, self.slot_for_offset(offset))
    }

    /// Creates a typed storage slot accessor.
    pub fn storage_backed<T>(&self, offset: u8) -> StorageBacked<T> {
        StorageBacked::new(self.account, self.slot_for_offset(offset))
    }
}

#[cfg(test)]
mod tests {
    use core::str::FromStr;

    use revm::primitives::FixedBytes;

    use super::{StorageSpace, Subspace};

    #[test]
    fn calculates_known_root_slots() {
        let root = StorageSpace::arbos();
        let slot = root.slot_for_offset(0x00);
        let expected = FixedBytes::from_str(
            "15fed0451499512d95f3ec5a41c878b9de55f21878b5b4e190d4667ec709b400",
        )
        .unwrap();
        assert_eq!(slot, expected);
    }

    #[test]
    fn calculates_known_subspace_slots() {
        let root = StorageSpace::arbos();
        let l2_pricing = root.open_subspace_with_key(Subspace::L2Pricing as u8);
        let slot = l2_pricing.slot_for_offset(0x01);
        let expected = FixedBytes::from_str(
            "e54de2a4cdacc0a0059d2b6e16348103df8c4aff409c31e40ec73d11926c8201",
        )
        .unwrap();
        assert_eq!(slot, expected);
    }
}
