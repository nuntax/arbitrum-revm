use super::*;
use crate::arb_journal::ArbPrecompileCtx;

pub(super) fn run_arb_statistics<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let call = match ArbStatistics::ArbStatisticsCalls::abi_decode(input) {
        Ok(c) => c,
        Err(e) => {
            return revert_result(gas_limit, &format!("ArbStatistics: invalid calldata: {e}"));
        }
    };

    match call {
        ArbStatistics::ArbStatisticsCalls::getStats(_) => {
            // Post-Nitro: only block number is meaningful; all Classic stats are 0.
            let num: u64 = ctx.block_number();
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(
                    U256::from(num),
                    U256::ZERO,
                    U256::ZERO,
                    U256::ZERO,
                    U256::ZERO,
                    U256::ZERO,
                )),
            )
        }
    }
}
