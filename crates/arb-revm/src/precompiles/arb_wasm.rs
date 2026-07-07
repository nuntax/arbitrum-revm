use super::*;
use crate::arb_journal::ArbPrecompileCtx;
use crate::storage::{stylus_param_layout as layout, unpack_uint};

const MIN_INIT_GAS_UNITS: u64 = 128;
const MIN_CACHED_INIT_GAS_UNITS: u64 = 32;
const COST_SCALAR_PERCENT_UNITS: u64 = 2;
const ARBOS_VERSION_STYLUS: u64 = 30;
const ARBOS_VERSION_STYLUS_CHARGING_FIXES: u64 = 32;

pub(super) fn run_arb_wasm<CTX>(ctx: &mut CTX, input: &[u8], gas_limit: u64) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let call = match ArbWasm::ArbWasmCalls::abi_decode(input) {
        Ok(c) => c,
        Err(e) => return revert_result(gas_limit, &format!("ArbWasm: invalid calldata: {e}")),
    };

    let state = ArbosState::open();
    let arbos_version = match state.arbos_version.get(ctx.journal_mut()) {
        Ok(v) => v,
        Err(e) => return revert_result(gas_limit, &format!("ArbWasm: storage error: {e}")),
    };
    if arbos_version < ARBOS_VERSION_STYLUS {
        return revert_result(
            gas_limit,
            "ArbWasm: unavailable before ArbOS Stylus activation",
        );
    }

    // Read the single packed 32-byte word that holds all Stylus params.
    // Nitro reference: arbos/programs/params.go Params().
    let word = match state.programs.read_params_word(ctx.journal_mut()) {
        Ok(w) => w,
        Err(e) => {
            return revert_result(gas_limit, &format!("ArbWasm: params read error: {e}"));
        }
    };

    match call {
        ArbWasm::ArbWasmCalls::stylusVersion(_) => {
            let v = unpack_uint(&word, layout::VERSION.0, layout::VERSION.1) as u16;
            ok_result(gas_limit, alloy_core::sol_types::SolValue::abi_encode(&(v,)))
        }
        ArbWasm::ArbWasmCalls::inkPrice(_) => {
            let v = unpack_uint(&word, layout::INK_PRICE.0, layout::INK_PRICE.1);
            ok_result(gas_limit, alloy_core::sol_types::SolValue::abi_encode(&(v,)))
        }
        ArbWasm::ArbWasmCalls::maxStackDepth(_) => {
            let v = unpack_uint(&word, layout::MAX_STACK_DEPTH.0, layout::MAX_STACK_DEPTH.1);
            ok_result(gas_limit, alloy_core::sol_types::SolValue::abi_encode(&(v,)))
        }
        ArbWasm::ArbWasmCalls::freePages(_) => {
            let v = unpack_uint(&word, layout::FREE_PAGES.0, layout::FREE_PAGES.1) as u16;
            ok_result(gas_limit, alloy_core::sol_types::SolValue::abi_encode(&(v,)))
        }
        ArbWasm::ArbWasmCalls::pageGas(_) => {
            let v = unpack_uint(&word, layout::PAGE_GAS.0, layout::PAGE_GAS.1) as u16;
            ok_result(gas_limit, alloy_core::sol_types::SolValue::abi_encode(&(v,)))
        }
        ArbWasm::ArbWasmCalls::pageRamp(_) => {
            // PageRamp is NOT stored in the packed word.  Nitro initialises the
            // struct field with the constant and never persists it.
            let v = layout::PAGE_RAMP_CONSTANT;
            ok_result(gas_limit, alloy_core::sol_types::SolValue::abi_encode(&(v,)))
        }
        ArbWasm::ArbWasmCalls::pageLimit(_) => {
            let v = unpack_uint(&word, layout::PAGE_LIMIT.0, layout::PAGE_LIMIT.1) as u16;
            ok_result(gas_limit, alloy_core::sol_types::SolValue::abi_encode(&(v,)))
        }
        ArbWasm::ArbWasmCalls::minInitGas(_) => {
            if arbos_version < ARBOS_VERSION_STYLUS_CHARGING_FIXES {
                return revert_result(
                    gas_limit,
                    "ArbWasm: minInitGas unavailable before charging fixes",
                );
            }
            let gas_units =
                unpack_uint(&word, layout::MIN_INIT_GAS.0, layout::MIN_INIT_GAS.1) as u64;
            let cached_units = unpack_uint(
                &word,
                layout::MIN_CACHED_INIT_GAS.0,
                layout::MIN_CACHED_INIT_GAS.1,
            ) as u64;
            let gas = gas_units.saturating_mul(MIN_INIT_GAS_UNITS);
            let cached = cached_units.saturating_mul(MIN_CACHED_INIT_GAS_UNITS);
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(gas, cached)),
            )
        }
        ArbWasm::ArbWasmCalls::initCostScalar(_) => {
            let units =
                unpack_uint(&word, layout::INIT_COST_SCALAR.0, layout::INIT_COST_SCALAR.1) as u64;
            let v = units.saturating_mul(COST_SCALAR_PERCENT_UNITS);
            ok_result(gas_limit, alloy_core::sol_types::SolValue::abi_encode(&(v,)))
        }
        ArbWasm::ArbWasmCalls::expiryDays(_) => {
            let v = unpack_uint(&word, layout::EXPIRY_DAYS.0, layout::EXPIRY_DAYS.1) as u16;
            ok_result(gas_limit, alloy_core::sol_types::SolValue::abi_encode(&(v,)))
        }
        ArbWasm::ArbWasmCalls::keepaliveDays(_) => {
            let v = unpack_uint(&word, layout::KEEPALIVE_DAYS.0, layout::KEEPALIVE_DAYS.1) as u16;
            ok_result(gas_limit, alloy_core::sol_types::SolValue::abi_encode(&(v,)))
        }
        ArbWasm::ArbWasmCalls::blockCacheSize(_) => {
            let v =
                unpack_uint(&word, layout::BLOCK_CACHE_SIZE.0, layout::BLOCK_CACHE_SIZE.1) as u16;
            ok_result(gas_limit, alloy_core::sol_types::SolValue::abi_encode(&(v,)))
        }
        ArbWasm::ArbWasmCalls::codehashVersion(_)
        | ArbWasm::ArbWasmCalls::codehashAsmSize(_)
        | ArbWasm::ArbWasmCalls::programVersion(_)
        | ArbWasm::ArbWasmCalls::programInitGas(_)
        | ArbWasm::ArbWasmCalls::programMemoryFootprint(_)
        | ArbWasm::ArbWasmCalls::programTimeLeft(_)
        | ArbWasm::ArbWasmCalls::activateProgram(_)
        | ArbWasm::ArbWasmCalls::codehashKeepalive(_) => revert_result(
            gas_limit,
            "ArbWasm: per-program queries not yet implemented",
        ),
    }
}
