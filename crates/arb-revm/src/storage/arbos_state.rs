use revm::primitives::{Address, U256};

use super::{
    AddressSet, AddressTable, ArbFeatures, ArbosMetadataOffset, ArbosPrograms, BlockHashes,
    ChainConfig, L1Pricing, L2Pricing, Retryables, SendMerkle, StorageBacked, StorageSpace,
    Subspace,
};

/// Root typed ArbOS state view.
pub struct ArbosState {
    pub arbos_version: StorageBacked<u64>,
    pub upgrade_version: StorageBacked<u64>,
    pub upgrade_timestamp: StorageBacked<u64>,
    pub network_fee_account: StorageBacked<Address>,
    pub genesis_block_number: StorageBacked<u64>,
    pub l1_pricing: L1Pricing,
    pub l2_pricing: L2Pricing,
    pub retryables: Retryables,
    pub address_table: AddressTable,
    pub chain_owners: AddressSet,
    pub send_merkle: SendMerkle,
    pub block_hashes: BlockHashes,
    pub chain_config: ChainConfig,
    pub programs: ArbosPrograms,
    pub features: ArbFeatures,
    pub native_token_owners: AddressSet,
    pub transaction_filterers: AddressSet,
    pub chain_id: StorageBacked<U256>,
    pub infra_fee_account: StorageBacked<Address>,
    pub brotli_compression_level: StorageBacked<u64>,
    pub native_token_enabled_from_timestamp: StorageBacked<u64>,
    pub transaction_filtering_enabled_from_timestamp: StorageBacked<u64>,
    pub filtered_funds_recipient: StorageBacked<Address>,
    pub collect_tips: StorageBacked<u64>,
    pub root: StorageSpace,
}

impl ArbosState {
    pub fn open() -> Self {
        let root = StorageSpace::arbos();
        Self {
            arbos_version: root.storage_backed(ArbosMetadataOffset::Version as u8),
            upgrade_version: root.storage_backed(ArbosMetadataOffset::UpgradeVersion as u8),
            upgrade_timestamp: root.storage_backed(ArbosMetadataOffset::UpgradeTimestamp as u8),
            network_fee_account: root.storage_backed(ArbosMetadataOffset::NetworkFeeAccount as u8),
            genesis_block_number: root
                .storage_backed(ArbosMetadataOffset::GenesisBlockNumber as u8),
            l1_pricing: L1Pricing::open(&root.open_subspace_with_key(Subspace::L1Pricing as u8)),
            l2_pricing: L2Pricing::open(&root.open_subspace_with_key(Subspace::L2Pricing as u8)),
            retryables: Retryables::open(&root.open_subspace_with_key(Subspace::Retryables as u8)),
            address_table: AddressTable::open(
                root.open_subspace_with_key(Subspace::AddressTable as u8),
            ),
            chain_owners: AddressSet::open(
                &root.open_subspace_with_key(Subspace::ChainOwners as u8),
            ),
            send_merkle: SendMerkle::open(&root.open_subspace_with_key(Subspace::SendMerkle as u8)),
            block_hashes: BlockHashes::open(
                &root.open_subspace_with_key(Subspace::BlockHashes as u8),
            ),
            chain_config: ChainConfig::open(
                &root.open_subspace_with_key(Subspace::ChainConfig as u8),
            ),
            programs: ArbosPrograms::open(&root.open_subspace_with_key(Subspace::Programs as u8)),
            features: ArbFeatures::open(&root.open_subspace_with_key(Subspace::Features as u8)),
            native_token_owners: AddressSet::open(
                &root.open_subspace_with_key(Subspace::NativeTokenOwners as u8),
            ),
            transaction_filterers: AddressSet::open(
                &root.open_subspace_with_key(Subspace::TransactionFilterers as u8),
            ),
            chain_id: root.storage_backed(ArbosMetadataOffset::ChainId as u8),
            infra_fee_account: root.storage_backed(ArbosMetadataOffset::InfraFeeAccount as u8),
            brotli_compression_level: root
                .storage_backed(ArbosMetadataOffset::BrotliCompressionLevel as u8),
            native_token_enabled_from_timestamp: root
                .storage_backed(ArbosMetadataOffset::NativeTokenEnabledFromTimestamp as u8),
            transaction_filtering_enabled_from_timestamp: root.storage_backed(
                ArbosMetadataOffset::TransactionFilteringEnabledFromTimestamp as u8,
            ),
            filtered_funds_recipient: root
                .storage_backed(ArbosMetadataOffset::FilteredFundsRecipient as u8),
            collect_tips: root.storage_backed(ArbosMetadataOffset::CollectTips as u8),
            root,
        }
    }

