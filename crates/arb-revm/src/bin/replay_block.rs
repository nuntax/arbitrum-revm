use std::collections::BTreeMap;
use std::env;
use std::str::FromStr;
use std::time::Duration;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network_primitives::ReceiptResponse;
use alloy_provider::ext::DebugApi;
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_client::ClientBuilder;
use alloy_rpc_types_trace::geth::{GethDebugTracingOptions, GethTrace};
use alloy_transport::layers::RetryBackoffLayer;
use arbitrum_alloy_network::Arbitrum;
use arbitrum_alloy_rpc_types::{ArbTransaction as RpcArbTransaction, ArbTransactionReceipt};
use arb_revm::replay::{
    BlockFixture, ExpectedAccountState, ExpectedLog, ExpectedTx, REPLAY_FIXTURE_SCHEMA, RecordingDb,
    ReplayFixture, StorageEntry,
};
use arb_revm::transaction::arb_envelope_to_tx_env;
use arb_revm::{ArbBuilder, ArbChainContext, ArbContext, ArbSpecId, ArbTransaction, DefaultArb};
use eyre::{Result, eyre};
use revm::{
    ExecuteCommitEvm, ExecuteEvm,
    bytecode::opcode::OpCode,
    context::{BlockEnv, CfgEnv, TxEnv},
    database::CacheDB,
    database_interface::WrapDatabaseAsync,
    inspector::{InspectCommitEvm, Inspector},
    interpreter::{
        CallInputs, CallOutcome, Interpreter, interpreter::EthInterpreter,
        interpreter_types::Jumps,
    },
    primitives::{Address, B256, Bytes, KECCAK_EMPTY, U256, keccak256},
    state::EvmState,
};
use revm_database::{AlloyDB, AlloyDBError, async_db::DatabaseAsyncRef};
use alloy_trie::{Nibbles, TrieAccount, proof::verify_proof};
use revm::{
    bytecode::Bytecode,
    primitives::{StorageKey, StorageValue},
    state::AccountInfo,
};

const EXEC_MAX_ATTEMPTS: usize = 8;

/// Wraps [`AlloyDB`] to distinguish a genuinely non-existent account from an existing
/// empty one. Over RPC both read back as zero-balance / zero-nonce / empty-code, so plain
/// `AlloyDB` always returns `Some(empty)`, which makes revm treat absent accounts as
/// existing, corrupting EIP-7702 refunds, empty-account gas, and (provider-dependently)
/// the write set. For an empty-looking account we run one `eth_getProof` and, if its
/// account proof proves *absence* under the state root, return `None` like a real
/// trie-backed node would. Present-but-empty accounts that hold storage (e.g. the ArbOS
/// state account `0xa4b05fff…`) prove as *present*, so they correctly stay `Some`.
struct ExistenceAwareDb<P: Provider<Arbitrum>> {
    inner: AlloyDB<Arbitrum, P>,
    provider: P,
    block_id: BlockId,
    state_root: B256,
}

impl<P: Provider<Arbitrum> + Clone> ExistenceAwareDb<P> {
    fn new(provider: P, block_id: BlockId, state_root: B256) -> Self {
        Self {
            inner: AlloyDB::new(provider.clone(), block_id),
            provider,
            block_id,
            state_root,
        }
    }
}

impl<P: Provider<Arbitrum>> DatabaseAsyncRef for ExistenceAwareDb<P> {
    type Error = AlloyDBError;

    async fn basic_async_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let info = self.inner.basic_async_ref(address).await?;
        if info.as_ref().is_some_and(|a| a.is_empty()) {
            let proof = self
                .provider
                .get_proof(address, Vec::new())
                .block_id(self.block_id)
                .await?;
            let key = Nibbles::unpack(keccak256(address));
            if verify_proof(self.state_root, key, None, &proof.account_proof).is_ok() {
                return Ok(None); // proven absent from the state trie
            }
        }
        Ok(info)
    }

    async fn code_by_hash_async_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.inner.code_by_hash_async_ref(code_hash).await
    }

    async fn storage_async_ref(
        &self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        self.inner.storage_async_ref(address, index).await
    }

    async fn block_hash_async_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash_async_ref(number).await
    }
}

#[derive(Debug, Default, Clone)]
struct AccountWriteSet {
    balance: Option<U256>,
    nonce: Option<u64>,
    code_hash: Option<B256>,
    storage: BTreeMap<U256, U256>,
    /// Our execution marked this account `Created` (revm `mark_created`). revm-database's
    /// `apply_account_state` writes a created account via `newly_created` *before* the EIP-161
    /// empty-clear, so a created-but-empty account (e.g. the ArbOS pre-Stylus "zombie escrow")
    /// is kept in the trie. Mirror that here so the witness root matches the node/canonical.
    created: bool,
}

#[derive(Debug, Clone)]
struct TraceSummary {
    failed: bool,
    output: Option<Bytes>,
}

fn usage() -> &'static str {
    "usage: replay_block <rpc_url> <block_number> [--fail-fast] [--skip-system-sender] [--record <path>]"
}

fn parse_record_path(args: &[String]) -> Option<String> {
    args.iter()
        .position(|arg| arg == "--record")
        .and_then(|idx| args.get(idx + 1).cloned())
}

fn parse_trace_tx(args: &[String]) -> Option<usize> {
    args.iter()
        .position(|arg| arg == "--trace-tx")
        .and_then(|idx| args.get(idx + 1))
        .and_then(|s| s.parse().ok())
}

/// Opcode/call-frequency inspector for diagnosing execution divergence (e.g. a tx
/// that runs far more gas than Nitro): records what the engine actually does.
#[derive(Default)]
struct TraceInsp {
    enabled: bool,
    steps: u64,
    opcodes: BTreeMap<u8, u64>,
    op_gas: BTreeMap<u8, u64>,
    calls: BTreeMap<Address, u64>,
    /// Total gas charged for the CALL opcode targeting each address (includes the
    /// precompile/sub-call cost). Precompile calls have no sub-steps, so the CALL
    /// opcode's gas delta is exactly the full cost of invoking that precompile.
    call_gas: BTreeMap<Address, u64>,
    last_op: u8,
    last_gas: u64,
    last_pc: usize,
    pending_call_target: Option<Address>,
    /// Linear (pc, opcode, gasCost) sequence for diffing against a Nitro structLog.
    linear: Vec<(usize, u8, u64)>,
    /// Top-of-stack snapshot (BEFORE each op, geth structLog convention) for value diffs.
    stacks: Vec<Vec<String>>,
}

