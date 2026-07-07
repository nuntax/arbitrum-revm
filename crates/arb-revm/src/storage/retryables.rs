use eyre::Result;
use revm::{
    context_interface::journaled_state::TransferError,
    primitives::{Address, B256, Bytes, U256, keccak256},
};

use super::{StorageBacked, StorageBytes, StorageQueue, StorageSpace};
use crate::arb_journal::ArbJournal;
use crate::util::address_to_u256;

const TIMEOUT_QUEUE_KEY: u8 = 0;
const CALLDATA_KEY: u8 = 1;

const NUM_TRIES_OFFSET: u8 = 0;
const FROM_OFFSET: u8 = 1;
const TO_OFFSET: u8 = 2;
const CALLVALUE_OFFSET: u8 = 3;
const BENEFICIARY_OFFSET: u8 = 4;
const TIMEOUT_OFFSET: u8 = 5;
const TIMEOUT_WINDOWS_LEFT_OFFSET: u8 = 6;
const RETRYABLE_ESCROW_TAG: &[u8] = b"retryable escrow";

pub const RETRYABLE_LIFETIME_SECONDS: u64 = 7 * 24 * 60 * 60;
pub const RETRYABLE_REAP_PRICE: u64 = 58_000;

/// ArbOS retryables root substorage.
#[derive(Debug)]
pub struct Retryables {
    root: StorageSpace,
    pub timeout_queue: StorageQueue,
}

impl Retryables {
    pub fn open(storage: &StorageSpace) -> Self {
        let timeout_queue_space = storage.open_subspace_with_key(TIMEOUT_QUEUE_KEY);
        Self {
            root: storage.clone(),
            timeout_queue: StorageQueue::open(&timeout_queue_space),
        }
    }

    pub fn initialize<J: ArbJournal>(&self, journal: &mut J) -> Result<()> {
        self.timeout_queue.initialize(journal)
    }

    pub fn ensure_timeout_queue_initialized<J: ArbJournal>(&self, journal: &mut J) -> Result<()> {
        self.timeout_queue.ensure_initialized(journal)
    }

    pub fn retryable(&self, id: B256) -> RetryableRecord {
        let subspace = self
            .root
            .open_subspace(Bytes::copy_from_slice(id.as_slice()));
        RetryableRecord::open(id, &subspace)
    }

    pub fn delete_retryable<J: ArbJournal>(&self, id: B256, journal: &mut J) -> Result<bool> {
        let retryable = self.retryable(id);
        let timeout = retryable.timeout.get(journal)?;
        if timeout == 0 {
            return Ok(false);
        }

        let beneficiary = retryable.beneficiary.get(journal)?;
        let escrow = retryable_escrow_address(id);
        let escrow_balance = journal.account_balance(escrow)?;
        if escrow_balance > U256::ZERO {
            let transfer_error = journal.transfer(escrow, beneficiary, escrow_balance)?;
            map_transfer_error(transfer_error, "retryable escrow release")?;
        }

        retryable.num_tries.set(0, journal)?;
        retryable.from.set(Address::ZERO, journal)?;
        retryable.to_raw.set(U256::ZERO, journal)?;
        retryable.callvalue.set(U256::ZERO, journal)?;
        retryable.beneficiary.set(Address::ZERO, journal)?;
        retryable.timeout.set(0, journal)?;
        retryable.timeout_windows_left.set(0, journal)?;
        retryable.calldata.clear(journal)?;

        Ok(true)
    }

    /// Attempts to reap one retryable from the timeout queue.
    ///
    /// Returns `Ok(true)` if an entry was consumed or modified, `Ok(false)` if no
    /// work was needed.
    pub fn try_to_reap_one<J: ArbJournal>(
        &self,
        current_timestamp: u64,
        journal: &mut J,
    ) -> Result<bool> {
        let Some(id) = self.timeout_queue.peek(journal)? else {
            return Ok(false);
        };

        let retryable = self.retryable(id);
        let timeout = retryable.timeout.get(journal)?;
        if timeout == 0 {
            // Already deleted; discard stale queue entry.
            let _ = self.timeout_queue.get(journal)?;
            return Ok(true);
        }

        let windows_left = retryable.timeout_windows_left.get(journal)?;
        if timeout >= current_timestamp {
            return Ok(false);
        }

        // Either expired or consumed one extension window.
        let _ = self.timeout_queue.get(journal)?;

        if windows_left == 0 {
            let _ = self.delete_retryable(id, journal)?;
            return Ok(true);
        }

        retryable
            .timeout
            .set(timeout.saturating_add(RETRYABLE_LIFETIME_SECONDS), journal)?;
        retryable
            .timeout_windows_left
            .set(windows_left.saturating_sub(1), journal)?;
        Ok(true)
    }
}

