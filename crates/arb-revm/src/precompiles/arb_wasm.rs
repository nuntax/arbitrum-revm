use super::*;
use crate::arb_journal::{ArbCall, ArbPrecompileCtx};
use crate::storage::{programs::ARBITRUM_START_TIME, stylus_param_layout as layout, unpack_uint};
#[cfg(feature = "stylus")]
use crate::{
    arb_journal::ArbJournal,
    storage::programs::ProgramInfo,
    stylus::params::StylusParams,
    stylus::program::{stylus_activate, stylus_code},
};
use revm::interpreter::InterpreterResult;
use revm::primitives::{B256, Bytes, keccak256};
#[cfg(feature = "stylus")]
use revm::{
    interpreter::{Gas, InstructionResult},
    primitives::Log,
};

const MIN_INIT_GAS_UNITS: u64 = 128;
const MIN_CACHED_INIT_GAS_UNITS: u64 = 32;
const COST_SCALAR_PERCENT_UNITS: u64 = 2;
const ARBOS_VERSION_STYLUS: u64 = 30;
const ARBOS_VERSION_STYLUS_CHARGING_FIXES: u64 = 32;
// Only the activation path reads these, and that path is behind the `stylus` feature.
#[cfg(feature = "stylus")]
const ARBOS_VERSION_STYLUS_ACTIVATION_GAS: u64 = 59;
#[cfg(feature = "stylus")]
const COLD_ACCOUNT_READ_GAS: u64 = 2_600;
#[cfg(feature = "stylus")]
const WARM_ACCOUNT_READ_GAS: u64 = 100;
#[cfg(feature = "stylus")]
const COPY_WORD_GAS: u64 = 3;
/// Nitro charges this once when `Programs.Params()` reads the packed Stylus parameter word.
const PARAMS_WARM_READ_GAS: u64 = 100;
/// Nitro's ArbOS storage abstraction charges the pre-EIP-2929 SLOAD price for a program record.
const PROGRAM_READ_GAS: u64 = 800;
/// Reading the configurable activation-gas slot uses Nitro's pre-EIP-2929 storage price.
#[cfg(feature = "stylus")]
const ACTIVATION_GAS_READ_GAS: u64 = 800;

/// Fixed up-front computation burn ArbWasm.ActivateProgram charges (Nitro `ArbWasm.go`).
#[cfg(feature = "stylus")]
const ACTIVATION_FIXED_GAS: u64 = 1_659_168;
/// ArbOS-storage + warm-read gas Nitro's `ActivateProgram` burns through the programs burner for a
/// fresh, classic (pre-v60), non-cached program at ArbOS 30-59: getProgram read (800) + dataPricer
/// 5 reads (4000) + NetworkFeeAccount read (800) + 4 writes at 20000 each (moduleHash, demand,
/// lastUpdateTime, program). The shared Params warm-read is charged by `run_arb_wasm`.
#[cfg(feature = "stylus")]
const ACTIVATION_STORAGE_GAS: u64 = 800 + 4_000 + 800 + 4 * 20_000;
/// `ProgramActivated(bytes32,bytes32,address,uint256,uint16)` event signature.
#[cfg(feature = "stylus")]
const PROGRAM_ACTIVATED_EVENT_SIGNATURE: &[u8] =
    b"ProgramActivated(bytes32,bytes32,address,uint256,uint16)";
/// LOG gas Nitro burns emitting `ProgramActivated`: LogGas(375) + 2 topics * LogTopicGas(375) +
/// 128 data bytes * LogDataGas(8) = 2149.
#[cfg(feature = "stylus")]
const ACTIVATION_EVENT_GAS: u64 = 375 + 2 * 375 + 128 * 8;