impl<CTX> Inspector<CTX, EthInterpreter> for TraceInsp {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _ctx: &mut CTX) {
        if self.enabled {
            let op = interp.bytecode.opcode();
            *self.opcodes.entry(op).or_default() += 1;
            self.steps += 1;
            self.last_op = op;
            self.last_gas = interp.gas.remaining();
            self.last_pc = interp.bytecode.pc();
            let data = interp.stack.data();
            let n = data.len();
            let top: Vec<String> =
                data[n.saturating_sub(5)..].iter().rev().map(|v| format!("{v:#x}")).collect();
            self.stacks.push(top);
        }
    }

    fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, _ctx: &mut CTX) {
        if self.enabled {
            let delta = self.last_gas.saturating_sub(interp.gas.remaining());
            *self.op_gas.entry(self.last_op).or_default() += delta;
            self.linear.push((self.last_pc, self.last_op, delta));
            if let Some(t) = self.pending_call_target.take() {
                *self.call_gas.entry(t).or_default() += delta;
            }
        }
    }

    fn call(&mut self, _ctx: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if self.enabled {
            *self.calls.entry(inputs.bytecode_address).or_default() += 1;
            self.pending_call_target = Some(inputs.bytecode_address);
        }
        None
    }
}

fn is_retryable_message(message: &str) -> bool {
    let msg = message.to_ascii_lowercase();
    msg.contains("temporary internal error")
        || msg.contains("trace-id")
        || msg.contains("timed out")
        || msg.contains("timeout")
        || msg.contains("too many requests")
        || msg.contains("http 500")
        || msg.contains("http 502")
        || msg.contains("http 503")
        || msg.contains("http 504")
        || msg.contains("connection reset")
        || msg.contains("connection closed")
        || msg.contains("connection refused")
        || msg.contains("transport")
        || msg.contains("neither result nor error")
        || msg.contains("failed to warm arbos")
        || msg.contains("failed to read arbos version")
        || msg.contains("failed to read retryable existence")
        || msg.contains("returned null result")
}

fn is_debug_unsupported(message: &str) -> bool {
    let msg = message.to_ascii_lowercase();
    msg.contains("method not found")
        || msg.contains("does not exist")
        || msg.contains("debug_tracetransaction")
}

/// Fetch a default geth trace summary for `tx_hash`. Returns `Ok(None)` when the
/// node lacks the `debug` namespace so the caller can disable byte-level checks.
async fn fetch_trace_summary<P: Provider<Arbitrum>>(
    provider: &P,
    tx_hash: B256,
) -> Result<Option<TraceSummary>> {
    match provider
        .debug_trace_transaction(tx_hash, GethDebugTracingOptions::default())
        .await
    {
        Ok(GethTrace::Default(frame)) => Ok(Some(TraceSummary {
            failed: frame.failed,
            output: Some(frame.return_value),
        })),
        Ok(_) => Ok(Some(TraceSummary {
            failed: false,
            output: None,
        })),
        Err(err) => {
            if is_debug_unsupported(&err.to_string()) {
                Ok(None)
            } else {
                Err(eyre!("debug_traceTransaction failed: {err}"))
            }
        }
    }
}

