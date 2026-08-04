use eyre::Result;
use revm::primitives::{B256, FixedBytes, U256};

use super::{StorageBacked, StorageSpace};
use crate::arb_journal::ArbJournal;

const NEXT_PUT_OFFSET: u8 = 0;
const NEXT_GET_OFFSET: u8 = 1;
const QUEUE_INITIAL_OFFSET: u64 = 2;

/// Nitro queue layout used by ArbOS retryables.
#[derive(Debug)]
pub struct StorageQueue {
    storage: StorageSpace,
    next_put_offset: StorageBacked<u64>,
    next_get_offset: StorageBacked<u64>,
}

impl StorageQueue {
    pub fn open(storage: &StorageSpace) -> Self {
        Self {
            storage: storage.clone(),
            next_put_offset: storage.storage_backed(NEXT_PUT_OFFSET),
            next_get_offset: storage.storage_backed(NEXT_GET_OFFSET),
        }
    }

    pub fn initialize<J: ArbJournal>(&self, journal: &mut J) -> Result<()> {
        self.next_put_offset.set(QUEUE_INITIAL_OFFSET, journal)?;
        self.next_get_offset.set(QUEUE_INITIAL_OFFSET, journal)?;
        Ok(())
    }

    /// Initializes queue pointers if the queue has never been initialized.
    pub fn ensure_initialized<J: ArbJournal>(&self, journal: &mut J) -> Result<()> {
        let put = self.next_put_offset.get(journal)?;
        let get = self.next_get_offset.get(journal)?;
        if put == 0 && get == 0 {
            self.initialize(journal)?;
        }
        Ok(())
    }

    pub fn next_put<J: ArbJournal>(&self, journal: &mut J) -> Result<u64> {
        self.next_put_offset.get(journal)
    }

    pub fn next_get<J: ArbJournal>(&self, journal: &mut J) -> Result<u64> {
        self.next_get_offset.get(journal)
    }

    pub fn is_empty<J: ArbJournal>(&self, journal: &mut J) -> Result<bool> {
        let put = self.next_put_offset.get(journal)?;
        let get = self.next_get_offset.get(journal)?;
        Ok(put == get)
    }

    pub fn size<J: ArbJournal>(&self, journal: &mut J) -> Result<u64> {
        let put = self.next_put_offset.get(journal)?;
        let get = self.next_get_offset.get(journal)?;
        Ok(put.saturating_sub(get))
    }

    pub fn peek<J: ArbJournal>(&self, journal: &mut J) -> Result<Option<B256>> {
        if self.is_empty(journal)? {
            return Ok(None);
        }
        let next = self.next_get_offset.get(journal)?;
        Ok(Some(self.get_entry(next, journal)?))
    }

    pub fn get<J: ArbJournal>(&self, journal: &mut J) -> Result<Option<B256>> {
        if self.is_empty(journal)? {
            return Ok(None);
        }
        let current = self.next_get_offset.get(journal)?;
        self.next_get_offset
            .set(current.saturating_add(1), journal)?;
        let entry = self.get_entry(current, journal)?;
        self.set_entry(current, B256::ZERO, journal)?;
        Ok(Some(entry))
    }

    pub fn put<J: ArbJournal>(&self, value: B256, journal: &mut J) -> Result<()> {
        let current = self.next_put_offset.get(journal)?;
        self.next_put_offset
            .set(current.saturating_add(1), journal)?;
        self.set_entry(current, value, journal)?;
        Ok(())
    }

    pub fn shift<J: ArbJournal>(&self, journal: &mut J) -> Result<bool> {
        if !self.is_empty(journal)? {
            return Ok(false);
        }
        self.next_get_offset.set(QUEUE_INITIAL_OFFSET, journal)?;
        self.next_put_offset.set(QUEUE_INITIAL_OFFSET, journal)?;
        Ok(true)
    }

    fn get_entry<J: ArbJournal>(&self, offset: u64, journal: &mut J) -> Result<B256> {
        let value = self.storage.get_u256(U256::from(offset), journal)?.data;
        Ok(B256::from(value.to_be_bytes::<32>()))
    }

    fn set_entry<J: ArbJournal>(&self, offset: u64, value: B256, journal: &mut J) -> Result<()> {
        self.storage.set(
            FixedBytes::from(U256::from(offset).to_be_bytes()),
            U256::from_be_bytes(value.0),
            journal,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use revm::{
        context_interface::ContextTr,
        database_interface::EmptyDB,
        primitives::{B256, U256},
    };

    use super::{QUEUE_INITIAL_OFFSET, StorageQueue};
    use crate::{
        api::default_ctx::{ArbContext, DefaultArb},
        storage::StorageSpace,
    };

    #[test]
    fn queue_round_trip_and_shift_clears_entries() {
        let mut ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();
        let queue = StorageQueue::open(&StorageSpace::arbos().open_subspace_with_key(0xfe));
        let journal = ctx.journal_mut();
        queue.initialize(journal).unwrap();

        assert!(queue.is_empty(journal).unwrap());
        for value in 0_u64..150 {
            queue
                .put(
                    B256::from(U256::from(value + 853_139_508).to_be_bytes()),
                    journal,
                )
                .unwrap();
        }
        assert_eq!(queue.size(journal).unwrap(), 150);

        for value in 0_u64..150 {
            assert_eq!(
                queue.get(journal).unwrap(),
                Some(B256::from(U256::from(value + 853_139_508).to_be_bytes()))
            );
        }
        assert!(queue.is_empty(journal).unwrap());
        assert_eq!(queue.peek(journal).unwrap(), None);
        assert!(queue.shift(journal).unwrap());
        assert_eq!(queue.next_get(journal).unwrap(), QUEUE_INITIAL_OFFSET);
        assert_eq!(queue.next_put(journal).unwrap(), QUEUE_INITIAL_OFFSET);

        for offset in QUEUE_INITIAL_OFFSET..QUEUE_INITIAL_OFFSET + 150 {
            assert_eq!(queue.get_entry(offset, journal).unwrap(), B256::ZERO);
        }
    }
}
