use crate::{
    ArbBuilder, ArbChainContext, ArbContext, ArbTransaction, ArbosState, DefaultArb,
    constants::{ARB_RETRYABLE_TX_ADDRESS, ARBITRUM_INTERNAL_TX_TYPE, HISTORY_STORAGE_ADDRESS},
    executor::contract::{
        ArbExecCfg, ArbExecOutcome, ArbExecutionInput, ArbTxExecution, ArbWriteEffect,
        ArbWriteStage, ArbWriteTarget,
    },
    executor::hooks::{
        ArbExecutionHooks, ArbStartBlockDerived, ArbSystemCall, DefaultArbExecutionHooks,
    },
    transaction::arb_envelope_to_tx_env,
};
use arb_alloy_consensus::transactions::{ArbTxEnvelope, TxRetry};
use revm::{
    Database, DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::{Block, ContextTr, JournalTr},
    handler::{EvmTr, SYSTEM_ADDRESS, SystemCallCommitEvm, SystemCallEvm},
    primitives::{Bytes, Log, TxKind, U256, keccak256},
};
use std::collections::VecDeque;

/// Error type for stateless message execution.
pub type ArbExecError<DB> = revm::context_interface::result::EVMError<
    <DB as Database>::Error,
    revm::context_interface::result::InvalidTransaction,
>;

#[derive(Clone)]
struct QueuedTx {
    tx: ArbTxEnvelope,
    tx_index: Option<usize>,
    write_stage: ArbWriteStage,
}

const REDEEM_SCHEDULED_EVENT_SIGNATURE: &[u8] =
    b"RedeemScheduled(bytes32,bytes32,uint64,uint64,address,uint256,uint256)";
const ABI_WORD_SIZE: usize = 32;

fn build_block_env(
    parent: crate::executor::contract::ArbParentHeader,
    cfg: ArbExecCfg,
    input: &ArbExecutionInput,
) -> BlockEnv {
    let next_timestamp = input.message.l1_timestamp.max(parent.timestamp);
    let l2_block_number = parent.number.saturating_add(1);

    let mut block = BlockEnv::default();
    block.number = U256::from(l2_block_number);
    block.beneficiary = input.message.poster;
    block.timestamp = U256::from(next_timestamp);
    block.gas_limit = cfg.block_gas_limit.min(parent.gas_limit);
    block.basefee = parent.basefee;
    block.difficulty = parent.difficulty;
    block.prevrandao = parent.prevrandao;
    block
}

fn start_block_internal_tx(call: ArbSystemCall, chain_id: u64) -> ArbTransaction<TxEnv> {
    let mut tx = TxEnv::default();
    tx.tx_type = ARBITRUM_INTERNAL_TX_TYPE;
    tx.caller = call.caller;
    tx.kind = TxKind::Call(call.target);
    tx.data = call.data;
    tx.gas_limit = 0;
    tx.gas_price = 0;
    tx.nonce = 0;
    tx.chain_id = Some(chain_id);
    ArbTransaction::new(tx)
}

