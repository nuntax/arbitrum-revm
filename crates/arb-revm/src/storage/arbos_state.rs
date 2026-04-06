use revm::primitives::{Address, U256};

use super::{
    AddressSet, AddressTable, ArbosMetadataOffset, BlockHashes, L1Pricing, L2Pricing,
    StorageBacked, StorageSpace, Subspace,
};

/// Root typed ArbOS state view.
pub struct ArbosState {
    pub arbos_version: StorageBacked<u64>,
    pub upgrade_version: StorageBacked<u64>,
    pub upgrade_timestamp: StorageBacked<u64>,
    pub network_fee_account: StorageBacked<Address>,
    pub l1_pricing: L1Pricing,
    pub l2_pricing: L2Pricing,
    pub address_table: AddressTable,
    pub chain_owners: AddressSet,
    pub block_hashes: BlockHashes,
    pub native_token_owners: AddressSet,
    pub chain_id: StorageBacked<U256>,
    pub infra_fee_account: StorageBacked<Address>,
    pub brotli_compression_level: StorageBacked<u64>,
    pub native_token_enabled_from_timestamp: StorageBacked<u64>,
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
            l1_pricing: L1Pricing::open(&root.open_subspace_with_key(Subspace::L1Pricing as u8)),
            l2_pricing: L2Pricing::open(&root.open_subspace_with_key(Subspace::L2Pricing as u8)),
            address_table: AddressTable::open(
                root.open_subspace_with_key(Subspace::AddressTable as u8),
            ),
            chain_owners: AddressSet::open(
                &root.open_subspace_with_key(Subspace::ChainOwners as u8),
            ),
            block_hashes: BlockHashes::open(
                &root.open_subspace_with_key(Subspace::BlockHashes as u8),
            ),
            native_token_owners: AddressSet::open(
                &root.open_subspace_with_key(Subspace::NativeTokenOwners as u8),
            ),
            chain_id: root.storage_backed(ArbosMetadataOffset::ChainId as u8),
            infra_fee_account: root.storage_backed(ArbosMetadataOffset::InfraFeeAccount as u8),
            brotli_compression_level: root
                .storage_backed(ArbosMetadataOffset::BrotliCompressionLevel as u8),
            native_token_enabled_from_timestamp: root
                .storage_backed(ArbosMetadataOffset::NativeTokenEnabledFromTimestamp as u8),
            root,
        }
    }
}