fn merge_state_writes(writes: &mut BTreeMap<Address, AccountWriteSet>, state: &EvmState) {
    for (address, account) in state {
        let info_changed = account.info != account.original_info()
            || account.is_created()
            || account.is_selfdestructed();
        let mut changed_slots = account.changed_storage_slots().peekable();
        if !info_changed && changed_slots.peek().is_none() {
            continue;
        }

        let entry = writes.entry(*address).or_default();
        entry.created |= account.is_created();
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

/// Direct value comparison of our write-set against `eth_getStorageAt`/`get_balance`
/// reads. Superseded as the primary gate by `verify_writes_against_state_root` (proof-
/// anchored, fewer calls); kept as a non-proof fallback localizer.
#[allow(dead_code)]
async fn compare_state_writes<P: Provider<Arbitrum>>(
    provider: &P,
    block_number: u64,
    writes: &BTreeMap<Address, AccountWriteSet>,
) -> Result<Vec<String>> {
    let mut errors = Vec::new();
    let block_id = BlockId::number(block_number);

    for (address, expected) in writes {
        if let Some(expected_balance) = expected.balance {
            let actual_balance = provider
                .get_balance(*address)
                .block_id(block_id)
                .await?;
            if actual_balance != expected_balance {
                errors.push(format!(
                    "state {address:#x} balance mismatch: expected {expected_balance:#x}, got {actual_balance:#x}"
                ));
            }
        }

        if let Some(expected_nonce) = expected.nonce {
            let actual_nonce = provider
                .get_transaction_count(*address)
                .block_id(block_id)
                .await?;
            if actual_nonce != expected_nonce {
                errors.push(format!(
                    "state {address:#x} nonce mismatch: expected {expected_nonce}, got {actual_nonce}"
                ));
            }
        }

        if let Some(expected_code_hash) = expected.code_hash {
            let actual_code = provider.get_code_at(*address).block_id(block_id).await?;
            let actual_code_hash = if actual_code.is_empty() {
                KECCAK_EMPTY
            } else {
                keccak256(actual_code.as_ref())
            };
            if actual_code_hash != expected_code_hash {
                errors.push(format!(
                    "state {address:#x} code_hash mismatch: expected {expected_code_hash:#x}, got {actual_code_hash:#x}"
                ));
            }
        }

        for (slot, expected_value) in &expected.storage {
            let actual_value = provider
                .get_storage_at(*address, *slot)
                .block_id(block_id)
                .await?;
            if actual_value != *expected_value {
                errors.push(format!(
                    "state {address:#x} slot {slot:#x} mismatch: expected {expected_value:#x}, got {actual_value:#x}"
                ));
            }
        }
    }

    Ok(errors)
}

/// Verifies our write-set against the canonical state committed in the block's
/// `stateRoot`, via `eth_getProof` + Merkle proof verification. For each account we
/// wrote: prove the canonical account is in `state_root`, then compare each field we
/// wrote against the proven value; for each slot we wrote: verify our value is committed
/// under the account's canonical storage root. This anchors our writes to the consensus
/// state root instead of trusting raw `eth_getStorageAt` reads, and bundles each account
/// into a single `getProof` call (fewer RPC round-trips than per-slot reads).
///
/// Blind spot (closed by the Stage-2 witness-root recompute): a write Nitro made that we
/// missed entirely is not in `writes`, so it is never queried here.
async fn verify_writes_against_state_root<P: Provider<Arbitrum>>(
    provider: &P,
    block_number: u64,
    state_root: B256,
    writes: &BTreeMap<Address, AccountWriteSet>,
) -> Result<Vec<String>> {
    let mut errors = Vec::new();
    let block_id = BlockId::number(block_number);

    for (address, expected) in writes {
        let slot_keys: Vec<B256> = expected
            .storage
            .keys()
            .map(|s| B256::from(s.to_be_bytes::<32>()))
            .collect();
        let proof = retry_read(|| {
            provider
                .get_proof(*address, slot_keys.clone())
                .block_id(block_id)
        })
        .await?;

        // 1) Prove the canonical account (nonce/balance/storageRoot/codeHash) is committed
        //    in the block's state root, authenticates the values we compare against.
        let account = TrieAccount {
            nonce: proof.nonce,
            balance: proof.balance,
            storage_root: proof.storage_hash,
            code_hash: proof.code_hash,
        };
        let account_key = Nibbles::unpack(keccak256(address));
        if let Err(e) = verify_proof(
            state_root,
            account_key,
            Some(alloy_rlp::encode(account)),
            &proof.account_proof,
        ) {
            errors.push(format!(
                "state {address:#x}: account proof failed against stateRoot {state_root:#x}: {e}"
            ));
            continue;
        }

        // 2) Each account field we wrote must match the proven canonical value.
        if let Some(b) = expected.balance
            && b != proof.balance {
                errors.push(format!(
                    "state {address:#x}: balance mismatch: expected {:#x}, got(ours) {b:#x}",
                    proof.balance
                ));
            }
        if let Some(n) = expected.nonce
            && n != proof.nonce {
                errors.push(format!(
                    "state {address:#x}: nonce mismatch: expected {}, got(ours) {n}",
                    proof.nonce
                ));
            }
        if let Some(h) = expected.code_hash
            && h != proof.code_hash {
                errors.push(format!(
                    "state {address:#x}: code_hash mismatch: expected {:#x}, got(ours) {h:#x}",
                    proof.code_hash
                ));
            }

        // 3) Each slot we wrote must be committed (with our value) under the storage root.
        //    getProof returns storage proofs in request order, matching `slot_keys`.
        for ((slot, value), sp) in expected.storage.iter().zip(proof.storage_proof.iter()) {
            let key = Nibbles::unpack(keccak256(B256::from(slot.to_be_bytes::<32>())));
            let expected_value = if value.is_zero() {
                None
            } else {
                Some(alloy_rlp::encode(value))
            };
            if let Err(e) = verify_proof(proof.storage_hash, key, expected_value, &sp.proof) {
                errors.push(format!(
                    "state {address:#x} slot {slot:#x}: our value {value:#x} not committed under storageRoot: {e}"
                ));
            }
        }
    }

    Ok(errors)
}

/// Stage-2 witness state-root check: recompute `header(N).stateRoot` from the PARENT
/// (N-1) trie structure plus OUR writes, and compare. Unlike the per-write proof check,
/// this is anchored on the canonical root rather than our write set, so a change Nitro
/// made that we missed entirely surfaces as a root mismatch (the untouched account keeps
/// its stale N-1 hash). Returns `None` on match, `Some(diagnostic)` on mismatch.
async fn witness_state_root_check<P: Provider<Arbitrum>>(
    provider: &P,
    block_number: u64,
    post_state_root: B256,
    writes: &BTreeMap<Address, AccountWriteSet>,
) -> Result<Option<String>> {
    let parent = block_number - 1;
    let parent_id = BlockId::number(parent);
    let parent_block = retry_read(|| provider.get_block_by_number(BlockNumberOrTag::Number(parent)))
        .await?
        .ok_or_else(|| eyre!("parent block {parent} not found"))?;
    let parent_state_root = parent_block.header.inner.state_root;

    let mut account_proofs: Vec<Bytes> = Vec::new();
    let mut account_updates: BTreeMap<Nibbles, Option<Vec<u8>>> = BTreeMap::new();
    // CONTROL: re-encode each account with its UNCHANGED N-1 values; recomputing with this
    // must reproduce parent_state_root (a no-op). If it doesn't, the bug is in our
    // encoding/reconstruction, not a missing write.
    let mut control_updates: BTreeMap<Nibbles, Option<Vec<u8>>> = BTreeMap::new();
    // Accounts whose storage write-set contains a DELETION (a slot set to 0). The witness
    // storage-root recompute cannot reconstruct a deletion-induced trie branch-collapse when
    // the surviving sibling is revealed only by hash, so it can produce a wrong storage root
    // (hence a wrong account leaf) even when our writes are correct. On a root mismatch these
    // accounts are rechecked against the canonical block-N proof, see `deletion_collapse_recheck`.
    let mut deletion_accounts: Vec<(Address, Nibbles, u64, U256, B256)> = Vec::new();

    for (address, expected) in writes {
        let slot_keys: Vec<B256> = expected
            .storage
            .keys()
            .map(|s| B256::from(s.to_be_bytes::<32>()))
            .collect();
        let proof = retry_read(|| {
            provider
                .get_proof(*address, slot_keys.clone())
                .block_id(parent_id)
        })
        .await?;
        account_proofs.extend(proof.account_proof.iter().cloned());

        // Recompute this account's post-state storage root from the N-1 storage trie + our
        // slot writes. (No slot writes ⇒ storage root unchanged from N-1.)
        // geth's eth_getProof reports an ABSENT account with all-zero codeHash/storageHash
        // (vs KECCAK_EMPTY / EMPTY_ROOT_HASH for a present-but-empty account). Normalize the
        // N-1 view so absent accounts read as the canonical "empty/non-existent" values.
        let absent = proof.code_hash == B256::ZERO;
        let n1_nonce = if absent { 0 } else { proof.nonce };
        let n1_balance = if absent { U256::ZERO } else { proof.balance };
        let n1_code_hash = if absent { KECCAK_EMPTY } else { proof.code_hash };
        let n1_storage_root = if absent || proof.storage_hash == B256::ZERO {
            alloy_trie::EMPTY_ROOT_HASH
        } else {
            proof.storage_hash
        };
        let new_storage_root = if expected.storage.is_empty() {
            n1_storage_root
        } else {
            let storage_nodes: Vec<Bytes> = proof
                .storage_proof
                .iter()
                .flat_map(|sp| sp.proof.iter().cloned())
                .collect();
            let mut su: BTreeMap<Nibbles, Option<Vec<u8>>> = BTreeMap::new();
            for (slot, value) in &expected.storage {
                let key = Nibbles::unpack(keccak256(B256::from(slot.to_be_bytes::<32>())));
                su.insert(
                    key,
                    if value.is_zero() {
                        None
                    } else {
                        Some(alloy_rlp::encode(value))
                    },
                );
            }
            arb_revm::state_trie::recompute_root(n1_storage_root, &storage_nodes, &su)
                .map_err(|e| eyre!("storage root recompute for {address:#x}: {e}"))?
        };

        // Assemble the post-state account: fields we changed, else the unchanged N-1 value.
        let nonce = expected.nonce.unwrap_or(n1_nonce);
        let balance = expected.balance.unwrap_or(n1_balance);
        let code_hash = expected.code_hash.unwrap_or(n1_code_hash);
        let key = Nibbles::unpack(keccak256(address));

        // Control entry: the unchanged N-1 account, re-encoded (absent ⇒ stays absent).
        let control_empty = n1_nonce == 0
            && n1_balance.is_zero()
            && n1_code_hash == KECCAK_EMPTY
            && n1_storage_root == alloy_trie::EMPTY_ROOT_HASH;
        control_updates.insert(
            key,
            if control_empty {
                None
            } else {
                Some(alloy_rlp::encode(TrieAccount {
                    nonce: n1_nonce,
                    balance: n1_balance,
                    storage_root: n1_storage_root,
                    code_hash: n1_code_hash,
                }))
            },
        );

        // EIP-161: an account that becomes empty is removed from the trie, UNLESS our
        // execution explicitly materialized it (revm `Created`). A created account is written
        // by revm-database `newly_created` even when empty (the `is_created` check precedes the
        // empty-clear), which is how the ArbOS pre-Stylus "zombie escrow" stays present.
        let is_empty = nonce == 0
            && balance.is_zero()
            && code_hash == KECCAK_EMPTY
            && new_storage_root == alloy_trie::EMPTY_ROOT_HASH;
        if is_empty && !expected.created {
            account_updates.insert(key, None);
        } else {
            let account = TrieAccount {
                nonce,
                balance,
                storage_root: new_storage_root,
                code_hash,
            };
            account_updates.insert(key, Some(alloy_rlp::encode(account)));
        }

        // A slot set to 0 is a storage deletion; flag the account for the collapse recheck.
        if expected.storage.values().any(|v| v.is_zero()) {
            deletion_accounts.push((*address, key, nonce, balance, code_hash));
        }

        // ACCT_DEBUG=1: print our recomputed post-state account fields so the diverging
        // account can be named by diffing storage_root/nonce/balance vs canonical getProof@N.
        if std::env::var("ACCT_DEBUG").is_ok() {
            println!(
                "  ACCT {address:?} nonce={nonce} balance={balance} storage_root={new_storage_root:#x} code_hash={code_hash:#x} empty={is_empty}"
            );
        }
    }

    // Control check: re-encoding unchanged N-1 accounts must reproduce the parent root.
    let control_root =
        arb_revm::state_trie::recompute_root(parent_state_root, &account_proofs, &control_updates)
            .map_err(|e| eyre!("control recompute: {e}"))?;
    if control_root != parent_state_root {
        // Re-encoding unchanged accounts should be a no-op; if it isn't, the recompute
        // itself is buggy and any "mismatch" below is unreliable.
        return Ok(Some(format!(
            "WITNESS-CONTROL FAILED: N-1 re-encode gives {control_root:#x} != parentStateRoot \
             {parent_state_root:#x} (recompute bug, not a state divergence)"
        )));
    }

    let recomputed =
        arb_revm::state_trie::recompute_root(parent_state_root, &account_proofs, &account_updates)
            .map_err(|e| eyre!("state root recompute: {e}"))?;
    if recomputed == post_state_root {
        return Ok(None);
    }
    // The witness recompute mismatched. Before reporting a divergence, rule out the recompute's
    // deletion-collapse blind spot (a wrong storage root for an account whose write-set deleted a
    // slot): re-derive each such account's leaf from the canonical block-N proof, after verifying
    // our writes are consistent with it, then recompute. A genuine missing/extra write, to any
    // non-deleting account, or a wrong value in a deleting one, still surfaces as a mismatch.
    deletion_collapse_recheck(
        provider,
        block_number,
        parent_state_root,
        post_state_root,
        recomputed,
        &account_proofs,
        &account_updates,
        &deletion_accounts,
        writes,
    )
    .await
}

/// Disambiguate a witness-root mismatch caused by the storage-deletion branch-collapse blind spot
/// (see [`recompute_root`]) from a real state divergence.
///
/// For each account whose write-set deleted a slot, the witness storage-root recompute may be
/// wrong, so we trust the canonical block-N proof for that account's leaf, but only after proving
/// our execution agrees with it: our nonce/balance/code must equal canonical@N, and every slot we
/// wrote must hold our value at canonical@N (sets present, deletes ⇒ 0). We then substitute the
/// (collapse-free) canonical@N storage root into the account leaf and recompute. If it now matches,
/// the original mismatch was purely the recompute artifact and our state is correct; otherwise the
/// divergence is real and reported.
///
/// Residual blind spot (documented, deferred to the full-trie node): a slot Nitro changed in a
/// *storage-deleting* account that we did not write is masked, because we adopt canonical@N's
/// storage root without recomputing it. Missing writes to any other account, and all wrong values
/// are still caught.
#[allow(clippy::too_many_arguments)]
async fn deletion_collapse_recheck<P: Provider<Arbitrum>>(
    provider: &P,
    block_number: u64,
    parent_state_root: B256,
    post_state_root: B256,
    recomputed: B256,
    account_proofs: &[Bytes],
    account_updates: &BTreeMap<Nibbles, Option<Vec<u8>>>,
    deletion_accounts: &[(Address, Nibbles, u64, U256, B256)],
    writes: &BTreeMap<Address, AccountWriteSet>,
) -> Result<Option<String>> {
    if deletion_accounts.is_empty() {
        return Ok(Some(format!(
            "witness root {recomputed:#x} != header.stateRoot {post_state_root:#x} \
             (no storage deletions to explain a recompute collapse ⇒ real missing/extra write)"
        )));
    }
    let block_id = BlockId::number(block_number);
    let mut patched = account_updates.clone();
    for (address, key, our_nonce, our_balance, our_code_hash) in deletion_accounts {
        // Canonical block-N account leaf (authoritative; its storage root is collapse-free).
        let proof = retry_read(|| provider.get_proof(*address, Vec::new()).block_id(block_id)).await?;
        let canon_code_hash = if proof.code_hash == B256::ZERO {
            KECCAK_EMPTY
        } else {
            proof.code_hash
        };
        let canon_storage_root = if proof.storage_hash == B256::ZERO {
            alloy_trie::EMPTY_ROOT_HASH
        } else {
            proof.storage_hash
        };
        // Non-storage fields must match canonical@N, else the divergence is real (not a collapse).
        if proof.nonce != *our_nonce
            || proof.balance != *our_balance
            || canon_code_hash != *our_code_hash
        {
            return Ok(Some(format!(
                "deletion-recheck {address:#x}: account fields differ from canonical@N \
                 (nonce {our_nonce}/{}, balance {our_balance:#x}/{:#x}, code {our_code_hash:#x}/{canon_code_hash:#x}) \
                 ⇒ real divergence",
                proof.nonce, proof.balance
            )));
        }
        // Every slot we wrote must hold our value at canonical N (a deletion reads back as 0).
        for (slot, our_val) in &writes[address].storage {
            let canon_val =
                retry_read(|| provider.get_storage_at(*address, *slot).block_id(block_id)).await?;
            if canon_val != *our_val {
                return Ok(Some(format!(
                    "deletion-recheck {address:#x} slot {slot:#x}: our {our_val:#x} != \
                     canonical@N {canon_val:#x} ⇒ real divergence"
                )));
            }
        }
        // Verified: substitute the canonical@N leaf in place of our collapse-suspect one.
        let is_empty = *our_nonce == 0
            && our_balance.is_zero()
            && canon_code_hash == KECCAK_EMPTY
            && canon_storage_root == alloy_trie::EMPTY_ROOT_HASH;
        patched.insert(
            *key,
            if is_empty {
                None
            } else {
                Some(alloy_rlp::encode(TrieAccount {
                    nonce: *our_nonce,
                    balance: *our_balance,
                    storage_root: canon_storage_root,
                    code_hash: *our_code_hash,
                }))
            },
        );
    }
    let rechecked =
        arb_revm::state_trie::recompute_root(parent_state_root, account_proofs, &patched)
            .map_err(|e| eyre!("deletion-collapse recheck recompute: {e}"))?;
    if rechecked == post_state_root {
        eprintln!(
            "  note: block {block_number} hit the storage-deletion collapse blind spot; \
             verified against canonical@N leaf substitution for {} account(s), state is correct",
            deletion_accounts.len()
        );
        Ok(None)
    } else {
        Ok(Some(format!(
            "witness root {recomputed:#x} != header.stateRoot {post_state_root:#x}; still \
             mismatches after canonical@N substitution for {} storage-deleting account(s) \
             ⇒ real missing/extra write (NOT the deletion-collapse blind spot)",
            deletion_accounts.len()
        )))
    }
}

fn compare_logs(
    actual: &[revm::primitives::Log],
    expected: &[alloy_rpc_types_eth::Log],
) -> Vec<String> {
    let mut errors = Vec::new();
    if actual.len() != expected.len() {
        errors.push(format!(
            "log length mismatch: expected {}, got {}",
            expected.len(),
            actual.len()
        ));
    }

    for (idx, (actual_log, expected_log)) in actual.iter().zip(expected.iter()).enumerate() {
        let expected_address = expected_log.inner.address;
        if actual_log.address != expected_address {
            errors.push(format!(
                "log[{idx}] address mismatch: expected {expected_address:#x}, got {:#x}",
                actual_log.address
            ));
        }

        let expected_topics = expected_log.topics();
        if actual_log.topics().len() != expected_topics.len() {
            errors.push(format!(
                "log[{idx}] topics length mismatch: expected {}, got {}",
                expected_topics.len(),
                actual_log.topics().len()
            ));
        }
        for (topic_idx, (actual_topic, expected_topic)) in actual_log
            .topics()
            .iter()
            .zip(expected_topics.iter())
            .enumerate()
        {
            if actual_topic != expected_topic {
                errors.push(format!(
                    "log[{idx}] topic[{topic_idx}] mismatch: expected {expected_topic:#x}, got {actual_topic:#x}"
                ));
            }
        }

        let expected_data = &expected_log.inner.data.data;
        if actual_log.data.data.as_ref() != expected_data.as_ref() {
            errors.push(format!(
                "log[{idx}] data mismatch: expected 0x{}, got 0x{}",
                hex::encode(expected_data.as_ref()),
                hex::encode(actual_log.data.data.as_ref())
            ));
        }
    }
    errors
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| eyre!("failed to install rustls aws-lc provider"))?;

    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        return Err(eyre!(usage()));
    }

    let rpc_url = args[1].clone();
    let block_number: u64 = args[2]
        .parse()
        .map_err(|e| eyre!("invalid block_number `{}`: {e}", args[2]))?;
    let fail_fast = args.iter().any(|arg| arg == "--fail-fast");
    let skip_system_sender = args.iter().any(|arg| arg == "--skip-system-sender");
    let record_path = parse_record_path(&args);
    let trace_tx = parse_trace_tx(&args);

    let state_block_number = block_number
        .checked_sub(1)
        .ok_or_else(|| eyre!("block 0 cannot be replayed against parent state"))?;

    // alloy's ClientBuilder handles the JSON-RPC transport; map ws->http inline since
    // the http transport is what we wire through here.
    let http_url = match rpc_url.strip_prefix("ws://") {
        Some(rest) => format!("http://{rest}"),
        None => match rpc_url.strip_prefix("wss://") {
            Some(rest) => format!("https://{rest}"),
            None => rpc_url.clone(),
        },
    };
    let url_parsed = url::Url::parse(&http_url)
        .map_err(|e| eyre!("invalid rpc url `{http_url}`: {e}"))?;

    // RetryBackoffLayer replaces the hand-rolled retry loop for transport-level errors.
    let client = ClientBuilder::default()
        .layer(RetryBackoffLayer::new(10, 200, 100))
        .http(url_parsed);
    let provider = ProviderBuilder::<_, _, Arbitrum>::default()
        .connect_client(client)
        .erased();

    let chain_id = provider.get_chain_id().await?;

    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(block_number))
        .full()
        .await?
        .ok_or_else(|| eyre!("block {block_number} not found"))?;

    let header = &block.header.inner;
    let state_root = header.state_root;
    let block_env = BlockEnv {
        number: U256::from(header.number),
        timestamp: U256::from(header.timestamp),
        gas_limit: header.gas_limit,
        basefee: header.base_fee_per_gas.unwrap_or(0),
        difficulty: header.difficulty,
        beneficiary: header.beneficiary,
        prevrandao: Some(header.mix_hash),
        ..Default::default()
    };
    let recorded_block_env = block_env.clone();
    // Nitro encodes the L1 block number in MixDigest[8:16] (go-ethereum arb_types.go);
    // the EVM NUMBER opcode returns it instead of the L2 block number.
    let l1_block_number = u64::from_be_bytes(
        header.mix_hash[8..16]
            .try_into()
            .expect("mix_hash is 32 bytes"),
    );

    let transactions: Vec<RpcArbTransaction> = block.transactions.into_transactions_vec();

    println!(
        "replaying block={} txs={} state_block={} chain_id={}",
        block_number,
        transactions.len(),
        state_block_number,
        chain_id
    );

    let mut receipts: Vec<ArbTransactionReceipt> = Vec::with_capacity(transactions.len());
    for tx in &transactions {
        let tx_hash = tx.as_ref().hash();
        receipts.push(fetch_receipt(&provider, tx_hash).await?);
    }

    // Probe the debug namespace once: a successful (or simply non-"unsupported")
    // call enables byte-level output parity checks.
    let mut debug_trace_enabled = match transactions.first() {
        Some(tx) => fetch_trace_summary(&provider, tx.as_ref().hash())
            .await
            .map(|opt| opt.is_some())
            .unwrap_or(true),
        None => false,
    };
    if debug_trace_enabled {
        println!("debug trace output parity: enabled");
    } else {
        println!("debug trace output parity: unavailable (skipping byte-level output checks)");
    }

    // State root at the parent (N-1) block, anchors the non-existence proofs in the
    // existence-aware DB so absent accounts read back as `None`, not `Some(empty)`.
    let parent_state_root =
        retry_read(|| provider.get_block_by_number(BlockNumberOrTag::Number(state_block_number)))
            .await?
            .ok_or_else(|| eyre!("parent block {state_block_number} not found"))?
            .header
            .inner
            .state_root;
    let alloy_db = ExistenceAwareDb::new(
        provider.clone(),
        BlockId::from(state_block_number),
        parent_state_root,
    );
    let wrapped = WrapDatabaseAsync::new(alloy_db)
        .ok_or_else(|| eyre!("failed to create WrapDatabaseAsync; use multi-thread tokio"))?;
    // Record every state read served to the engine so the run can be re-emitted as a
    // self-contained, node-free replay fixture. CacheDB sits above the recorder, so
    // only first-touch (prestate) reads pass through and get logged.
    let recorder = RecordingDb::new(wrapped);
    let mut db = CacheDB::new(&recorder);

    // Select the EVM spec from the block's *effective* ArbOS version (current version,
    // or a scheduled upgrade that's due this block). These reads also go through the
    // recorder, so the version/upgrade slots land in the fixture prestate.
    let block_timestamp: u64 = block_env.timestamp.try_into().unwrap_or(u64::MAX);
    let arbos_version = arb_revm::ArbosState::read_effective_version(&db, block_timestamp);
    let spec = ArbSpecId::from_arbos_version(arbos_version);
    println!("arbos_version={arbos_version} spec={spec:?} eth_spec={:?}", spec.into_eth_spec());
    let mut cfg_env = CfgEnv::new_with_spec(spec)
        .with_chain_id(chain_id)
        .with_disable_priority_fee_check(true);
    cfg_env.disable_balance_check = true;
    // EIP-7623 calldata floor applies only when the ArbOS calldata_price_increase feature is on
    // (mirrors run.rs / Nitro state_transition.go). Off => Arbitrum prices calldata via the L1
    // poster fee instead, so the floor must be disabled.
    cfg_env.disable_eip7623 = !arb_revm::ArbosState::open()
        .features
        .read_calldata_price_increase_db(&mut db);

    // --trace-tx <idx>: run with an opcode/call-frequency inspector and dump what the
    // engine does for tx[idx] (diagnosing gas-divergence). Earlier txs build up state.
    if let Some(idx) = trace_tx {
        let chain = ArbChainContext::new(None).with_l1_block_number(l1_block_number);
        let context: ArbContext<&mut _> = ArbContext::arb_with_chain_context(chain)
            .with_db(&mut db)
            .with_cfg(cfg_env)
            .with_block(block_env)
            .with_tx(ArbTransaction::<TxEnv>::default());
        let mut evm = context.build_arb_with_inspector(TraceInsp::default());
        for (i, tx) in transactions.iter().enumerate() {
            if i > idx {
                break;
            }
            let tx_env = arb_envelope_to_tx_env(tx.as_ref())
                .map_err(|e| eyre!("failed to map tx[{i}]: {e}"))?;
            evm.0.inspector.enabled = i == idx;
            let outcome = evm.inspect_tx_commit(tx_env);
            if i == idx {
                println!(
                    "trace tx[{idx}] {:#x}: result={}",
                    transactions[idx].as_ref().hash(),
                    match &outcome {
                        Ok(r) => format!("gas_used={}, success={}", r.tx_gas_used(), r.is_success()),
                        Err(e) => format!("error: {e:?}"),
                    }
                );
            }
        }
        let insp = &evm.0.inspector;
        println!("=== {} opcode steps ===", insp.steps);
        let mut ops: Vec<_> = insp.opcodes.iter().collect();
        ops.sort_by(|a, b| b.1.cmp(a.1));
        println!("top opcodes BY GAS:");
        let mut by_gas: Vec<_> = insp.op_gas.iter().collect();
        by_gas.sort_by(|a, b| b.1.cmp(a.1));
        let total_op_gas: u64 = insp.op_gas.values().sum();
        for (op, gas) in by_gas.iter().take(16) {
            let name = OpCode::new(**op).map(|o| o.as_str()).unwrap_or("?");
            let count = insp.opcodes.get(*op).copied().unwrap_or(0);
            println!("  0x{:02x} {:<16} gas={gas:<8} x {count}", op, name);
        }
        println!("  (sum of all op_gas = {total_op_gas})");
        let mut calls: Vec<_> = insp.calls.iter().collect();
        calls.sort_by(|a, b| b.1.cmp(a.1));
        println!("call targets (count / total CALL-opcode gas incl. callee):");
        for (addr, count) in calls.iter().take(14) {
            let gas = insp.call_gas.get(*addr).copied().unwrap_or(0);
            println!("  {addr:#x} x {count} gas={gas}");
        }
        let mut linear_out = String::new();
        for (i, (pc, op, gas)) in insp.linear.iter().enumerate() {
            let stk = insp.stacks.get(i).map(|s| s.join(",")).unwrap_or_default();
            let _ = &stk;
            let name = OpCode::new(*op).map(|o| o.as_str()).unwrap_or("?");
            linear_out.push_str(&format!("{i:04} pc={pc} {name} gas={gas} stack=[{stk}]\n"));
        }
        std::fs::write("/tmp/our_trace.txt", &linear_out).ok();
        println!("wrote {} steps to /tmp/our_trace.txt", insp.linear.len());
        return Ok(());
    }

    let context: ArbContext<&mut _> =
        ArbContext::arb_with_chain_context(
            ArbChainContext::new(None).with_l1_block_number(l1_block_number),
        )
            .with_db(&mut db)
            .with_cfg(cfg_env)
            .with_block(block_env)
            .with_tx(ArbTransaction::<TxEnv>::default());
    let mut evm = context.build_arb();

    let system_sender = Address::from_str("0x00000000000000000000000000000000000A4B05")
        .map_err(|e| eyre!("invalid hardcoded system sender: {e}"))?;
    let mut mismatches = 0usize;
    let mut executed = 0usize;
    let mut skipped = 0usize;
    let mut aborted_early = false;
    let mut state_writes: BTreeMap<Address, AccountWriteSet> = BTreeMap::new();

    for (idx, (tx, receipt)) in transactions.iter().zip(receipts.iter()).enumerate() {
        let tx_hash = tx.as_ref().hash();
        let tx_hash_hex = format!("{tx_hash:#x}");
        let tx_sender = tx.as_ref().sender().ok();

        if skip_system_sender && tx_sender == Some(system_sender) {
            skipped += 1;
            println!(
                "skip tx[{}] hash={} sender={:#x}",
                idx, tx_hash_hex, system_sender
            );
            continue;
        }

        let tx_env = arb_envelope_to_tx_env(tx.as_ref())
            .map_err(|e| eyre!("failed to map rpc tx {} to tx env: {e}", tx_hash_hex))?;

        let out = {
            let mut final_result = None;
            let mut final_error = None;
            for attempt in 1..=EXEC_MAX_ATTEMPTS {
                match evm.transact(tx_env.clone()) {
                    Ok(value) => {
                        final_result = Some(value);
                        break;
                    }
                    Err(err) => {
                        let err_msg = format!("{err:?}");
                        let retryable = is_retryable_message(&err_msg);
                        if retryable && attempt < EXEC_MAX_ATTEMPTS {
                            let backoff_ms = 150_u64 * (1_u64 << ((attempt - 1).min(6)));
                            eprintln!(
                                "tx[{}] hash={} execution failed (attempt {attempt}/{EXEC_MAX_ATTEMPTS}): {}; retry in {backoff_ms}ms",
                                idx, tx_hash_hex, err_msg
                            );
                            std::thread::sleep(Duration::from_millis(backoff_ms));
                            continue;
                        }
                        final_error = Some(err_msg);
                        break;
                    }
                }
            }
            match final_result {
                Some(value) => value,
                None => {
                    mismatches += 1;
                    println!(
                        "mismatch tx[{}] hash={}: execution error: {}",
                        idx,
                        tx_hash_hex,
                        final_error.unwrap_or_else(|| "unknown error".to_string())
                    );
                    if fail_fast {
                        aborted_early = true;
                        break;
                    }
                    continue;
                }
            }
        };

        executed += 1;
        let expected_success = receipt.status();
        let mut tx_errors = Vec::new();

        if out.result.is_success() != expected_success {
            tx_errors.push(format!(
                "status mismatch: expected success={}, got success={}",
                expected_success,
                out.result.is_success()
            ));
        }
        let expected_gas_used = receipt.gas_used();
        if out.result.tx_gas_used() != expected_gas_used {
            tx_errors.push(format!(
                "gas used mismatch: expected {}, got {}",
                expected_gas_used,
                out.result.tx_gas_used()
            ));
        }
        let expected_created_address = receipt.contract_address();
        let actual_created_address = out.result.created_address();
        if actual_created_address != expected_created_address {
            tx_errors.push(format!(
                "created address mismatch: expected {:?}, got {:?}",
                expected_created_address, actual_created_address
            ));
        }

        tx_errors.extend(compare_logs(out.result.logs(), receipt.inner.logs()));
        if debug_trace_enabled {
            match fetch_trace_summary(&provider, tx_hash).await {
                Ok(Some(trace)) => {
                    if trace.failed == out.result.is_success() {
                        tx_errors.push(format!(
                            "trace status mismatch: trace failed={}, local success={}",
                            trace.failed,
                            out.result.is_success()
                        ));
                    }
                    if let Some(expected_output) = trace.output {
                        let actual_output = out.result.output().cloned().unwrap_or_default();
                        if actual_output != expected_output {
                            tx_errors.push(format!(
                                "output data mismatch: expected 0x{}, got 0x{}",
                                hex::encode(expected_output.as_ref()),
                                hex::encode(actual_output.as_ref())
                            ));
                        }
                    }
                }
                Ok(None) => {
                    debug_trace_enabled = false;
                    println!(
                        "debug_traceTransaction unavailable at tx[{}]; disabling byte-level output checks",
                        idx
                    );
                }
                Err(err) => {
                    tx_errors.push(format!("debug trace fetch failed: {err}"));
                }
            }
        }

        if tx_errors.is_empty() {
            println!(
                "ok tx[{}] hash={} logs={} gas={}",
                idx,
                tx_hash_hex,
                out.result.logs().len(),
                out.result.tx_gas_used()
            );
        } else {
            mismatches += 1;
            println!("mismatch tx[{}] hash={}", idx, tx_hash_hex);
            for err in tx_errors {
                println!("  - {err}");
            }
            if fail_fast {
                aborted_early = true;
                break;
            }
        }

        merge_state_writes(&mut state_writes, &out.state);
        evm.commit(out.state);
    }

    // When recording, the live state-parity check is redundant with `expected_state`
    // (and doubles the archive reads, which matters on rate-limited endpoints), so it
    // is computed once via `build_expected_state` and verified by offline replay.
    if !aborted_early && record_path.is_none() {
        let state_slot_count: usize = state_writes.values().map(|entry| entry.storage.len()).sum();
        println!(
            "state write parity: accounts={} slots={}",
            state_writes.len(),
            state_slot_count
        );
        // DUMP_WRITES=1: enumerate our net write-set (account: nonce/balance + slots) so a
        // "missing write" can be diffed against the canonical prestateTracer diff.
        if std::env::var("DUMP_WRITES").is_ok() {
            for (addr, w) in &state_writes {
                println!("  WRITE {addr:?} nonce={:?} balance={:?}", w.nonce, w.balance);
                for (slot, val) in &w.storage {
                    println!("      slot {slot:#x} = {val:#x}");
                }
            }
        }
        // Primary gate: Stage-2 witness state-root recompute (catches missing writes too).
        match witness_state_root_check(&provider, block_number, state_root, &state_writes).await? {
            None => {
                println!(
                    "state-root parity: ok (witness root == header.stateRoot {state_root:#x})"
                );
            }
            Some(diag) => {
                // Root diverged, localize with the per-write proof check (Stage 1).
                println!("state-root parity: MISMATCH, {diag}");
                let state_errors = verify_writes_against_state_root(
                    &provider,
                    block_number,
                    state_root,
                    &state_writes,
                )
                .await?;
                if state_errors.is_empty() {
                    // Root differs but every write we made checks out ⇒ a write Nitro made
                    // that we MISSED entirely (invisible to the per-write check).
                    mismatches += 1;
                    println!(
                        "  - root mismatch with all our writes correct ⇒ a missing write (state \
                         change Nitro made that we did not). Use prestateTracer diff to find it."
                    );
                } else {
                    mismatches += state_errors.len();
                    println!("  per-write localizer found {} diff(s):", state_errors.len());
                    for err in state_errors {
                        println!("  - {err}");
                    }
                }
            }
        }
    } else {
        println!("state write parity: skipped due to fail-fast early exit");
    }

    println!(
        "summary block={} executed={} skipped={} mismatches={}",
        block_number, executed, skipped, mismatches
    );

    if let Some(path) = record_path {
        if skip_system_sender {
            eprintln!(
                "warning: --record with --skip-system-sender may produce an inconsistent fixture; \
                 record full blocks for faithful replay"
            );
        }
        let expected = transactions
            .iter()
            .zip(receipts.iter())
            .map(|(tx, receipt)| build_expected_tx(tx, receipt))
            .collect::<Vec<_>>();
        let fixture = ReplayFixture {
            schema: REPLAY_FIXTURE_SCHEMA.to_string(),
            chain_id,
            block: BlockFixture {
                number: recorded_block_env.number.try_into().unwrap_or(u64::MAX),
                l1_block_number,
                timestamp: recorded_block_env.timestamp.try_into().unwrap_or(u64::MAX),
                basefee: recorded_block_env.basefee,
                gas_limit: recorded_block_env.gas_limit,
                difficulty: recorded_block_env.difficulty,
                beneficiary: recorded_block_env.beneficiary,
                prevrandao: recorded_block_env.prevrandao,
            },
            prestate: recorder.to_prestate(),
            transactions: transactions.clone(),
            expected,
            expected_state: build_expected_state(&provider, block_number, &state_writes).await?,
        };
        let json = serde_json::to_string_pretty(&fixture)?;
        std::fs::write(&path, json).map_err(|e| eyre!("failed to write fixture {path}: {e}"))?;
        println!(
            "wrote replay fixture to {path} (accounts={} block_hashes={})",
            fixture.prestate.accounts.len(),
            fixture.prestate.block_hashes.len()
        );
    }

    if mismatches > 0 {
        return Err(eyre!("found {mismatches} mismatching transaction(s)"));
    }
    Ok(())
}

