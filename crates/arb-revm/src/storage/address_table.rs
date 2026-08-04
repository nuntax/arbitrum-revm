use alloy_rlp::{Decodable, Header};
use eyre::{Result, eyre};
use revm::primitives::{Address, Bytes, FixedBytes, U256};

use super::{StorageBacked, StorageSpace};
use crate::arb_journal::ArbJournal;

/// ArbOS address table wrapper.
#[derive(Debug)]
pub struct AddressTable {
    backing_storage: StorageSpace,
    by_address: StorageSpace,
    num_items: StorageBacked<u64>,
}

impl AddressTable {
    pub fn open(backing: StorageSpace) -> Self {
        Self {
            num_items: backing.storage_backed(0),
            by_address: backing.open_subspace(Bytes::new()),
            backing_storage: backing,
        }
    }

    pub fn register<J: ArbJournal>(&self, address: Address, journal: &mut J) -> Result<u64> {
        let mut bytes = [0_u8; 32];
        bytes[12..].copy_from_slice(address.as_slice());
        let key = FixedBytes::<32>::from(bytes);

        let existing = *self.by_address.get(key, journal)?;
        if existing != U256::ZERO {
            return Ok(u64::try_from(existing)? - 1);
        }

        let new_len = self.num_items.get(journal)? + 1;
        // forward map: index (1-based) -> address
        self.backing_storage.set(
            FixedBytes::from(U256::from(new_len).to_be_bytes()),
            U256::from_be_bytes(bytes),
            journal,
        )?;
        // reverse map: address -> index (1-based)
        self.by_address.set(key, U256::from(new_len), journal)?;
        // persist table length at slot 0
        self.num_items.set(new_len, journal)?;
        Ok(new_len - 1)
    }

    pub fn lookup<J: ArbJournal>(&self, address: Address, journal: &mut J) -> Result<Option<u64>> {
        let mut bytes = [0_u8; 32];
        bytes[12..].copy_from_slice(address.as_slice());
        let key = FixedBytes::<32>::from(bytes);
        let stored = *self.by_address.get(key, journal)?;
        if stored == U256::ZERO {
            Ok(None)
        } else {
            Ok(Some(u64::try_from(stored)? - 1))
        }
    }

    pub fn len<J: ArbJournal>(&self, journal: &mut J) -> Result<u64> {
        self.num_items.get(journal)
    }

    pub fn lookup_index<J: ArbJournal>(
        &self,
        index: u64,
        journal: &mut J,
    ) -> Result<Option<Address>> {
        let len = self.num_items.get(journal)?;
        if index >= len {
            return Ok(None);
        }
        let stored = self
            .backing_storage
            .get_u256(U256::from(index + 1), journal)?
            .data
            .to_be_bytes::<32>();
        Ok(Some(Address::from_slice(&stored[12..])))
    }

    pub fn compress<J: ArbJournal>(&self, address: Address, journal: &mut J) -> Result<Vec<u8>> {
        if let Some(index) = self.lookup(address, journal)? {
            Ok(alloy_rlp::encode(index))
        } else {
            Ok(alloy_rlp::encode(address.as_slice()))
        }
    }

    pub fn decompress<J: ArbJournal>(
        &self,
        input: &[u8],
        journal: &mut J,
    ) -> Result<(Address, usize)> {
        let mut payload = input;
        let header = Header::decode(&mut payload)?;
        if header.list {
            return Err(eyre!("compressed address must be an RLP string"));
        }

        if header.payload_length == 20 {
            let address_bytes = payload
                .get(..20)
                .ok_or_else(|| eyre!("truncated compressed address"))?;
            let consumed = input.len() - payload.len() + 20;
            return Ok((Address::from_slice(address_bytes), consumed));
        }

        let mut encoded_index = input;
        let index = u64::decode(&mut encoded_index)?;
        let consumed = input.len() - encoded_index.len();
        let address = self
            .lookup_index(index, journal)?
            .ok_or_else(|| eyre!("invalid index in compressed address"))?;
        Ok((address, consumed))
    }
}

#[cfg(test)]
mod tests {
    use revm::{
        context_interface::ContextTr,
        database_interface::EmptyDB,
        primitives::{Address, address},
    };

    use super::AddressTable;
    use crate::{
        api::default_ctx::{ArbContext, DefaultArb},
        storage::{StorageSpace, Subspace},
    };

    const ACCOUNT: Address = address!("c5d2460186f7233c927e7db2dcc703c0e500b653");

    fn fresh() -> (ArbContext<EmptyDB>, AddressTable) {
        let ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();
        let table = AddressTable::open(
            StorageSpace::arbos().open_subspace_with_key(Subspace::AddressTable as u8),
        );
        (ctx, table)
    }

    #[test]
    fn starts_empty_and_registers_idempotently() {
        let (mut ctx, table) = fresh();
        let journal = ctx.journal_mut();

        assert_eq!(table.len(journal).unwrap(), 0);
        assert_eq!(table.lookup(ACCOUNT, journal).unwrap(), None);
        assert_eq!(table.lookup_index(0, journal).unwrap(), None);

        assert_eq!(table.register(ACCOUNT, journal).unwrap(), 0);
        assert_eq!(table.register(ACCOUNT, journal).unwrap(), 0);
        assert_eq!(table.len(journal).unwrap(), 1);
        assert_eq!(table.lookup(ACCOUNT, journal).unwrap(), Some(0));
        assert_eq!(table.lookup_index(0, journal).unwrap(), Some(ACCOUNT));
        assert_eq!(table.lookup_index(1, journal).unwrap(), None);
    }

    #[test]
    fn compresses_and_decompresses_unregistered_address() {
        let (mut ctx, table) = fresh();
        let encoded = table.compress(ACCOUNT, ctx.journal_mut()).unwrap();

        assert_eq!(encoded.len(), 21);
        assert_eq!(encoded[0], 0x94);
        assert_eq!(&encoded[1..], ACCOUNT.as_slice());
        assert_eq!(
            table.decompress(&encoded, ctx.journal_mut()).unwrap(),
            (ACCOUNT, 21)
        );
    }

    #[test]
    fn compresses_and_decompresses_registered_address() {
        let (mut ctx, table) = fresh();
        table.register(ACCOUNT, ctx.journal_mut()).unwrap();
        let encoded = table.compress(ACCOUNT, ctx.journal_mut()).unwrap();

        assert_eq!(encoded, vec![0x80]);
        assert_eq!(
            table.decompress(&encoded, ctx.journal_mut()).unwrap(),
            (ACCOUNT, 1)
        );
    }
}
