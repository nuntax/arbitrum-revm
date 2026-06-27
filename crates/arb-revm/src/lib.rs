//! Arbitrum-specific building blocks for extending `revm`.
//!
//! This crate intentionally starts with the extractable, stable pieces:
//! ArbOS storage layout, typed slot helpers, and constants/precompile addresses.
//! Nitro-faithful execution remains a separate layer because it depends heavily
//! on global node state and runtime data that do not belong in a portable storage crate.

pub mod api;
pub mod arb_journal;
pub mod arbos_init;
pub mod chain;
pub mod constants;
mod deposit_tx;
pub mod evm;
pub mod executor;
pub mod handler;
mod internal_tx;
mod l1_cost;
pub mod precompiles;
pub mod replay;
mod retry_tx;
pub mod spec;
pub mod state_trie;
pub mod storage;
#[cfg(feature = "stylus")]
pub mod stylus;
mod submit_retryable_tx;
pub mod transaction;
mod util;

pub use api::{
    builder::{ArbBuilder, DefaultArbEvm},
    default_ctx::{ArbContext, DefaultArb},
};
pub use chain::ArbChainContext;
pub use evm::ArbEvm;
pub use executor::{
    ArbExecCfg, ArbExecOutcome, ArbExecutionHooks, ArbExecutionInput, ArbExecutionMode,
    ArbMessageEnvelope, ArbParentHeader, ArbRunner, ArbRunnerError, ArbStartBlockDerived,
    ArbSystemCall, ArbTxExecution, ArbWriteEffect, ArbWriteStage, ArbWriteTarget,
    DefaultArbExecutionHooks, execute_message, execute_message_with_hooks,
};
pub use handler::ArbHandler;
pub use precompiles::ArbPrecompiles;
pub use revm;
pub use spec::ArbSpecId;
pub use storage::{
    AddressSet, AddressTable, ArbFeatures, ArbosPrograms, ArbosState, BatchPosterState,
    BatchPosterTable, BlockHashes, ChainConfig, L1Pricing, L2Pricing, ProgramDataPricer,
    RetryableRecord, Retryables, SendMerkle, StorageBacked, StorageBytes, StorageQueue,
    StorageSlot, StorageSpace,
};
pub use submit_retryable_tx::{
    build_scheduled_retry_from_submit, submit_retryable_auto_redeem_scheduled,
};
pub use transaction::{ArbTransaction, ArbTxTr, TxConversionError};
pub use util::{
    address_to_u256, i256_to_u256_twos_complement, inverse_remap_l1_address, remap_l1_address,
    u256_twos_complement_to_i256,
};
