use eyre::Result;
use revm::primitives::{Address, Bytes, U256};

use super::{AddressSet, StorageBacked, StorageSpace};
use crate::arb_journal::ArbJournal;

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

    pub fn total_funds_due<J: ArbJournal>(&self, journal: &mut J) -> Result<U256> {
        self.total_funds_due.get(journal)
    }

    pub fn add_poster<'a, J: ArbJournal>(
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

    pub fn open_poster_checked<'a, J: ArbJournal>(
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
            eyre::bail!("poster not registered in ArbOS batch poster table");
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
    pub fn funds_due<J: ArbJournal>(&self, journal: &mut J) -> Result<U256> {
        self.funds_due.get(journal)
    }

    pub fn pay_to<J: ArbJournal>(&self, journal: &mut J) -> Result<Address> {
        self.pay_to.get(journal)
    }

    pub fn set_pay_to<J: ArbJournal>(&self, pay_to: Address, journal: &mut J) -> Result<()> {
        self.pay_to.set(pay_to, journal)?;
        Ok(())
    }

    pub fn set_funds_due<J: ArbJournal>(&self, funds_due: U256, journal: &mut J) -> Result<()> {
        let prev_funds_due = self.funds_due.get(journal)?;
        let prev_total_funds_due = self.posters_table.total_funds_due.get(journal)?;
        let next_total_funds_due = prev_total_funds_due
            .checked_add(funds_due)
            .and_then(|sum| sum.checked_sub(prev_funds_due))
            .unwrap_or(U256::MAX);

        self.posters_table
            .total_funds_due
            .set(next_total_funds_due, journal)?;
        self.funds_due.set(funds_due, journal)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use revm::{
        context_interface::ContextTr,
        database_interface::EmptyDB,
        primitives::{Address, U256, address},
    };

    use super::BatchPosterTable;
    use crate::{
        api::default_ctx::{ArbContext, DefaultArb},
        storage::StorageSpace,
    };

    #[test]
    fn poster_registration_and_total_funds_due_round_trip() {
        const POSTER_1: Address = address!("0000000000000000000000000000000000010203");
        const PAY_TO_1: Address = address!("0000000000000000000000000000000004050607");
        const POSTER_2: Address = address!("0000000000000000000000000000000000020406");
        const PAY_TO_2: Address = address!("00000000000000000000000000000000080a0c0e");

        let mut ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();
        let table = BatchPosterTable::open(&StorageSpace::arbos().open_subspace_with_key(0xfd));
        let journal = ctx.journal_mut();

        assert!(
            !table
                .poster_address_set
                .is_member(POSTER_1, journal)
                .unwrap()
        );
        let poster1 = table.add_poster(POSTER_1, PAY_TO_1, journal).unwrap();
        assert_eq!(poster1.pay_to(journal).unwrap(), PAY_TO_1);
        assert_eq!(poster1.funds_due(journal).unwrap(), U256::ZERO);
        assert!(
            table
                .poster_address_set
                .is_member(POSTER_1, journal)
                .unwrap()
        );

        let poster2 = table.add_poster(POSTER_2, PAY_TO_2, journal).unwrap();
        poster1.set_pay_to(POSTER_2, journal).unwrap();
        assert_eq!(poster1.pay_to(journal).unwrap(), POSTER_2);

        poster1.set_funds_due(U256::from(13), journal).unwrap();
        poster2.set_funds_due(U256::from(42), journal).unwrap();
        assert_eq!(table.total_funds_due(journal).unwrap(), U256::from(55));

        poster1.set_funds_due(U256::from(5), journal).unwrap();
        assert_eq!(table.total_funds_due(journal).unwrap(), U256::from(47));
    }
}
