use eyre::Result;
use revm::{
    context_interface::JournalTr,
    primitives::{FixedBytes, B256, U256},
};

use super::{StorageBacked, StorageSpace};

const SIZE_OFFSET: u8 = 0;
const PARTIALS_BASE_OFFSET: u64 = 2;

/// ArbOS send Merkle accumulator storage view.
#[derive(Debug)]
pub struct SendMerkle {
    storage: StorageSpace,
    size: StorageBacked<u64>,
}

impl SendMerkle {
    pub fn open(storage: &StorageSpace) -> Self {
        Self {
            storage: storage.clone(),
            size: storage.storage_backed(SIZE_OFFSET),
        }
    }

    pub fn size<J: JournalTr>(&self, journal: &mut J) -> Result<u64> {
        self.size.get(journal)
    }

    pub fn set_size<J: JournalTr>(&self, size: u64, journal: &mut J) -> Result<()> {
        self.size.set(size, journal)?;
        Ok(())
    }

    pub fn partial<J: JournalTr>(&self, level: u64, journal: &mut J) -> Result<B256> {
        let word = self
            .storage
            .get_u256(
                U256::from(PARTIALS_BASE_OFFSET.saturating_add(level)),
                journal,
            )?
            .data;
        Ok(B256::from(word.to_be_bytes::<32>()))
    }

    pub fn set_partial<J: JournalTr>(
        &self,
        level: u64,
        value: B256,
        journal: &mut J,
    ) -> Result<()> {
        self.storage.set(
            FixedBytes::from(U256::from(PARTIALS_BASE_OFFSET.saturating_add(level)).to_be_bytes()),
            U256::from_be_bytes(value.0),
            journal,
        )?;
        Ok(())
    }

    pub fn partial_count_for_size(size: u64) -> u64 {
        if size <= 1 {
            0
        } else {
            (64 - (size - 1).leading_zeros()) as u64
        }
    }
}