/// Derives the scheduled retry (auto-redeem) transactions implied by a transaction's
/// `RedeemScheduled` logs, reading the retryable state from `ctx`.
///
/// This is the ArbOS "scheduled retry" mechanism: a successful `SubmitRetryable` (auto-redeem) or
/// `ArbRetryableTx.redeem` precompile call emits a `RedeemScheduled` event, and ArbOS then runs the
/// corresponding redeem transaction **within the same block**. [`execute_message`] uses this in its
/// own loop; the reth node's block-builder loop ([`crate`] consumers) calls it after each committed
/// tx so produced blocks include the same auto-redeem txs (and thus match Nitro's gas/state/roots).
pub fn scheduled_retries_from_redeem_logs<CTX>(
    ctx: &mut CTX,
    logs: &[Log],
    chain_id: u64,
) -> Vec<ArbTxEnvelope>
where
    CTX: ContextTr<Journal: JournalTr>,
{
    let mut scheduled = Vec::new();
    let signature_hash = keccak256(REDEEM_SCHEDULED_EVENT_SIGNATURE);
    let base_fee = U256::from(ctx.block().basefee());
    let arbos_state = ArbosState::open();

    for log in logs {
        if log.address != ARB_RETRYABLE_TX_ADDRESS {
            continue;
        }
        let topics = log.topics();
        if topics.len() != 4 || topics[0] != signature_hash {
            continue;
        }

        let data = log.data.data.as_ref();
        if data.len() != ABI_WORD_SIZE * 4 {
            continue;
        }

        let donated_gas = match u64::try_from(u256_word(&data[0..ABI_WORD_SIZE])) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if donated_gas == 0 {
            continue;
        }
        let gas_donor = address_word(&data[ABI_WORD_SIZE..ABI_WORD_SIZE * 2]);
        let max_refund = u256_word(&data[ABI_WORD_SIZE * 2..ABI_WORD_SIZE * 3]);
        let submission_fee_refund = u256_word(&data[ABI_WORD_SIZE * 3..ABI_WORD_SIZE * 4]);
        let sequence_num = match u64::try_from(U256::from_be_bytes(topics[3].0)) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ticket_id = topics[1];

        let journal = ctx.journal_mut();
        let retryable = arbos_state.retryables.retryable(ticket_id);

        let num_tries = match retryable.num_tries.get(journal) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if num_tries == 0 || num_tries.saturating_sub(1) != sequence_num {
            continue;
        }

        let from = match retryable.from.get(journal) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let to = match retryable.to(journal) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let value = match retryable.callvalue.get(journal) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let input = match retryable.calldata.get(journal) {
            Ok(v) => v,
            Err(_) => continue,
        };

        scheduled.push(ArbTxEnvelope::from(TxRetry {
            chain_id: U256::from(chain_id),
            nonce: sequence_num,
            from,
            gas_fee_cap: base_fee,
            gas_limit: donated_gas,
            to: match to {
                Some(dest) => TxKind::Call(dest),
                None => TxKind::Create,
            },
            value,
            input: Bytes::from(input),
            ticket_id,
            refund_to: gas_donor,
            max_refund,
            submission_fee_refund,
        }));
    }

    scheduled
}

#[inline]
fn u256_word(word: &[u8]) -> U256 {
    let mut out = [0_u8; 32];
    out.copy_from_slice(word);
    U256::from_be_bytes(out)
}

#[inline]
fn address_word(word: &[u8]) -> revm::primitives::Address {
    revm::primitives::Address::from_slice(&word[12..32])
}

/// Executes one sequencer message with no retained engine-local state.
///
/// Durable writes are performed by `transact_commit` and are reported in
/// [`ArbExecOutcome::writes`].
pub fn execute_message<'a, DB: Database + DatabaseCommit>(
    db: &'a mut DB,
    input: &ArbExecutionInput,
) -> Result<ArbExecOutcome, ArbExecError<&'a mut DB>> {
    execute_message_with_hooks(db, input, &DefaultArbExecutionHooks)
}

