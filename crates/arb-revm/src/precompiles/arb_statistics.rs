use super::*;
use revm::context_interface::Block;

pub(super) fn run_arb_statistics<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
) -> InterpreterResult
where
    CTX: ContextTr<Journal: JournalTr>,
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
            let num: u64 = ctx.block().number().try_into().unwrap_or(u64::MAX);
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
