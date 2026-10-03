use super::*;
use crate::arb_journal::{ArbJournal, ArbPrecompileCtx};

const BALANCE_GAS: u64 = 700; // params.BalanceGasEIP1884
const COLD_SLOAD_GAS: u64 = 2_100; // params.ColdSloadCostEIP2929

fn out_of_gas(gas_limit: u64) -> InterpreterResult {
    InterpreterResult {
        result: revm::interpreter::InstructionResult::OutOfGas,
        gas: revm::interpreter::Gas::new_spent_with_reservoir(gas_limit, 0),
        output: revm::primitives::Bytes::new(),
    }
}

fn charge(mut result: InterpreterResult, cost: u64) -> InterpreterResult {
    if !result.gas.record_regular_cost(cost) {
        return out_of_gas(result.gas.limit());
    }
    result
}

pub(super) fn run_arb_info<CTX>(ctx: &mut CTX, input: &[u8], gas_limit: u64) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let call = match ArbInfo::ArbInfoCalls::abi_decode(input) {
        Ok(c) => c,
        Err(_) => return gated_revert_result(gas_limit),
    };

    match call {
        ArbInfo::ArbInfoCalls::getBalance(c) => {
            // Nitro burns `params.BalanceGasEIP1884` (700) before reading the balance.
            if gas_limit < BALANCE_GAS {
                return out_of_gas(gas_limit);
            }
            let balance = match ctx.journal_mut().account_balance(c.account) {
                Ok(b) => b,
                Err(e) => return fatal_result(gas_limit, &format!("ArbInfo: load error: {e}")),
            };
            charge(
                ok_result(
                    gas_limit,
                    alloy_core::sol_types::SolValue::abi_encode_params(&(balance,)),
                ),
                BALANCE_GAS,
            )
        }
        ArbInfo::ArbInfoCalls::getCode(c) => {
            // Nitro burns `params.ColdSloadCostEIP2929` (2,100) up front, then the copy cost of the
            // code it returns.
            if gas_limit < COLD_SLOAD_GAS {
                return out_of_gas(gas_limit);
            }
            let code = match ctx.journal_mut().account_code(c.account) {
                Ok(c) => c,
                Err(e) => return fatal_result(gas_limit, &format!("ArbInfo: code error: {e}")),
            };
            let copy = COPY_GAS * words_for_bytes(code.len());
            charge(
                ok_result(
                    gas_limit,
                    alloy_core::sol_types::SolValue::abi_encode_params(&(code,)),
                ),
                COLD_SLOAD_GAS + copy,
            )
        }
    }
}