pub(super) fn run_arb_wasm<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
    call_inputs: &ArbCall,
) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let call = match ArbWasm::ArbWasmCalls::abi_decode(input) {
        Ok(c) => c,
        Err(_) => return gated_revert_result(gas_limit),
    };

    let state = ArbosState::open();
    let arbos_version = match state.arbos_version.get(ctx.journal_mut()) {
        Ok(v) => v,
        Err(e) => return fatal_result(gas_limit, &format!("ArbWasm: storage error: {e}")),
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

    let result = match call {
        ArbWasm::ArbWasmCalls::stylusVersion(_) => {
            let v = unpack_uint(&word, layout::VERSION.0, layout::VERSION.1) as u16;
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(v,)),
            )
        }
        ArbWasm::ArbWasmCalls::inkPrice(_) => {
            let v = unpack_uint(&word, layout::INK_PRICE.0, layout::INK_PRICE.1);
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(v,)),
            )
        }
        ArbWasm::ArbWasmCalls::maxStackDepth(_) => {
            let v = unpack_uint(&word, layout::MAX_STACK_DEPTH.0, layout::MAX_STACK_DEPTH.1);
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(v,)),
            )
        }
        ArbWasm::ArbWasmCalls::freePages(_) => {
            let v = unpack_uint(&word, layout::FREE_PAGES.0, layout::FREE_PAGES.1) as u16;
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(v,)),
            )
        }
        ArbWasm::ArbWasmCalls::pageGas(_) => {
            let v = unpack_uint(&word, layout::PAGE_GAS.0, layout::PAGE_GAS.1) as u16;
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(v,)),
            )
        }
        ArbWasm::ArbWasmCalls::pageRamp(_) => {
            // PageRamp is NOT stored in the packed word.  Nitro initialises the
            // struct field with the constant and never persists it.
            let v = layout::PAGE_RAMP_CONSTANT;
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(v,)),
            )
        }
        ArbWasm::ArbWasmCalls::pageLimit(_) => {
            let v = unpack_uint(&word, layout::PAGE_LIMIT.0, layout::PAGE_LIMIT.1) as u16;
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(v,)),
            )
        }
        ArbWasm::ArbWasmCalls::minInitGas(_) => {
            if arbos_version < ARBOS_VERSION_STYLUS_CHARGING_FIXES {
                revert_result(
                    gas_limit,
                    "ArbWasm: minInitGas unavailable before charging fixes",
                )
            } else {
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
        }
        ArbWasm::ArbWasmCalls::initCostScalar(_) => {
            let units = unpack_uint(
                &word,
                layout::INIT_COST_SCALAR.0,
                layout::INIT_COST_SCALAR.1,
            ) as u64;
            let v = units.saturating_mul(COST_SCALAR_PERCENT_UNITS);
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(v,)),
            )
        }
        ArbWasm::ArbWasmCalls::expiryDays(_) => {
            let v = unpack_uint(&word, layout::EXPIRY_DAYS.0, layout::EXPIRY_DAYS.1) as u16;
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(v,)),
            )
        }
        ArbWasm::ArbWasmCalls::keepaliveDays(_) => {
            let v = unpack_uint(&word, layout::KEEPALIVE_DAYS.0, layout::KEEPALIVE_DAYS.1) as u16;
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(v,)),
            )
        }
        ArbWasm::ArbWasmCalls::blockCacheSize(_) => {
            let v = unpack_uint(
                &word,
                layout::BLOCK_CACHE_SIZE.0,
                layout::BLOCK_CACHE_SIZE.1,
            ) as u16;
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(v,)),
            )
        }
        ArbWasm::ArbWasmCalls::codehashVersion(c) => {
            codehash_version(ctx, gas_limit, &word, c.codehash)
        }
        ArbWasm::ArbWasmCalls::activateProgram(_c) => {
            #[cfg(feature = "stylus")]
            {
                activate_program(
                    ctx,
                    call_inputs,
                    gas_limit,
                    arbos_version,
                    &word,
                    _c.program,
                )
            }
            #[cfg(not(feature = "stylus"))]
            {
                let _ = call_inputs;
                revert_result(gas_limit, "ArbWasm: activation requires the stylus feature")
            }
        }
        ArbWasm::ArbWasmCalls::codehashAsmSize(_)
        | ArbWasm::ArbWasmCalls::programVersion(_)
        | ArbWasm::ArbWasmCalls::programInitGas(_)
        | ArbWasm::ArbWasmCalls::programMemoryFootprint(_)
        | ArbWasm::ArbWasmCalls::programTimeLeft(_)
        | ArbWasm::ArbWasmCalls::codehashKeepalive(_) => revert_result(
            gas_limit,
            "ArbWasm: per-program queries not yet implemented",
        ),
    };
    charge_result(result, PARAMS_WARM_READ_GAS)
}

