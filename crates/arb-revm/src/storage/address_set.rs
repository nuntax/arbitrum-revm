use eyre::Result;
use revm::primitives::{Address, FixedBytes, U256};

use crate::arb_journal::ArbJournal;
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

    pub fn is_member<J: ArbJournal>(&self, address: Address, journal: &mut J) -> Result<bool> {
        let key = FixedBytes::from(address_to_u256(address).to_be_bytes());
        Ok(self.by_address.get(key, journal)?.data != U256::ZERO)
    }

    pub fn add<J: ArbJournal>(&self, address: Address, journal: &mut J) -> Result<()> {
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

    /// Returns all members of the set in storage order (1-indexed).
    pub fn all_members<J: ArbJournal>(&self, journal: &mut J) -> Result<Vec<Address>> {
        let size = self.size.get(journal)?;
        let mut members = Vec::with_capacity(size as usize);
        for i in 1..=size {
            let raw = self.backing.get_u256(U256::from(i), journal)?.data;
            let bytes = raw.to_be_bytes::<32>();
            members.push(Address::from_slice(&bytes[12..]));
        }
        Ok(members)
    }

    /// Clears the list (the 1-indexed slots and the size), leaving the by-address mapping intact.
    /// Mirrors Nitro `AddressSet.ClearList`: it zeroes each list slot `1..=size` and resets the
    /// size to 0, so the members remain resolvable via the mapping until it is rectified. The v11
    /// ArbOS upgrade calls this to allow later rectification of the chain-owners mapping.
    pub fn clear_list<J: ArbJournal>(&self, journal: &mut J) -> Result<()> {
        let size = self.size.get(journal)?;
        if size == 0 {
            return Ok(());
        }
        for i in 1..=size {
            self.backing.set(
                FixedBytes::from(U256::from(i).to_be_bytes()),
                U256::ZERO,
                journal,
            )?;
        }
        self.size.set(0, journal)?;
        Ok(())
    }

    pub fn remove<J: ArbJournal>(&self, address: Address, journal: &mut J) -> Result<()> {
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
