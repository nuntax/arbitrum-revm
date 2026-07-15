//! A closed-world database for fixture execution.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use alloy_trie::{
    TrieAccount,
    root::{state_root_unhashed, storage_root_unhashed},
};
use revm::{
    DatabaseRef,
    primitives::{Address, B256, Bytes, KECCAK_EMPTY, StorageKey, StorageValue, U256, keccak256},
    state::{AccountInfo, Bytecode},
};
use serde::{Deserialize, Serialize};

/// Complete account state supplied by a synthetic fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteAccount {
    pub address: Address,
    pub exists: bool,
    #[serde(default)]
    pub nonce: u64,
    #[serde(default)]
    pub balance: U256,
    #[serde(default)]
    pub code: Bytes,
    pub storage: CompleteStorage,
}

/// Storage coverage for one complete account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteStorage {
    pub complete: bool,
    #[serde(default)]
    pub slots: Vec<CompleteStorageSlot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteStorageSlot {
    pub slot: U256,
    pub value: U256,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteBlockHash {
    pub number: u64,
    pub hash: B256,
}

/// A complete synthetic state. An unlisted account is a proven absent account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteState {
    pub kind: String,
    pub state_root: B256,
    #[serde(default)]
    pub accounts: Vec<CompleteAccount>,
    #[serde(default)]
    pub block_hashes: Vec<CompleteBlockHash>,
}

impl CompleteState {
    pub fn validate(&self) -> Result<(), IncompleteWitness> {
        if self.kind != "complete" {
            return Err(IncompleteWitness::InvalidCompleteState(format!(
                "unsupported complete-state kind {:?}",
                self.kind
            )));
        }
        let mut addresses = BTreeSet::new();
        for account in &self.accounts {
            if !addresses.insert(account.address) {
                return Err(IncompleteWitness::InvalidCompleteState(format!(
                    "duplicate account {:#x}",
                    account.address
                )));
            }
            if !account.exists
                && (!account.code.is_empty()
                    || account.nonce != 0
                    || account.balance != U256::ZERO
                    || !account.storage.slots.is_empty())
            {
                return Err(IncompleteWitness::InvalidCompleteState(format!(
                    "absent account {:#x} contains state",
                    account.address
                )));
            }
            let mut slots = BTreeSet::new();
            for entry in &account.storage.slots {
                if !slots.insert(entry.slot) {
                    return Err(IncompleteWitness::InvalidCompleteState(format!(
                        "duplicate storage slot {:#x} for account {:#x}",
                        entry.slot, account.address
                    )));
                }
            }
        }
        let mut numbers = BTreeSet::new();
        for header in &self.block_hashes {
            if !numbers.insert(header.number) {
                return Err(IncompleteWitness::InvalidCompleteState(format!(
                    "duplicate block hash at {}",
                    header.number
                )));
            }
        }
        Ok(())
    }
}

/// Rebuilds the root of a complete synthetic state from account and storage
/// values. This is data validation, not an execution helper.
pub fn complete_state_root(state: &CompleteState) -> Result<B256, IncompleteWitness> {
    state.validate()?;
    let accounts = state
        .accounts
        .iter()
        .filter(|account| account.exists)
        .map(|account| {
            let storage_root = storage_root_unhashed(
                account
                    .storage
                    .slots
                    .iter()
                    .filter(|entry| entry.value != U256::ZERO)
                    .map(|entry| (B256::from(entry.slot.to_be_bytes::<32>()), entry.value)),
            );
            let code_hash = if account.code.is_empty() {
                KECCAK_EMPTY
            } else {
                keccak256(&account.code)
            };
            (
                account.address,
                TrieAccount {
                    nonce: account.nonce,
                    balance: account.balance,
                    storage_root,
                    code_hash,
                },
            )
        });
    Ok(state_root_unhashed(accounts))
}

/// Error returned for a read that a fixture did not prove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncompleteWitness {
    AccountStorage { address: Address, slot: U256 },
    Code { code_hash: B256 },
    BlockHash { number: u64 },
    InvalidCompleteState(String),
}

impl fmt::Display for IncompleteWitness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AccountStorage { address, slot } => {
                write!(f, "incomplete witness: storage {address:#x}[{slot:#x}]")
            }
            Self::Code { code_hash } => write!(f, "incomplete witness: code {code_hash:#x}"),
            Self::BlockHash { number } => write!(f, "incomplete witness: block hash {number}"),
            Self::InvalidCompleteState(message) => write!(f, "invalid complete state: {message}"),
        }
    }
}

impl std::error::Error for IncompleteWitness {}

impl revm::database_interface::DBErrorMarker for IncompleteWitness {}

