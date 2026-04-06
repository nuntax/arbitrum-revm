use eyre::Result;
use revm::{
    context_interface::JournalTr,
    primitives::{Address, Bytes, U256},
};

use super::{AddressSet, StorageBacked, StorageSpace};

const POSTER_ADDR_SUBSPACE: u8 = 0;
const POSTER_INFO_SUBSPACE: u8 = 1;
const TOTAL_FUNDS_DUE_OFFSET: u8 = 0;

/// ArbOS batch-poster table.
#[derive(Debug)]
pub struct BatchPosterTable {
    pub poster_info: StorageSpace,
    pub poster_address_set: AddressSet,
    pub total_funds_due: StorageBacked<U256>,
}

/// Batch-poster scoped view.
pub struct BatchPosterState<'a> {
    pub funds_due: StorageBacked<U256>,
    pub pay_to: StorageBacked<Address>,
    pub posters_table: &'a BatchPosterTable,
}

impl BatchPosterTable {
    pub fn open(storage: &StorageSpace) -> Self {
        Self {
            poster_info: storage.open_subspace_with_key(POSTER_INFO_SUBSPACE),
            poster_address_set: AddressSet::open(
                &storage.open_subspace_with_key(POSTER_ADDR_SUBSPACE),
            ),
            total_funds_due: storage.storage_backed(TOTAL_FUNDS_DUE_OFFSET),
        }
    }

    pub fn total_funds_due<J: JournalTr>(&self, journal: &mut J) -> Result<U256> {
        self.total_funds_due.get(journal)
    }

    pub fn add_poster<'a, J: JournalTr>(
        &'a self,
        poster: Address,
        pay_to: Address,
        journal: &mut J,
    ) -> Result<BatchPosterState<'a>> {
        if self.poster_address_set.is_member(poster, journal)? {
            eyre::bail!("poster already registered in ArbOS batch poster table");
        }

        let state = self.internal_open(poster);
        state.funds_due.set(U256::ZERO, journal)?;
        state.pay_to.set(pay_to, journal)?;
        self.poster_address_set.add(poster, journal)?;
        Ok(state)
    }

    pub fn open_poster_checked<'a, J: JournalTr>(
        &'a self,
        poster: Address,
        journal: &mut J,
        create_if_missing: bool,
    ) -> Result<BatchPosterState<'a>> {
        if self.poster_address_set.is_member(poster, journal)? {
            Ok(self.internal_open(poster))
        } else if create_if_missing {
            self.add_poster(poster, poster, journal)
        } else {
            eyre::bail!("poster not registered in ArbOS batch poster table")
        }
    }

    fn internal_open<'a>(&'a self, poster: Address) -> BatchPosterState<'a> {
        let poster_storage = self
            .poster_info
            .open_subspace(Bytes::copy_from_slice(poster.as_slice()));
        BatchPosterState {
            funds_due: poster_storage.storage_backed(0),
            pay_to: poster_storage.storage_backed(1),
            posters_table: self,
        }
    }
}

impl BatchPosterState<'_> {
    pub fn funds_due<J: JournalTr>(&self, journal: &mut J) -> Result<U256> {
        self.funds_due.get(journal)
    }

    pub fn pay_to<J: JournalTr>(&self, journal: &mut J) -> Result<Address> {
        self.pay_to.get(journal)
    }

    pub fn set_pay_to<J: JournalTr>(&self, pay_to: Address, journal: &mut J) -> Result<()> {
        self.pay_to.set(pay_to, journal)?;
        Ok(())
    }

    pub fn set_funds_due<J: JournalTr>(&self, funds_due: U256, journal: &mut J) -> Result<()> {
        self.funds_due.set(funds_due, journal)?;
        Ok(())
    }
}
