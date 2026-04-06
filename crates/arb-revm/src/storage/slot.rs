use core::marker::PhantomData;

use eyre::{Result, WrapErr};
use revm::{
    context_interface::{JournalTr, context::SStoreResult, journaled_state::StateLoad},
    primitives::{Address, FixedBytes, I256, StorageValue, U256},
};

use crate::util::{i256_to_u256_twos_complement, u256_twos_complement_to_i256};

/// Concrete `(account, slot)` pointer into state.
#[derive(Clone, Debug)]
pub struct StorageSlot {
    account: Address,
    slot: FixedBytes<32>,
}

impl StorageSlot {
    pub fn new(account: Address, slot: FixedBytes<32>) -> Self {
        Self { account, slot }
    }

    pub fn account(&self) -> Address {
        self.account
    }

    pub fn slot(&self) -> FixedBytes<32> {
        self.slot
    }

    pub fn get_inner<J: JournalTr>(&self, journal: &mut J) -> Result<StateLoad<StorageValue>> {
        journal
            .load_account(self.account)
            .wrap_err("failed to warm ArbOS storage account")?;
        journal
            .sload(self.account, self.slot.into())
            .wrap_err("failed to read ArbOS storage slot")
    }

    pub fn set_inner<J: JournalTr>(
        &self,
        value: U256,
        journal: &mut J,
    ) -> Result<StateLoad<SStoreResult>> {
        journal
            .sstore(self.account, self.slot.into(), value)
            .wrap_err("failed to write ArbOS storage slot")
    }
}

/// Typed view over an ArbOS storage slot.
#[derive(Debug)]
pub struct StorageBacked<T> {
    slot: StorageSlot,
    marker: PhantomData<T>,
}

impl<T> StorageBacked<T> {
    pub fn new(account: Address, slot: FixedBytes<32>) -> Self {
        Self {
            slot: StorageSlot::new(account, slot),
            marker: PhantomData,
        }
    }

    pub fn account_and_key(&self) -> (Address, FixedBytes<32>) {
        (self.slot.account(), self.slot.slot())
    }

    pub fn slot(&self) -> &StorageSlot {
        &self.slot
    }
}

impl StorageBacked<U256> {
    pub fn get<J: JournalTr>(&self, journal: &mut J) -> Result<U256> {
        Ok(*self.slot.get_inner(journal)?)
    }

    pub fn set<J: JournalTr>(
        &self,
        value: U256,
        journal: &mut J,
    ) -> Result<StateLoad<SStoreResult>> {
        self.slot.set_inner(value, journal)
    }
}

impl StorageBacked<Address> {
    pub fn get<J: JournalTr>(&self, journal: &mut J) -> Result<Address> {
        let bytes = self.slot.get_inner(journal)?.to_be_bytes::<32>();
        Ok(Address::from_slice(&bytes[12..]))
    }

    pub fn set<J: JournalTr>(
        &self,
        address: Address,
        journal: &mut J,
    ) -> Result<StateLoad<SStoreResult>> {
        let mut bytes = [0_u8; 32];
        bytes[12..].copy_from_slice(address.as_slice());
        self.slot.set_inner(U256::from_be_bytes(bytes), journal)
    }
}

impl StorageBacked<u64> {
    pub fn get<J: JournalTr>(&self, journal: &mut J) -> Result<u64> {
        Ok((*self.slot.get_inner(journal)?).try_into()?)
    }

    pub fn set<J: JournalTr>(
        &self,
        value: u64,
        journal: &mut J,
    ) -> Result<StateLoad<SStoreResult>> {
        self.slot.set_inner(U256::from(value), journal)
    }
}

impl StorageBacked<I256> {
    pub fn get<J: JournalTr>(&self, journal: &mut J) -> Result<I256> {
        Ok(u256_twos_complement_to_i256(*self.slot.get_inner(journal)?))
    }

    pub fn set_checked<J: JournalTr>(
        &self,
        value: I256,
        journal: &mut J,
    ) -> Result<StateLoad<SStoreResult>> {
        if value < I256::ZERO {
            let raw = i256_to_u256_twos_complement(value);
            if raw.bit_len() < 256 || !raw.bit(255) {
                eyre::bail!("underflow in signed ArbOS slot write: {value}");
            }
            self.slot.set_inner(raw, journal)
        } else {
            let raw = U256::from(value);
            if raw.bit_len() >= 256 {
                eyre::bail!("overflow in signed ArbOS slot write: {value}");
            }
            self.slot.set_inner(raw, journal)
        }
    }

    pub fn set_saturating_with_warning<J: JournalTr>(
        &self,
        value: I256,
        name: &'static str,
        journal: &mut J,
    ) -> Result<StateLoad<SStoreResult>> {
        let min = U256::ONE << 255;
        let max = min - U256::ONE;

        if value < I256::ZERO {
            let raw = i256_to_u256_twos_complement(value);
            if raw.bit_len() < 256 || !raw.bit(255) {
                tracing::warn!("ArbOS signed slot underflowed name={name} value={value}");
                self.slot.set_inner(min, journal)
            } else {
                self.slot.set_inner(raw, journal)
            }
        } else {
            let raw = U256::from(value);
            if raw.bit_len() >= 256 {
                tracing::warn!("ArbOS signed slot overflowed name={name} value={value}");
                self.slot.set_inner(max, journal)
            } else {
                self.slot.set_inner(raw, journal)
            }
        }
    }

    pub fn set_pre_version7<J: JournalTr>(
        &self,
        value: I256,
        journal: &mut J,
    ) -> Result<StateLoad<SStoreResult>> {
        let magnitude = if value < I256::ZERO {
            U256::from(-value)
        } else {
            U256::from(value)
        };
        self.slot.set_inner(magnitude, journal)
    }

    pub fn set_by_uint<J: JournalTr>(
        &self,
        value: u64,
        journal: &mut J,
    ) -> Result<StateLoad<SStoreResult>> {
        self.slot.set_inner(U256::from(value), journal)
    }
}