/// ArbOS retryable ticket storage record.
#[derive(Debug)]
pub struct RetryableRecord {
    pub id: B256,
    pub storage: StorageSpace,
    pub num_tries: StorageBacked<u64>,
    pub from: StorageBacked<Address>,
    pub to_raw: StorageBacked<U256>,
    pub callvalue: StorageBacked<U256>,
    pub beneficiary: StorageBacked<Address>,
    pub timeout: StorageBacked<u64>,
    pub timeout_windows_left: StorageBacked<u64>,
    pub calldata: StorageBytes,
}

impl RetryableRecord {
    pub fn open(id: B256, storage: &StorageSpace) -> Self {
        Self {
            id,
            storage: storage.clone(),
            num_tries: storage.storage_backed(NUM_TRIES_OFFSET),
            from: storage.storage_backed(FROM_OFFSET),
            to_raw: storage.storage_backed(TO_OFFSET),
            callvalue: storage.storage_backed(CALLVALUE_OFFSET),
            beneficiary: storage.storage_backed(BENEFICIARY_OFFSET),
            timeout: storage.storage_backed(TIMEOUT_OFFSET),
            timeout_windows_left: storage.storage_backed(TIMEOUT_WINDOWS_LEFT_OFFSET),
            calldata: StorageBytes::open(&storage.open_subspace_with_key(CALLDATA_KEY)),
        }
    }

    pub fn exists<J: ArbJournal>(&self, current_timestamp: u64, journal: &mut J) -> Result<bool> {
        let timeout = self.timeout.get(journal)?;
        Ok(timeout != 0 && timeout >= current_timestamp)
    }

    pub fn to<J: ArbJournal>(&self, journal: &mut J) -> Result<Option<Address>> {
        let raw = self.to_raw.get(journal)?;
        if raw == nil_address_representation() {
            return Ok(None);
        }
        let bytes = raw.to_be_bytes::<32>();
        Ok(Some(Address::from_slice(&bytes[12..])))
    }

    pub fn set_to<J: ArbJournal>(&self, to: Option<Address>, journal: &mut J) -> Result<()> {
        let raw = match to {
            Some(address) => address_to_u256(address),
            None => nil_address_representation(),
        };
        self.to_raw.set(raw, journal)?;
        Ok(())
    }

    pub fn timeout_with_windows<J: ArbJournal>(&self, journal: &mut J) -> Result<u64> {
        let timeout = self.timeout.get(journal)?;
        let windows = self.timeout_windows_left.get(journal)?;
        Ok(timeout.saturating_add(windows.saturating_mul(RETRYABLE_LIFETIME_SECONDS)))
    }
}

fn nil_address_representation() -> U256 {
    U256::from(1_u8) << 255
}

fn retryable_escrow_address(ticket_id: B256) -> Address {
    let mut preimage = Vec::with_capacity(RETRYABLE_ESCROW_TAG.len() + ticket_id.len());
    preimage.extend_from_slice(RETRYABLE_ESCROW_TAG);
    preimage.extend_from_slice(ticket_id.as_slice());
    let hash = keccak256(preimage);
    Address::from_slice(&hash[12..])
}

fn map_transfer_error(transfer_error: Option<TransferError>, label: &str) -> Result<()> {
    match transfer_error {
        None => Ok(()),
        Some(TransferError::OutOfFunds) => eyre::bail!("{label} failed: out of funds"),
        Some(TransferError::OverflowPayment) => eyre::bail!("{label} failed: overflow payment"),
        Some(TransferError::CreateCollision) => eyre::bail!("{label} failed: create collision"),
    }
}
