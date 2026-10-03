use super::*;
use crate::arb_journal::ArbPrecompileCtx;

pub(super) fn run_arb_debug<CTX>(_ctx: &mut CTX, input: &[u8], gas_limit: u64) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let call = match ArbDebug::ArbDebugCalls::abi_decode(input) {
        Ok(c) => c,
        Err(_) => return gated_revert_result(gas_limit),
    };

    // ArbDebug is only available on debug/dev nodes.  All methods revert in
    // production-equivalent environments.
    match call {
        ArbDebug::ArbDebugCalls::customRevert(c) => {
            // Revert with the provided error number encoded as a string.
            plain_error(gas_limit, &format!("ArbDebug: custom revert {}", c.number))
        }
        ArbDebug::ArbDebugCalls::panic(_) | ArbDebug::ArbDebugCalls::legacyError(_) => {
            plain_error(gas_limit, "ArbDebug: panic")
        }
        ArbDebug::ArbDebugCalls::eventsView(_) => ok_result(gas_limit, vec![]),
        ArbDebug::ArbDebugCalls::events(_)
        | ArbDebug::ArbDebugCalls::becomeChainOwner(_)
        | ArbDebug::ArbDebugCalls::overwriteContractCode(_) => {
            plain_error(gas_limit, "ArbDebug: not available in production")
        }
    }
}