fn charge_result(mut result: InterpreterResult, cost: u64) -> InterpreterResult {
    if !result.gas.record_regular_cost(cost) {
        result.result = InstructionResult::OutOfGas;
        result.output = Bytes::new();
    }
    result
}

fn custom_error_result(gas_limit: u64, signature: &[u8], args: &[u8]) -> InterpreterResult {
    let selector = keccak256(signature);
    let mut output = Vec::with_capacity(4 + args.len());
    output.extend_from_slice(&selector[..4]);
    output.extend_from_slice(args);
    InterpreterResult {
        result: InstructionResult::Revert,
        output: Bytes::from(output),
        gas: Gas::new(gas_limit),
    }
}

fn codehash_version<CTX>(
    ctx: &mut CTX,
    gas_limit: u64,
    params_word: &[u8; 32],
    code_hash: B256,
) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let state = ArbosState::open();
    let params_version = unpack_uint(params_word, layout::VERSION.0, layout::VERSION.1) as u16;
    let program = match state.programs.read_program(code_hash, ctx.journal_mut()) {
        Ok(program) => program,
        Err(error) => {
            return fatal_result(gas_limit, &format!("ArbWasm: program read error: {error}"));
        }
    };
    let result = if program.version == 0 {
        custom_error_result(gas_limit, b"ProgramNotActivated()", &[])
    } else if program.version != params_version {
        let args = alloy_core::sol_types::SolValue::abi_encode(&(program.version, params_version));
        custom_error_result(gas_limit, b"ProgramNeedsUpgrade(uint16,uint16)", &args)
    } else {
        let activated_at = ARBITRUM_START_TIME
            .saturating_add(u64::from(program.activated_at).saturating_mul(3600));
        let age = ctx.block_timestamp().saturating_sub(activated_at);
        let expiry_days = u64::from(unpack_uint(
            params_word,
            layout::EXPIRY_DAYS.0,
            layout::EXPIRY_DAYS.1,
        ));
        let expiry = expiry_days.saturating_mul(24 * 60 * 60);
        if age > expiry {
            let args = alloy_core::sol_types::SolValue::abi_encode(&(age,));
            custom_error_result(gas_limit, b"ProgramExpired(uint64)", &args)
        } else {
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(program.version,)),
            )
        }
    };
    charge_result(result, PROGRAM_READ_GAS)
}

