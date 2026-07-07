use super::*;
use crate::arb_journal::ArbPrecompileCtx;

pub(super) fn run_arb_function_table<CTX>(
    _ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let call = match ArbFunctionTable::ArbFunctionTableCalls::abi_decode(input) {
        Ok(c) => c,
        Err(e) => {
            return revert_result(
                gas_limit,
                &format!("ArbFunctionTable: invalid calldata: {e}"),
            );
        }
    };

    // ArbFunctionTable is vestigial post-Nitro; tables are always empty.
    match call {
        ArbFunctionTable::ArbFunctionTableCalls::size(_) => ok_result(
            gas_limit,
            alloy_core::sol_types::SolValue::abi_encode(&(U256::ZERO,)),
        ),
        ArbFunctionTable::ArbFunctionTableCalls::upload(_) => {
            // No-op upload.
            ok_result(gas_limit, vec![])
        }
        ArbFunctionTable::ArbFunctionTableCalls::get(_) => {
            revert_result(gas_limit, "ArbFunctionTable: table is empty")
        }
    }
}
