//! Stylus frame dispatch: detect a Stylus program in the current call frame and run it.
//!
//! Ties the Stylus modules together on [`ArbEvm`]: extract the call inputs from the frame,
//! fetch/compile/activate the program, charge init/page gas, build the hostio bridge, run
//! the WASM, and return the result as an [`InterpreterAction`]. [`ArbEvm::frame_run`] calls
//! [`ArbEvm::frame_run_stylus`] when the frame's bytecode carries the Stylus discriminant.

use std::{cmp::min, mem, sync::Arc};

use arbutil::evm::{
    api::{EvmApiMethod, EvmApiStatus, Gas as ArbGas, VecReader},
    req::EvmApiRequestor,
};
use revm::{
    Database,
    context::{ContextError, FrameStack},
    context_interface::{Cfg, ContextTr, JournalTr},
    handler::{
        EthFrame, EvmTr, FrameResult, ItemOrResult, PrecompileProvider,
        instructions::InstructionProvider,
    },
    interpreter::{
        CallInput, CallInputs, CallScheme, CallValue, CreateInputs, CreateScheme, FrameInput, Gas,
        InstructionResult, InterpreterAction, InterpreterResult, interpreter::EthInterpreter,
        interpreter_action::FrameInit,
    },
    primitives::{Address, Bytes, KECCAK_EMPTY, U256},
    state::Bytecode,
};
use stylus::prover::programs::config::{CompileConfig, StylusConfig};

use crate::{
    api::exec::ArbContextTr,
    evm::ArbEvm,
    storage::ArbosState,
    stylus::{
        api::{HostCallFunc, StylusHandler, handle_request},
        executor::{build_evm_data, run_program},
        gas::{cached_gas_cost, init_gas_cost, stylus_call_cost},
        params::StylusParams,
        program::{PROGRAM_CACHE, stylus_activate, stylus_code, stylus_compile},
    },
};