/// Strict read-only parent state used beneath [`revm::database::CacheDB`].
#[derive(Debug, Clone)]
pub struct StrictDatabase {
    accounts: BTreeMap<Address, StrictAccount>,
    code: BTreeMap<B256, Bytecode>,
    block_hashes: BTreeMap<u64, B256>,
}

#[derive(Debug, Clone)]
struct StrictAccount {
    info: Option<AccountInfo>,
    storage_complete: bool,
    storage: BTreeMap<U256, U256>,
}

impl StrictDatabase {
    pub fn from_complete_state(state: CompleteState) -> Result<Self, IncompleteWitness> {
        state.validate()?;
        let actual_root = complete_state_root(&state)?;
        if actual_root != state.state_root {
            return Err(IncompleteWitness::InvalidCompleteState(format!(
                "state root mismatch: expected {:#x}, got {actual_root:#x}",
                state.state_root
            )));
        }
        let mut accounts = BTreeMap::new();
        let mut code = BTreeMap::new();
        code.insert(KECCAK_EMPTY, Bytecode::default());

        for account in state.accounts {
            let storage = account
                .storage
                .slots
                .into_iter()
                .map(|entry| (entry.slot, entry.value))
                .collect();
            let info = if account.exists {
                let bytecode = Bytecode::new_raw(account.code);
                let code_hash = if bytecode.is_empty() {
                    KECCAK_EMPTY
                } else {
                    keccak256(bytecode.original_bytes())
                };
                if code_hash != KECCAK_EMPTY {
                    code.insert(code_hash, bytecode.clone());
                }
                Some(AccountInfo::new(
                    account.balance,
                    account.nonce,
                    code_hash,
                    bytecode,
                ))
            } else {
                None
            };
            accounts.insert(
                account.address,
                StrictAccount {
                    info,
                    storage_complete: account.storage.complete,
                    storage,
                },
            );
        }
        let block_hashes = state
            .block_hashes
            .into_iter()
            .map(|entry| (entry.number, entry.hash))
            .collect();
        Ok(Self {
            accounts,
            code,
            block_hashes,
        })
    }
}

impl DatabaseRef for StrictDatabase {
    type Error = IncompleteWitness;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        Ok(self
            .accounts
            .get(&address)
            .and_then(|account| account.info.clone()))
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.code
            .get(&code_hash)
            .cloned()
            .ok_or(IncompleteWitness::Code { code_hash })
    }

    fn storage_ref(
        &self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        let Some(account) = self.accounts.get(&address) else {
            return Ok(U256::ZERO);
        };
        if account.info.is_none() {
            return Ok(U256::ZERO);
        }
        match account.storage.get(&index).copied() {
            Some(value) => Ok(value),
            None if account.storage_complete => Ok(U256::ZERO),
            None => Err(IncompleteWitness::AccountStorage {
                address,
                slot: index,
            }),
        }
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.block_hashes
            .get(&number)
            .copied()
            .ok_or(IncompleteWitness::BlockHash { number })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> CompleteState {
        CompleteState {
            kind: "complete".to_owned(),
            state_root: B256::ZERO,
            accounts: vec![CompleteAccount {
                address: Address::with_last_byte(1),
                exists: true,
                nonce: 1,
                balance: U256::from(2),
                code: Bytes::new(),
                storage: CompleteStorage {
                    complete: false,
                    slots: vec![],
                },
            }],
            block_hashes: vec![],
        }
    }

    #[test]
    fn unlisted_account_is_proven_absent() {
        let mut state = state();
        state.state_root = complete_state_root(&state).unwrap();
        let db = StrictDatabase::from_complete_state(state).unwrap();
        assert_eq!(db.basic_ref(Address::with_last_byte(2)).unwrap(), None);
        assert_eq!(
            db.storage_ref(Address::with_last_byte(2), U256::ZERO)
                .unwrap(),
            U256::ZERO
        );
    }

    #[test]
    fn missing_incomplete_slot_fails() {
        let mut state = state();
        state.state_root = complete_state_root(&state).unwrap();
        let db = StrictDatabase::from_complete_state(state).unwrap();
        assert!(matches!(
            db.storage_ref(Address::with_last_byte(1), U256::ZERO),
            Err(IncompleteWitness::AccountStorage { .. })
        ));
    }

    #[test]
    fn missing_header_fails() {
        let mut state = state();
        state.state_root = complete_state_root(&state).unwrap();
        let db = StrictDatabase::from_complete_state(state).unwrap();
        assert!(matches!(
            db.block_hash_ref(1),
            Err(IncompleteWitness::BlockHash { number: 1 })
        ));
    }

    #[test]
    fn rejects_wrong_complete_state_root() {
        let state = state();
        assert!(matches!(
            StrictDatabase::from_complete_state(state),
            Err(IncompleteWitness::InvalidCompleteState(_))
        ));
    }
}