/// Retries an RPC read on transient failures (rate-limit / "request timeout on the
/// free tier" / load-balancer hiccups) with linear backoff.
async fn retry_read<F, Fut, T, E>(mut make: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::IntoFuture<Output = std::result::Result<T, E>>,
    E: std::fmt::Display,
{
    const ATTEMPTS: usize = 10;
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        match make().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                last = err.to_string();
                if attempt < ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(300 * attempt as u64)).await;
                }
            }
        }
    }
    Err(eyre!("rpc read failed after {ATTEMPTS} attempts: {last}"))
}

/// Fetches Nitro's post-block values for exactly the accounts/fields/slots the local
/// engine wrote, producing the state-parity oracle stored in the fixture.
async fn build_expected_state<P: Provider<Arbitrum>>(
    provider: &P,
    block_number: u64,
    writes: &BTreeMap<Address, AccountWriteSet>,
) -> Result<Vec<ExpectedAccountState>> {
    let block_id = BlockId::number(block_number);
    let mut out = Vec::with_capacity(writes.len());
    for (address, w) in writes {
        let balance = if w.balance.is_some() {
            Some(retry_read(|| provider.get_balance(*address).block_id(block_id)).await?)
        } else {
            None
        };
        let nonce = if w.nonce.is_some() {
            Some(retry_read(|| provider.get_transaction_count(*address).block_id(block_id)).await?)
        } else {
            None
        };
        let code_hash = if w.code_hash.is_some() {
            let code =
                retry_read(|| provider.get_code_at(*address).block_id(block_id)).await?;
            Some(if code.is_empty() {
                KECCAK_EMPTY
            } else {
                keccak256(code.as_ref())
            })
        } else {
            None
        };
        let mut storage = Vec::with_capacity(w.storage.len());
        for slot in w.storage.keys() {
            let value =
                retry_read(|| provider.get_storage_at(*address, *slot).block_id(block_id)).await?;
            storage.push(StorageEntry { slot: *slot, value });
        }
        out.push(ExpectedAccountState {
            address: *address,
            balance,
            nonce,
            code_hash,
            storage,
        });
    }
    Ok(out)
}

