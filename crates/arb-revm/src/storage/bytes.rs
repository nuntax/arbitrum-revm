use eyre::Result;
use revm::{
    context_interface::JournalTr,
    primitives::{FixedBytes, U256},
};

use super::StorageSpace;

/// Nitro-style byte blob encoding in ArbOS storage:
/// - slot 0: length
/// - slots 1..: 32-byte chunks (last chunk right-aligned)
#[derive(Debug)]
pub struct StorageBytes {
    storage: StorageSpace,
}

impl StorageBytes {
    pub fn open(storage: &StorageSpace) -> Self {
        Self {
            storage: storage.clone(),
        }
    }

    pub fn size<J: JournalTr>(&self, journal: &mut J) -> Result<u64> {
        Ok(self
            .storage
            .get_u256(U256::ZERO, journal)?
            .data
            .try_into()?)
    }

    pub fn get<J: JournalTr>(&self, journal: &mut J) -> Result<Vec<u8>> {
        let mut bytes_left = self.size(journal)?;
        if bytes_left == 0 {
            return Ok(Vec::new());
        }

        let mut out = Vec::with_capacity(bytes_left as usize);
        let mut offset = 1_u64;
        while bytes_left >= 32 {
            let word = self.storage.get_u256(U256::from(offset), journal)?.data;
            out.extend_from_slice(&word.to_be_bytes::<32>());
            bytes_left -= 32;
            offset = offset.saturating_add(1);
        }

        if bytes_left > 0 {
            let word = self.storage.get_u256(U256::from(offset), journal)?.data;
            let word_bytes = word.to_be_bytes::<32>();
            out.extend_from_slice(&word_bytes[32 - bytes_left as usize..]);
        }
        Ok(out)
    }

    pub fn clear<J: JournalTr>(&self, journal: &mut J) -> Result<()> {
        let mut bytes_left = self.size(journal)?;
        let mut offset = 1_u64;
        while bytes_left > 0 {
            self.storage.set(
                FixedBytes::from(U256::from(offset).to_be_bytes()),
                U256::ZERO,
                journal,
            )?;
            offset = offset.saturating_add(1);
            bytes_left = bytes_left.saturating_sub(32);
        }
        self.storage.set(
            FixedBytes::from(U256::ZERO.to_be_bytes()),
            U256::ZERO,
            journal,
        )?;
        Ok(())
    }

    pub fn set<J: JournalTr>(&self, value: &[u8], journal: &mut J) -> Result<()> {
        self.clear(journal)?;
        self.storage.set(
            FixedBytes::from(U256::ZERO.to_be_bytes()),
            U256::from(value.len()),
            journal,
        )?;

        let mut offset = 1_u64;
        for chunk in value.chunks(32) {
            let mut word = [0_u8; 32];
            if chunk.len() == 32 {
                word.copy_from_slice(chunk);
            } else {
                word[32 - chunk.len()..].copy_from_slice(chunk);
            }
            self.storage.set(
                FixedBytes::from(U256::from(offset).to_be_bytes()),
                U256::from_be_bytes(word),
                journal,
            )?;
            offset = offset.saturating_add(1);
        }
        Ok(())
    }
}
