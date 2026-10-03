use super::*;
use alloy_core::sol_types::SolError;

/// Nitro `ArbosActs` (0xa4b05) reached by an ordinary call.
///
/// ArbOS applies its own start-block and batch-posting-report actions in the block executor (see
/// `handler.rs`, internal transactions), never through this dispatcher. Every method body in Nitro
/// is `return con.CallerNotArbOSError()`: a Solidity error, so the call reverts with its selector
/// as data after the usual decode, state-open and result-copy charges.
pub(super) fn run_arbos_acts(input: &[u8], gas_limit: u64) -> InterpreterResult {
    if ArbosActs::ArbosActsCalls::abi_decode(input).is_err() {
        return gated_revert_result(gas_limit);
    }
    let mut result = ok_result(gas_limit, ArbosActs::CallerNotArbOS {}.abi_encode());
    result.result = revm::interpreter::InstructionResult::Revert;
    result
}
