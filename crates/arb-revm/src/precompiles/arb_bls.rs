use super::revert_result;
use crate::arb_journal::ArbPrecompileCtx;
use revm::interpreter::InterpreterResult;

/// ArbBLS, legacy BLS public key registry from Classic-era Arbitrum.
///
/// In Nitro this precompile has no active methods; the struct exists solely to
/// occupy address 0x67 in the registry.  Any call is treated as a call to a
/// contract that exists but has no matching selector: return empty bytes with
/// gas refunded, matching Nitro's "no method found" revert path.
pub(super) fn run_arb_bls<CTX>(
    _ctx: &mut CTX,
    _input: &[u8],
    gas_limit: u64,
) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    revert_result(gas_limit, "ArbBLS: no active methods")
}
