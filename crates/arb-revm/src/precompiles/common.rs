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
    let _ = gas.record_regular_cost(PRECOMPILE_BASE_GAS);
    InterpreterResult {
        result: InstructionResult::Return,
        gas,
        output: Bytes::from(output),
    }
}

/// A method failure that Nitro reports as a plain Go error.
///
/// Nitro's precompile wrapper only ever produces two kinds of failure: a Solidity custom error,
/// which reverts with its encoded data, and a plain Go error, which reverts with empty data (and
/// before ArbOS 11 burns the remaining gas). It never produces `Error(string)`. `_reason` keeps
/// Nitro's error text beside each check for readers; it is not observable on chain.
#[inline]
pub(super) fn plain_error(gas_limit: u64, _reason: &str) -> InterpreterResult {
    ordinary_error_result(gas_limit)
}

/// Build an internal marker for an ordinary error returned by an ArbOS precompile method.
///
/// Nitro distinguishes plain Go errors from Solidity errors in the shared precompile wrapper. A
/// plain error burns all remaining gas before ArbOS 11; from ArbOS 11 onward it preserves the
/// remaining gas. Both versions return empty revert data. The method body does not own that version
/// policy, so [`super::ArbPrecompilesEnum::run_active_dispatch`] normalizes this marker before the
/// result can leave the precompile provider.
#[inline]
pub(super) fn ordinary_error_result(gas_limit: u64) -> InterpreterResult {
    InterpreterResult {
        result: InstructionResult::PrecompileError,
        gas: Gas::new(gas_limit),
        output: Bytes::new(),
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
/// Nitro returns `ErrExecutionReverted` with `gasLeft = 0`, a revert that consumes ALL the
/// supplied gas (unlike a normal business-logic revert, which keeps the remaining gas).
#[inline]
pub(super) fn gated_revert_result(gas_limit: u64) -> InterpreterResult {
    InterpreterResult {
        result: InstructionResult::Revert,
        gas: Gas::new_spent_with_reservoir(gas_limit, 0),
        output: Bytes::new(),
    }
}

/// Build a **fatal** `InterpreterResult` for a genuine backend/storage fault (a failed state
/// read/write), as opposed to a business-logic revert. `InstructionResult::FatalExternalError` is
/// a sentinel that the precompile provider (`precompiles/mod.rs::run`) turns into an aborting
/// `EVMError`, so a backend fault halts execution (Nitro's fatal path) instead of being masked as
/// a keep-gas revert that would silently diverge the state root during replay. The message rides
/// in `output` for the provider to surface.
#[inline]
pub(super) fn fatal_result(gas_limit: u64, msg: &str) -> InterpreterResult {
    InterpreterResult {
        result: InstructionResult::FatalExternalError,
        gas: Gas::new(gas_limit),
        output: Bytes::from(msg.as_bytes().to_vec()),
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
