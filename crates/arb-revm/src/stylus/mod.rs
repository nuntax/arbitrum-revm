//! Stylus (WASM smart-contract) execution.
//!
//! Ported from the **arbos-revm** reference implementation (iosiro, Arbitrum Stylus
//! Sprint) and built on Nitro's stylus runtime via the `iosiro/arbos-foundry-stylus`
//! fork (`stylus`/`arbutil`/`wasmer` crates). Feature-gated behind `stylus` because it
//! pulls the forked wasmer.
//!
//! Layout (mirrors arbos-revm):
//! - [`constants`], Stylus discriminant + initial param values + memory model.
//! - [`gas`], pure gas-model helpers (page/init/cached costs).
//! - `executor`, run/compile/activate/cache flow + the `EvmData` builder.
//! - `api`, the `StylusHandler` `RequestHandler` bridge to revm state.

pub mod api;
pub mod constants;
pub mod dispatch;
pub mod executor;
pub mod gas;
pub mod params;
pub mod program;
