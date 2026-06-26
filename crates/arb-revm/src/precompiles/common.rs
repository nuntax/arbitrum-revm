use revm::{
    context_interface::ContextTr,
    interpreter::{Gas, InstructionResult, InterpreterResult},
    primitives::Bytes,
};

// Arbitrum's precompiles are "nearly free" from the EVM perspective: the actual
// L2 execution cost is captured by ArbOS's own pricing model.
const PRECOMPILE_BASE_GAS: u64 = 0;

/// Build a successful `InterpreterResult` carrying ABI-encoded `output`.
#[inline]
pub(super) fn ok_result(gas_limit: u64, output: Vec<u8>) -> InterpreterResult {
    let mut gas = Gas::new(gas_limit);
    let _ = gas.record_cost(PRECOMPILE_BASE_GAS);
    InterpreterResult {
        result: InstructionResult::Return,
        gas,
        output: Bytes::from(output),
    }
}

/// Build a revert `InterpreterResult` with an ABI-encoded error string.
#[inline]
pub(super) fn revert_result(gas_limit: u64, msg: &str) -> InterpreterResult {
    // Encode as `Error(string)` = selector 0x08c379a0 + abi_encode(msg)
    let selector: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];
    let msg_bytes = msg.as_bytes();
    let offset: u32 = 32;
    let length = msg_bytes.len() as u32;
    let padded_len = (msg_bytes.len() + 31) & !31;
    let mut output = Vec::with_capacity(4 + 64 + padded_len);
    output.extend_from_slice(&selector);
    output.extend_from_slice(&[0u8; 28]);
    output.extend_from_slice(&offset.to_be_bytes());
    output.extend_from_slice(&[0u8; 28]);
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(msg_bytes);
    output.resize(4 + 64 + padded_len, 0);

    InterpreterResult {
        result: InstructionResult::Revert,
        gas: Gas::new(gas_limit),
        output: Bytes::from(output),
    }
}

/// A call to an ArbOS precompile that is not yet active at the current ArbOS version.
/// Nitro (`precompile.go` Call, `arbosVersion < p.arbosVersion`) treats this exactly like a
/// call to an account with no code: empty return, success, and **no gas consumed**.
#[inline]
pub(super) fn empty_active_result(gas_limit: u64) -> InterpreterResult {
    InterpreterResult {
        result: InstructionResult::Return,
        gas: Gas::new(gas_limit),
        output: Bytes::new(),
    }
}

/// A call to an ArbOS precompile method that does not exist at the current ArbOS version
/// (selector too short, method below its `arbosVersion`, or above its `maxArbosVersion`).
/// Nitro returns `ErrExecutionReverted` with `gasLeft = 0` — a revert that consumes ALL the
/// supplied gas (unlike a normal business-logic revert, which keeps the remaining gas).
#[inline]
pub(super) fn gated_revert_result(gas_limit: u64) -> InterpreterResult {
    InterpreterResult {
        result: InstructionResult::Revert,
        gas: Gas::new_spent_with_reservoir(gas_limit, 0),
        output: Bytes::new(),
    }
}

/// Extract raw input bytes from a `CallInputs`, resolving any shared-buffer
/// reference against the context.
#[inline]
pub(super) fn input_bytes<CTX: ContextTr>(
    ctx: &CTX,
    input: &revm::interpreter::CallInput,
) -> Bytes {
    input.bytes(ctx)
}
