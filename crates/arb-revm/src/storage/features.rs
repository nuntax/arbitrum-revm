use eyre::Result;
use revm::primitives::U256;

use super::{StorageBacked, StorageSpace};
use crate::arb_journal::ArbJournal;

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

    pub fn set_feature<J: ArbJournal>(
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

    pub fn is_feature_enabled<J: ArbJournal>(
        &self,
        feature_index: usize,
        journal: &mut J,
    ) -> Result<bool> {
        let bits = self.bits.get(journal)?;
        Ok(bits.bit(feature_index))
    }

    pub fn set_calldata_price_increase<J: ArbJournal>(
        &self,
        enabled: bool,
        journal: &mut J,
    ) -> Result<()> {
        self.set_feature(FEATURE_INCREASED_CALLDATA_PRICE, enabled, journal)
    }

    pub fn is_calldata_price_increase_enabled<J: ArbJournal>(
        &self,
        journal: &mut J,
    ) -> Result<bool> {
        self.is_feature_enabled(FEATURE_INCREASED_CALLDATA_PRICE, journal)
    }

    /// Reads the calldata-price-increase (EIP-7623) feature flag directly from the database,
    /// for configuring the EVM `CfgEnv` before the journal exists. Returns `false` (feature
    /// off) when the slot is unreadable / uninitialized, matching Nitro, which only applies
    /// the EIP-7623 floor when this ArbOS feature is explicitly enabled.
    pub fn read_calldata_price_increase_db<DB: revm::Database>(&self, db: &mut DB) -> bool {
        let (account, slot) = self.bits.account_and_key();
        db.storage(account, U256::from_be_bytes(slot.0))
            .map(|bits| bits.bit(FEATURE_INCREASED_CALLDATA_PRICE))
            .unwrap_or(false)
    }
}
