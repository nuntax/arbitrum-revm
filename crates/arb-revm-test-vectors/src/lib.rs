//! Offline, closed-world STF vector execution.
//!
//! Fixture files are immutable observations from Nitro. This crate reads them,
//! executes only the input against a strict state database, and compares actual
//! results without network access.

pub mod runner;
pub mod strict_db;
pub mod witness;

pub use runner::{
    DerivedTransactionsInput, ExpectedSequenceOutput, SequenceAccountDelta, SequenceTxOutput,
    VectorReport, run_case,
};
pub use strict_db::{
    CompleteAccount, CompleteState, IncompleteWitness, StrictDatabase, complete_state_root,
};
pub use witness::{ExecutionWitnessPrestate, WitnessDatabase};