impl<CTX, INSP, I, P> ArbEvm<CTX, INSP, I, P, EthFrame<EthInterpreter>>
where
    CTX: ArbContextTr,
    I: InstructionProvider<Context = CTX, InterpreterTypes = EthInterpreter>,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    /// If the current call frame targets a Stylus program, execute it and return its result
    /// action. Returns `None` if it isn't a Stylus call (caller falls back to the EVM).
    pub(crate) fn frame_run_stylus(&mut self) -> Option<InterpreterAction> {
        // Extract the call frame inputs.
        let call = match &self.0.frame_stack.get().input {
            FrameInput::Call(call) => call.clone(),
            _ => return None,
        };
        // Calldata is owned `Bytes` for a top-level tx or a Stylus-hostio sub-call, but a
        // `SharedBuffer` range into the caller frame's shared memory when a normal EVM
        // CALL/DELEGATECALL/STATICCALL opcode targets this Stylus program. Materialize both to
        // owned bytes now, before the WASM runs; `bytes()` copies the shared-buffer slice.
        let calldata = call.input.bytes(&self.0.ctx);
        let target = call.target_address;
        let caller = call.caller;
        let value = call.value.get();
        let gas_limit = call.gas_limit;
        let is_static = call.is_static;
        let bytecode_address = call.bytecode_address;

        // All context-dependent setup. Scoped so the `&mut self.0.ctx` borrow is released
        // before `self.build_stylus_api`, which needs `&mut self` to re-enter sub-frames.
        let (serialized, compile_config, stylus_config, evm_data, gas, pages_open) = {
            let ctx = &mut self.0.ctx;

            // Bytecode + code hash of the program.
            let code_hash = ctx.journal_mut().code_hash(bytecode_address).ok()?.data;
            let bytecode = ctx.journal_mut().code(bytecode_address).ok()?.data;
            let wasm = match stylus_code(&bytecode) {
                Ok(Some(wasm)) => wasm,
                Ok(None) => return None,
                Err(err) => return Some(revert(gas_limit, err)),
            };

            // Stylus params + ArbOS version.
            let params_word = ArbosState::open()
                .programs
                .read_params_word(ctx.journal_mut())
                .ok()?;
            let params = StylusParams::from_word(&params_word);
            let arbos_version = ctx.cfg().spec().arbos_version();
            let debug = ArbosState::open().debug_mode(ctx.journal_mut());

            // Stored program metadata, Nitro's source of truth for init/page gas, set at
            // activation. We still compile/activate below for the executable module, but charge
            // gas from these stored values (re-deriving from the WASM can differ by a few units).
            let program = ArbosState::open()
                .programs
                .read_program(code_hash, ctx.journal_mut())
                .ok()?;

            // Fetch (or compile+activate, caching) the native module.
            let compile_config = CompileConfig::version(params.version, debug);
            let serialized = {
                let mut cache = PROGRAM_CACHE.lock().unwrap();
                match cache.try_get_or_insert::<_, String>(code_hash, || {
                    let serialized = stylus_compile(&wasm, &compile_config)?;
                    let (module, data) = stylus_activate(
                        None,
                        &wasm,
                        code_hash,
                        arbos_version as u16,
                        params.version,
                        params.page_limit,
                        debug,
                    )?;
                    Ok((serialized, module, data))
                }) {
                    Ok((serialized, _module, _data)) => serialized.clone(),
                    Err(err) => return Some(revert(gas_limit, err.into_bytes())),
                }
            };

            // Charge page + init/cached gas before running, from the stored program info.
            // Per Nitro programs.go `CallProgram`: for a cached program OR Stylus version > 1,
            // the cached-init cost is charged; for a non-cached program the init cost is charged
            // too (version 1 folded cached into init). recentWasmsCacheHit is ArbOS >= 60 only.
            let mut gas = Gas::new(gas_limit);
            // Stylus memory model: price page growth against the tx's current open/ever pages,
            // then add this program's footprint (Nitro statedb AddStylusPages). `open` is
            // restored after the run below (Nitro's deferred SetStylusPagesOpen); `ever` persists.
            let pages_open = ctx.chain().stylus_pages_open;
            let pages_ever = ctx.chain().stylus_pages_ever;
            let page_cost = stylus_call_cost(
                program.footprint,
                pages_open,
                pages_ever,
                params.free_pages,
                params.page_gas,
            );
            let new_open = pages_open.saturating_add(program.footprint);
            ctx.chain_mut().stylus_pages_open = new_open;
            ctx.chain_mut().stylus_pages_ever = pages_ever.max(new_open);
            let mut init_cost = 0u64;
            if program.cached || program.version > 1 {
                init_cost += cached_gas_cost(
                    program.cached_cost,
                    params.min_cached_init_gas,
                    params.cached_cost_scalar,
                );
            }
            if !program.cached {
                init_cost += init_gas_cost(
                    program.init_cost,
                    params.min_init_gas,
                    params.init_cost_scalar,
                );
            }
            if !gas.record_regular_cost(page_cost.saturating_add(init_cost)) {
                return Some(InterpreterAction::Return(InterpreterResult {
                    result: InstructionResult::OutOfGas,
                    output: Bytes::new(),
                    gas: Gas::new(gas_limit),
                }));
            }

            // TODO(parity): module_hash should be the activated module's hash, not the code hash.
            let evm_data = build_evm_data(ctx, target, caller, value, code_hash, 0, program.cached);
            let stylus_config =
                StylusConfig::new(params.version, params.max_stack_depth, params.ink_price);
            (serialized, compile_config, stylus_config, evm_data, gas, pages_open)
        };

        // Build the hostio bridge capturing the whole EVM (so call/create hostios can re-enter
        // sub-frames), then run the WASM synchronously.
        let evm_api = self.build_stylus_api(target, caller, is_static);
        // Isolate this Stylus frame's EVM-sub-call refunds: reset the accumulator before the run
        // and restore the parent's afterward, so nested Stylus frames don't double-count.
        let saved_refund = self.0.ctx.chain().stylus_sub_refund;
        self.0.ctx.chain_mut().stylus_sub_refund = 0;
        let mut result = run_program(
            &serialized,
            compile_config,
            stylus_config,
            evm_api,
            evm_data,
            &calldata,
            gas,
        );
        // Fold the refunds accrued by this frame's call/create hostios onto the result gas so they
        // reach the tx (revm's `frame_return` does this for ordinary frames, but the Stylus hostio
        // ran its sub-frames out-of-band via `run_exec_loop`). Then restore the parent accumulator.
        let frame_sub_refund = self.0.ctx.chain().stylus_sub_refund;
        self.0.ctx.chain_mut().stylus_sub_refund = saved_refund;
        // Nitro keeps refunds in StateDB's journal, so reverting or halting the enclosing
        // Stylus call also reverts refunds produced by otherwise-successful EVM sub-calls.
        // Our out-of-band sub-frame accumulator is not journaled, so enforce that rollback here.
        if result.result.is_ok() {
            result.gas.record_refund(frame_sub_refund);
        }
        // Restore the open-pages high-water to its pre-call value (Nitro's deferred
        // SetStylusPagesOpen); the `ever` mark set during the run persists across the tx.
        self.0.ctx.chain_mut().stylus_pages_open = pages_open;
        Some(InterpreterAction::Return(result))
    }

    /// Builds the Stylus hostio bridge for a call executing as `contract` (entered by
    /// `caller`, `is_static` if in a static context), capturing the whole EVM so that the
    /// call/create hostios can synchronously re-enter sub-frames. State hostios go straight to
    /// [`handle_request`] against the context.
    ///
    /// # Safety mirror
    /// The returned requestor holds a raw `*mut Self`; it is sound only because the Stylus
    /// program runs synchronously within the frame that owns `self`, driven by
    /// [`run_program`], and dropped before control returns to the caller.
    fn build_stylus_api(
        &mut self,
        contract: Address,
        caller: Address,
        is_static: bool,
    ) -> EvmApiRequestor<VecReader, StylusHandler> {
        let evm_ptr: *mut Self = self;
        let callback = move |req_type: EvmApiMethod, req_data: Vec<u8>| {
            // SAFETY: synchronous, unaliased execution within the owning frame (see above).
            let evm = unsafe { &mut *evm_ptr };
            match req_type {
                EvmApiMethod::ContractCall
                | EvmApiMethod::DelegateCall
                | EvmApiMethod::StaticCall => {
                    evm.handle_stylus_call(contract, caller, is_static, req_type, req_data)
                }
                EvmApiMethod::Create1 | EvmApiMethod::Create2 => {
                    evm.handle_stylus_create(contract, is_static, req_type, req_data)
                }
                _ => handle_request(&mut evm.0.ctx, contract, req_type, req_data),
            }
        };
        // Erase the borrowed lifetime to 'static (sound under the synchronous-run contract).
        let callback: Arc<
            Box<dyn Fn(EvmApiMethod, Vec<u8>) -> (Vec<u8>, VecReader, ArbGas) + '_>,
        > = Arc::new(Box::new(callback));
        let callback: Arc<Box<HostCallFunc>> = unsafe { mem::transmute(callback) };
        EvmApiRequestor::new(StylusHandler::new(callback))
    }

    /// Runs a freshly-initialized sub-frame to completion, recursing through `frame_run`
    /// (which re-dispatches nested Stylus calls). Mirrors revm's `Handler::run_exec_loop`.
    fn run_exec_loop(
        &mut self,
        first_frame_input: FrameInit,
    ) -> Result<FrameResult, ContextError<<<CTX as ContextTr>::Db as Database>::Error>> {
        if let ItemOrResult::Result(result) = self.frame_init(first_frame_input)? {
            return Ok(result);
        }
        loop {
            let result = match self.frame_run()? {
                ItemOrResult::Item(init) => match self.frame_init(init)? {
                    ItemOrResult::Item(_) => continue,
                    ItemOrResult::Result(result) => result,
                },
                ItemOrResult::Result(result) => result,
            };
            if let Some(result) = self.frame_return_result(result)? {
                return Ok(result);
            }
        }
    }

    /// Stylus `ContractCall`/`DelegateCall`/`StaticCall` hostio: run a revm sub-call frame to
    /// completion and return `(status, return-data, gas-spent)`. Wire format (arbutil
    /// `EvmApiRequestor`): `bytecode_addr(20) value(32) gas_left(8) gas_req(8) calldata`.
    /// Grounded in Nitro's call semantics; frame mechanics mirror arbos-revm on revm 36.
    fn handle_stylus_call(
        &mut self,
        contract: Address,
        parent_caller: Address,
        parent_is_static: bool,
        req_type: EvmApiMethod,
        req_data: Vec<u8>,
    ) -> (Vec<u8>, VecReader, ArbGas) {
        let fail = |gas: u64| {
            (
                vec![EvmApiStatus::Failure as u8],
                VecReader::new(Vec::new()),
                ArbGas(gas),
            )
        };
        if req_data.len() < 68 {
            return fail(0);
        }
        let bytecode_address = Address::from_slice(&req_data[0..20]);
        let value = U256::from_be_slice(&req_data[20..52]);
        let gas_left = u64::from_be_bytes(req_data[52..60].try_into().unwrap());
        let gas_req = u64::from_be_bytes(req_data[60..68].try_into().unwrap());
        let calldata = Bytes::copy_from_slice(&req_data[68..]);

        let is_static = matches!(req_type, EvmApiMethod::StaticCall) || parent_is_static;
        // DelegateCall keeps the parent's storage context + caller; others target the callee.
        let (target_address, caller) = if matches!(req_type, EvmApiMethod::DelegateCall) {
            (contract, parent_caller)
        } else {
            (bytecode_address, contract)
        };

        if is_static && !value.is_zero() {
            return (
                vec![EvmApiStatus::WriteProtection as u8],
                VecReader::new(Vec::new()),
                ArbGas(gas_left),
            );
        }

        // Nitro charges the call's static/dynamic cost before applying EIP-150.
        let mut gas = Gas::new(gas_left);

        // EIP-2929 account access cost (cold 2600 / warm 100).
        let is_cold = self
            .0
            .ctx
            .journal_mut()
            .load_account(bytecode_address)
            .map(|acc| acc.is_cold)
            .unwrap_or(true);
        if !gas.record_regular_cost(if is_cold { 2600 } else { 100 }) {
            return fail(gas.total_gas_spent());
        }

        // Value-transfer surcharges, mirroring the EVM CALL opcode: only a real CALL moves value
        // (DELEGATECALL inherits the parent's, STATICCALL forbids it). Nitro's Stylus
        // `call_contract` runs the sub-call through geth's CALL, so the program's ink is billed
        // CallValueTransferGas (9000), plus CallNewAccountGas (25000) when the transfer would
        // create an empty recipient. The 2300-gas callee stipend is added after the EIP-150 cap.
        if matches!(req_type, EvmApiMethod::ContractCall) && !value.is_zero() {
            let mut transfer_cost = 9000u64;
            let recipient_empty = self
                .0
                .ctx
                .journal_mut()
                .load_account(target_address)
                .map(|acc| acc.is_empty())
                .unwrap_or(false);
            if recipient_empty {
                transfer_cost += 25000;
            }
            if !gas.record_regular_cost(transfer_cost) {
                return fail(gas.total_gas_spent());
            }
        }

        // Load the bytecode + hash for `bytecode_address` so revm's sub-frame executes
        // the correct code. Mirrors revm 41's CALL opcode (contract.rs: load_acc_and_calc_gas
        // → known_bytecode: (bytecode_hash, bytecode)). Uses load_account_with_code to ensure
        // the bytecode is fetched from the database and stored in the journal cache; then takes
        // `code` directly from the account info (it is Some after load_account_with_code).
        let (kb_hash, kb_bytecode) = {
            match self.0.ctx.journal_mut().load_account_with_code(bytecode_address) {
                Ok(acc) => {
                    let hash = acc.info.code_hash;
                    let bytecode = acc.info.code.clone().unwrap_or_default();
                    (hash, bytecode)
                }
                Err(_) => (KECCAK_EMPTY, Bytecode::default()),
            }
        };

        // Nitro's Stylus bridge intentionally uses floor(available * 63 / 64), which differs by
        // one from geth's usual `available - available / 64` expression when there is a remainder.
        let mut gas_limit = stylus_call_gas_limit(gas.remaining(), gas_req);
        // Match CALL's value stipend. It is part of the callee budget and only costs the caller
        // to the extent that the callee consumes it.
        if matches!(req_type, EvmApiMethod::ContractCall) && !value.is_zero() {
            gas_limit = gas_limit.saturating_add(2300);
        }
        let base_cost = gas.total_gas_spent();

        let frame_input = FrameInput::Call(Box::new(CallInputs {
            input: CallInput::Bytes(calldata),
            return_memory_offset: 0..0,
            gas_limit,
            reservoir: 0,
            bytecode_address,
            known_bytecode: (kb_hash, kb_bytecode),
            target_address,
            caller,
            value: CallValue::Transfer(value),
            scheme: CallScheme::Call,
            is_static,
            charged_new_account_state_gas: false,
        }));

        // Initialize the sub-frame off the current (Stylus) frame, then run it in a fresh
        // frame stack so it doesn't disturb the suspended Stylus frame; restore after.
        let frame_result: Result<_, ContextError<<<CTX as ContextTr>::Db as Database>::Error>> =
            self.0
                .frame_stack
                .get()
                .process_next_action(&mut self.0.ctx, InterpreterAction::NewFrame(frame_input));
        let original_frame_stack = mem::replace(&mut self.0.frame_stack, FrameStack::new());
        if let Ok(ItemOrResult::Item(frame_init)) = frame_result {
            let result = self.run_exec_loop(frame_init);
            self.0.frame_stack = original_frame_stack;
            self.0
                .frame_stack
                .get()
                .interpreter
                .memory
                .free_child_context();

            if let Ok(FrameResult::Call(outcome)) = result {
                let status = if outcome.instruction_result().is_ok() {
                    EvmApiStatus::Success
                } else {
                    EvmApiStatus::Failure
                };
                let output = outcome.output().to_vec();
                // Nitro applies an EVM sub-call's gas refund to the statedb refund counter; carry
                // it up so `frame_run_stylus` folds it onto the tx (refunds are applied at tx end,
                // not deducted from the WASM ink budget).
                if outcome.instruction_result().is_ok() {
                    self.0.ctx.chain_mut().stylus_sub_refund += outcome.gas().refunded();
                }
                let call_cost = gas_limit.saturating_sub(outcome.gas().remaining());
                return (
                    vec![status as u8],
                    VecReader::new(output),
                    ArbGas(base_cost.saturating_add(call_cost)),
                );
            }
        }
        fail(gas_left)
    }

    /// Stylus `Create1`/`Create2` hostio: run a revm create sub-frame and return the result.
    /// Wire format: `gas(8) endowment(32) [salt(32) if Create2] init_code`. Response (per
    /// Nitro `create_request`): a 21-byte `0x01 ++ address` on success (zero address = failed
    /// create), otherwise `0x00 ++ message` for a revert/error. Mirrors arbos-revm on revm 36.
    fn handle_stylus_create(
        &mut self,
        contract: Address,
        parent_is_static: bool,
        req_type: EvmApiMethod,
        req_data: Vec<u8>,
    ) -> (Vec<u8>, VecReader, ArbGas) {
        const CREATE_BASE_GAS: u64 = 32_000;
        const CREATE2_KECCAK_WORD_GAS: u64 = 6;
        let empty = || VecReader::new(Vec::new());
        let fail_addr = |gas: u64| {
            (
                [vec![0x01], Address::ZERO.to_vec()].concat(),
                VecReader::new(Vec::new()),
                ArbGas(gas),
            )
        };

        let is_create2 = matches!(req_type, EvmApiMethod::Create2);
        let header = if is_create2 { 72 } else { 40 };
        if req_data.len() < header {
            return fail_addr(0);
        }
        let gas_remaining = u64::from_be_bytes(req_data[0..8].try_into().unwrap());
        let value = U256::from_be_slice(&req_data[8..40]);
        let (salt, code_off) = if is_create2 {
            (U256::from_be_slice(&req_data[40..72]), 72)
        } else {
            (U256::ZERO, 40)
        };
        let init_code = Bytes::copy_from_slice(&req_data[code_off..]);

        // CREATE is forbidden in a static context.
        if parent_is_static {
            return (
                [vec![0x00], b"write protection".to_vec()].concat(),
                empty(),
                ArbGas(0),
            );
        }

        // EVM create gas: CREATE base + EIP-3860 init-code word cost + (Create2) keccak words.
        // ArbOS >= 40 is post-Shanghai, so EIP-3860 always applies.
        let len = init_code.len();
        // EIP-3860 max init-code size check. The per-word init-code gas (2/word) is charged by
        // the create frame itself, so it is NOT added to gas_cost here (doing so double-counts).
        if len != 0 {
            let max_initcode = self.0.ctx.cfg().max_code_size().saturating_mul(2);
            if len > max_initcode {
                return fail_addr(gas_remaining);
            }
        }
        let mut gas_cost = CREATE_BASE_GAS;
        let scheme = if is_create2 {
            // CREATE2 also pays to keccak the init code for address derivation (6/word).
            gas_cost += CREATE2_KECCAK_WORD_GAS * num_words(len);
            CreateScheme::Create2 { salt }
        } else {
            CreateScheme::Create
        };
        // Charge the EVM create gas (base + init-code) up front so it is included in the cost
        // reported back to the WASM, then withhold the EIP-150 63/64 stipend; the remainder
        // funds the create frame. (revm charges the base in the CREATE opcode, which we bypass.)
        let mut gas = Gas::new(gas_remaining);
        if !gas.record_regular_cost(gas_cost) {
            return (
                [vec![0x00], b"out of gas".to_vec()].concat(),
                empty(),
                ArbGas(0),
            );
        }
        let gas_stipend = gas.remaining() / 64;
        let _ = gas.record_regular_cost(gas_stipend);

        let frame_input = FrameInput::Create(Box::new(CreateInputs::new(
            contract,
            scheme,
            value,
            init_code,
            gas.remaining(),
            0, // reservoir: Arbitrum does not use EIP-8037 state-gas
        )));
        let frame_result: Result<_, ContextError<<<CTX as ContextTr>::Db as Database>::Error>> =
            self.0
                .frame_stack
                .get()
                .process_next_action(&mut self.0.ctx, InterpreterAction::NewFrame(frame_input));
        let original_frame_stack = mem::replace(&mut self.0.frame_stack, FrameStack::new());
        gas.spend_all();

        if let Ok(ItemOrResult::Item(frame_init)) = frame_result {
            let result = self.run_exec_loop(frame_init);
            self.0.frame_stack = original_frame_stack;
            self.0
                .frame_stack
                .get()
                .interpreter
                .memory
                .free_child_context();

            if let Ok(FrameResult::Create(outcome)) = result {
                if *outcome.instruction_result() == InstructionResult::Revert {
                    let output = outcome.output().to_vec();
                    return ([vec![0x00], output].concat(), empty(), ArbGas(gas.total_gas_spent()));
                }
                if let Some(address) = outcome.address {
                    gas.erase_cost(outcome.gas().remaining() + gas_stipend);
                    // Carry the create sub-frame's refund up to the tx (see handle_stylus_call).
                    self.0.ctx.chain_mut().stylus_sub_refund += outcome.gas().refunded();
                    return (
                        [vec![0x01], address.to_vec()].concat(),
                        empty(),
                        ArbGas(gas.total_gas_spent()),
                    );
                }
            }
        }
        fail_addr(gas.total_gas_spent())
    }
}

