use super::*;
use crate::arb_journal::ArbPrecompileCtx;

/// Nitro `ArbosTest` (0x69). Its single method is `pure`, so it opens no ArbOS state.
pub(super) fn run_arbos_test<CTX>(_ctx: &mut CTX, input: &[u8], gas_limit: u64) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let call = match ArbosTest::ArbosTestCalls::abi_decode(input) {
        Ok(c) => c,
        Err(_) => return gated_revert_result(gas_limit),
    };

    match call {
        ArbosTest::ArbosTestCalls::burnArbGas(c) => {
            // Nitro: `if !gasAmount.IsUint64() { return errors.New("not a uint64") }`.
            let Ok(amount) = u64::try_from(c.gasAmount) else {
                return ordinary_error_result(gas_limit);
            };
            // Nitro then calls `c.Burn(amount)` and discards its error, so an amount larger than
            // what is left burns the remainder and the call still succeeds. The argument copy
            // cost is burned before the method runs and is added by the dispatcher, so the body
            // may only take what that leaves.
            let args_cost = COPY_GAS * words_for_bytes(input.len().saturating_sub(4));
            let mut result = ok_result(gas_limit, vec![]);
            let burn = amount.min(gas_limit.saturating_sub(args_cost));
            let recorded = result.gas.record_regular_cost(burn);
            debug_assert!(recorded);
            result
        }
    }
}
