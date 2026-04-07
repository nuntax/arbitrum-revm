use crate::{
    constants::{ARBITRUM_INTERNAL_TX_TYPE, HISTORY_STORAGE_ADDRESS},
    executor::contract::{
        ArbExecCfg, ArbExecOutcome, ArbExecutionInput, ArbTxExecution, ArbWriteEffect,
        ArbWriteStage, ArbWriteTarget,
    },
    executor::hooks::{
        ArbExecutionHooks, ArbStartBlockDerived, ArbSystemCall, DefaultArbExecutionHooks,
    },
    transaction::arb_envelope_to_tx_env,
    ArbBuilder, ArbChainContext, ArbContext, ArbTransaction, DefaultArb,
};
use revm::{
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::{ContextTr, JournalTr},
    handler::{SystemCallCommitEvm, SystemCallEvm, SYSTEM_ADDRESS},
    primitives::{Bytes, TxKind, U256},
    Database, DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
};

/// Error type for stateless message execution.
pub type ArbExecError<DB> = revm::context_interface::result::EVMError<
    <DB as Database>::Error,
    revm::context_interface::result::InvalidTransaction,
>;

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
    let chain = ArbChainContext::new(message.sequence_number);

    let mut cfg_env = CfgEnv::new_with_spec(cfg.spec_id)
        .with_chain_id(cfg.chain_id)
        .with_disable_priority_fee_check(cfg.disable_priority_fee_check);
    cfg_env.disable_balance_check = cfg.disable_balance_check;

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
            let start_block_result = evm.transact_commit(start_block_tx)?;
            out.start_block_success = start_block_result.is_success();
            out.start_block_gas_used = start_block_result.gas_used();
            out.writes.push(ArbWriteEffect {
                stage: ArbWriteStage::StartBlockPrelude,
                tx_index: None,
                target: ArbWriteTarget::StateDatabase,
            });
        } else {
            let start_block_result = evm.transact(start_block_tx)?.result;
            out.start_block_success = start_block_result.is_success();
            out.start_block_gas_used = start_block_result.gas_used();
        }
    }

    for (idx, tx) in message.txs.iter().enumerate() {
        let tx_env = match arb_envelope_to_tx_env(tx) {
            Ok(tx) => tx,
            Err(_) => {
                out.skipped_unsupported = out.skipped_unsupported.saturating_add(1);
                continue;
            }
        };

        let result = if commits_state {
            evm.transact_commit(tx_env)?
        } else {
            evm.transact(tx_env)?.result
        };
        out.executed = out.executed.saturating_add(1);
        out.txs.push(ArbTxExecution {
            tx_hash: tx.hash(),
            gas_used: result.gas_used(),
            success: result.is_success(),
        });
        if commits_state {
            out.writes.push(ArbWriteEffect {
                stage: ArbWriteStage::UserTransaction,
                tx_index: Some(idx),
                target: ArbWriteTarget::StateDatabase,
            });
        }
    }

    Ok(out)
}
