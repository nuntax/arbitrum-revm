use eyre::Result;
use revm::{
    context_interface::JournalTr,
    primitives::{Address, FixedBytes, U256},
};

use crate::util::address_to_u256;

use super::{StorageBacked, StorageSpace};

/// ArbOS `AddressSet` helper with stable slot addressing.
#[derive(Debug)]
pub struct AddressSet {
    backing: StorageSpace,
    pub size: StorageBacked<u64>,
    by_address: StorageSpace,
}

impl AddressSet {
    pub fn open(backing: &StorageSpace) -> Self {
        Self {
            backing: backing.clone(),
            size: backing.storage_backed(0),
            by_address: backing.open_subspace_with_key(0),
        }
    }

    pub fn is_member<J: JournalTr>(&self, address: Address, journal: &mut J) -> Result<bool> {
        let key = FixedBytes::from(address_to_u256(address).to_be_bytes());
        Ok(self.by_address.get(key, journal)?.data != U256::ZERO)
    }

    pub fn add<J: JournalTr>(&self, address: Address, journal: &mut J) -> Result<()> {
        if self.is_member(address, journal)? {
            return Ok(());
        }

        let address_value = address_to_u256(address);
        let mut size = self.size.get(journal)?;
        size = size.saturating_add(1);

        self.by_address.set(
            FixedBytes::from(address_value.to_be_bytes()),
            U256::from(size),
            journal,
        )?;
        self.backing.set(
            FixedBytes::from(U256::from(size).to_be_bytes()),
            address_value,
            journal,
        )?;
        self.size.set(size, journal)?;

        Ok(())
    }

    pub fn remove<J: JournalTr>(&self, address: Address, journal: &mut J) -> Result<()> {
        let address_value = address_to_u256(address);
        let position = self
            .by_address
            .get(FixedBytes::from(address_value.to_be_bytes()), journal)?
            .data;

        if position == U256::ZERO {
            return Ok(());
        }

        let mut size = self.size.get(journal)?;
        if position != U256::from(size) {
            let last_address = self.backing.get_u256(U256::from(size), journal)?.data;
            self.by_address.set(
                FixedBytes::from(last_address.to_be_bytes()),
                position,
                journal,
            )?;
            self.backing.set(
                FixedBytes::from(position.to_be_bytes()),
                last_address,
                journal,
            )?;
        }

        self.by_address.set(
            FixedBytes::from(address_value.to_be_bytes()),
            U256::ZERO,
            journal,
        )?;
        self.backing.set(
            FixedBytes::from(U256::from(size).to_be_bytes()),
            U256::ZERO,
            journal,
        )?;
        size = size.saturating_sub(1);
        self.size.set(size, journal)?;

        Ok(())
    }
}
