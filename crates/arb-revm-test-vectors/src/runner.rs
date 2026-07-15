//! Production-message runner for complete-state transaction-sequence vectors.

use std::{collections::BTreeMap, path::Path};

use alloy_eips::eip2718::Decodable2718;
use alloy_trie::Nibbles;
use arb_revm::{
    ArbExecCfg, ArbExecutionInput, ArbExecutionMode, ArbMessageEnvelope, ArbParentHeader,
    ArbSpecId, ArbTxExecution, ArbosState, StorageBacked, execute_message,
};
use arb_stf_fixture::{FixtureCase, FixtureInput, FixturePrestate, ObjectStore};
use arbitrum_alloy_consensus::transactions::ArbTxEnvelope;
use revm::{
    Database, DatabaseCommit,
    database::CacheDB,
    primitives::{Address, B256, Bytes, U256},
    state::{Account, AccountInfo},
};
use serde::{Deserialize, Serialize};

use crate::{
    strict_db::{CompleteState, IncompleteWitness, StrictDatabase, complete_state_root},
    witness::{ExecutionWitnessPrestate, WitnessDatabase},
};

const DERIVED_TRANSACTIONS_INPUT_SCHEMA: &str = "arb-stf-derived-transactions-input-v1";
const SEQUENCE_OUTPUT_SCHEMA: &str = "arb-stf-sequence-output-v1";