    /// `(account, slot)` of the on-chain ArbOS version word.
    pub fn version_slot() -> (Address, U256) {
        let (account, slot) = Self::open().arbos_version.account_and_key();
        (account, U256::from_be_bytes(slot.0))
    }

    /// Reads the on-chain ArbOS version directly from a state source.
    ///
    /// Used to select the execution spec ([`crate::ArbSpecId::from_arbos_version`])
    /// before the EVM is built, since the version lives in state but the spec must be
    /// fixed in `Cfg` up front. Returns 0 (treated as genesis) when unreadable.
    pub fn read_version<DB: revm::DatabaseRef>(db: &DB) -> u64 {
        let (account, slot) = Self::version_slot();
        db.storage_ref(account, slot)
            .ok()
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(0)
    }

    /// [`Self::read_version`] for a mutable [`revm::Database`] (e.g. the message
    /// execution path, which has `&mut DB: Database` rather than `DatabaseRef`).
    pub fn read_version_db<DB: revm::Database>(db: &mut DB) -> u64 {
        let (account, slot) = Self::version_slot();
        db.storage(account, slot)
            .ok()
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(0)
    }

    /// The ArbOS version a block at `block_timestamp` will execute *under*, accounting
    /// for a scheduled upgrade. Mirrors Nitro's `UpgradeArbosVersionIfNecessary`: if a
    /// scheduled upgrade (`upgrade_version`) is past its flag-day (`upgrade_timestamp`),
    /// the block runs under the new version (the start-block then bumps stored state).
    /// Without this, an activation block would execute under the parent's hardfork.
    pub fn read_effective_version<DB: revm::DatabaseRef>(db: &DB, block_timestamp: u64) -> u64 {
        let state = Self::open();
        let read = |backed: &StorageBacked<u64>| -> u64 {
            let (account, slot) = backed.account_and_key();
            db.storage_ref(account, U256::from_be_bytes(slot.0))
                .ok()
                .and_then(|value| u64::try_from(value).ok())
                .unwrap_or(0)
        };
        Self::resolve_effective_version(
            read(&state.arbos_version),
            read(&state.upgrade_version),
            read(&state.upgrade_timestamp),
            block_timestamp,
        )
    }

    /// [`Self::read_effective_version`] for a mutable [`revm::Database`].
    pub fn read_effective_version_db<DB: revm::Database>(db: &mut DB, block_timestamp: u64) -> u64 {
        let state = Self::open();
        let read = |db: &mut DB, backed: &StorageBacked<u64>| -> u64 {
            let (account, slot) = backed.account_and_key();
            db.storage(account, U256::from_be_bytes(slot.0))
                .ok()
                .and_then(|value| u64::try_from(value).ok())
                .unwrap_or(0)
        };
        Self::resolve_effective_version(
            read(db, &state.arbos_version),
            read(db, &state.upgrade_version),
            read(db, &state.upgrade_timestamp),
            block_timestamp,
        )
    }

    fn resolve_effective_version(
        current: u64,
        upgrade_version: u64,
        upgrade_timestamp: u64,
        block_timestamp: u64,
    ) -> u64 {
        if upgrade_version > current && block_timestamp >= upgrade_timestamp {
            upgrade_version
        } else {
            current
        }
    }
}