/// `ArbWasm.activateProgram(address)`: compile+instrument the program's WASM, charge the activation
/// gas + data fee, write the activation metadata (program record + module hash), and emit
/// `ProgramActivated`. Byte-for-byte port of Nitro `ArbWasm.ActivateProgram` +
/// `programs.ActivateProgram` for the classic (pre-v60), non-cached path.
#[cfg(feature = "stylus")]
fn activate_program<CTX>(
    ctx: &mut CTX,
    call_inputs: &ArbCall,
    gas_limit: u64,
    arbos_version: u64,
    params_word: &[u8; 32],
    program: Address,
) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let params = StylusParams::from_word(params_word);
    let state = ArbosState::open();

    // ArbOS 59 introduced a configurable activation burn. Reading its storage-backed value also
    // costs one ArbOS storage read, even when the configured value is zero.
    let configured_activation_gas = if arbos_version >= ARBOS_VERSION_STYLUS_ACTIVATION_GAS {
        match state.programs.activation_gas.get(ctx.journal_mut()) {
            Ok(gas) => gas,
            Err(e) => {
                return revert_result(
                    gas_limit,
                    &format!("ArbWasm: activation gas read error: {e}"),
                );
            }
        }
    } else {
        0
    };

    // Program bytecode + its code hash (Nitro statedb.GetCode / GetCodeHash, not burned here).
    let code = match ctx.journal_mut().account_code(program) {
        Ok(c) => c,
        Err(e) => return revert_result(gas_limit, &format!("ArbWasm: code read error: {e}")),
    };
    let code_hash = keccak256(&code);

    // Reject re-activation of an already up-to-date program (Nitro ProgramUpToDateError).
    let existing = match state.programs.read_program(code_hash, ctx.journal_mut()) {
        Ok(p) => p,
        Err(e) => return revert_result(gas_limit, &format!("ArbWasm: program read error: {e}")),
    };
    if existing.version == params.version && existing.activated_at != 0 {
        return revert_result(gas_limit, "ArbWasm: program already activated");
    }

    // Charge the fixed + storage gas up front, then let stylus_activate burn the variable
    // instrumentation gas out of the remainder (Nitro `activateProgram` burns suppliedGas-gasLeft).
    let mut gas = Gas::new(gas_limit);
    let activation_gas_read = activation_gas_read_cost(arbos_version);
    if !gas.record_regular_cost(
        activation_gas_read
            .saturating_add(configured_activation_gas)
            .saturating_add(ACTIVATION_FIXED_GAS)
            .saturating_add(ACTIVATION_STORAGE_GAS),
    ) {
        return InterpreterResult {
            result: InstructionResult::OutOfGas,
            output: Bytes::new(),
            gas: Gas::new(gas_limit),
        };
    }

    // Root programs load their fragment contracts from state. Activation alone charges these
    // reads: reserve enough gas for a maximum-sized fragment before touching state, then burn the
    // actual cold/warm account access plus three gas per copied 32-byte word.
    let max_code_size = ctx.max_code_size() as u64;
    let mut fragment_oog = false;
    let wasm = match stylus_code(
        &code,
        arbos_version,
        params.max_wasm_size(arbos_version),
        params.max_fragment_count,
        true,
        |address| {
            let is_cold = ctx
                .journal_mut()
                .account_is_cold(address)
                .map_err(|error| format!("fragment access error: {error}").into_bytes())?;
            let maximum_cost = fragment_read_gas_cost(is_cold, max_code_size);
            if gas.remaining() < maximum_cost {
                gas.spend_all();
                fragment_oog = true;
                return Err(b"out of gas reading Stylus fragment".to_vec());
            }
            let loaded = ctx
                .journal_mut()
                .account_code_load(address)
                .map_err(|error| format!("fragment code read error: {error}").into_bytes())?;
            let cost = fragment_read_gas_cost(loaded.is_cold, loaded.data.len() as u64);
            if !gas.record_regular_cost(cost) {
                fragment_oog = true;
                return Err(b"out of gas reading Stylus fragment".to_vec());
            }
            Ok(loaded.data)
        },
    ) {
        Ok(Some(wasm)) => wasm,
        Ok(None) => {
            return InterpreterResult {
                result: InstructionResult::Revert,
                output: Bytes::from_static(b"ArbWasm: program is not a Stylus program"),
                gas,
            };
        }
        Err(err) => {
            return InterpreterResult {
                result: if fragment_oog {
                    InstructionResult::OutOfGas
                } else {
                    InstructionResult::Revert
                },
                output: if fragment_oog {
                    Bytes::new()
                } else {
                    Bytes::from(err)
                },
                gas,
            };
        }
    };

    let debug = state.debug_mode(ctx.journal_mut());
    let (module, stylus_data) = match stylus_activate(
        Some(&mut gas),
        &wasm,
        code_hash,
        arbos_version as u16,
        params.version,
        params.page_limit,
        debug,
    ) {
        Ok(v) => v,
        Err(err) => {
            // Nitro takes all gas on a failed activation (BurnOut).
            let mut gas = Gas::new(gas_limit);
            gas.spend_all();
            return InterpreterResult {
                result: InstructionResult::Revert,
                output: Bytes::from(err.into_bytes()),
                gas,
            };
        }
    };
    let module_hash = B256::from(module.hash().0);
    let time = ctx.block_timestamp();

    // Data fee for the estimated asm size (advances + persists the demand model).
    let data_fee =
        match state
            .programs
            .update_data_model(stylus_data.asm_estimate, time, ctx.journal_mut())
        {
            Ok(f) => f,
            Err(e) => return revert_result(gas_limit, &format!("ArbWasm: data pricer error: {e}")),
        };

    // Persist the module hash + program activation record.
    if let Err(e) = state
        .programs
        .write_module_hash(code_hash, module_hash, ctx.journal_mut())
    {
        return revert_result(gas_limit, &format!("ArbWasm: module hash write error: {e}"));
    }
    let activated_at = ((time.saturating_sub(ARBITRUM_START_TIME)) / 3600).min(0x00FF_FFFF) as u32;
    let asm_estimate_kb = stylus_data.asm_estimate.div_ceil(1024).min(0x00FF_FFFF);
    let info = ProgramInfo {
        version: params.version,
        init_cost: stylus_data.init_cost,
        cached_cost: stylus_data.cached_init_cost,
        footprint: stylus_data.footprint,
        activated_at,
        asm_estimate_kb,
        cached: false,
    };
    if let Err(e) = state
        .programs
        .write_program(code_hash, &info, ctx.journal_mut())
    {
        return revert_result(gas_limit, &format!("ArbWasm: program write error: {e}"));
    }

    // Pay the data fee: the caller must have sent value >= dataFee; the fee goes to the network fee
    // account and the remainder is refunded (Nitro payActivationDataFee).
    let value = call_inputs.value;
    let arb_wasm_addr = call_inputs.bytecode_address;
    if value < data_fee {
        return revert_result(
            gas_limit,
            "ArbWasm: insufficient value for activation data fee",
        );
    }
    let network = match state.network_fee_account.get(ctx.journal_mut()) {
        Ok(a) => a,
        Err(e) => return revert_result(gas_limit, &format!("ArbWasm: network fee account: {e}")),
    };
    match ctx.journal_mut().transfer(arb_wasm_addr, network, data_fee) {
        Ok(None) => {}
        Ok(Some(_)) | Err(_) => {
            return revert_result(gas_limit, "ArbWasm: activation fee transfer failed");
        }
    }
    let repay = value - data_fee;
    match ctx
        .journal_mut()
        .transfer(arb_wasm_addr, call_inputs.caller, repay)
    {
        Ok(None) => {}
        Ok(Some(_)) | Err(_) => {
            return revert_result(gas_limit, "ArbWasm: activation refund transfer failed");
        }
    }

    // ProgramActivated(codehash indexed, moduleHash, program, dataFee, version)
    let data = alloy_core::sol_types::SolValue::abi_encode(&(
        module_hash,
        program,
        data_fee,
        params.version,
    ));
    ctx.journal_mut().emit_log(Log::new_unchecked(
        arb_wasm_addr,
        vec![keccak256(PROGRAM_ACTIVATED_EVENT_SIGNATURE), code_hash],
        Bytes::from(data),
    ));
    // Charge the event's LOG gas (Nitro burns it through the precompile burner on emit).
    let _ = gas.record_regular_cost(ACTIVATION_EVENT_GAS);

    InterpreterResult {
        result: InstructionResult::Return,
        output: Bytes::from(alloy_core::sol_types::SolValue::abi_encode(&(
            params.version,
            data_fee,
        ))),
        gas,
    }
}