/// Executes one sequencer message with explicit hook overrides.
pub fn execute_message_with_hooks<'a, DB, H>(
    db: &'a mut DB,
    input: &ArbExecutionInput,
    hooks: &H,
) -> Result<ArbExecOutcome, ArbExecError<&'a mut DB>>
where
    DB: Database + DatabaseCommit,
    H: ArbExecutionHooks,
{
    let parent = input.parent;
    let message = &input.message;
    let cfg = input.cfg;
    let commits_state = input.mode.commits_state();

    // Nitro's createNewHeader semantics: block timestamp is max(l1Timestamp, parent.Time)
    let next_timestamp = message.l1_timestamp.max(parent.timestamp);
    // Nitro's InternalTxStartBlock packs timePassed as newHeader.Time - lastHeader.Time.
    let time_last_block = next_timestamp.saturating_sub(parent.timestamp);
    let l2_block_number = parent.number.saturating_add(1);

    let block = build_block_env(parent, cfg, input);
    let chain =
        ArbChainContext::new(message.sequence_number).with_l1_block_number(message.l1_block_number);

    // Derive the EVM spec from the block's *effective* ArbOS version (current version,
    // or a scheduled upgrade due at this block's timestamp). Fall back to the configured
    // spec when the version is unreadable / uninitialized (e.g. a fresh in-memory DB).
    let arbos_version = ArbosState::read_effective_version_db(db, next_timestamp);
    let spec = if arbos_version == 0 {
        cfg.spec_id
    } else {
        crate::ArbSpecId::from_arbos_version(arbos_version)
    };

    let mut cfg_env = CfgEnv::new_with_spec(spec)
        .with_chain_id(cfg.chain_id)
        .with_disable_priority_fee_check(cfg.disable_priority_fee_check);
    cfg_env.disable_balance_check = cfg.disable_balance_check;
    // EIP-7623 calldata floor is applied only when the ArbOS `calldata_price_increase` feature
    // is enabled (Nitro state_transition.go: `IsPrague && IsCalldataPricingIncreaseEnabled()`).
    // Arbitrum otherwise prices calldata via its own L1 poster fee, so the floor must be off.
    cfg_env.disable_eip7623 = !ArbosState::open().features.read_calldata_price_increase_db(db);

    let context: ArbContext<&mut DB> = ArbContext::arb_with_chain_context(chain)
        .with_db(db)
        .with_cfg(cfg_env)
        .with_block(block)
        .with_tx(ArbTransaction::<TxEnv>::default());

    let mut evm = context.build_arb();

    let mut out = ArbExecOutcome {
        attempted: message.txs.len(),
        start_block_success: false,
        start_block_gas_used: 0,
        ..ArbExecOutcome::default()
    };

    // Nitro calls ProcessParentBlockHash for ArbOS v40+ before StartBlock.
    //
    // We execute the same system call here. On pre-v40 chains where the history
    // storage contract is not installed, this is a no-op state transition.
    if commits_state {
        let prev_hash = if l2_block_number == 0 {
            revm::primitives::B256::ZERO
        } else {
            evm.0
                .ctx
                .journal_mut()
                .db_mut()
                .block_hash(l2_block_number - 1)?
        };
        let parent_hash_result = evm.system_call_with_caller_commit(
            SYSTEM_ADDRESS,
            HISTORY_STORAGE_ADDRESS,
            Bytes::copy_from_slice(prev_hash.as_slice()),
        )?;
        if parent_hash_result.is_success() {
            out.writes.push(ArbWriteEffect {
                stage: ArbWriteStage::StartBlockParentHash,
                tx_index: None,
                target: ArbWriteTarget::StateDatabase,
            });
        }
    } else {
        let prev_hash = if l2_block_number == 0 {
            revm::primitives::B256::ZERO
        } else {
            evm.0
                .ctx
                .journal_mut()
                .db_mut()
                .block_hash(l2_block_number - 1)?
        };
        let _ = evm.system_call_with_caller(
            SYSTEM_ADDRESS,
            HISTORY_STORAGE_ADDRESS,
            Bytes::copy_from_slice(prev_hash.as_slice()),
        )?;
    }

    // Nitro prepends InternalTxStartBlock before all user txs.
    // We model that prelude as a typed internal tx (0x6a) under ArbOS actor identity.
    //
    // StartBlock prelude semantics are wired for local execution.
    //
    // TODO(parity): remaining Nitro parity items:
    // 1. Handle retryable reaping and scheduled ArbOS upgrades from the StartBlock path.
    // 2. Include header finalization extras (send_root/send_count/l1_block_number/arbos_version)
    //    so produced block/header hashes can match Nitro.
    if let Some(start_block_call) = hooks.start_block_prelude(
        input,
        ArbStartBlockDerived {
            l2_block_number,
            time_last_block,
        },
    ) {
        let start_block_tx = start_block_internal_tx(start_block_call, cfg.chain_id);
        if commits_state {
            let start_block_result = evm.transact(start_block_tx)?;
            out.start_block_success = start_block_result.result.is_success();
            out.start_block_gas_used = start_block_result.result.tx_gas_used();
            evm.commit(start_block_result.state);
            out.writes.push(ArbWriteEffect {
                stage: ArbWriteStage::StartBlockPrelude,
                tx_index: None,
                target: ArbWriteTarget::StateDatabase,
            });
        } else {
            let start_block_result = evm.transact(start_block_tx)?.result;
            out.start_block_success = start_block_result.is_success();
            out.start_block_gas_used = start_block_result.tx_gas_used();
        }
    }

    let mut queue: VecDeque<QueuedTx> = message
        .txs
        .iter()
        .cloned()
        .enumerate()
        .map(|(idx, tx)| QueuedTx {
            tx,
            tx_index: Some(idx),
            write_stage: ArbWriteStage::UserTransaction,
        })
        .collect();

    while let Some(queued) = queue.pop_front() {
        let tx_env = match arb_envelope_to_tx_env(&queued.tx) {
            Ok(tx) => tx,
            Err(_) => {
                out.skipped_unsupported = out.skipped_unsupported.saturating_add(1);
                continue;
            }
        };

        let tx_result = evm.transact(tx_env)?;

        let result = tx_result.result;
        out.executed = out.executed.saturating_add(1);
        out.txs.push(ArbTxExecution {
            tx_hash: queued.tx.hash(),
            gas_used: result.tx_gas_used(),
            success: result.is_success(),
        });
        if commits_state {
            evm.commit(tx_result.state);
            out.writes.push(ArbWriteEffect {
                stage: queued.write_stage,
                tx_index: queued.tx_index,
                target: ArbWriteTarget::StateDatabase,
            });
            // Scheduled retries — including a submit-retryable's auto-redeem — are
            // derived uniformly from the `RedeemScheduled` logs emitted during this
            // transaction. The submit path emits exactly that log when it auto-redeems,
            // so it must not be scheduled a second time here.
            if result.is_success() {
                for retry_tx in
                    scheduled_retries_from_redeem_logs(evm.ctx_mut(), result.logs(), cfg.chain_id)
                {
                    queue.push_back(QueuedTx {
                        tx: retry_tx,
                        tx_index: None,
                        write_stage: ArbWriteStage::ScheduledRetryTransaction,
                    });
                }
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::execute_message;
    use crate::{
        ArbExecCfg, ArbExecutionInput, ArbMessageEnvelope, ArbParentHeader,
        constants::ARB_RETRYABLE_TX_ADDRESS, executor::ArbWriteStage,
    };
    use arb_alloy_consensus::transactions::{
        ArbTxEnvelope, TxUnsigned, submit_retryable::SubmitRetryableTx,
    };
    use revm::{
        database::InMemoryDB,
        primitives::{Address, B256, Bytes, TxKind, U256, keccak256},
    };

    #[test]
    fn submit_retryable_schedules_and_executes_retry_tx() {
        let sender = Address::with_last_byte(0x11);
        let retry_to = Address::with_last_byte(0x22);
        let request_id = B256::with_last_byte(0x33);
        let submit = SubmitRetryableTx::new(
            U256::from(42161_u64),
            request_id,
            sender,
            U256::from(7_u64),
            U256::from(200_000_000_000_000_u64),
            U256::from(1_000_000_000_u64),
            U256::from(100_000_u64),
            TxKind::Call(retry_to),
            U256::ZERO,
            retry_to,
            U256::from(100_000_000_000_000_u64),
            retry_to,
            Bytes::new(),
        );
        let submit_hash = submit.tx_hash();

        let input = ArbExecutionInput::new(
            ArbParentHeader {
                number: 0,
                timestamp: 1,
                beneficiary: Address::ZERO,
                basefee: 100_000_000,
                gas_limit: 30_000_000,
                difficulty: U256::ZERO,
                prevrandao: Some(B256::ZERO),
            },
            ArbMessageEnvelope {
                sequence_number: Some(1),
                l1_block_number: 1,
                l1_timestamp: 1,
                poster: Address::ZERO,
                l1_base_fee_wei: U256::from(7_u64),
                delayed_messages_read: 0,
                txs: vec![ArbTxEnvelope::from(submit)],
            },
            ArbExecCfg::default(),
        );

        let mut db = InMemoryDB::default();
        let outcome = execute_message(&mut db, &input).expect("message execution should succeed");

        assert_eq!(outcome.txs.len(), 2, "submit should schedule one retry tx");
        assert_eq!(outcome.txs[0].tx_hash, submit_hash);
        assert_eq!(outcome.txs[0].gas_used, 100_000);
        assert!(outcome.txs[0].success, "submit tx should succeed");
        assert!(outcome.txs[1].success, "scheduled retry tx should execute");
        assert_eq!(
            outcome.txs[1].gas_used, 21_000,
            "scheduled retry should charge intrinsic gas"
        );
        assert!(
            outcome
                .writes
                .iter()
                .any(|write| write.stage == ArbWriteStage::ScheduledRetryTransaction),
            "scheduled retry write should be reported"
        );
    }

    #[test]
    fn redeem_precompile_log_schedules_retry_tx() {
        let sender = Address::with_last_byte(0x31);
        let redeemer = Address::with_last_byte(0x32);
        let retry_to = Address::with_last_byte(0x33);
        let request_id = B256::with_last_byte(0x34);
        let submit = SubmitRetryableTx::new(
            U256::from(42161_u64),
            request_id,
            sender,
            U256::from(1_u64),
            U256::from(4_000_u64),
            U256::from(1_u64),
            U256::from(250_000_u64),
            TxKind::Call(retry_to),
            U256::from(9_u64),
            Address::with_last_byte(0x35),
            U256::from(2_000_u64),
            Address::with_last_byte(0x36),
            Bytes::from(vec![0xaa, 0xbb]),
        );
        let ticket_id = submit.tx_hash();

        let mut redeem_input = Vec::with_capacity(4 + 32);
        let selector = keccak256("redeem(bytes32)");
        redeem_input.extend_from_slice(&selector[..4]);
        redeem_input.extend_from_slice(ticket_id.as_slice());
        let redeem = TxUnsigned {
            chain_id: U256::from(42161_u64),
            from: redeemer,
            nonce: 0,
            gas_fee_cap: U256::ZERO,
            gas_limit: 300_000,
            to: TxKind::Call(ARB_RETRYABLE_TX_ADDRESS),
            value: U256::ZERO,
            input: Bytes::from(redeem_input),
        };

        let input = ArbExecutionInput::new(
            ArbParentHeader {
                number: 0,
                timestamp: 1,
                beneficiary: Address::ZERO,
                basefee: 0,
                gas_limit: 30_000_000,
                difficulty: U256::ZERO,
                prevrandao: Some(B256::ZERO),
            },
            ArbMessageEnvelope {
                sequence_number: Some(1),
                l1_block_number: 1,
                l1_timestamp: 1,
                poster: Address::ZERO,
                l1_base_fee_wei: U256::from(1_u64),
                delayed_messages_read: 0,
                txs: vec![ArbTxEnvelope::from(submit), ArbTxEnvelope::from(redeem)],
            },
            ArbExecCfg::default(),
        );

        let mut db = InMemoryDB::default();
        let outcome = execute_message(&mut db, &input).expect("message execution should succeed");

        let scheduled_count = outcome
            .writes
            .iter()
            .filter(|write| write.stage == ArbWriteStage::ScheduledRetryTransaction)
            .count();
        assert_eq!(
            scheduled_count, 1,
            "redeem precompile should schedule exactly one retry tx in this flow"
        );
        assert_eq!(outcome.txs.len(), 3, "submit + redeem + scheduled retry");
        assert!(
            outcome.txs[2].success,
            "scheduled retry generated from redeem should execute"
        );
    }
}
