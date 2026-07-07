use super::{ArbosState, ok_result, revert_result};
use crate::arb_journal::ArbPrecompileCtx;
use revm::interpreter::InterpreterResult;

/// ArbOS version at which ArbFilteredTransactionsManager becomes active.
/// Nitro reference: params.ArbosVersion_TransactionFiltering = ArbosVersion_60 = 60.
const ARBOS_VERSION_TRANSACTION_FILTERING: u64 = 60;

/// ArbFilteredTransactionsManager, filtered tx list management for authorised callers.
///
/// Active from ArbOS v60.  Before that version Nitro returns empty bytes (the
/// precompile "doesn't exist yet").  After activation, full implementation is
/// still a TODO; callers get an explicit revert rather than silent failure.
pub(super) fn run_arb_filtered_transactions_manager<CTX>(
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
            return revert_result(
                gas_limit,
                &format!("ArbFilteredTransactionsManager: storage error: {e}"),
            );
        }
    };

    // Nitro behaviour: return empty bytes before version gate is reached.
    if arbos_version < ARBOS_VERSION_TRANSACTION_FILTERING {
        return ok_result(gas_limit, vec![]);
    }

    if input.len() < 4 {
        return revert_result(
            gas_limit,
            "ArbFilteredTransactionsManager: calldata too short",
        );
    }

    // TODO: implement AddFilteredTransaction / DeleteFilteredTransaction /
    // IsTransactionFiltered dispatch.
    revert_result(
        gas_limit,
        "ArbFilteredTransactionsManager: method not yet implemented",
    )
}