#[cfg(feature = "stylus")]
fn fragment_read_gas_cost(is_cold: bool, code_size: u64) -> u64 {
    let access = if is_cold {
        COLD_ACCOUNT_READ_GAS
    } else {
        WARM_ACCOUNT_READ_GAS
    };
    access.saturating_add(code_size.div_ceil(32).saturating_mul(COPY_WORD_GAS))
}

#[cfg(feature = "stylus")]
const fn activation_gas_read_cost(arbos_version: u64) -> u64 {
    if arbos_version >= ARBOS_VERSION_STYLUS_ACTIVATION_GAS {
        ACTIVATION_GAS_READ_GAS
    } else {
        0
    }
}

#[cfg(all(test, feature = "stylus"))]
mod tests {
    use alloy_core::sol_types::SolCall;
    use revm::{
        context_interface::{ContextTr, JournalTr},
        database_interface::EmptyDB,
        interpreter::InstructionResult,
        primitives::{Address, U256, address},
        state::Bytecode,
    };

    use super::{
        ACTIVATION_FIXED_GAS, ACTIVATION_STORAGE_GAS, ArbWasm, activation_gas_read_cost,
        fragment_read_gas_cost, run_arb_wasm,
    };
    use crate::{
        api::default_ctx::{ArbContext, DefaultArb},
        arb_journal::ArbCall,
        arbos_init::{ArbosInitConfig, initialize_arbos_state},
        stylus::constants::STYLUS_ROOT_DISCRIMINANT,
    };

