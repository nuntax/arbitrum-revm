//! Stylus gas-model helpers, ported verbatim from the arbos-revm reference
//! (`stylus_executor.rs`) / Nitro `arbos/programs`. Pure functions over the param values.

use super::constants::{COST_SCALAR_PERCENT, MEMORY_EXPONENTS, MIN_CACHED_GAS_UNITS, MIN_INIT_GAS_UNITS};

/// EVM gas charged for a Stylus call's memory pages (`new` opened this call, `open`
/// already open, `ever` the high-water mark), per the page model.
pub fn stylus_call_cost(new: u16, open: u16, ever: u16, free_pages: u16, page_gas: u16) -> u64 {
    let new_open = open.saturating_add(new);
    let new_ever = ever.max(new_open);

    if new_ever <= free_pages {
        return 0;
    }

    let sub_free = |pages: u16| pages.saturating_sub(free_pages);
    let adding = sub_free(new_open).saturating_sub(sub_free(open));
    let linear = (adding as u64).saturating_mul(page_gas as u64);

    let exp = |x: u16| -> u64 {
        if (x as usize) < MEMORY_EXPONENTS.len() {
            MEMORY_EXPONENTS[x as usize] as u64
        } else {
            u64::MAX
        }
    };
    let expand = exp(new_ever) - exp(ever);

    linear.saturating_add(expand)
}

/// EVM gas to charge for first-time (non-cached) program init.
pub fn init_gas_cost(init_cost: u16, min_init_gas: u8, init_cost_scaler: u8) -> u64 {
    let base = min_init_gas as u64 * MIN_INIT_GAS_UNITS;
    let dyno = (init_cost as u64).saturating_mul(init_cost_scaler as u64 * COST_SCALAR_PERCENT);
    base.saturating_add(dyno.div_ceil(100))
}

/// EVM gas to charge for cached program init.
pub fn cached_gas_cost(cached_init_cost: u16, min_cached_init_gas: u8, cached_init_cost_scaler: u8) -> u64 {
    let base = min_cached_init_gas as u64 * MIN_CACHED_GAS_UNITS;
    let dyno =
        (cached_init_cost as u64).saturating_mul(cached_init_cost_scaler as u64 * COST_SCALAR_PERCENT);
    base.saturating_add(dyno.div_ceil(100))
}