/// Number of 32-byte EVM words spanning `len` bytes (rounding up).
fn num_words(len: usize) -> u64 {
    (len as u64).div_ceil(32)
}

/// Nitro's Stylus-specific EIP-150 cap. This is deliberately not expressed as
/// `available - available / 64`: that form rounds up when `available` is not divisible by 64.
fn stylus_call_gas_limit(available: u64, requested: u64) -> u64 {
    min(((available as u128) * 63 / 64) as u64, requested)
}

fn revert(gas_limit: u64, output: Vec<u8>) -> InterpreterAction {
    InterpreterAction::Return(InterpreterResult {
        result: InstructionResult::Revert,
        output: output.into(),
        gas: Gas::new(gas_limit),
    })
}

#[cfg(test)]
mod tests {
    use super::stylus_call_gas_limit;

    #[test]
    fn stylus_eip150_cap_matches_nitro_rounding() {
        // Regression for Arbitrum One block 482292023, tx 42. The call has 417140 gas before
        // its 2600 cold-account cost. Nitro forwards floor(414540 * 63 / 64) = 408062.
        assert_eq!(stylus_call_gas_limit(414_540, u64::MAX), 408_062);
        assert_eq!(stylus_call_gas_limit(398_531, u64::MAX), 392_303);
    }

    #[test]
    fn stylus_eip150_cap_respects_requested_gas() {
        assert_eq!(stylus_call_gas_limit(64_000, 12_345), 12_345);
    }
}