    const PROGRAM: Address = address!("00000000000000000000000000000000deadbeef");
    const FRAGMENT: Address = address!("000000000000000000000000000000000000f00d");
    const ARB_WASM: Address = address!("0000000000000000000000000000000000000071");

    /// Everything ArbWasm.activateProgram charges before it reaches the first fragment read.
    const UPFRONT_GAS: u64 = ACTIVATION_FIXED_GAS + ACTIVATION_STORAGE_GAS + 800;

    fn ctx_with_root_program(arbos_version: u64) -> ArbContext<EmptyDB> {
        let mut ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();
        initialize_arbos_state(
            &ArbosInitConfig {
                initial_arbos_version: arbos_version,
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

        // A root program naming one fragment contract. The fragment's code is never reached in
        // the out-of-gas case, which is the point: the reservation happens first.
        let mut root = STYLUS_ROOT_DISCRIMINANT.to_vec();
        root.push(0);
        root.extend_from_slice(&64u32.to_be_bytes());
        root.extend_from_slice(FRAGMENT.as_slice());
        // `set_code` assumes the account is already loaded.
        ctx.journal_mut()
            .load_account(PROGRAM)
            .expect("load program");
        ctx.journal_mut()
            .set_code(PROGRAM, Bytecode::new_raw(root.into()));
        ctx
    }

    fn activate(
        ctx: &mut ArbContext<EmptyDB>,
        gas_limit: u64,
    ) -> revm::interpreter::InterpreterResult {
        let input = ArbWasm::activateProgramCall { program: PROGRAM }.abi_encode();
        let call = ArbCall {
            input: &input,
            gas_limit,
            caller: Address::ZERO,
            value: U256::ZERO,
            bytecode_address: ARB_WASM,
            acting_address: ARB_WASM,
            is_static: false,
        };
        run_arb_wasm(ctx, &input, gas_limit, &call)
    }

    /// Nitro's `canReadNewFragment` refuses to touch state unless the caller can afford a
    /// maximum-sized fragment, and an activation that cannot is out of gas rather than a revert:
    /// it consumes everything and returns no message.
    #[test]
    fn fragment_read_reservation_runs_out_of_gas_before_touching_state() {
        let mut ctx = ctx_with_root_program(61);
        // Enough for the fixed activation charges, but well under a cold max-sized fragment read.
        let gas_limit = UPFRONT_GAS + 1_000;
        let reservation = fragment_read_gas_cost(true, 24_576);
        assert!(reservation > 1_000, "reservation should exceed the surplus");

        let result = activate(&mut ctx, gas_limit);

        assert_eq!(result.result, InstructionResult::OutOfGas);
        assert!(result.output.is_empty(), "out of gas carries no message");
        assert_eq!(result.gas.remaining(), 0, "out of gas consumes everything");
    }

    /// The same activation one gas below the fixed charges fails earlier, proving the previous
    /// case really did get past them and fail on the fragment reservation.
    ///
    /// The two paths report gas differently: the fixed-charge path returns the unspent remainder
    /// because `record_regular_cost` leaves it alone on failure, while the fragment reservation
    /// spends it explicitly. Neither is observable, since revm consumes all gas for any
    /// `OutOfGas` result, but the difference is what separates the two failures here.
    #[test]
    fn activation_below_the_fixed_charges_fails_before_the_fragment_reservation() {
        let mut ctx = ctx_with_root_program(61);
        let result = activate(&mut ctx, UPFRONT_GAS - 1);

        assert_eq!(result.result, InstructionResult::OutOfGas);
        assert!(
            result.gas.remaining() > 0,
            "failed on the fixed charges, not the fragment reservation",
        );
    }

    #[test]
    fn fragment_read_gas_uses_access_temperature_and_copy_words() {
        assert_eq!(fragment_read_gas_cost(true, 0), 2_600);
        assert_eq!(fragment_read_gas_cost(false, 0), 100);
        assert_eq!(fragment_read_gas_cost(true, 1), 2_603);
        assert_eq!(fragment_read_gas_cost(false, 33), 106);
    }

    #[test]
    fn configurable_activation_gas_read_starts_at_arbos_59() {
        assert_eq!(activation_gas_read_cost(58), 0);
        assert_eq!(activation_gas_read_cost(59), 800);
        assert_eq!(activation_gas_read_cost(61), 800);
    }
}
