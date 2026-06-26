use eyre::Result;

use super::{StorageBytes, StorageSpace};
use crate::arb_journal::ArbJournal;

/// ArbOS chain-config blob storage.
#[derive(Debug)]
pub struct ChainConfig {
    bytes: StorageBytes,
}

impl ChainConfig {
    pub fn open(storage: &StorageSpace) -> Self {
        Self {
            bytes: StorageBytes::open(storage),
        }
    }

    pub fn get<J: ArbJournal>(&self, journal: &mut J) -> Result<Vec<u8>> {
        self.bytes.get(journal)
    }

    pub fn set<J: ArbJournal>(&self, value: &[u8], journal: &mut J) -> Result<()> {
        self.bytes.set(value, journal)
    }

    pub fn clear<J: ArbJournal>(&self, journal: &mut J) -> Result<()> {
        self.bytes.clear(journal)
    }

    pub fn size<J: ArbJournal>(&self, journal: &mut J) -> Result<u64> {
        self.bytes.size(journal)
    }
}
