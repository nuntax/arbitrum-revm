use revm::primitives::{Address, B256, U256};
use std::sync::OnceLock;

use super::{
    AddressSet, AddressTable, ArbFeatures, ArbosMetadataOffset, ArbosPrograms, BlockHashes,
    ChainConfig, L1Pricing, L2Pricing, Retryables, SendMerkle, StorageBacked, StorageSpace,
    Subspace,
};
use crate::arb_journal::ArbJournal;

/// Post-execution ArbOS values that feed an Arbitrum block header
/// (`HeaderInfo` + nonce/base-fee), read from state after a block is processed.
///
/// Mirrors what Nitro's `FinalizeBlock` / `createNewHeader` pull from `arbosState`:
/// the send-Merkle root + size (→ `extra_data` / `mix_hash`), the (possibly
/// upgraded) ArbOS version (→ `mix_hash`), and the L2 base fee (→ `base_fee_per_gas`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ArbBlockHeaderInfo {
    /// Merkle root of the delayed send queue (Nitro `SendRoot` → header `extra_data`).
    pub send_root: B256,
    /// Number of sends accumulated so far (Nitro `SendCount` → `mix_hash[0..8]`).
    pub send_count: u64,
    /// ArbOS format version after this block (→ `mix_hash[16..24]`).
    pub arbos_version: u64,
    /// L1 block number ArbOS recorded for this block (Nitro `Blockhashes().L1BlockNumber()`,
    /// → `mix_hash[8..16]`). This is the value set by the start-block internal tx, which may
    /// differ from the raw message `l1BlockNumber` (version shims / no-advance blocks).
    pub l1_block_number: u64,
    /// Raw chain-wide tip-collection setting (Nitro `CollectTips()`). The block-level flag in
    /// `mix_hash[25]` additionally requires the coinbase to be the batch poster, applied by the
    /// caller, which knows the block coinbase.
    pub collect_tips: bool,
    /// L2 base fee in wei after start-of-block pricing (→ header `base_fee_per_gas`).
    pub base_fee_wei: U256,
}

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
    /// Returns the immutable ArbOS storage layout.
    ///
    /// The account, subspace keys, and concrete slots are protocol constants. Building the layout
    /// hashes every subspace and slot, so doing it for every transaction is needlessly expensive.
    /// State values are still read through the transaction's journal and are never cached here.
    pub fn open() -> &'static Self {
        static STATE: OnceLock<ArbosState> = OnceLock::new();
        STATE.get_or_init(Self::build)
    }

    fn build() -> Self {
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

    /// Reads the post-execution header-info values ([`ArbBlockHeaderInfo`]) through a journal.
    ///
    /// Called after all of a block's transactions have been committed (the journal then
    /// loads the committed values straight from the underlying state), so the send-Merkle
    /// root/size, the (possibly upgraded) ArbOS version, and the L2 base fee all reflect the
    /// finalized block, exactly the inputs Nitro feeds into the block header.
    /// Whether the chain runs in debug mode (Nitro `ChainConfig.DebugMode()` =
    /// `arbitrum.AllowDebugPrecompiles`). Read from the stored chain-config JSON; arb_revm storage
    /// reads are free, matching Nitro reading it off the in-memory chain config (no gas). Stylus
    /// activation + execution instrument differently under debug, so this must key those paths.
    pub fn debug_mode<J: ArbJournal>(&self, journal: &mut J) -> bool {
        let bytes = match self.chain_config.get(journal) {
            Ok(b) => b,
            Err(_) => return false,
        };
        let value: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => return false,
        };
        value
            .get("arbitrum")
            .and_then(|a| a.get("AllowDebugPrecompiles"))
            .and_then(|d| d.as_bool())
            .unwrap_or(false)
    }

    pub fn read_block_header_info<J: ArbJournal>(
        journal: &mut J,
    ) -> eyre::Result<ArbBlockHeaderInfo> {
        let state = Self::open();
        Ok(ArbBlockHeaderInfo {
            send_root: state.send_merkle.root(journal)?,
            send_count: state.send_merkle.size(journal)?,
            arbos_version: state.arbos_version.get(journal)?,
            l1_block_number: state.block_hashes.l1_block_number(journal)?,
            collect_tips: state.collect_tips.get(journal)? != 0,
            base_fee_wei: state.l2_pricing.base_fee_wei.get(journal)?,
        })
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
