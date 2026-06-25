use eyre::Result;
use revm::{
    context_interface::JournalTr,
    primitives::{B256, FixedBytes, U256, keccak256},
};

use super::{StorageBacked, StorageSpace};

/// ArbOS block-hash ring buffer view.
pub struct BlockHashes {
    backing_storage: StorageSpace,
    l1_block_number: StorageBacked<u64>,
}

impl BlockHashes {
    pub fn open(backing_storage: &StorageSpace) -> Self {
        Self {
            backing_storage: backing_storage.clone(),
            l1_block_number: backing_storage.storage_backed(0),
        }
    }

    pub fn l1_block_number<J: JournalTr>(&self, journal: &mut J) -> Result<u64> {
        self.l1_block_number.get(journal)
    }

    pub fn block_hash<J: JournalTr>(&self, block_number: u64, journal: &mut J) -> Result<B256> {
        let current = self.l1_block_number(journal)?;
        if block_number >= current || block_number + 256 < current {
            return Ok(B256::ZERO);
        }

        let index = 1 + (block_number % 256);
        Ok(self
            .backing_storage
            .get_u256(U256::from(index), journal)?
            .data
            .to_be_bytes()
            .into())
    }

    pub fn set_l1_block_number<J: JournalTr>(
        &self,
        block_number: u64,
        journal: &mut J,
    ) -> Result<()> {
        self.l1_block_number.set(block_number, journal)?;
        Ok(())
    }

    pub fn set_block_hash<J: JournalTr>(
        &self,
        block_number: u64,
        block_hash: B256,
        journal: &mut J,
    ) -> Result<()> {
        let index = 1 + (block_number % 256);
        self.backing_storage.set(
            FixedBytes::from(U256::from(index).to_be_bytes()),
            U256::from_be_bytes(block_hash.0),
            journal,
        )?;
        Ok(())
    }

    /// Nitro-compatible ring-buffer update for newly observed L1 blocks.
    ///
    /// `number` is the L1 block number whose hash is provided, and the
    /// `l1_block_number` cursor is advanced to `number + 1`.
    pub fn record_new_l1_block<J: JournalTr>(
        &self,
        number: u64,
        block_hash: B256,
        arbos_version: u64,
        journal: &mut J,
    ) -> Result<()> {
        let mut next_number = self.l1_block_number(journal)?;
        if number < next_number {
            return Ok(());
        }
        if next_number.saturating_add(256) < number {
            next_number = number - 256;
        }

        let mut rolling_hash = block_hash;
        while next_number.saturating_add(1) < number {
            next_number = next_number.saturating_add(1);
            let mut next_num_buf = [0_u8; 8];
            if arbos_version >= 8 {
                next_num_buf = next_number.to_le_bytes();
            }
            rolling_hash = keccak256(&[rolling_hash.as_slice(), &next_num_buf].concat());
            self.set_block_hash(next_number, rolling_hash, journal)?;
        }

        self.set_block_hash(number, block_hash, journal)?;
        self.set_l1_block_number(number.saturating_add(1), journal)?;
        Ok(())
    }
}