/// Fetches a receipt, retrying when the (load-balanced) endpoint transiently returns
/// `null`, dRPC and similar LBs route requests across backends with differing archive
/// depth, so a receipt that's missing on one node is usually present on a retry.
async fn fetch_receipt<P: Provider<Arbitrum>>(
    provider: &P,
    tx_hash: B256,
) -> Result<ArbTransactionReceipt> {
    const ATTEMPTS: usize = 8;
    for attempt in 1..=ATTEMPTS {
        if let Some(receipt) = provider.get_transaction_receipt(tx_hash).await? {
            return Ok(receipt);
        }
        if attempt < ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(150 * attempt as u64)).await;
        }
    }
    Err(eyre!(
        "receipt for {tx_hash:#x} not found after {ATTEMPTS} attempts"
    ))
}

fn build_expected_tx(tx: &RpcArbTransaction, receipt: &ArbTransactionReceipt) -> ExpectedTx {
    let logs = receipt
        .inner
        .logs()
        .iter()
        .map(|log| ExpectedLog {
            address: log.inner.address,
            topics: log.topics().to_vec(),
            data: log.inner.data.data.clone(),
        })
        .collect::<Vec<_>>();
    ExpectedTx {
        tx_hash: tx.as_ref().hash(),
        success: receipt.status(),
        gas_used: receipt.gas_used(),
        created_address: receipt.contract_address(),
        logs,
    }
}
