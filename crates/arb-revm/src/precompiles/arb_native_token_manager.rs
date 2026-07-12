use super::{ArbosState, fatal_result, ok_result, revert_result};
use crate::arb_journal::ArbPrecompileCtx;
use alloy_core::sol;
use revm::interpreter::InterpreterResult;

/// ArbOS version at which ArbNativeTokenManager becomes active.
/// Nitro reference: params.ArbosVersion_41 = 41.
const ARBOS_VERSION_NATIVE_TOKEN_MANAGER: u64 = 41;

sol! {
    interface ArbNativeTokenManager {
        function mintNativeToken(uint256 amount) external;
        function burnNativeToken(uint256 amount) external;
    }
}

/// ArbNativeTokenManager, mint/burn native token for authorised callers.
///
/// Active from ArbOS v41.  Before that version Nitro returns empty bytes (the
/// precompile "doesn't exist yet").  After activation, full implementation is
/// still a TODO; for now we return a revert so callers get a clear error rather
/// than silent data corruption.
pub(super) fn run_arb_native_token_manager<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let state = ArbosState::open();
    let arbos_version = match state.arbos_version.get(ctx.journal_mut()) {
        Ok(v) => v,
        Err(e) => {
            return fatal_result(
                gas_limit,
                &format!("ArbNativeTokenManager: storage error: {e}"),
            );
        }
    };

    // Nitro behaviour: if the precompile version gate hasn't been reached,
    // treat the call as if the contract doesn't exist → return empty bytes
    // without consuming gas (gasSupplied returned as gasLeft).
    if arbos_version < ARBOS_VERSION_NATIVE_TOKEN_MANAGER {
        return ok_result(gas_limit, vec![]);
    }

    if input.len() < 4 {
        return revert_result(gas_limit, "ArbNativeTokenManager: calldata too short");
    }

    // TODO: implement MintNativeToken / BurnNativeToken dispatch.
    revert_result(
        gas_limit,
        "ArbNativeTokenManager: method not yet implemented",
    )
}
