use super::*;
use crate::arb_journal::{ArbJournal, ArbPrecompileCtx};

pub(super) fn run_arb_info<CTX>(ctx: &mut CTX, input: &[u8], gas_limit: u64) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let call = match ArbInfo::ArbInfoCalls::abi_decode(input) {
        Ok(c) => c,
        Err(e) => return revert_result(gas_limit, &format!("ArbInfo: invalid calldata: {e}")),
    };

    match call {
        ArbInfo::ArbInfoCalls::getBalance(c) => {
            let balance = match ctx.journal_mut().account_balance(c.account) {
                Ok(b) => b,
                Err(e) => return revert_result(gas_limit, &format!("ArbInfo: load error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(balance,)),
            )
        }
        ArbInfo::ArbInfoCalls::getCode(c) => {
            let code = match ctx.journal_mut().account_code(c.account) {
                Ok(c) => c,
                Err(e) => return revert_result(gas_limit, &format!("ArbInfo: code error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(code,)),
            )
        }
    }
}
