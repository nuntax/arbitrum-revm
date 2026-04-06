use crate::{
    ArbBuilder, ArbChainContext, ArbContext, ArbSpecId, ArbTransaction, DefaultArb,
    constants::ARBOS_ACTS_ADDRESS, transaction::arb_envelope_to_tx_env,
};
use alloy_core::{sol, sol_types::SolCall};
use revm::{
    Database, DatabaseCommit, ExecuteCommitEvm, SystemCallEvm,
    context::{BlockEnv, CfgEnv, TxEnv},
    primitives::{Address, B256, Bytes, U256},
};

sol! {
    interface ArbosActs {
        function startBlock(
            uint256 l1BaseFee,
            uint64 l1BlockNumber,
            uint64 l2BlockNumber,
            uint64 timeLastBlock
        ) external;
    }
}

/// Parent header values needed to derive the next execution block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArbParentHeader {
    pub number: u64,
    pub timestamp: u64,
    pub beneficiary: Address,
    pub basefee: u64,
    pub gas_limit: u64,
    pub difficulty: U256,
    pub prevrandao: Option<B256>,
}

impl Default for ArbParentHeader {
    fn default() -> Self {
        Self {
            number: 0,
            timestamp: 0,
            beneficiary: Address::ZERO,
            basefee: 0,
            gas_limit: u64::MAX,
            difficulty: U256::ZERO,
            prevrandao: Some(B256::ZERO),
        }
    }
}

/// Message-scoped STF input for a single sequencer message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArbMessageEnvelope {
    pub sequence_number: Option<u64>,
    pub l1_block_number: u64,
    pub l1_timestamp: u64,
    pub poster: Address,
    pub l1_base_fee_wei: U256,
    pub delayed_messages_read: u64,
    pub txs: Vec<arb_sequencer_consensus::transactions::ArbTxEnvelope>,
}

/// Static execution configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArbExecCfg {
    pub chain_id: u64,
    pub spec_id: ArbSpecId,
    pub block_gas_limit: u64,
    pub disable_priority_fee_check: bool,
    pub disable_balance_check: bool,
}

impl Default for ArbExecCfg {
    fn default() -> Self {
        Self {
            chain_id: 42161,
            spec_id: ArbSpecId::NITRO,
            block_gas_limit: 1 << 50,
            disable_priority_fee_check: true,
            disable_balance_check: true,
        }
    }
}

/// Per-transaction execution summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArbTxExecution {
    pub tx_hash: revm::primitives::B256,
    pub gas_used: u64,
    pub success: bool,
}

/// Stateless execution result for one sequencer message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArbExecOutcome {
    pub attempted: usize,
    pub executed: usize,
    pub skipped_unsupported: usize,
    pub start_block_success: bool,
    pub start_block_gas_used: u64,
    pub txs: Vec<ArbTxExecution>,
}

/// Error type for stateless message execution.
pub type ArbExecError<DB> = revm::context_interface::result::EVMError<
    <DB as Database>::Error,
    revm::context_interface::result::InvalidTransaction,
>;

/// Executes a sequencer message with no retained engine-local state.
///
/// This function:
/// - creates a fresh revm context/EVM for this call,
/// - executes all supported transactions in the message,
/// - commits state updates into `db`.
pub fn execute_message<'a, DB: Database + DatabaseCommit>(
    db: &'a mut DB,
    parent: ArbParentHeader,
    message: &ArbMessageEnvelope,
    cfg: ArbExecCfg,
) -> Result<ArbExecOutcome, ArbExecError<&'a mut DB>> {
    // Nitro's createNewHeader semantics: block timestamp is max(l1Timestamp, parent.Time)
    let next_timestamp = message.l1_timestamp.max(parent.timestamp);
    // Nitro's InternalTxStartBlock packs timePassed as newHeader.Time - lastHeader.Time.
    let time_last_block = next_timestamp.saturating_sub(parent.timestamp);
    let l2_block_number = parent.number.saturating_add(1);

    let mut block = BlockEnv::default();
    block.number = U256::from(l2_block_number);
    block.beneficiary = message.poster;
    block.timestamp = U256::from(next_timestamp);
    block.gas_limit = cfg.block_gas_limit.min(parent.gas_limit);
    block.basefee = parent.basefee;
    block.difficulty = parent.difficulty;
    block.prevrandao = parent.prevrandao;

    let chain = ArbChainContext::new(
        Some(message.l1_block_number),
        Some(message.l1_base_fee_wei),
        Some(message.delayed_messages_read),
        message.sequence_number,
    );

    let context: ArbContext<&mut DB> = ArbContext::arb_with_chain_context(chain)
        .with_db(db)
        .with_cfg(CfgEnv::new_with_spec(cfg.spec_id).with_chain_id(cfg.chain_id))
        .with_block(block)
        .with_tx(ArbTransaction::<TxEnv>::default());

    let mut evm = context.build_arb();

    let start_block_l1_base_fee =
        alloy_core::primitives::U256::from_limbs(*message.l1_base_fee_wei.as_limbs());
    let start_block_data = ArbosActs::startBlockCall::new((
        start_block_l1_base_fee,
        message.l1_block_number,
        l2_block_number,
        time_last_block,
    ))
    .abi_encode();

    let mut out = ArbExecOutcome {
        attempted: message.txs.len(),
        start_block_success: false,
        start_block_gas_used: 0,
        ..ArbExecOutcome::default()
    };

    // Nitro prepends InternalTxStartBlock before all user txs.
    // Here we run the equivalent calldata as a system call under ArbOS actor identity.
    let start_block_result = evm.system_call_with_caller(
        ARBOS_ACTS_ADDRESS,
        ARBOS_ACTS_ADDRESS,
        Bytes::from(start_block_data),
    )?;
    out.start_block_success = start_block_result.result.is_success();
    out.start_block_gas_used = start_block_result.result.gas_used();
    evm.commit(start_block_result.state);

    for tx in &message.txs {
        let tx_env = match arb_envelope_to_tx_env(tx) {
            Ok(tx) => tx,
            Err(_) => {
                out.skipped_unsupported = out.skipped_unsupported.saturating_add(1);
                continue;
            }
        };

        let result = evm.transact_commit(tx_env)?;
        out.executed = out.executed.saturating_add(1);
        out.txs.push(ArbTxExecution {
            tx_hash: tx.hash(),
            gas_used: result.gas_used(),
            success: result.is_success(),
        });
    }

    Ok(out)
}
