use eyre::{Result, eyre};
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

    /// Clears both the list and every reverse-map entry.
    pub fn clear<J: ArbJournal>(&self, journal: &mut J) -> Result<()> {
        let size = self.size.get(journal)?;
        if size == 0 {
            return Ok(());
        }
        for i in 1..=size {
            let position = FixedBytes::from(U256::from(i).to_be_bytes());
            let address = self.backing.get(position, journal)?.data;
            self.backing.set(position, U256::ZERO, journal)?;
            self.by_address
                .set(FixedBytes::from(address.to_be_bytes()), U256::ZERO, journal)?;
        }
        self.size.set(0, journal)?;
        Ok(())
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

    /// Repairs a member whose reverse-map position no longer points at its list entry.
    ///
    /// ArbOS 11 cleared the chain-owner list while retaining its historically stale reverse map.
    /// Owners subsequently call this operation to repopulate the list with correct positions.
    pub fn rectify_mapping<J: ArbJournal>(&self, address: Address, journal: &mut J) -> Result<()> {
        if !self.is_member(address, journal)? {
            return Err(eyre!("RectifyMapping: Address is not an owner"));
        }

        let address_value = address_to_u256(address);
        let key = FixedBytes::from(address_value.to_be_bytes());
        let position = self.by_address.get(key, journal)?.data;
        let at_position = self.backing.get_u256(position, journal)?.data;
        let size = self.size.get(journal)?;

        if at_position == address_value && position <= U256::from(size) {
            return Err(eyre!("RectifyMapping: Owner address is correctly mapped"));
        }

        self.by_address.set(key, U256::ZERO, journal)?;
        self.add(address, journal)
    }

    pub fn remove<J: ArbJournal>(
        &self,
        address: Address,
        arbos_version: u64,
        journal: &mut J,
    ) -> Result<()> {
        let address_value = address_to_u256(address);
        let position = self
            .by_address
            .get(FixedBytes::from(address_value.to_be_bytes()), journal)?
            .data;

        if position == U256::ZERO {
            return Ok(());
        }

        self.by_address.set(
            FixedBytes::from(address_value.to_be_bytes()),
            U256::ZERO,
            journal,
        )?;

        let mut size = self.size.get(journal)?;
        if position < U256::from(size) {
            let last_address = self.backing.get_u256(U256::from(size), journal)?.data;
            self.backing.set(
                FixedBytes::from(position.to_be_bytes()),
                last_address,
                journal,
            )?;
            // ArbOS 10 and earlier failed to update the moved member's reverse mapping. This
            // historical stale index is consensus-visible and was repaired starting in ArbOS 11.
            if arbos_version >= 11 {
                self.by_address.set(
                    FixedBytes::from(last_address.to_be_bytes()),
                    position,
                    journal,
                )?;
            }
        }

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

#[cfg(test)]
mod tests {
    use revm::{
        context_interface::ContextTr,
        database_interface::EmptyDB,
        primitives::{Address, FixedBytes, U256, address},
    };

    use super::AddressSet;
    use crate::{
        api::default_ctx::{ArbContext, DefaultArb},
        storage::{StorageSpace, Subspace},
        util::address_to_u256,
    };

    const ARB1_OWNER_1: Address = address!("d345e41ae2cb00311956aa7109fc801ae8c81a52");
    const ARB1_OWNER_2: Address = address!("98e4db7e07e584f89a2f6043e7b7c89dc27769ed");
    const ARB1_OWNER_3: Address = address!("cf57572261c7c2bcf21ffd220ea7d1a27d40a827");

    fn fresh() -> (ArbContext<EmptyDB>, AddressSet) {
        let ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();
        let set = AddressSet::open(
            &StorageSpace::arbos().open_subspace_with_key(Subspace::ChainOwners as u8),
        );
        (ctx, set)
    }

    fn member_position(set: &AddressSet, address: Address, ctx: &mut ArbContext<EmptyDB>) -> U256 {
        let key = FixedBytes::from(address_to_u256(address).to_be_bytes());
        set.by_address.get(key, ctx.journal_mut()).unwrap().data
    }

    fn apply_arb1_owner_history(version: u64) -> (ArbContext<EmptyDB>, AddressSet) {
        let (mut ctx, set) = fresh();
        let journal = ctx.journal_mut();
        set.add(ARB1_OWNER_1, journal).unwrap();
        set.add(ARB1_OWNER_2, journal).unwrap();
        set.add(ARB1_OWNER_3, journal).unwrap();
        set.remove(ARB1_OWNER_1, version, journal).unwrap();
        set.remove(ARB1_OWNER_2, version, journal).unwrap();
        (ctx, set)
    }

    fn assert_consistent(set: &AddressSet, possible: &[Address], ctx: &mut ArbContext<EmptyDB>) {
        let members = set.all_members(ctx.journal_mut()).unwrap();
        let mut unique = members.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(members.len(), unique.len());
        assert!(members.iter().all(|address| possible.contains(address)));
        for address in possible {
            assert_eq!(
                set.is_member(*address, ctx.journal_mut()).unwrap(),
                members.contains(address)
            );
        }
    }

    fn rectify_all(
        set: &AddressSet,
        owners: &[Address],
        clear_list: bool,
        ctx: &mut ArbContext<EmptyDB>,
    ) {
        if clear_list {
            set.clear_list(ctx.journal_mut()).unwrap();
        }
        for (index, owner) in owners.iter().enumerate() {
            set.rectify_mapping(*owner, ctx.journal_mut()).unwrap();
            assert_eq!(member_position(set, *owner, ctx), U256::from(index + 1));
        }
        assert_eq!(set.all_members(ctx.journal_mut()).unwrap(), owners);
    }

    #[test]
    fn empty_add_remove_and_all_members_match_nitro() {
        let (mut ctx, set) = fresh();
        let possible = [ARB1_OWNER_1, ARB1_OWNER_2, ARB1_OWNER_3];

        assert_eq!(set.size.get(ctx.journal_mut()).unwrap(), 0);
        assert!(!set.is_member(Address::ZERO, ctx.journal_mut()).unwrap());
        set.remove(Address::ZERO, 11, ctx.journal_mut()).unwrap();

        set.add(ARB1_OWNER_1, ctx.journal_mut()).unwrap();
        set.add(ARB1_OWNER_2, ctx.journal_mut()).unwrap();
        set.add(ARB1_OWNER_1, ctx.journal_mut()).unwrap();
        assert_eq!(set.size.get(ctx.journal_mut()).unwrap(), 2);
        assert_consistent(&set, &possible, &mut ctx);

        set.remove(ARB1_OWNER_1, 11, ctx.journal_mut()).unwrap();
        set.add(ARB1_OWNER_3, ctx.journal_mut()).unwrap();
        set.remove(ARB1_OWNER_3, 11, ctx.journal_mut()).unwrap();
        set.add(ARB1_OWNER_1, ctx.journal_mut()).unwrap();
        assert_consistent(&set, &possible, &mut ctx);
        assert_eq!(set.all_members(ctx.journal_mut()).unwrap().len(), 2);

        set.clear(ctx.journal_mut()).unwrap();
        assert_eq!(set.size.get(ctx.journal_mut()).unwrap(), 0);
        assert!(set.all_members(ctx.journal_mut()).unwrap().is_empty());
        assert!(
            possible
                .iter()
                .all(|address| !set.is_member(*address, ctx.journal_mut()).unwrap())
        );
    }

    #[test]
    fn deterministic_add_remove_sequence_keeps_membership_and_list_in_sync() {
        let (mut ctx, set) = fresh();
        let possible = [ARB1_OWNER_1, ARB1_OWNER_2, ARB1_OWNER_3];
        let mut state = 0x4d59_5df4_d0f3_3173_u64;

        for _ in 0..512 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let address = possible[(state as usize) % possible.len()];
            if state & 1 == 0 {
                set.add(address, ctx.journal_mut()).unwrap();
            } else {
                set.remove(address, 11, ctx.journal_mut()).unwrap();
            }
            assert_consistent(&set, &possible, &mut ctx);
        }
    }

    #[test]
    fn arbos_10_preserves_historical_stale_member_position() {
        let (mut ctx, set) = apply_arb1_owner_history(10);

        assert_eq!(set.size.get(ctx.journal_mut()).unwrap(), 1);
        assert_eq!(
            set.all_members(ctx.journal_mut()).unwrap(),
            vec![ARB1_OWNER_3]
        );
        assert!(set.is_member(ARB1_OWNER_3, ctx.journal_mut()).unwrap());
        assert_eq!(member_position(&set, ARB1_OWNER_3, &mut ctx), U256::from(3));
    }

    #[test]
    fn arbos_11_updates_moved_member_position() {
        let (mut ctx, set) = apply_arb1_owner_history(11);

        assert_eq!(set.size.get(ctx.journal_mut()).unwrap(), 1);
        assert_eq!(
            set.all_members(ctx.journal_mut()).unwrap(),
            vec![ARB1_OWNER_3]
        );
        assert!(set.is_member(ARB1_OWNER_3, ctx.journal_mut()).unwrap());
        assert_eq!(member_position(&set, ARB1_OWNER_3, &mut ctx), U256::from(1));
    }

    #[test]
    fn rectifies_arbos_10_chain_owner_history_after_v11_list_clear() {
        let (mut ctx, set) = apply_arb1_owner_history(10);

        set.clear_list(ctx.journal_mut()).unwrap();
        assert_eq!(set.size.get(ctx.journal_mut()).unwrap(), 0);
        assert!(set.is_member(ARB1_OWNER_3, ctx.journal_mut()).unwrap());

        set.rectify_mapping(ARB1_OWNER_3, ctx.journal_mut())
            .unwrap();
        assert_eq!(set.size.get(ctx.journal_mut()).unwrap(), 1);
        assert_eq!(
            set.all_members(ctx.journal_mut()).unwrap(),
            vec![ARB1_OWNER_3]
        );
        assert_eq!(member_position(&set, ARB1_OWNER_3, &mut ctx), U256::from(1));

        assert!(
            set.rectify_mapping(ARB1_OWNER_3, ctx.journal_mut())
                .is_err()
        );
        assert!(
            set.rectify_mapping(Address::with_last_byte(0xff), ctx.journal_mut())
                .is_err()
        );
    }

    #[test]
    fn rectifies_nova_arb1_and_goerli_historical_owner_sequences() {
        const NOVA: [Address; 4] = [
            address!("9C040726F2A657226Ed95712245DeE84b650A1b5"),
            address!("d345e41ae2cb00311956aa7109fc801ae8c81a52"),
            address!("d0749b3e537ed52de4e6a3ae1eb6fc26059d0895"),
            address!("86a02dd71363c440b21f4c0e5b2ad01ffe1a7482"),
        ];
        const GOERLI: [Address; 4] = [
            address!("186B56023d42B2B4E7616589a5C62EEf5FCa21DD"),
            address!("c8efdb677afeb775ce1617dd976b56b3a6e95bba"),
            address!("c3f86bb81e32295d29c288ffb4828936538cf326"),
            address!("67acb531a05160a81dcd03079347f264c4fa2da3"),
        ];

        let (mut ctx, set) = fresh();
        set.add(NOVA[0], ctx.journal_mut()).unwrap();
        set.add(NOVA[1], ctx.journal_mut()).unwrap();
        set.remove(NOVA[0], 10, ctx.journal_mut()).unwrap();
        set.add(NOVA[2], ctx.journal_mut()).unwrap();
        set.add(NOVA[3], ctx.journal_mut()).unwrap();
        set.remove(NOVA[1], 10, ctx.journal_mut()).unwrap();
        set.remove(NOVA[2], 10, ctx.journal_mut()).unwrap();
        assert_eq!(set.all_members(ctx.journal_mut()).unwrap(), vec![NOVA[1]]);
        assert!(!set.is_member(NOVA[1], ctx.journal_mut()).unwrap());
        assert!(set.is_member(NOVA[3], ctx.journal_mut()).unwrap());
        rectify_all(&set, &[NOVA[3]], true, &mut ctx);

        set.clear(ctx.journal_mut()).unwrap();
        for owner in [ARB1_OWNER_1, ARB1_OWNER_2, ARB1_OWNER_3] {
            set.add(owner, ctx.journal_mut()).unwrap();
        }
        set.remove(ARB1_OWNER_1, 10, ctx.journal_mut()).unwrap();
        set.remove(ARB1_OWNER_2, 10, ctx.journal_mut()).unwrap();
        rectify_all(&set, &[ARB1_OWNER_3], true, &mut ctx);

        set.clear(ctx.journal_mut()).unwrap();
        set.add(GOERLI[0], ctx.journal_mut()).unwrap();
        set.add(GOERLI[1], ctx.journal_mut()).unwrap();
        set.add(GOERLI[2], ctx.journal_mut()).unwrap();
        set.remove(GOERLI[0], 10, ctx.journal_mut()).unwrap();
        set.add(GOERLI[3], ctx.journal_mut()).unwrap();
        set.remove(GOERLI[2], 10, ctx.journal_mut()).unwrap();
        set.remove(GOERLI[1], 10, ctx.journal_mut()).unwrap();
        assert_eq!(set.all_members(ctx.journal_mut()).unwrap(), vec![GOERLI[2]]);
        assert!(!set.is_member(GOERLI[2], ctx.journal_mut()).unwrap());
        assert!(set.is_member(GOERLI[3], ctx.journal_mut()).unwrap());
        rectify_all(&set, &[GOERLI[3]], true, &mut ctx);
    }

    #[test]
    fn rectify_mapping_repairs_corrupt_list_map_and_map_only_member() {
        let (mut ctx, set) = fresh();
        let owners = [ARB1_OWNER_1, ARB1_OWNER_2, ARB1_OWNER_3];
        for owner in owners {
            set.add(owner, ctx.journal_mut()).unwrap();
        }

        set.backing
            .set(
                FixedBytes::from(U256::from(1).to_be_bytes()),
                address_to_u256(ARB1_OWNER_2),
                ctx.journal_mut(),
            )
            .unwrap();
        rectify_all(&set, &owners, true, &mut ctx);

        set.by_address
            .set(
                FixedBytes::from(address_to_u256(ARB1_OWNER_2).to_be_bytes()),
                U256::from(6),
                ctx.journal_mut(),
            )
            .unwrap();
        rectify_all(&set, &owners, true, &mut ctx);

        let map_only = Address::with_last_byte(0x44);
        set.by_address
            .set(
                FixedBytes::from(address_to_u256(map_only).to_be_bytes()),
                U256::from(1),
                ctx.journal_mut(),
            )
            .unwrap();
        rectify_all(&set, &owners, true, &mut ctx);

        assert_eq!(
            set.rectify_mapping(ARB1_OWNER_1, ctx.journal_mut())
                .unwrap_err()
                .to_string(),
            "RectifyMapping: Owner address is correctly mapped"
        );
        assert_eq!(
            set.rectify_mapping(Address::with_last_byte(0xff), ctx.journal_mut())
                .unwrap_err()
                .to_string(),
            "RectifyMapping: Address is not an owner"
        );
    }
}
