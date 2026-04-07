use eyre::Result;
use revm::{context_interface::JournalTr, primitives::U256};

use super::{StorageBacked, StorageSpace};

/// Feature index in Nitro's `features` bitset.
pub const FEATURE_INCREASED_CALLDATA_PRICE: usize = 0;

/// ArbOS feature flags bitset.
#[derive(Debug)]
pub struct ArbFeatures {
    bits: StorageBacked<U256>,
}

impl ArbFeatures {
    pub fn open(storage: &StorageSpace) -> Self {
        Self {
            bits: storage.storage_backed(0),
        }
    }

    pub fn set_feature<J: JournalTr>(
        &self,
        feature_index: usize,
        enabled: bool,
        journal: &mut J,
    ) -> Result<()> {
        let mut bits = self.bits.get(journal)?;
        if enabled {
            bits |= U256::from(1_u8) << feature_index;
        } else {
            bits &= !(U256::from(1_u8) << feature_index);
        }
        self.bits.set(bits, journal)?;
        Ok(())
    }

    pub fn is_feature_enabled<J: JournalTr>(
        &self,
        feature_index: usize,
        journal: &mut J,
    ) -> Result<bool> {
        let bits = self.bits.get(journal)?;
        Ok(bits.bit(feature_index))
    }

    pub fn set_calldata_price_increase<J: JournalTr>(
        &self,
        enabled: bool,
        journal: &mut J,
    ) -> Result<()> {
        self.set_feature(FEATURE_INCREASED_CALLDATA_PRICE, enabled, journal)
    }

    pub fn is_calldata_price_increase_enabled<J: JournalTr>(
        &self,
        journal: &mut J,
    ) -> Result<bool> {
        self.is_feature_enabled(FEATURE_INCREASED_CALLDATA_PRICE, journal)
    }
}
