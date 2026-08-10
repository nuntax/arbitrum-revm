use super::*;
use crate::arb_journal::{
    ArbCall, ArbJournal, ArbPrecompileCtx, MeteredJournal, STORAGE_CODE_HASH_COST,
};
use revm::primitives::{B256, Bytes, Log, keccak256};

const ARBOS_VERSION_STYLUS: u64 = 30;
const ARBOS_VERSION_STYLUS_FIXES: u64 = 31;
const UPDATE_PROGRAM_CACHE_EVENT_SIGNATURE: &str = "UpdateProgramCache(address,bytes32,bool)";

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
    let timestamp = ctx.block_timestamp();
    let mut journal = MeteredJournal::new(ctx.journal_mut());

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
        ArbWasmCache::ArbWasmCacheCalls::cacheProgram(c) => {
            if arbos_version < ARBOS_VERSION_STYLUS_FIXES {
                return revert_result(
                    gas_limit,
                    "ArbWasmCache: cacheProgram unavailable before ArbOS Stylus fixes",
                );
            }
            // Nitro's precompile context performs a separate `Storage.GetCodeHash` here. Its
            // burner charge is 2,600 gas, not the 800-gas flat cost of an ArbOS slot read.
            journal.charge(STORAGE_CODE_HASH_COST);
            let (code_hash, code) = match journal.account_code_hash_and_code(c.program) {
                Ok(code) => code,
                Err(error) => {
                    return fatal_result(
                        gas_limit,
                        &format!("ArbWasmCache: program code hash read error: {error}"),
                    );
                }
            };
            // `CacheProgram` resolves the code hash before delegating to `setProgramCached`,
            // whose first action is this access check.
            if !caller_has_access(&state, &mut journal, call_inputs.caller) {
                // Nitro's `setProgramCached` calls `BurnOut` for an unauthorized manager.
                return gated_revert_result(gas_limit);
            }
            set_program_cached(
                &state,
                &mut journal,
                gas_limit,
                code_hash,
                true,
                !code.is_empty(),
                call_inputs.caller,
                call_inputs.bytecode_address,
                timestamp,
            )
        }
        ArbWasmCache::ArbWasmCacheCalls::evictCodehash(c) => {
            if !caller_has_access(&state, &mut journal, call_inputs.caller) {
                return gated_revert_result(gas_limit);
            }
            set_program_cached(
                &state,
                &mut journal,
                gas_limit,
                c.codehash,
                false,
                false,
                call_inputs.caller,
                call_inputs.bytecode_address,
                timestamp,
            )
        }
    };

    let burned = journal.burned;
    if !result.gas.record_regular_cost(burned) {
        result.result = revm::interpreter::InstructionResult::OutOfGas;
        result.output = revm::primitives::Bytes::new();
    }
    result
}

