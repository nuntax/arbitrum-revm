//! Hermetic block-replay fixtures.
//!
//! A [`ReplayFixture`] is a self-contained snapshot of everything needed to replay
//! one Arbitrum block against the local engine with **no live node**: the block
//! environment, the exact prestate the block reads (captured via [`RecordingDb`]),
//! the transactions as received over RPC, and the values Nitro produced for them.
//!
//! The flow is:
//!  * **record**, wrap a live state source in [`RecordingDb`] while replaying a block
//!    so every account / storage slot / blockhash the block touches is logged, then
//!    serialize the result with the transactions and Nitro's receipts.
//!  * **replay**, [`replay_fixture`] seeds an in-memory DB from the snapshot, runs
//!    each transaction through the same conversion + handler the binary uses, and
//!    diffs the outcome against the recorded expectations.
//!
//! This turns each tricky case captured off a testnode into a deterministic,
//! infra-free `cargo test`.

use std::cell::RefCell;
use std::collections::BTreeMap;

use arb_alloy_rpc_types::ArbTransaction as RpcArbTransaction;
use revm::{
    DatabaseRef, ExecuteCommitEvm, ExecuteEvm,
    context::{BlockEnv, CfgEnv, TxEnv},
    database::CacheDB,
    primitives::{Address, B256, Bytes, KECCAK_EMPTY, StorageKey, StorageValue, U256},
    state::{AccountInfo, Bytecode, EvmState},
};
use serde::{Deserialize, Serialize};

use crate::{
    ArbBuilder, ArbChainContext, ArbContext, ArbSpecId, ArbTransaction, DefaultArb,
    transaction::arb_envelope_to_tx_env,
};

/// Current on-disk fixture schema identifier.
pub const REPLAY_FIXTURE_SCHEMA: &str = "arb-revm-replay-v1";

/// A self-contained, replayable snapshot of one block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayFixture {
    /// Schema tag; see [`REPLAY_FIXTURE_SCHEMA`].
    pub schema: String,
    /// Chain id the block belongs to.
    pub chain_id: u64,
    /// Block environment the transactions execute under.
    pub block: BlockFixture,
    /// Minimal prestate the block reads.
    pub prestate: PrestateFixture,
    /// Transactions exactly as they arrive over RPC.
    pub transactions: Vec<RpcArbTransaction>,
    /// Per-transaction outcomes Nitro produced (the parity oracle).
    pub expected: Vec<ExpectedTx>,
    /// Nitro's post-block state for every account/slot the engine wrote, the
    /// state-parity oracle. Captured at block `N` for exactly the accounts and
    /// slots our execution touched. Empty when not recorded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_state: Vec<ExpectedAccountState>,
}

/// Nitro's post-block values for one account, restricted to the fields/slots the
/// local engine wrote. `None` fields are not asserted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedAccountState {
    pub address: Address,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<U256>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_hash: Option<B256>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub storage: Vec<StorageEntry>,
}

/// Block environment fields needed to reconstruct a [`BlockEnv`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockFixture {
    /// L2 block number (`BlockEnv.number`, used for chain rules / BLOCKHASH).
    pub number: u64,
    /// L1 block number returned by the `NUMBER` opcode on Arbitrum. Defaults to 0
    /// for older fixtures that predate the field.
    #[serde(default)]
    pub l1_block_number: u64,
    pub timestamp: u64,
    pub basefee: u64,
    pub gas_limit: u64,
    pub difficulty: U256,
    pub beneficiary: Address,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prevrandao: Option<B256>,
}

