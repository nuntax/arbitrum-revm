use super::*;
use revm::interpreter::CallInputs;

const ARBOS_VERSION_STYLUS: u64 = 30;
const ARBOS_VERSION_STYLUS_FIXES: u64 = 31;

pub(super) fn run_arb_wasm_cache<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
    call_inputs: &CallInputs,
) -> InterpreterResult
where
    CTX: ContextTr<Journal: JournalTr>,
{
    let call = match ArbWasmCache::ArbWasmCacheCalls::abi_decode(input) {
        Ok(c) => c,
        Err(e) => return revert_result(gas_limit, &format!("ArbWasmCache: invalid calldata: {e}")),
    };

    let state = ArbosState::open();
    let arbos_version = match state.arbos_version.get(ctx.journal_mut()) {
        Ok(v) => v,
        Err(e) => return revert_result(gas_limit, &format!("ArbWasmCache: storage error: {e}")),
    };
    if arbos_version < ARBOS_VERSION_STYLUS {
        return revert_result(
            gas_limit,
            "ArbWasmCache: unavailable before ArbOS Stylus activation",
        );
    }

    let mut caller_has_access = || {
        let caller = call_inputs.caller;
        let is_cache_manager = match state
            .programs
            .cache_managers
            .is_member(caller, ctx.journal_mut())
        {
            Ok(v) => v,
            Err(_) => return false,
        };
        if is_cache_manager {
            return true;
        }
        state
            .chain_owners
            .is_member(caller, ctx.journal_mut())
            .unwrap_or(false)
    };

    match call {
        ArbWasmCache::ArbWasmCacheCalls::isCacheManager(c) => {
            let is_manager = match state
                .programs
                .cache_managers
                .is_member(c.account, ctx.journal_mut())
            {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbWasmCache: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(is_manager,)),
            )
        }
        ArbWasmCache::ArbWasmCacheCalls::allCacheManagers(_) => {
            let managers = match state.programs.cache_managers.all_members(ctx.journal_mut()) {
                Ok(m) => m,
                Err(e) => return revert_result(gas_limit, &format!("ArbWasmCache: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(managers,)),
            )
        }
        ArbWasmCache::ArbWasmCacheCalls::codehashIsCached(_) => ok_result(
            gas_limit,
            alloy_core::sol_types::SolValue::abi_encode(&(false,)),
        ),
        ArbWasmCache::ArbWasmCacheCalls::cacheProgram(_) => {
            if arbos_version < ARBOS_VERSION_STYLUS_FIXES {
                return revert_result(
                    gas_limit,
                    "ArbWasmCache: cacheProgram unavailable before ArbOS Stylus fixes",
                );
            }
            if !caller_has_access() {
                return revert_result(
                    gas_limit,
                    "ArbWasmCache: caller lacks cache-manager/owner access",
                );
            }
            ok_result(gas_limit, vec![])
        }
        ArbWasmCache::ArbWasmCacheCalls::evictCodehash(_) => {
            if !caller_has_access() {
                return revert_result(
                    gas_limit,
                    "ArbWasmCache: caller lacks cache-manager/owner access",
                );
            }
            ok_result(gas_limit, vec![])
        }
    }
}
