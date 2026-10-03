use super::gated_revert_result;
use crate::arb_journal::ArbPrecompileCtx;
use revm::interpreter::InterpreterResult;

/// ArbBLS (0x67), the Classic-era BLS key registry.
///
/// Nitro still registers the address, but its interface declares no methods, so every call takes
/// the unknown-selector path in `precompile.go` Call: `ErrExecutionReverted` with no gas left.
/// Measured on Robinhood Chain: any selector, and empty calldata, revert using the whole budget.
pub(super) fn run_arb_bls<CTX>(_ctx: &mut CTX, _input: &[u8], gas_limit: u64) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    gated_revert_result(gas_limit)
}
