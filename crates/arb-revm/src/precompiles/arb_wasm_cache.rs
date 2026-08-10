use super::*;
use crate::arb_journal::{ArbCall, ArbPrecompileCtx, MeteredJournal};

const ARBOS_VERSION_STYLUS: u64 = 30;
const ARBOS_VERSION_STYLUS_FIXES: u64 = 31;

pub(super) fn run_arb_wasm_cache<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
    call_inputs: &ArbCall,
) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let call = match ArbWasmCache::ArbWasmCacheCalls::abi_decode(input) {
        Ok(c) => c,
        Err(_) => return gated_revert_result(gas_limit),
    };

    let state = ArbosState::open();
    let arbos_version = match state.arbos_version.get(ctx.journal_mut()) {
        Ok(v) => v,
        Err(e) => return fatal_result(gas_limit, &format!("ArbWasmCache: storage error: {e}")),
    };
    if arbos_version < ARBOS_VERSION_STYLUS {
        return revert_result(
            gas_limit,
            "ArbWasmCache: unavailable before ArbOS Stylus activation",
        );
    }

    // Nitro bills every ArbOS storage operation performed by a precompile method through its
    // burner. The common dispatcher separately accounts for the one OpenArbosState read; this
    // wrapper accounts for the method-specific reads below.
    let mut journal = MeteredJournal::new(ctx.journal_mut());

    let mut caller_has_access = || {
        let caller = call_inputs.caller;
        let is_cache_manager = match state
            .programs
            .cache_managers
            .is_member(caller, &mut journal)
        {
            Ok(v) => v,
            Err(_) => return false,
        };
        if is_cache_manager {
            return true;
        }
        state
            .chain_owners
            .is_member(caller, &mut journal)
            .unwrap_or(false)
    };

    let mut result = match call {
        ArbWasmCache::ArbWasmCacheCalls::isCacheManager(c) => {
            let is_manager = match state
                .programs
                .cache_managers
                .is_member(c.account, &mut journal)
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
            let managers = match state.programs.cache_managers.all_members(&mut journal) {
                Ok(m) => m,
                Err(e) => return revert_result(gas_limit, &format!("ArbWasmCache: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(managers,)),
            )
        }
        ArbWasmCache::ArbWasmCacheCalls::codehashIsCached(c) => {
            let cached = match state.programs.read_program(c.codehash, &mut journal) {
                Ok(program) => program.cached,
                Err(e) => return revert_result(gas_limit, &format!("ArbWasmCache: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(cached,)),
            )
        }
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
    };

    let burned = journal.burned;
    if !result.gas.record_regular_cost(burned) {
        result.result = revm::interpreter::InstructionResult::OutOfGas;
        result.output = revm::primitives::Bytes::new();
    }
    result
}

#[cfg(test)]
mod tests {
    use alloy_core::sol_types::{SolCall, SolValue};
    use revm::{
        context_interface::ContextTr,
        database_interface::EmptyDB,
        interpreter::InstructionResult,
        primitives::{Address, B256, U256, address},
    };

    use super::{ArbPrecompilesEnum, ArbWasmCache};
    use crate::{
        api::default_ctx::{ArbContext, DefaultArb},
        arb_journal::ArbCall,
        arbos_init::{ArbosInitConfig, initialize_arbos_state},
        storage::{ArbosState, programs::ProgramInfo},
    };

    const ARB_WASM_CACHE: Address = address!("0000000000000000000000000000000000000072");

    fn ctx() -> ArbContext<EmptyDB> {
        let mut ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();
        initialize_arbos_state(
            &ArbosInitConfig {
                initial_arbos_version: 31,
                initial_chain_owner: Address::ZERO,
                chain_id: U256::from(412_346_u64),
                genesis_block_number: 0,
                initial_l1_base_fee: U256::from(50_000_000_000_u64),
                serialized_chain_config: b"{\"chainId\":412346}".to_vec(),
                debug_precompiles: false,
            },
            ctx.journal_mut(),
        )
        .expect("initialize ArbOS state");
        ctx
    }

    #[test]
    fn codehash_is_cached_reads_program_state_and_charges_storage() {
        let mut ctx = ctx();
        let codehash = B256::with_last_byte(0x42);
        ArbosState::open()
            .programs
            .write_program(
                codehash,
                &ProgramInfo {
                    cached: true,
                    ..Default::default()
                },
                ctx.journal_mut(),
            )
            .expect("write program record");

        let input = ArbWasmCache::codehashIsCachedCall { codehash }.abi_encode();
        let call = ArbCall {
            input: &input,
            gas_limit: 100_000,
            caller: Address::ZERO,
            value: U256::ZERO,
            bytecode_address: ARB_WASM_CACHE,
            acting_address: ARB_WASM_CACHE,
            is_static: true,
        };
        let result = ArbPrecompilesEnum::ArbWasmCache.run_dispatch(&mut ctx, &call);

        assert_eq!(result.result, InstructionResult::Return);
        assert!(<(bool,)>::abi_decode(&result.output).unwrap().0);
        // OpenArbosState and one argument/output word cost 806 in the shared dispatcher; the
        // method's ProgramCached storage read costs a further 800 through Nitro's burner.
        assert_eq!(result.gas.total_gas_spent(), 1_606);
    }
}
