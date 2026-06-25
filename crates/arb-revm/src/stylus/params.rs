//! Stylus parameters decoded from the packed ArbOS Programs params word.
//!
//! Nitro stores all Stylus config in one 32-byte word in the Programs substorage; arb_revm
//! exposes the raw word via [`crate::storage::programs::ArbosPrograms::read_params_word`]
//! and the field offsets via [`crate::storage::programs::stylus_param_layout`]. This decodes
//! the word into the values the executor needs for `StylusConfig` and the init/page gas model.

use crate::storage::programs::stylus_param_layout as layout;

/// Decoded Stylus parameters (mirrors Nitro `StylusParams`).
#[derive(Debug, Clone, Copy)]
pub struct StylusParams {
    pub version: u16,
    pub ink_price: u32,
    pub max_stack_depth: u32,
    pub free_pages: u16,
    pub page_gas: u16,
    pub page_limit: u16,
    pub min_init_gas: u8,
    pub min_cached_init_gas: u8,
    pub init_cost_scalar: u8,
    pub cached_cost_scalar: u8,
    pub block_cache_size: u16,
    pub max_wasm_size: u32,
}

impl StylusParams {
    /// Decode from the packed 32-byte params word.
    pub fn from_word(word: &[u8; 32]) -> Self {
        Self {
            version: be_u16(word, layout::VERSION.0),
            ink_price: be_uint(word, layout::INK_PRICE) as u32,
            max_stack_depth: be_u32(word, layout::MAX_STACK_DEPTH.0),
            free_pages: be_u16(word, layout::FREE_PAGES.0),
            page_gas: be_u16(word, layout::PAGE_GAS.0),
            page_limit: be_u16(word, layout::PAGE_LIMIT.0),
            min_init_gas: word[layout::MIN_INIT_GAS.0],
            min_cached_init_gas: word[layout::MIN_CACHED_INIT_GAS.0],
            init_cost_scalar: word[layout::INIT_COST_SCALAR.0],
            cached_cost_scalar: word[layout::CACHED_COST_SCALAR.0],
            block_cache_size: be_u16(word, layout::BLOCK_CACHE_SIZE.0),
            max_wasm_size: be_u32(word, layout::MAX_WASM_SIZE.0),
        }
    }
}

/// Read a big-endian unsigned integer of `len` bytes at `off`.
fn be_uint(word: &[u8; 32], (off, len): (usize, usize)) -> u64 {
    word[off..off + len].iter().fold(0u64, |acc, &b| (acc << 8) | b as u64)
}

fn be_u16(word: &[u8; 32], off: usize) -> u16 {
    u16::from_be_bytes([word[off], word[off + 1]])
}

fn be_u32(word: &[u8; 32], off: usize) -> u32 {
    u32::from_be_bytes([word[off], word[off + 1], word[off + 2], word[off + 3]])
}
