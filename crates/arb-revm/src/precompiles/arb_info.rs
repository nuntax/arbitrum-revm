use super::*;

pub(super) fn run_arb_info<CTX>(ctx: &mut CTX, input: &[u8], gas_limit: u64) -> InterpreterResult
where
    CTX: ContextTr<Journal: JournalTr>,
{
    let call = match ArbInfo::ArbInfoCalls::abi_decode(input) {
        Ok(c) => c,
        Err(e) => return revert_result(gas_limit, &format!("ArbInfo: invalid calldata: {e}")),
    };

    match call {
        ArbInfo::ArbInfoCalls::getBalance(c) => {
            let account = match ctx.journal_mut().load_account(c.account) {
                Ok(a) => a,
                Err(e) => return revert_result(gas_limit, &format!("ArbInfo: load error: {e}")),
            };
            let balance = account.info.balance;
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(balance,)),
            )
        }
        ArbInfo::ArbInfoCalls::getCode(c) => {
            let code = match ctx.journal_mut().code(c.account) {
                Ok(s) => s.data,
                Err(e) => return revert_result(gas_limit, &format!("ArbInfo: code error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(code,)),
            )
        }
    }
}
