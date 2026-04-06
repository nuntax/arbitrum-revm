//! Arbitrum-specific building blocks for extending `revm`.
//!
//! This crate intentionally starts with the extractable, stable pieces:
//! ArbOS storage layout, typed slot helpers, and constants/precompile addresses.
//! Nitro-faithful execution remains a separate layer because it depends heavily
//! on global node state and runtime data that do not belong in a portable storage crate.

pub mod api;
pub mod chain;
pub mod constants;
pub mod evm;
pub mod executor;
pub mod handler;
pub mod precompiles;
pub mod spec;
pub mod storage;
pub mod transaction;
mod util;

pub use api::{
    builder::{ArbBuilder, DefaultArbEvm},
    default_ctx::{ArbContext, DefaultArb},
};
pub use chain::ArbChainContext;
pub use evm::ArbEvm;
pub use executor::{
    ArbExecCfg, ArbExecOutcome, ArbMessageEnvelope, ArbParentHeader, ArbTxExecution,
    execute_message,
};
pub use handler::ArbHandler;
pub use precompiles::ArbPrecompiles;
pub use revm;
pub use spec::ArbSpecId;
pub use storage::{
    AddressSet, AddressTable, ArbosState, BatchPosterState, BatchPosterTable, BlockHashes,
    L1Pricing, L2Pricing, StorageBacked, StorageSlot, StorageSpace,
};
pub use transaction::{ArbTransaction, ArbTxTr};
pub use util::{
    address_to_u256, i256_to_u256_twos_complement, inverse_remap_l1_address, remap_l1_address,
    u256_twos_complement_to_i256,
};