/// Nitro `ArbWasmCache.hasAccess`. Storage failures deliberately deny access: Nitro then burns the
/// entire call budget rather than exposing a partially charged business-logic revert.
fn caller_has_access<J: ArbJournal>(
    state: &ArbosState,
    journal: &mut MeteredJournal<'_, J>,
    caller: Address,
) -> bool {
    match state.programs.cache_managers.is_member(caller, journal) {
        Ok(true) => true,
        Ok(false) => state
            .chain_owners
            .is_member(caller, journal)
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Nitro `Programs.SetProgramCached`, excluding its non-consensus native runtime cache update.
/// The consensus effects are the event, the dynamically billed init cost, and the persisted cached
/// flag. The module-hash read is retained because Nitro performs and bills it before that runtime
/// update even though its value is not part of the EVM state transition.
fn set_program_cached<J: ArbJournal>(
    state: &ArbosState,
    journal: &mut MeteredJournal<'_, J>,
    gas_limit: u64,
    code_hash: B256,
    cached: bool,
    code_present: bool,
    manager: Address,
    precompile_address: Address,
    timestamp: u64,
) -> InterpreterResult {
    // Nitro's `Programs.Params()` makes several physical storage reads, but explicitly bills
    // them as one warm computation read (100 gas). Bypass the wrapper's normal 800-gas slot
    // charge and account for that logical operation directly.
    let params_word = match state.programs.read_params_word(journal.inner_mut()) {
        Ok(word) => word,
        Err(error) => {
            return fatal_result(
                gas_limit,
                &format!("ArbWasmCache: params read error: {error}"),
            );
        }
    };
    journal.charge(100);
    let params_version = u16::from_be_bytes([params_word[0], params_word[1]]);
    let expiry_days = u64::from(u16::from_be_bytes([params_word[19], params_word[20]]));
    let mut program = match state.programs.read_program(code_hash, journal) {
        Ok(program) => program,
        Err(error) => {
            return fatal_result(
                gas_limit,
                &format!("ArbWasmCache: program read error: {error}"),
            );
        }
    };
    let activated_at = crate::storage::programs::ARBITRUM_START_TIME
        .saturating_add(u64::from(program.activated_at).saturating_mul(3600));
    let age = timestamp.saturating_sub(activated_at);
    let expired = age > expiry_days.saturating_mul(24 * 60 * 60);

    if cached && program.version != params_version {
        let args = alloy_core::sol_types::SolValue::abi_encode(&(program.version, params_version));
        return super::arb_wasm::custom_error_result(
            gas_limit,
            b"ProgramNeedsUpgrade(uint16,uint16)",
            &args,
        );
    }
    if cached && expired {
        let args = alloy_core::sol_types::SolValue::abi_encode(&(age,));
        return super::arb_wasm::custom_error_result(gas_limit, b"ProgramExpired(uint64)", &args);
    }
    if program.cached == cached {
        return ok_result(gas_limit, vec![]);
    }

    let mut manager_topic = [0_u8; 32];
    manager_topic[12..].copy_from_slice(manager.as_slice());
    journal.emit_log(Log::new_unchecked(
        precompile_address,
        vec![
            keccak256(UPDATE_PROGRAM_CACHE_EVENT_SIGNATURE),
            B256::from(manager_topic),
            code_hash,
        ],
        Bytes::from(alloy_core::sol_types::SolValue::abi_encode(&(cached,))),
    ));

    // Nitro burns the program's dynamic init cost through its storage-access burner before
    // loading the module hash and updating the persisted cache flag.
    journal.charge(u64::from(program.init_cost));
    if let Err(error) = state
        .programs
        .module_hashes
        .get_u256(U256::from_be_bytes(code_hash.0), journal)
    {
        return fatal_result(
            gas_limit,
            &format!("ArbWasmCache: module hash read error: {error}"),
        );
    }
    // Nitro's `cacheProgram` now resolves the bytecode from its unmetered code reader. We have
    // already loaded exactly that code through the no-warming journal helper; preserve the same
    // ordinary failure before changing the persisted cache bit.
    if cached && !code_present {
        return ordinary_error_result(gas_limit);
    }
    program.cached = cached;
    if let Err(error) = state.programs.write_program(code_hash, &program, journal) {
        return fatal_result(
            gas_limit,
            &format!("ArbWasmCache: program write error: {error}"),
        );
    }
    ok_result(gas_limit, vec![])
}

#[cfg(test)]
mod tests {
    use alloy_core::sol_types::{SolCall, SolValue};
    use revm::{
        context_interface::{ContextTr, JournalTr},
        database_interface::EmptyDB,
        interpreter::InstructionResult,
        primitives::{Address, B256, Bytes, U256, address, keccak256},
        state::Bytecode,
    };

    use super::{ArbPrecompilesEnum, ArbWasmCache};
    use crate::{
        api::default_ctx::{ArbContext, DefaultArb},
        arb_journal::{ArbCall, ArbJournal},
        arbos_init::{ArbosInitConfig, initialize_arbos_state},
        storage::{
            ArbosState,
            programs::{ARBITRUM_START_TIME, ProgramInfo},
        },
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

    #[test]
    fn cache_program_updates_active_program_and_charges_codehash_access() {
        let mut ctx = ctx();
        let program = Address::with_last_byte(0x42);
        let code = Bytes::from_static(&[0x60, 0x00, 0x56]);
        let codehash = keccak256(&code);
        ctx.journal_mut()
            .load_account(program)
            .expect("load program account");
        ctx.journal_mut().set_code(program, Bytecode::new_raw(code));

        let params_word = ArbosState::open()
            .programs
            .read_params_word(ctx.journal_mut())
            .expect("read Stylus params");
        ArbosState::open()
            .programs
            .write_program(
                codehash,
                &ProgramInfo {
                    version: u16::from_be_bytes([params_word[0], params_word[1]]),
                    init_cost: 500,
                    ..Default::default()
                },
                ctx.journal_mut(),
            )
            .expect("write active program");

        let input = ArbWasmCache::cacheProgramCall { program }.abi_encode();
        let call = ArbCall {
            input: &input,
            gas_limit: 100_000,
            // The initialized chain owner is authorized in Nitro and in the local precompile.
            caller: Address::ZERO,
            value: U256::ZERO,
            bytecode_address: ARB_WASM_CACHE,
            acting_address: ARB_WASM_CACHE,
            is_static: false,
        };
        let result = ArbPrecompilesEnum::ArbWasmCache.run_dispatch(&mut ctx, &call);

        assert_eq!(result.result, InstructionResult::Return);
        assert!(
            ArbosState::open()
                .programs
                .read_program(codehash, ctx.journal_mut())
                .expect("read updated program")
                .cached
        );
        // This covers the otherwise easy-to-miss `Storage.GetCodeHash` burner charge (2,600)
        // and the 100-gas logical `Programs.Params` read used by Nitro.
        assert_eq!(result.gas.total_gas_spent(), 28_959);
    }

    #[test]
    fn cache_program_rejects_missing_code_without_mutating_cache_state() {
        let mut ctx = ctx();
        let program = Address::with_last_byte(0x43);
        let (codehash, code) = ctx
            .journal_mut()
            .account_code_hash_and_code(program)
            .expect("read absent program account");
        assert!(code.is_empty());
        let params_word = ArbosState::open()
            .programs
            .read_params_word(ctx.journal_mut())
            .expect("read Stylus params");
        ArbosState::open()
            .programs
            .write_program(
                codehash,
                &ProgramInfo {
                    version: u16::from_be_bytes([params_word[0], params_word[1]]),
                    ..Default::default()
                },
                ctx.journal_mut(),
            )
            .expect("write active program");

        let input = ArbWasmCache::cacheProgramCall { program }.abi_encode();
        let call = ArbCall {
            input: &input,
            gas_limit: 100_000,
            caller: Address::ZERO,
            value: U256::ZERO,
            bytecode_address: ARB_WASM_CACHE,
            acting_address: ARB_WASM_CACHE,
            is_static: false,
        };
        let result = ArbPrecompilesEnum::ArbWasmCache.run_dispatch(&mut ctx, &call);

        // A plain Nitro error is normalized to an empty revert from ArbOS 11 onward.
        assert_eq!(result.result, InstructionResult::Revert);
        assert!(result.output.is_empty());
        assert!(
            !ArbosState::open()
                .programs
                .read_program(codehash, ctx.journal_mut())
                .expect("read unchanged program")
                .cached
        );
    }

    #[test]
    fn cache_program_reports_the_canonical_upgrade_error() {
        let mut ctx = ctx();
        let program = Address::with_last_byte(0x44);
        let code = Bytes::from_static(&[0x60, 0x00, 0x56]);
        let codehash = keccak256(&code);
        ctx.journal_mut()
            .load_account(program)
            .expect("load program account");
        ctx.journal_mut().set_code(program, Bytecode::new_raw(code));
        let params_word = ArbosState::open()
            .programs
            .read_params_word(ctx.journal_mut())
            .expect("read Stylus params");
        let params_version = u16::from_be_bytes([params_word[0], params_word[1]]);
        ArbosState::open()
            .programs
            .write_program(
                codehash,
                &ProgramInfo {
                    version: params_version.saturating_add(1),
                    ..Default::default()
                },
                ctx.journal_mut(),
            )
            .expect("write stale program");

        let input = ArbWasmCache::cacheProgramCall { program }.abi_encode();
        let call = ArbCall {
            input: &input,
            gas_limit: 100_000,
            caller: Address::ZERO,
            value: U256::ZERO,
            bytecode_address: ARB_WASM_CACHE,
            acting_address: ARB_WASM_CACHE,
            is_static: false,
        };
        let result = ArbPrecompilesEnum::ArbWasmCache.run_dispatch(&mut ctx, &call);

        assert_eq!(result.result, InstructionResult::Revert);
        assert_eq!(
            &result.output[..4],
            &keccak256("ProgramNeedsUpgrade(uint16,uint16)")[..4]
        );
    }

    #[test]
    fn cache_program_reports_the_canonical_expiry_error() {
        let mut ctx = ctx();
        let program = Address::with_last_byte(0x45);
        let code = Bytes::from_static(&[0x60, 0x00, 0x56]);
        let codehash = keccak256(&code);
        ctx.journal_mut()
            .load_account(program)
            .expect("load program account");
        ctx.journal_mut().set_code(program, Bytecode::new_raw(code));
        let params_word = ArbosState::open()
            .programs
            .read_params_word(ctx.journal_mut())
            .expect("read Stylus params");
        let params_version = u16::from_be_bytes([params_word[0], params_word[1]]);
        let expiry_days = u64::from(u16::from_be_bytes([params_word[19], params_word[20]]));
        ctx.block.timestamp = U256::from(
            ARBITRUM_START_TIME
                .saturating_add(expiry_days.saturating_mul(24 * 60 * 60))
                .saturating_add(1),
        );
        ArbosState::open()
            .programs
            .write_program(
                codehash,
                &ProgramInfo {
                    version: params_version,
                    activated_at: 0,
                    ..Default::default()
                },
                ctx.journal_mut(),
            )
            .expect("write expired program");

        let input = ArbWasmCache::cacheProgramCall { program }.abi_encode();
        let call = ArbCall {
            input: &input,
            gas_limit: 100_000,
            caller: Address::ZERO,
            value: U256::ZERO,
            bytecode_address: ARB_WASM_CACHE,
            acting_address: ARB_WASM_CACHE,
            is_static: false,
        };
        let result = ArbPrecompilesEnum::ArbWasmCache.run_dispatch(&mut ctx, &call);

        assert_eq!(result.result, InstructionResult::Revert);
        assert_eq!(
            &result.output[..4],
            &keccak256("ProgramExpired(uint64)")[..4]
        );
    }
}
