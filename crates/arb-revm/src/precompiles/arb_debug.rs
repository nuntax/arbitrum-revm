use super::*;
use crate::arb_journal::ArbPrecompileCtx;

pub(super) fn run_arb_debug<CTX>(_ctx: &mut CTX, input: &[u8], gas_limit: u64) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let call = match ArbDebug::ArbDebugCalls::abi_decode(input) {
        Ok(c) => c,
        Err(e) => return revert_result(gas_limit, &format!("ArbDebug: invalid calldata: {e}")),
    };

    // ArbDebug is only available on debug/dev nodes.  All methods revert in
    // production-equivalent environments.
    match call {
        ArbDebug::ArbDebugCalls::customRevert(c) => {
            // Revert with the provided error number encoded as a string.
            revert_result(gas_limit, &format!("ArbDebug: custom revert {}", c.number))
        }
        ArbDebug::ArbDebugCalls::panic(_) | ArbDebug::ArbDebugCalls::legacyError(_) => {
            revert_result(gas_limit, "ArbDebug: panic")
        }
        ArbDebug::ArbDebugCalls::eventsView(_) => ok_result(gas_limit, vec![]),
        ArbDebug::ArbDebugCalls::events(_)
        | ArbDebug::ArbDebugCalls::becomeChainOwner(_)
        | ArbDebug::ArbDebugCalls::overwriteContractCode(_) => {
            revert_result(gas_limit, "ArbDebug: not available in production")
        }
    }
}
