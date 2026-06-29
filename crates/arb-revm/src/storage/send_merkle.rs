use eyre::Result;
use revm::primitives::{B256, FixedBytes, U256, keccak256};

use super::{StorageBacked, StorageSpace};
use crate::arb_journal::ArbJournal;

const SIZE_OFFSET: u8 = 0;
const PARTIALS_BASE_OFFSET: u64 = 2;

/// ArbOS send Merkle accumulator storage view.
#[derive(Debug)]
pub struct SendMerkle {
    storage: StorageSpace,
    size: StorageBacked<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendMerkleUpdateEvent {
    pub level: u64,
    pub num_leaves: u64,
    pub hash: B256,
}

impl SendMerkle {
    pub fn open(storage: &StorageSpace) -> Self {
        Self {
            storage: storage.clone(),
            size: storage.storage_backed(SIZE_OFFSET),
        }
    }

    pub fn size<J: ArbJournal>(&self, journal: &mut J) -> Result<u64> {
        self.size.get(journal)
    }

    pub fn set_size<J: ArbJournal>(&self, size: u64, journal: &mut J) -> Result<()> {
        self.size.set(size, journal)?;
        Ok(())
    }

    pub fn partial<J: ArbJournal>(&self, level: u64, journal: &mut J) -> Result<B256> {
        let word = self
            .storage
            .get_u256(
                U256::from(PARTIALS_BASE_OFFSET.saturating_add(level)),
                journal,
            )?
            .data;
        Ok(B256::from(word.to_be_bytes::<32>()))
    }

    pub fn set_partial<J: ArbJournal>(
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
        if size == 0 {
            0
        } else {
            (64 - size.leading_zeros()) as u64
        }
    }

    /// Appends one send leaf hash to the Nitro-compatible accumulator.
    pub fn append<J: ArbJournal>(
        &self,
        item_hash: B256,
        journal: &mut J,
    ) -> Result<Vec<SendMerkleUpdateEvent>> {
        let old_size = self.size(journal)?;
        let new_size = old_size.saturating_add(1);
        self.set_size(new_size, journal)?;

        let mut events = Vec::new();
        let mut level = 0_u64;
        let mut so_far = keccak256(item_hash.as_slice());
        let old_partial_count = Self::partial_count_for_size(old_size);

        loop {
            if level == old_partial_count {
                self.set_partial(level, so_far, journal)?;
                break;
            }

            let this_level = self.partial(level, journal)?;
            if this_level == B256::ZERO {
                self.set_partial(level, so_far, journal)?;
                break;
            }

            // Combine hash goes through the journal so a `MeteredJournal` bills it (Nitro's
            // `merkleAccumulator.Keccak` → burner). The leaf hash above stays a raw `keccak256`,
            // matching Nitro's `crypto.Keccak256(itemHash)` which is NOT burner-charged.
            so_far = journal.keccak(&[this_level.as_slice(), so_far.as_slice()]);
            self.set_partial(level, B256::ZERO, journal)?;
            level = level.saturating_add(1);
            events.push(SendMerkleUpdateEvent {
                level,
                num_leaves: new_size.saturating_sub(1),
                hash: so_far,
            });
        }

        Ok(events)
    }

    /// Computes Nitro-compatible send accumulator root from stored partials.
    pub fn root<J: ArbJournal>(&self, journal: &mut J) -> Result<B256> {
        let size = self.size(journal)?;
        if size == 0 {
            return Ok(B256::ZERO);
        }

        let num_partials = Self::partial_count_for_size(size);
        let mut hash_so_far: Option<B256> = None;
        let mut capacity_in_hash = 0_u64;
        let mut capacity = 1_u64;

        for level in 0..num_partials {
            let partial = self.partial(level, journal)?;
            if partial != B256::ZERO {
                if let Some(mut current_hash) = hash_so_far {
                    while capacity_in_hash < capacity {
                        current_hash =
                            keccak_concat(current_hash.as_slice(), B256::ZERO.as_slice());
                        capacity_in_hash = capacity_in_hash.saturating_mul(2);
                    }
                    current_hash = keccak_concat(partial.as_slice(), current_hash.as_slice());
                    hash_so_far = Some(current_hash);
                    capacity_in_hash = capacity.saturating_mul(2);
                } else {
                    hash_so_far = Some(partial);
                    capacity_in_hash = capacity;
                }
            }
            capacity = capacity.saturating_mul(2);
        }

        Ok(hash_so_far.unwrap_or(B256::ZERO))
    }

    pub fn state_for_export<J: ArbJournal>(
        &self,
        journal: &mut J,
    ) -> Result<(u64, B256, Vec<B256>)> {
        let size = self.size(journal)?;
        let root = self.root(journal)?;
        let partial_count = Self::partial_count_for_size(size);
        let mut partials = Vec::with_capacity(partial_count as usize);
        for level in 0..partial_count {
            partials.push(self.partial(level, journal)?);
        }
        Ok((size, root, partials))
    }
}

#[inline]
fn keccak_concat(left: &[u8], right: &[u8]) -> B256 {
    let mut preimage = Vec::with_capacity(left.len() + right.len());
    preimage.extend_from_slice(left);
    preimage.extend_from_slice(right);
    keccak256(preimage)
}
