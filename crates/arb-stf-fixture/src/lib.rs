//! Neutral, implementation-independent fixture support for STF parity tests.
//!
//! This crate deliberately contains no execution code. It defines the durable
//! fixture boundary shared by the Nitro capture process and offline Rust runners.

pub mod objects;
pub mod schema;

pub use objects::{ObjectStore, ObjectStoreError};
pub use schema::{
    FixtureCase, FixtureError, FixtureInput, FixtureManifest, FixtureObject, FixturePrestate,
    FixtureProvenance, FixtureResult, FixtureSuite, ObjectCompression, STF_FIXTURE_SCHEMA,
    STF_FIXTURE_SCHEMA_VERSION,
};
