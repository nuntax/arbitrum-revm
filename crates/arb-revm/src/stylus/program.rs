//! Stylus program compile / activate / extraction + the compiled-program LRU cache.
//!
//! Ported from arbos-revm's `stylus_executor.rs` — only the runtime-facing, Context-free
//! helpers — against the canonical Nitro `stylus` crate. These wrap `native::compile` /
//! `native::activate` and brotli decompression; no revm Context is involved here.

use std::{num::NonZeroUsize, sync::Mutex};

use arbutil::{Bytes32, evm::api::Ink};
use lru::LruCache;
use revm::{
    interpreter::Gas,
    primitives::{B256, Bytes, FixedBytes},
};
use stylus::{
    brotli::{self, Dictionary},
    native,
    prover::{
        machine::Module,
        programs::{
            StylusData,
            config::{CompileConfig, PricingParams},
        },
    },
};
use wasmer_types::target::Target;

use super::constants::STYLUS_DISCRIMINANT;

type ProgramCacheEntry = (Vec<u8>, Module, StylusData);

lazy_static::lazy_static! {
    /// Compiled-program cache keyed by code hash: (serialized native module, prover module,
    /// Stylus metadata). Mirrors Nitro's in-memory program cache.
    pub static ref PROGRAM_CACHE: Mutex<LruCache<FixedBytes<32>, ProgramCacheEntry>> =
        Mutex::new(LruCache::new(NonZeroUsize::new(1024).unwrap()));
}

/// Extract (brotli-decompressing) the WASM from a Stylus-prefixed contract bytecode.
/// `Ok(None)` if the bytecode isn't a Stylus program; `Err(msg)` on a malformed one.
pub fn stylus_code(bytecode: &[u8]) -> Result<Option<Bytes>, Vec<u8>> {
    let Some(rest) = bytecode.strip_prefix(STYLUS_DISCRIMINANT) else {
        return Ok(None);
    };
    let Some((dictionary, compressed)) = rest.split_at_checked(1) else {
        return Err(b"specified bytecode is not a Stylus program".to_vec());
    };
    let dictionary = match dictionary[0] {
        0x00 => Dictionary::Empty,
        0x01 => Dictionary::StylusProgram,
        t => return Err(format!("unsupported dictionary {t}").into_bytes()),
    };
    let wasm = brotli::decompress(compressed, dictionary).or_else(|err| {
        // Uncompressed deployments are allowed when the dictionary is empty.
        if dictionary == Dictionary::Empty {
            Ok(compressed.to_vec())
        } else {
            Err(format!("failed decompression: {}", err as u8).into_bytes())
        }
    })?;
    Ok(Some(Bytes::from(wasm)))
}

/// Compile WASM to a serialized native module via Nitro's stylus runtime.
pub fn stylus_compile(wasm: &Bytes, compile_config: &CompileConfig) -> Result<Vec<u8>, String> {
    native::compile(
        wasm,
        compile_config.version,
        compile_config.debug.debug_funcs,
        Target::default(),
        false,
    )
    .map_err(|e| e.to_string())
}

/// Activate (validate + instrument) a Stylus program, charging activation gas out of `gas`
/// (the unused remainder is refunded on success; all of it is consumed on failure, matching
/// Nitro). Returns the prover module + Stylus metadata.
pub fn stylus_activate(
    mut gas: Option<&mut Gas>,
    wasm: &Bytes,
    code_hash: B256,
    arbos_version: u16,
    stylus_version: u16,
    page_limit: u16,
    debug: bool,
) -> Result<(Module, StylusData), String> {
    let mut activation_gas = if let Some(gas) = gas.as_deref_mut() {
        let remaining = gas.remaining();
        gas.spend_all();
        remaining
    } else {
        u64::MAX
    };
    let (module, stylus_data) = native::activate(
        wasm,
        &Bytes32::from(code_hash.0),
        stylus_version,
        arbos_version as u64,
        page_limit,
        debug,
        &mut activation_gas,
    )
    .map_err(|e| e.to_string())?;
    if let Some(gas) = gas {
        gas.erase_cost(activation_gas);
    }
    Ok((module, stylus_data))
}

/// Convert Stylus ink to EVM gas (ceiling).
pub fn ink_to_gas_ceil(pricing: PricingParams, ink: Ink) -> u64 {
    ink.0.div_ceil(pricing.ink_price as u64)
}

/// Insert a compiled program into the cache (keyed by code hash).
pub fn cache_program(code_hash: B256, serialized: Vec<u8>, module: Module, stylus_data: StylusData) {
    let mut cache = PROGRAM_CACHE.lock().unwrap();
    cache.get_or_insert(code_hash, || (serialized, module, stylus_data));
}
