use eyre::Result;
use revm::{
    context_interface::JournalTr,
    primitives::{Address, Bytes, B256, U256},
};

use super::{StorageBacked, StorageBytes, StorageQueue, StorageSpace};
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

    pub fn initialize<J: JournalTr>(&self, journal: &mut J) -> Result<()> {
        self.timeout_queue.initialize(journal)
    }

    pub fn retryable(&self, id: B256) -> RetryableRecord {
        let subspace = self
            .root
            .open_subspace(Bytes::copy_from_slice(id.as_slice()));
        RetryableRecord::open(id, &subspace)
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

    pub fn exists<J: JournalTr>(&self, current_timestamp: u64, journal: &mut J) -> Result<bool> {
        let timeout = self.timeout.get(journal)?;
        Ok(timeout != 0 && timeout >= current_timestamp)
    }

    pub fn to<J: JournalTr>(&self, journal: &mut J) -> Result<Option<Address>> {
        let raw = self.to_raw.get(journal)?;
        if raw == nil_address_representation() {
            return Ok(None);
        }
        let bytes = raw.to_be_bytes::<32>();
        Ok(Some(Address::from_slice(&bytes[12..])))
    }

    pub fn set_to<J: JournalTr>(&self, to: Option<Address>, journal: &mut J) -> Result<()> {
        let raw = match to {
            Some(address) => address_to_u256(address),
            None => nil_address_representation(),
        };
        self.to_raw.set(raw, journal)?;
        Ok(())
    }

    pub fn timeout_with_windows<J: JournalTr>(&self, journal: &mut J) -> Result<u64> {
        let timeout = self.timeout.get(journal)?;
        let windows = self.timeout_windows_left.get(journal)?;
        Ok(timeout.saturating_add(windows.saturating_mul(RETRYABLE_LIFETIME_SECONDS)))
    }
}

fn nil_address_representation() -> U256 {
    U256::from(1_u8) << 255
}