/// Complete derived-transaction input that can enter the production message
/// executor without network access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DerivedTransactionsInput {
    pub schema: String,
    pub chain_id: u64,
    pub parent: SequenceParent,
    pub message: SequenceMessage,
    #[serde(default)]
    pub disable_priority_fee_check: bool,
    #[serde(default)]
    pub disable_balance_check: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SequenceParent {
    pub number: u64,
    pub timestamp: u64,
    pub beneficiary: Address,
    pub basefee: u64,
    pub gas_limit: u64,
    #[serde(default)]
    pub difficulty: U256,
    #[serde(default)]
    pub prevrandao: Option<B256>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SequenceMessage {
    #[serde(default)]
    pub sequence_number: Option<u64>,
    pub l1_block_number: u64,
    pub l1_timestamp: u64,
    pub poster: Address,
    pub l1_base_fee_wei: U256,
    pub delayed_messages_read: u64,
    pub transactions: Vec<Bytes>,
}

impl DerivedTransactionsInput {
    fn into_execution_input(
        self,
        spec_id: arb_revm::ArbSpecId,
    ) -> Result<ArbExecutionInput, String> {
        if self.schema != DERIVED_TRANSACTIONS_INPUT_SCHEMA {
            return Err(format!(
                "unsupported derived-transactions input schema {:?}",
                self.schema
            ));
        }
        let mut transactions = Vec::with_capacity(self.message.transactions.len());
        for (index, bytes) in self.message.transactions.into_iter().enumerate() {
            let mut bytes = bytes.as_ref();
            let tx = ArbTxEnvelope::decode_2718(&mut bytes)
                .map_err(|error| format!("transaction[{index}] failed to decode: {error}"))?;
            if !bytes.is_empty() {
                return Err(format!("transaction[{index}] has trailing envelope bytes"));
            }
            transactions.push(tx);
        }
        let parent = ArbParentHeader {
            number: self.parent.number,
            timestamp: self.parent.timestamp,
            beneficiary: self.parent.beneficiary,
            basefee: self.parent.basefee,
            gas_limit: self.parent.gas_limit,
            difficulty: self.parent.difficulty,
            prevrandao: self.parent.prevrandao,
        };
        let message = ArbMessageEnvelope {
            sequence_number: self.message.sequence_number,
            l1_block_number: self.message.l1_block_number,
            l1_timestamp: self.message.l1_timestamp,
            poster: self.message.poster,
            l1_base_fee_wei: self.message.l1_base_fee_wei,
            delayed_messages_read: self.message.delayed_messages_read,
            txs: transactions,
        };
        let cfg = ArbExecCfg {
            chain_id: self.chain_id,
            spec_id,
            block_gas_limit: parent.gas_limit,
            disable_priority_fee_check: self.disable_priority_fee_check,
            disable_balance_check: self.disable_balance_check,
        };
        Ok(ArbExecutionInput::new(parent, message, cfg).with_mode(ArbExecutionMode::Commit))
    }
}

/// Expected output for the production-message path. The state delta is the exact
/// set of material writes emitted through `DatabaseCommit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedSequenceOutput {
    pub schema: String,
    pub post_state_root: B256,
    pub start_block_success: bool,
    pub start_block_gas_used: u64,
    pub transactions: Vec<SequenceTxOutput>,
    #[serde(default)]
    pub state: Vec<SequenceAccountDelta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SequenceTxOutput {
    pub tx_hash: B256,
    pub gas_used: u64,
    pub success: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SequenceAccountDelta {
    pub address: Address,
    pub exists: bool,
    #[serde(default)]
    pub nonce: u64,
    #[serde(default)]
    pub balance: U256,
    #[serde(default)]
    pub code_hash: B256,
    /// Required when this delta changes the account code hash. The bytes make a
    /// complete-state post-root independently reproducible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<Bytes>,
    #[serde(default)]
    pub storage: Vec<SequenceStorageDelta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SequenceStorageDelta {
    pub slot: U256,
    pub value: U256,
}

/// The actual result and every mismatch, suitable for a terse test assertion.
#[derive(Debug, Clone, Default)]
pub struct VectorReport {
    pub mismatches: Vec<String>,
}

impl VectorReport {
    pub fn is_parity(&self) -> bool {
        self.mismatches.is_empty()
    }
}

/// Runs a single complete-state / derived-transaction fixture without network
/// access. Execution enters `arb_revm::execute_message`, the production message
/// path used by block construction.
pub fn run_case(root: impl AsRef<Path>, fixture: &FixtureCase) -> VectorReport {
    let mut report = VectorReport::default();
    if let Err(error) = fixture.validate() {
        report
            .mismatches
            .push(format!("fixture validation failed: {error}"));
        return report;
    }
    let store = ObjectStore::new(root.as_ref());
    let prestate = match &fixture.prestate {
        FixturePrestate::Complete { object } => {
            decode_object::<CompleteState>(&store, object).map(LoadedPrestate::Complete)
        }
        FixturePrestate::ExecutionWitness { object } => {
            decode_object::<ExecutionWitnessPrestate>(&store, object).map(LoadedPrestate::Witness)
        }
    };
    let prestate = match prestate {
        Ok(prestate) => prestate,
        Err(error) => {
            report
                .mismatches
                .push(format!("prestate load failed: {error}"));
            return report;
        }
    };
    let input = match &fixture.input {
        FixtureInput::DerivedTransactions { object } => {
            decode_object::<DerivedTransactionsInput>(&store, object)
        }
        FixtureInput::RawFeed { .. } => {
            Err("raw-feed execution belongs to the production block runner".to_owned())
        }
    };
    let input = match input {
        Ok(input) => input,
        Err(error) => {
            report
                .mismatches
                .push(format!("input load failed: {error}"));
            return report;
        }
    };
    let expected = match decode_object::<ExpectedSequenceOutput>(&store, &fixture.expected.object) {
        Ok(expected) => expected,
        Err(error) => {
            report
                .mismatches
                .push(format!("expected output load failed: {error}"));
            return report;
        }
    };
    if expected.schema != SEQUENCE_OUTPUT_SCHEMA {
        report.mismatches.push(format!(
            "unsupported sequence output schema {:?}",
            expected.schema
        ));
        return report;
    }

    let (outcome, actual_state, actual_post_root) = match prestate {
        LoadedPrestate::Complete(prestate) => {
            let state = match StrictDatabase::from_complete_state(prestate.clone()) {
                Ok(state) => state,
                Err(error) => {
                    report
                        .mismatches
                        .push(format!("prestate validation failed: {error}"));
                    return report;
                }
            };
            let (outcome, state_delta) =
                match execute_with_db(state, input.clone(), fixture.effective_arbos_version) {
                    Ok(result) => result,
                    Err(error) => {
                        report.mismatches.push(error);
                        return report;
                    }
                };
            let root = match post_state_root(prestate, &state_delta) {
                Ok(root) => root,
                Err(error) => {
                    report
                        .mismatches
                        .push(format!("post-state root calculation failed: {error}"));
                    return report;
                }
            };
            (outcome, state_delta, root)
        }
        LoadedPrestate::Witness(prestate) => {
            let state = match WitnessDatabase::from_prestate(prestate) {
                Ok(state) => state,
                Err(error) => {
                    report
                        .mismatches
                        .push(format!("witness validation failed: {error}"));
                    return report;
                }
            };
            let root_source = state.clone();
            let (outcome, state_delta) =
                match execute_with_db(state, input, fixture.effective_arbos_version) {
                    Ok(result) => result,
                    Err(error) => {
                        report.mismatches.push(error);
                        return report;
                    }
                };
            let root = match witness_post_state_root(&root_source, &state_delta) {
                Ok(root) => root,
                Err(error) => {
                    report
                        .mismatches
                        .push(format!("post-state root calculation failed: {error}"));
                    return report;
                }
            };
            (outcome, state_delta, root)
        }
    };
    compare_output(
        &expected,
        &outcome,
        actual_state,
        actual_post_root,
        &mut report.mismatches,
    );
    report
}

enum LoadedPrestate {
    Complete(CompleteState),
    Witness(ExecutionWitnessPrestate),
}

fn execute_with_db<DB>(
    state: DB,
    input: DerivedTransactionsInput,
    expected_arbos_version: u64,
) -> Result<(arb_revm::ArbExecOutcome, Vec<SequenceAccountDelta>), String>
where
    DB: revm::DatabaseRef,
    DB::Error: core::fmt::Debug,
{
    let effective_arbos_version = read_effective_arbos_version(&state, input.message.l1_timestamp)?;
    if effective_arbos_version != expected_arbos_version {
        return Err(format!(
            "fixture ArbOS version mismatch: canonical header declares {expected_arbos_version}, \
             authenticated parent state resolves to {effective_arbos_version}"
        ));
    }
    let spec = exact_arbos_spec(effective_arbos_version)?;
    let input = input
        .into_execution_input(spec)
        .map_err(|error| format!("input decode failed: {error}"))?;
    let mut db = CommitRecordingDb::new(CacheDB::new(state));
    let outcome =
        execute_message(&mut db, &input).map_err(|error| format!("execution failed: {error:?}"))?;
    Ok((outcome, db.into_deltas()))
}

/// Reads every slot needed for Nitro's flag-day upgrade selection. Unlike the
/// production helper, this is deliberately strict: a fixture cannot fall back
/// to ArbOS 1 when its witness failed to prove one of these state reads.
fn read_effective_arbos_version<DB>(state: &DB, timestamp: u64) -> Result<u64, String>
where
    DB: revm::DatabaseRef,
    DB::Error: core::fmt::Debug,
{
    let arbos = ArbosState::open();
    let read = |slot: &StorageBacked<u64>, name: &str| -> Result<u64, String> {
        let (address, key) = slot.account_and_key();
        let value = state
            .storage_ref(address, U256::from_be_bytes(key.0))
            .map_err(|error| format!("fixture cannot prove ArbOS {name} slot: {error:?}"))?;
        u64::try_from(value).map_err(|_| format!("ArbOS {name} slot exceeds u64"))
    };
    let current = read(&arbos.arbos_version, "version")?;
    if current == 0 {
        return Err("fixture has no initialized ArbOS version".to_owned());
    }
    let upgrade_version = read(&arbos.upgrade_version, "upgrade version")?;
    let upgrade_timestamp = read(&arbos.upgrade_timestamp, "upgrade timestamp")?;
    Ok(
        if upgrade_version > current && timestamp >= upgrade_timestamp {
            upgrade_version
        } else {
            current
        },
    )
}

/// Every version must have an explicit executor representation. This avoids
/// silently treating ArbOS 59 as 51 or an unknown future version as 61.
fn exact_arbos_spec(version: u64) -> Result<ArbSpecId, String> {
    let spec = ArbSpecId::from_arbos_version(version);
    if spec.arbos_version() != version {
        return Err(format!(
            "ArbOS {version} is not explicitly represented by this executor"
        ));
    }
    Ok(spec)
}

fn decode_object<T: for<'de> Deserialize<'de>>(
    store: &ObjectStore,
    object: &arb_stf_fixture::FixtureObject,
) -> Result<T, String> {
    let bytes = store.get(object).map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

fn compare_output(
    expected: &ExpectedSequenceOutput,
    outcome: &arb_revm::ArbExecOutcome,
    actual_state: Vec<SequenceAccountDelta>,
    actual_post_root: B256,
    mismatches: &mut Vec<String>,
) {
    if outcome.start_block_success != expected.start_block_success {
        mismatches.push(format!(
            "StartBlock success mismatch: expected {}, got {}",
            expected.start_block_success, outcome.start_block_success
        ));
    }
    if outcome.start_block_gas_used != expected.start_block_gas_used {
        mismatches.push(format!(
            "StartBlock gas mismatch: expected {}, got {}",
            expected.start_block_gas_used, outcome.start_block_gas_used
        ));
    }
    let actual_txs = outcome
        .txs
        .iter()
        .map(tx_from_execution)
        .collect::<Vec<_>>();
    if actual_txs != expected.transactions {
        mismatches.push(format!(
            "transaction outcomes mismatch: expected {:?}, got {:?}",
            expected.transactions, actual_txs
        ));
    }
    if actual_state != expected.state {
        mismatches.push(format!(
            "state delta mismatch: expected {:?}, got {:?}",
            expected.state, actual_state
        ));
    }
    if actual_post_root != expected.post_state_root {
        mismatches.push(format!(
            "post-state root mismatch: expected {:#x}, got {actual_post_root:#x}",
            expected.post_state_root
        ));
    }
}

fn post_state_root(
    mut prestate: CompleteState,
    deltas: &[SequenceAccountDelta],
) -> Result<B256, IncompleteWitness> {
    let mut accounts: BTreeMap<Address, _> = prestate
        .accounts
        .drain(..)
        .map(|account| (account.address, account))
        .collect();
    for delta in deltas {
        if !delta.exists {
            accounts.remove(&delta.address);
            continue;
        }
        let account =
            accounts
                .entry(delta.address)
                .or_insert_with(|| crate::strict_db::CompleteAccount {
                    address: delta.address,
                    exists: true,
                    nonce: 0,
                    balance: U256::ZERO,
                    code: Bytes::new(),
                    storage: crate::strict_db::CompleteStorage {
                        complete: true,
                        slots: Vec::new(),
                    },
                });
        account.exists = true;
        account.nonce = delta.nonce;
        account.balance = delta.balance;
        if let Some(code) = &delta.code {
            if account_code_hash(code) != delta.code_hash {
                return Err(IncompleteWitness::InvalidCompleteState(format!(
                    "post-state account {:#x} has a code/hash mismatch",
                    delta.address
                )));
            }
            account.code = code.clone();
        } else if delta.code_hash != account_code_hash(&account.code) {
            return Err(IncompleteWitness::InvalidCompleteState(format!(
                "post-state account {:#x} changed code hash without canonical code bytes",
                delta.address
            )));
        }
        let mut storage: BTreeMap<U256, U256> = account
            .storage
            .slots
            .drain(..)
            .map(|entry| (entry.slot, entry.value))
            .collect();
        storage.extend(delta.storage.iter().map(|entry| (entry.slot, entry.value)));
        account.storage.slots = storage
            .into_iter()
            .map(|(slot, value)| crate::strict_db::CompleteStorageSlot { slot, value })
            .collect();
    }
    prestate.accounts = accounts.into_values().collect();
    complete_state_root(&prestate)
}

fn account_code_hash(code: &Bytes) -> B256 {
    if code.is_empty() {
        revm::primitives::KECCAK_EMPTY
    } else {
        revm::primitives::keccak256(code)
    }
}

fn witness_post_state_root(
    witness: &WitnessDatabase,
    deltas: &[SequenceAccountDelta],
) -> Result<B256, IncompleteWitness> {
    let nodes = witness.trie_nodes();
    let mut account_updates = BTreeMap::new();
    for delta in deltas {
        let account_key = Nibbles::unpack(revm::primitives::keccak256(delta.address));
        if !delta.exists {
            account_updates.insert(account_key, None);
            continue;
        }
        if let Some(code) = &delta.code {
            if account_code_hash(code) != delta.code_hash {
                return Err(IncompleteWitness::InvalidCompleteState(format!(
                    "post-state account {:#x} has a code/hash mismatch",
                    delta.address
                )));
            }
        }
        let before = witness.trie_account(delta.address)?.unwrap_or_default();
        let mut storage_updates = BTreeMap::new();
        for storage in &delta.storage {
            let key = B256::from(storage.slot.to_be_bytes::<32>());
            let value = (storage.value != U256::ZERO).then(|| alloy_rlp::encode(storage.value));
            storage_updates.insert(Nibbles::unpack(revm::primitives::keccak256(key)), value);
        }
        let storage_root =
            arb_revm::state_trie::recompute_root(before.storage_root, &nodes, &storage_updates)
                .map_err(IncompleteWitness::InvalidCompleteState)?;
        let account = alloy_trie::TrieAccount {
            nonce: delta.nonce,
            balance: delta.balance,
            storage_root,
            code_hash: delta.code_hash,
        };
        account_updates.insert(account_key, Some(alloy_rlp::encode(account)));
    }
    arb_revm::state_trie::recompute_root(witness.state_root(), &nodes, &account_updates)
        .map_err(IncompleteWitness::InvalidCompleteState)
}

fn tx_from_execution(tx: &ArbTxExecution) -> SequenceTxOutput {
    SequenceTxOutput {
        tx_hash: tx.tx_hash,
        gas_used: tx.gas_used,
        success: tx.success,
    }
}

/// Records exactly the state passed to commit, avoiding the read-set ambiguity of
/// a cache snapshot. It delegates all actual database behavior unchanged.
struct CommitRecordingDb<DB> {
    inner: DB,
    deltas: BTreeMap<Address, SequenceAccountDelta>,
}

impl<DB> CommitRecordingDb<DB> {
    fn new(inner: DB) -> Self {
        Self {
            inner,
            deltas: BTreeMap::new(),
        }
    }

    fn into_deltas(self) -> Vec<SequenceAccountDelta> {
        self.deltas.into_values().collect()
    }

    fn record(&mut self, address: Address, account: &Account) {
        if !account.is_touched() {
            return;
        }
        if account.is_selfdestructed() {
            self.deltas.insert(
                address,
                SequenceAccountDelta {
                    address,
                    exists: false,
                    nonce: 0,
                    balance: U256::ZERO,
                    code_hash: B256::ZERO,
                    code: None,
                    storage: Vec::new(),
                },
            );
            return;
        }
        let entry = self
            .deltas
            .entry(address)
            .or_insert_with(|| SequenceAccountDelta {
                address,
                exists: true,
                nonce: account.info.nonce,
                balance: account.info.balance,
                code_hash: account.info.code_hash,
                code: None,
                storage: Vec::new(),
            });
        entry.exists = true;
        entry.nonce = account.info.nonce;
        entry.balance = account.info.balance;
        entry.code_hash = account.info.code_hash;
        if account.is_created() || account.info.code_hash != account.original_info().code_hash {
            entry.code = account.info.code.as_ref().map(|code| code.original_bytes());
        }
        if account.is_created() {
            entry.storage.clear();
        }
        let mut storage: BTreeMap<U256, U256> = entry
            .storage
            .drain(..)
            .map(|slot| (slot.slot, slot.value))
            .collect();
        for (slot, value) in account.changed_storage_slots() {
            storage.insert(*slot, value.present_value());
        }
        entry.storage = storage
            .into_iter()
            .map(|(slot, value)| SequenceStorageDelta { slot, value })
            .collect();
    }
}

impl<DB: Database> Database for CommitRecordingDb<DB> {
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.inner.basic(address)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<revm::state::Bytecode, Self::Error> {
        self.inner.code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.inner.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash(number)
    }
}

impl<DB: Database + DatabaseCommit> DatabaseCommit for CommitRecordingDb<DB> {
    fn commit(&mut self, changes: revm::primitives::AddressMap<Account>) {
        for (address, account) in &changes {
            self.record(*address, account);
        }
        self.inner.commit(changes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsupported_input_schema() {
        let input = DerivedTransactionsInput {
            schema: "wrong".to_owned(),
            chain_id: 1,
            parent: SequenceParent {
                number: 0,
                timestamp: 0,
                beneficiary: Address::ZERO,
                basefee: 0,
                gas_limit: 1,
                difficulty: U256::ZERO,
                prevrandao: None,
            },
            message: SequenceMessage {
                sequence_number: None,
                l1_block_number: 0,
                l1_timestamp: 0,
                poster: Address::ZERO,
                l1_base_fee_wei: U256::ZERO,
                delayed_messages_read: 0,
                transactions: Vec::new(),
            },
            disable_priority_fee_check: true,
            disable_balance_check: true,
        };
        assert!(
            input
                .into_execution_input(arb_revm::ArbSpecId::NITRO)
                .is_err()
        );
    }

    #[test]
    fn rejects_versions_without_an_explicit_executor_spec() {
        assert_eq!(exact_arbos_spec(40).unwrap(), ArbSpecId::ARBOS_40);
        assert!(exact_arbos_spec(59).is_err());
        assert!(exact_arbos_spec(62).is_err());
    }
}
