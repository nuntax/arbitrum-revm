use eyre::Result;
use revm::primitives::{B256, FixedBytes, U256, keccak256};

use super::{StorageBacked, StorageSpace};
use crate::arb_journal::ArbJournal;

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

    pub fn l1_block_number<J: ArbJournal>(&self, journal: &mut J) -> Result<u64> {
        self.l1_block_number.get(journal)
    }

    pub fn block_hash<J: ArbJournal>(&self, block_number: u64, journal: &mut J) -> Result<B256> {
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

    pub fn set_l1_block_number<J: ArbJournal>(
        &self,
        block_number: u64,
        journal: &mut J,
    ) -> Result<()> {
        self.l1_block_number.set(block_number, journal)?;
        Ok(())
    }

    pub fn set_block_hash<J: ArbJournal>(
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
    pub fn record_new_l1_block<J: ArbJournal>(
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

        // Fill the gap between the last recorded L1 block and `number` with synthetic hashes,
        // matching Nitro Blockhashes.RecordNewL1Block: each fill = keccak(blockHash ‖ LE(n)) using
        // the ORIGINAL `block_hash` for every slot (NOT a rolling hash chaining prior results).
        while next_number.saturating_add(1) < number {
            next_number = next_number.saturating_add(1);
            let mut next_num_buf = [0_u8; 8];
            if arbos_version >= 8 {
                next_num_buf = next_number.to_le_bytes();
            }
            let fill = keccak256([block_hash.as_slice(), &next_num_buf].concat());
            self.set_block_hash(next_number, fill, journal)?;
        }

        self.set_block_hash(number, block_hash, journal)?;
        self.set_l1_block_number(number.saturating_add(1), journal)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use revm::{
        context_interface::ContextTr,
        database_interface::EmptyDB,
        primitives::{B256, keccak256},
    };

    use super::BlockHashes;
    use crate::{
        api::default_ctx::{ArbContext, DefaultArb},
        storage::{StorageSpace, Subspace},
    };

    fn fresh() -> (ArbContext<EmptyDB>, BlockHashes) {
        let ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();
        let hashes = BlockHashes::open(
            &StorageSpace::arbos().open_subspace_with_key(Subspace::BlockHashes as u8),
        );
        (ctx, hashes)
    }

    #[test]
    fn records_l1_hashes_and_enforces_the_256_block_window() {
        let (mut ctx, hashes) = fresh();
        let journal = ctx.journal_mut();
        assert_eq!(hashes.l1_block_number(journal).unwrap(), 0);
        assert_eq!(hashes.block_hash(0, journal).unwrap(), B256::ZERO);

        let hash0 = keccak256([0_u8]);
        hashes.record_new_l1_block(0, hash0, 8, journal).unwrap();
        assert_eq!(hashes.l1_block_number(journal).unwrap(), 1);
        assert_eq!(hashes.block_hash(0, journal).unwrap(), hash0);

        let hash4242 = keccak256([42_u8, 42]);
        hashes
            .record_new_l1_block(4242, hash4242, 8, journal)
            .unwrap();
        assert_eq!(hashes.l1_block_number(journal).unwrap(), 4243);
        assert_eq!(hashes.block_hash(4242, journal).unwrap(), hash4242);
        assert_ne!(hashes.block_hash(4241, journal).unwrap(), hash4242);
        assert_ne!(hashes.block_hash(4240, journal).unwrap(), B256::ZERO);
        assert_ne!(hashes.block_hash(4242 - 255, journal).unwrap(), B256::ZERO);
        assert_eq!(hashes.block_hash(4242 - 256, journal).unwrap(), B256::ZERO);
        assert_eq!(hashes.block_hash(4243, journal).unwrap(), B256::ZERO);
    }

    #[test]
    fn gap_hashes_switch_to_little_endian_block_numbers_at_arbos_8() {
        let supplied = keccak256("supplied L1 hash");

        let (mut pre_v8_ctx, pre_v8) = fresh();
        pre_v8
            .record_new_l1_block(3, supplied, 7, pre_v8_ctx.journal_mut())
            .unwrap();
        let legacy_fill = keccak256([supplied.as_slice(), &[0_u8; 8]].concat());
        assert_eq!(
            pre_v8.block_hash(1, pre_v8_ctx.journal_mut()).unwrap(),
            legacy_fill
        );
        assert_eq!(
            pre_v8.block_hash(2, pre_v8_ctx.journal_mut()).unwrap(),
            legacy_fill
        );

        let (mut v8_ctx, v8) = fresh();
        v8.record_new_l1_block(3, supplied, 8, v8_ctx.journal_mut())
            .unwrap();
        assert_eq!(
            v8.block_hash(1, v8_ctx.journal_mut()).unwrap(),
            keccak256([supplied.as_slice(), &1_u64.to_le_bytes()].concat())
        );
        assert_eq!(
            v8.block_hash(2, v8_ctx.journal_mut()).unwrap(),
            keccak256([supplied.as_slice(), &2_u64.to_le_bytes()].concat())
        );
    }
}
