use super::{
    ArbosState, InterpreterResult, fatal_result, gated_revert_result, ok_result, plain_error,
};
use crate::arb_journal::{ArbCall, ArbJournal, ArbPrecompileCtx, MeteredJournal};
use alloy_core::sol_types::SolInterface;
use arbitrum_alloy_precompiles::ArbNativeTokenManager;
use revm::{
    interpreter::InstructionResult,
    primitives::{B256, Bytes, Log, keccak256},
};

/// Nitro `mintBurnGasCost`: `WarmStorageReadCostEIP2929 + CallValueTransferGas`.
const MINT_BURN_GAS: u64 = 100 + 9_000;

/// Nitro `ArbNativeTokenManager` (0x73, ArbOS 41+; the dispatcher answers below that version).
///
/// Only a native token owner may mint or burn; anyone else has the call's whole gas burned
/// (`c.BurnOut()`). Minting credits the caller, burning debits it, and each logs its event.
pub(super) fn run_arb_native_token_manager<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
    call: &ArbCall,
) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let decoded = match ArbNativeTokenManager::ArbNativeTokenManagerCalls::abi_decode(input) {
        Ok(c) => c,
        Err(_) => return gated_revert_result(gas_limit),
    };
    let state = ArbosState::open();
    let mut j = MeteredJournal::new(ctx.journal_mut());
    let is_owner = match state.native_token_owners.is_member(call.caller, &mut j) {
        Ok(v) => v,
        Err(e) => return fatal_result(gas_limit, &format!("ArbNativeTokenManager: {e}")),
    };
    if !is_owner {
        return gated_revert_result(gas_limit);
    }
    j.charge(MINT_BURN_GAS);

    let (amount, signature) = match decoded {
        ArbNativeTokenManager::ArbNativeTokenManagerCalls::mintNativeToken(c) => {
            match j.credit_balance(call.caller, c.amount) {
                Ok(true) => {}
                Ok(false) => {
                    return fatal_result(gas_limit, "ArbNativeTokenManager: mint overflow");
                }
                Err(e) => return fatal_result(gas_limit, &format!("ArbNativeTokenManager: {e}")),
            }
            (c.amount, "NativeTokenMinted(address,uint256)")
        }
        ArbNativeTokenManager::ArbNativeTokenManagerCalls::burnNativeToken(c) => {
            let balance = match j.account_balance(call.caller) {
                Ok(b) => b,
                Err(e) => return fatal_result(gas_limit, &format!("ArbNativeTokenManager: {e}")),
            };
            if balance < c.amount {
                return charge(
                    plain_error(gas_limit, "burn amount exceeds balance"),
                    j.burned,
                );
            }
            match j.debit_balance(call.caller, c.amount) {
                Ok(true) => {}
                Ok(false) => {
                    return fatal_result(gas_limit, "ArbNativeTokenManager: burn underflow");
                }
                Err(e) => return fatal_result(gas_limit, &format!("ArbNativeTokenManager: {e}")),
            }
            (c.amount, "NativeTokenBurned(address,uint256)")
        }
    };
    let mut account_topic = [0u8; 32];
    account_topic[12..].copy_from_slice(call.caller.as_slice());
    j.emit_log(Log::new_unchecked(
        call.bytecode_address,
        vec![keccak256(signature), B256::from(account_topic)],
        Bytes::copy_from_slice(&amount.to_be_bytes::<32>()),
    ));
    let burned = j.burned;
    charge(ok_result(gas_limit, vec![]), burned)
}

fn charge(mut result: InterpreterResult, burned: u64) -> InterpreterResult {
    if !result.gas.record_regular_cost(burned) {
        result.result = InstructionResult::OutOfGas;
        result.output = Bytes::new();
    }
    result
}
