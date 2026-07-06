//! Stylus execution driver: builds the WASM program's view of the block/tx context and
//! runs the program against arb_revm's Context via the [`super::api`] bridge.
//!
//! Inspired by arbos-revm's `stylus_executor.rs` but written against arb_revm's own
//! Context and canonical Nitro, kept generic over revm's `ContextTr` and using explicit
//! inputs rather than coupling to revm-internal frame types.

use arbutil::{
    Bytes20, Bytes32,
    evm::{
        EvmData,
        api::{Gas as ArbGas, VecReader},
        req::EvmApiRequestor,
        user::UserOutcomeKind,
    },
};
use revm::{
    context_interface::{Block, Cfg, ContextTr, Transaction},
    interpreter::{Gas, InstructionResult, InterpreterResult},
    primitives::{Address, B256, U256},
};
use stylus::{
    native::NativeInstance,
    prover::programs::{
        config::{CompileConfig, StylusConfig},
        meter::MeteredMachine,
    },
    run::RunProgram,
};

use crate::{spec::ArbSpecId, stylus::api::StylusHandler};

/// Assemble the [`EvmData`] passed to a Stylus program for one call, the block, tx and
/// call-frame context the WASM hostios read from.
#[allow(clippy::too_many_arguments)]
pub fn build_evm_data<CTX>(
    ctx: &CTX,
    contract: Address,
    caller: Address,
    value: U256,
    module_hash: B256,
    reentrant: u32,
    cached: bool,
) -> EvmData
where
    CTX: ContextTr<Cfg: Cfg<Spec = ArbSpecId>>,
{
    let block = ctx.block();
    let basefee = block.basefee();
    let arbos_version = ctx.cfg().spec().arbos_version();
    let chain_id = ctx.cfg().chain_id();
    let beneficiary = block.beneficiary();
    let gas_limit = block.gas_limit();
    let number = block.number().saturating_to::<u64>();
    let timestamp = block.timestamp().saturating_to::<u64>();
    let tx = ctx.tx();
    let tx_origin = tx.caller();
    let tx_gas_price = tx.effective_gas_price(basefee as u128);

    EvmData {
        arbos_version,
        block_basefee: Bytes32::from(U256::from(basefee).to_be_bytes()),
        chainid: chain_id,
        block_coinbase: bytes20(beneficiary),
        block_gas_limit: gas_limit,
        block_number: number,
        block_timestamp: timestamp,
        contract_address: bytes20(contract),
        module_hash: Bytes32::from(module_hash.0),
        msg_sender: bytes20(caller),
        msg_value: Bytes32::from(value.to_be_bytes()),
        tx_gas_price: Bytes32::from(U256::from(tx_gas_price).to_be_bytes()),
        tx_origin: bytes20(tx_origin),
        reentrant,
        return_data_len: 0,
        cached,
        tracing: false,
    }
}

#[inline]
fn bytes20(address: Address) -> Bytes20 {
    // Address is always 20 bytes, so this conversion never fails.
    Bytes20::from(address.into_array())
}

/// Run a compiled Stylus program against the prepared hostio bridge and map the result to
/// an EVM [`InterpreterResult`].
///
/// This is the execution core: deserialize the native module, convert the available `gas`
/// to ink, run `main`, then refund the unused ink (as gas) and translate the Stylus
/// [`UserOutcomeKind`] to an [`InstructionResult`]. The surrounding flow (fetch/compile/
/// activate the program, charge init/page gas, build the bridge) feeds this.
pub fn run_program(
    serialized: &[u8],
    compile_config: CompileConfig,
    stylus_config: StylusConfig,
    evm_api: EvmApiRequestor<VecReader, StylusHandler>,
    evm_data: EvmData,
    calldata: &[u8],
    mut gas: Gas,
) -> InterpreterResult {
    // SAFETY: `serialized` is a module produced by our own `native::compile`.
    let mut instance =
        match unsafe { NativeInstance::deserialize(serialized, compile_config, evm_api, evm_data) } {
            Ok(instance) => instance,
            Err(err) => {
                return InterpreterResult {
                    result: InstructionResult::Revert,
                    output: err.to_string().into_bytes().into(),
                    gas,
                };
            }
        };

    let ink_limit = stylus_config.pricing.gas_to_ink(ArbGas(gas.remaining()));
    gas.spend_all();

    let (kind, output) = match instance.run_main(calldata, stylus_config, ink_limit) {
        Ok(outcome) => outcome.into_data(),
        Err(_) => (UserOutcomeKind::Failure, Vec::new()),
    };

    let mut gas_left = stylus_config.pricing.ink_to_gas(instance.ink_left().into()).0;
    let result = match kind {
        UserOutcomeKind::Success => InstructionResult::Return,
        UserOutcomeKind::Revert | UserOutcomeKind::Failure => InstructionResult::Revert,
        UserOutcomeKind::OutOfInk => InstructionResult::OutOfGas,
        UserOutcomeKind::OutOfStack => {
            gas_left = 0;
            InstructionResult::StackOverflow
        }
    };
    gas.erase_cost(gas_left);

    InterpreterResult {
        result,
        output: output.into(),
        gas,
    }
}
