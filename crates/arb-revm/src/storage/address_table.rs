use eyre::Result;
use revm::{
    context_interface::JournalTr,
    primitives::{Address, Bytes, FixedBytes, U256},
};

use super::{StorageBacked, StorageSpace};

/// ArbOS address table wrapper.
#[derive(Debug)]
pub struct AddressTable {
    backing_storage: StorageSpace,
    by_address: StorageSpace,
    num_items: StorageBacked<u64>,
}

impl AddressTable {
    pub fn open(backing: StorageSpace) -> Self {
        Self {
            num_items: backing.storage_backed(0),
            by_address: backing.open_subspace(Bytes::new()),
            backing_storage: backing,
        }
    }

    pub fn register<J: JournalTr>(&self, address: Address, journal: &mut J) -> Result<u64> {
        let mut bytes = [0_u8; 32];
        bytes[12..].copy_from_slice(address.as_slice());
        let key = FixedBytes::<32>::from(bytes);

        let existing = *self.by_address.get(key, journal)?;
        if existing != U256::ZERO {
            return Ok(u64::try_from(existing)? - 1);
        }

        let new_len = self.num_items.get(journal)? + 1;
        self.backing_storage
            .set(key, U256::from(new_len), journal)?;
        self.by_address.set(
            FixedBytes::from(U256::from(new_len).to_be_bytes()),
            U256::from_be_bytes(bytes),
            journal,
        )?;
        Ok(new_len - 1)
    }

    pub fn lookup<J: JournalTr>(&self, address: Address, journal: &mut J) -> Result<Option<u64>> {
        let mut bytes = [0_u8; 32];
        bytes[12..].copy_from_slice(address.as_slice());
        let key = FixedBytes::<32>::from(bytes);
        let stored = *self.by_address.get(key, journal)?;
        if stored == U256::ZERO {
            Ok(None)
        } else {
            Ok(Some(u64::try_from(stored)? - 1))
        }
    }

    pub fn len<J: JournalTr>(&self, journal: &mut J) -> Result<u64> {
        self.num_items.get(journal)
    }
}