/// The exact state reads a block performs, captured as a flat snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrestateFixture {
    pub accounts: Vec<AccountFixture>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contracts: Vec<ContractFixture>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub block_hashes: Vec<BlockHashFixture>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountFixture {
    pub address: Address,
    pub balance: U256,
    pub nonce: u64,
    pub code_hash: B256,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub storage: Vec<StorageEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageEntry {
    pub slot: U256,
    pub value: U256,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractFixture {
    pub code_hash: B256,
    pub code: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockHashFixture {
    pub number: u64,
    pub hash: B256,
}

/// One transaction's expected outcome, taken from Nitro's receipt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedTx {
    pub tx_hash: B256,
    pub success: bool,
    pub gas_used: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_address: Option<Address>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<ExpectedLog>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedLog {
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Bytes,
}

/// Result of replaying a fixture: the executed count and a list of human-readable
/// mismatches. Empty `mismatches` means the local engine matched Nitro exactly.
#[derive(Debug, Clone, Default)]
pub struct ReplayReport {
    pub executed: usize,
    pub mismatches: Vec<String>,
}

impl ReplayReport {
    /// Whether the replay reproduced Nitro's outcomes exactly.
    pub fn is_parity(&self) -> bool {
        self.mismatches.is_empty()
    }
}

/// A [`DatabaseRef`] wrapper that records every read served to the engine.
///
/// Because reads only miss through to the inner source on first access, and commits
/// land in the outer cache rather than here, the recorded set is exactly the block's
/// prestate, minimal and free of values produced during execution.
pub struct RecordingDb<DB> {
    inner: DB,
    recorded: RefCell<Recorded>,
}

#[derive(Default)]
struct Recorded {
    accounts: BTreeMap<Address, AccountInfo>,
    storage: BTreeMap<Address, BTreeMap<U256, U256>>,
    contracts: BTreeMap<B256, Bytecode>,
    block_hashes: BTreeMap<u64, B256>,
}

impl<DB> RecordingDb<DB> {
    pub fn new(inner: DB) -> Self {
        Self {
            inner,
            recorded: RefCell::new(Recorded::default()),
        }
    }

    /// Builds the prestate snapshot from everything read so far.
    pub fn to_prestate(&self) -> PrestateFixture {
        let recorded = self.recorded.borrow();
        let accounts = recorded
            .accounts
            .iter()
            .map(|(address, info)| AccountFixture {
                address: *address,
                balance: info.balance,
                nonce: info.nonce,
                code_hash: info.code_hash,
                storage: recorded
                    .storage
                    .get(address)
                    .into_iter()
                    .flatten()
                    .map(|(slot, value)| StorageEntry {
                        slot: *slot,
                        value: *value,
                    })
                    .collect(),
            })
            .collect();
        let contracts = recorded
            .contracts
            .iter()
            .filter(|(hash, _)| **hash != KECCAK_EMPTY)
            .map(|(code_hash, code)| ContractFixture {
                code_hash: *code_hash,
                code: code.original_bytes(),
            })
            .collect();
        let block_hashes = recorded
            .block_hashes
            .iter()
            .map(|(number, hash)| BlockHashFixture {
                number: *number,
                hash: *hash,
            })
            .collect();
        PrestateFixture {
            accounts,
            contracts,
            block_hashes,
        }
    }
}

impl<DB: DatabaseRef> DatabaseRef for RecordingDb<DB> {
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let info = self.inner.basic_ref(address)?;
        if let Some(info) = &info {
            let mut rec = self.recorded.borrow_mut();
            rec.accounts.insert(address, info.clone());
            if let Some(code) = info
                .code
                .as_ref()
                .filter(|_| info.code_hash != KECCAK_EMPTY)
            {
                rec.contracts.insert(info.code_hash, code.clone());
            }
        }
        Ok(info)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        let code = self.inner.code_by_hash_ref(code_hash)?;
        if code_hash != KECCAK_EMPTY {
            self.recorded
                .borrow_mut()
                .contracts
                .insert(code_hash, code.clone());
        }
        Ok(code)
    }

    fn storage_ref(
        &self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        let value = self.inner.storage_ref(address, index)?;
        self.recorded
            .borrow_mut()
            .storage
            .entry(address)
            .or_default()
            .insert(index, value);
        Ok(value)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        let hash = self.inner.block_hash_ref(number)?;
        self.recorded.borrow_mut().block_hashes.insert(number, hash);
        Ok(hash)
    }
}

/// Reconstructs the [`BlockEnv`] described by a fixture.
pub fn block_env(fixture: &ReplayFixture) -> BlockEnv {
    let block = &fixture.block;
    BlockEnv {
        number: U256::from(block.number),
        timestamp: U256::from(block.timestamp),
        basefee: block.basefee,
        gas_limit: block.gas_limit,
        difficulty: block.difficulty,
        beneficiary: block.beneficiary,
        prevrandao: block.prevrandao,
        ..BlockEnv::default()
    }
}

/// Seeds a fresh in-memory database from a fixture's prestate.
pub fn seed_db(fixture: &ReplayFixture) -> CacheDB<revm::database::EmptyDB> {
    let mut db = CacheDB::new(revm::database::EmptyDB::default());

    let contracts: BTreeMap<B256, Bytecode> = fixture
        .prestate
        .contracts
        .iter()
        .map(|c| (c.code_hash, Bytecode::new_raw(c.code.clone())))
        .collect();

    for account in &fixture.prestate.accounts {
        let code = contracts.get(&account.code_hash).cloned();
        let info = AccountInfo {
            balance: account.balance,
            nonce: account.nonce,
            code_hash: account.code_hash,
            code,
            ..AccountInfo::default()
        };
        db.insert_account_info(account.address, info);
        for entry in &account.storage {
            // Infallible for EmptyDB-backed CacheDB.
            let _ = db.insert_account_storage(account.address, entry.slot, entry.value);
        }
    }

    for bh in &fixture.prestate.block_hashes {
        db.cache.block_hashes.insert(U256::from(bh.number), bh.hash);
    }

    db
}

/// Replays a fixture against a seeded in-memory DB and diffs the result against the
/// recorded Nitro outcomes. The returned [`ReplayReport`] lists every mismatch.
pub fn replay_fixture(fixture: &ReplayFixture) -> ReplayReport {
    let mut db = seed_db(fixture);

    // Derive the EVM spec from the block's *effective* ArbOS version (accounting for a
    // scheduled upgrade due this block), since Nitro selects the hardfork by ArbOS
    // version and applies a due upgrade before the block's user txs run.
    let spec = ArbSpecId::from_arbos_version(crate::ArbosState::read_effective_version(
        &db,
        fixture.block.timestamp,
    ));
    let mut cfg_env = CfgEnv::new_with_spec(spec)
        .with_chain_id(fixture.chain_id)
        .with_disable_priority_fee_check(true);
    cfg_env.disable_balance_check = true;

    let chain = ArbChainContext::new(None).with_l1_block_number(fixture.block.l1_block_number);
    let context: ArbContext<&mut _> = ArbContext::arb_with_chain_context(chain)
        .with_db(&mut db)
        .with_cfg(cfg_env)
        .with_block(block_env(fixture))
        .with_tx(ArbTransaction::<TxEnv>::default());
    let mut evm = context.build_arb();

    let mut report = ReplayReport::default();
    // Accumulate the net post-block write set so we can diff it against Nitro.
    let mut writes: BTreeMap<Address, AccountWrites> = BTreeMap::new();

    for (idx, rpc_tx) in fixture.transactions.iter().enumerate() {
        let envelope = rpc_tx.as_ref();
        let tx_hash = envelope.hash();

        let tx_env = match arb_envelope_to_tx_env(envelope) {
            Ok(tx) => tx,
            Err(err) => {
                report
                    .mismatches
                    .push(format!("tx[{idx}] {tx_hash:#x}: conversion failed: {err}"));
                continue;
            }
        };

        let outcome = match evm.transact(tx_env) {
            Ok(outcome) => outcome,
            Err(err) => {
                report
                    .mismatches
                    .push(format!("tx[{idx}] {tx_hash:#x}: execution error: {err:?}"));
                continue;
            }
        };

        report.executed += 1;
        match fixture.expected.get(idx) {
            Some(expected) => {
                compare_tx(idx, expected, &outcome.result, &mut report.mismatches);
            }
            None => report
                .mismatches
                .push(format!("tx[{idx}] {tx_hash:#x}: no expected entry in fixture")),
        }
        merge_state_writes(&mut writes, &outcome.state);
        evm.commit(outcome.state);
    }

    compare_state(&writes, &fixture.expected_state, &mut report.mismatches);

    report
}

/// Net post-block write for one account: latest written info fields and slots.
#[derive(Default)]
struct AccountWrites {
    balance: Option<U256>,
    nonce: Option<u64>,
    code_hash: Option<B256>,
    storage: BTreeMap<U256, U256>,
}

/// Folds one transaction's state diff into the running net write set (later txs
/// overwrite earlier ones), mirroring `replay_block`'s live accumulation.
fn merge_state_writes(writes: &mut BTreeMap<Address, AccountWrites>, state: &EvmState) {
    for (address, account) in state {
        let info_changed = account.info != account.original_info()
            || account.is_created()
            || account.is_selfdestructed();
        let mut changed_slots = account.changed_storage_slots().peekable();
        if !info_changed && changed_slots.peek().is_none() {
            continue;
        }
        let entry = writes.entry(*address).or_default();
        if info_changed {
            entry.balance = Some(account.info.balance);
            entry.nonce = Some(account.info.nonce);
            entry.code_hash = Some(account.info.code_hash);
        }
        for (slot, value) in changed_slots {
            entry.storage.insert(*slot, value.present_value());
        }
    }
}

/// Diffs our net write set against Nitro's recorded post-block state. Each field
/// in `expected_state` that Nitro recorded is checked against what we wrote.
fn compare_state(
    writes: &BTreeMap<Address, AccountWrites>,
    expected_state: &[ExpectedAccountState],
    mismatches: &mut Vec<String>,
) {
    for expected in expected_state {
        let ours = writes.get(&expected.address);
        let addr = expected.address;

        if let Some(want) = expected.balance {
            let got = ours.and_then(|w| w.balance);
            if got != Some(want) {
                mismatches.push(format!(
                    "state {addr:#x}: balance mismatch: expected {want:#x}, got {}",
                    fmt_opt_u256(got)
                ));
            }
        }
        if let Some(want) = expected.nonce {
            let got = ours.and_then(|w| w.nonce);
            if got != Some(want) {
                mismatches.push(format!(
                    "state {addr:#x}: nonce mismatch: expected {want}, got {got:?}"
                ));
            }
        }
        if let Some(want) = expected.code_hash {
            let got = ours.and_then(|w| w.code_hash);
            if got != Some(want) {
                mismatches.push(format!(
                    "state {addr:#x}: code_hash mismatch: expected {want:#x}, got {got:?}"
                ));
            }
        }
        for entry in &expected.storage {
            let got = ours.and_then(|w| w.storage.get(&entry.slot).copied());
            if got != Some(entry.value) {
                mismatches.push(format!(
                    "state {addr:#x} slot {:#x}: mismatch: expected {:#x}, got {}",
                    entry.slot,
                    entry.value,
                    fmt_opt_u256(got)
                ));
            }
        }
    }
}

fn fmt_opt_u256(value: Option<U256>) -> String {
    match value {
        Some(v) => format!("{v:#x}"),
        None => "<unwritten>".to_string(),
    }
}

fn compare_tx(
    idx: usize,
    expected: &ExpectedTx,
    result: &revm::context::result::ExecutionResult,
    mismatches: &mut Vec<String>,
) {
    if result.is_success() != expected.success {
        mismatches.push(format!(
            "tx[{idx}] {:#x}: status mismatch: expected success={}, got success={}",
            expected.tx_hash,
            expected.success,
            result.is_success()
        ));
    }
    if result.tx_gas_used() != expected.gas_used {
        mismatches.push(format!(
            "tx[{idx}] {:#x}: gas_used mismatch: expected {}, got {}",
            expected.tx_hash,
            expected.gas_used,
            result.tx_gas_used()
        ));
    }
    let actual_created = result.created_address();
    if actual_created != expected.created_address {
        mismatches.push(format!(
            "tx[{idx}] {:#x}: created_address mismatch: expected {:?}, got {:?}",
            expected.tx_hash, expected.created_address, actual_created
        ));
    }

    let actual_logs = result.logs();
    if actual_logs.len() != expected.logs.len() {
        mismatches.push(format!(
            "tx[{idx}] {:#x}: log count mismatch: expected {}, got {}",
            expected.tx_hash,
            expected.logs.len(),
            actual_logs.len()
        ));
    }
    for (log_idx, (actual, exp)) in actual_logs.iter().zip(expected.logs.iter()).enumerate() {
        if actual.address != exp.address
            || actual.topics() != exp.topics.as_slice()
            || actual.data.data.as_ref() != exp.data.as_ref()
        {
            mismatches.push(format!(
                "tx[{idx}] {:#x}: log[{log_idx}] mismatch",
                expected.tx_hash
            ));
        }
    }
}
