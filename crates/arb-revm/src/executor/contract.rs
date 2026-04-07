use revm::primitives::{Address, B256, U256};

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
    pub spec_id: crate::ArbSpecId,
    pub block_gas_limit: u64,
    pub disable_priority_fee_check: bool,
    pub disable_balance_check: bool,
}

impl Default for ArbExecCfg {
    fn default() -> Self {
        Self {
            chain_id: 42161,
            spec_id: crate::ArbSpecId::NITRO,
            block_gas_limit: 1 << 50,
            disable_priority_fee_check: true,
            disable_balance_check: true,
        }
    }
}

/// Execution mode for one message pipeline invocation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ArbExecutionMode {
    /// Execute and commit state updates to the backing DB.
    #[default]
    Commit,
    /// Execute without committing state updates (cache/prefetch path).
    Prefetch,
    /// Execute in sequencer mode (currently commit semantics).
    Sequencing,
}

impl ArbExecutionMode {
    /// Returns whether this mode persists state updates.
    pub const fn commits_state(self) -> bool {
        !matches!(self, Self::Prefetch)
    }
}

/// Fully-specified execution input contract for one message execution call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArbExecutionInput {
    pub parent: ArbParentHeader,
    pub message: ArbMessageEnvelope,
    pub cfg: ArbExecCfg,
    pub mode: ArbExecutionMode,
}

impl ArbExecutionInput {
    pub fn new(parent: ArbParentHeader, message: ArbMessageEnvelope, cfg: ArbExecCfg) -> Self {
        Self {
            parent,
            message,
            cfg,
            mode: ArbExecutionMode::Commit,
        }
    }

    pub fn with_mode(mut self, mode: ArbExecutionMode) -> Self {
        self.mode = mode;
        self
    }
}

/// Per-transaction execution summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArbTxExecution {
    pub tx_hash: B256,
    pub gas_used: u64,
    pub success: bool,
}

/// Durable write stage in the execution pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArbWriteStage {
    /// Parent-hash system call write for EIP-2935 history storage.
    StartBlockParentHash,
    /// Start-of-block prelude write (Nitro `StartBlock` equivalent).
    StartBlockPrelude,
    /// User transaction state write.
    UserTransaction,
}

/// Durable write target category.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArbWriteTarget {
    /// Underlying EVM state database writes.
    StateDatabase,
}

/// Explicit write side effect emitted by the executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArbWriteEffect {
    pub stage: ArbWriteStage,
    pub tx_index: Option<usize>,
    pub target: ArbWriteTarget,
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
    pub writes: Vec<ArbWriteEffect>,
}
