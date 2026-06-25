use super::*;
use arb_alloy_consensus::transactions::TxRetry;
use revm::{
    context_interface::{Block, Transaction},
    interpreter::CallInputs,
    primitives::{Address, B256, Bytes, Log, TxKind, keccak256},
};

const REDEEM_SCHEDULED_EVENT_SIGNATURE: &[u8] =
    b"RedeemScheduled(bytes32,bytes32,uint64,uint64,address,uint256,uint256)";
const RETRY_TX_GAS_MINIMUM: u64 = 21_000;

pub(super) fn run_arb_retryable_tx<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
    call_inputs: &CallInputs,
) -> InterpreterResult
where
    CTX: ContextTr<Journal: JournalTr>,
{
    let call = match ArbRetryableTx::ArbRetryableTxCalls::abi_decode(input) {
        Ok(c) => c,
        Err(e) => {
            return revert_result(gas_limit, &format!("ArbRetryableTx: invalid calldata: {e}"));
        }
    };

    let state = ArbosState::open();

    match call {
        ArbRetryableTx::ArbRetryableTxCalls::getLifetime(_) => ok_result(
            gas_limit,
            alloy_core::sol_types::SolValue::abi_encode(&(U256::from(RETRYABLE_LIFETIME_SECONDS),)),
        ),
        ArbRetryableTx::ArbRetryableTxCalls::getTimeout(c) => {
            let record = state.retryables.retryable(c.ticketId);
            let timeout = match record.timeout_with_windows(ctx.journal_mut()) {
                Ok(t) => t,
                Err(e) => return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}")),
            };
            if timeout == 0 {
                return revert_result(gas_limit, "ArbRetryableTx: ticket does not exist");
            }
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(U256::from(timeout),)),
            )
        }
        ArbRetryableTx::ArbRetryableTxCalls::getBeneficiary(c) => {
            let record = state.retryables.retryable(c.ticketId);
            let timeout = match record.timeout.get(ctx.journal_mut()) {
                Ok(t) => t,
                Err(e) => return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}")),
            };
            if timeout == 0 {
                return revert_result(gas_limit, "ArbRetryableTx: ticket does not exist");
            }
            let beneficiary = match record.beneficiary.get(ctx.journal_mut()) {
                Ok(b) => b,
                Err(e) => return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(beneficiary,)),
            )
        }
        ArbRetryableTx::ArbRetryableTxCalls::getCurrentRedeemer(_) => {
            // Current redeemer is tracked in transient per-message state;
            // return zero address when no redeem is in progress.
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(Address::ZERO,)),
            )
        }
        ArbRetryableTx::ArbRetryableTxCalls::keepalive(c) => {
            let record = state.retryables.retryable(c.ticketId);
            let timeout = match record.timeout_with_windows(ctx.journal_mut()) {
                Ok(t) => t,
                Err(e) => return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}")),
            };
            if timeout == 0 {
                return revert_result(gas_limit, "ArbRetryableTx: ticket does not exist");
            }
            let new_timeout = timeout.saturating_add(RETRYABLE_LIFETIME_SECONDS);
            match record.timeout.set(new_timeout, ctx.journal_mut()) {
                Ok(_) => {}
                Err(e) => return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}")),
            }
            match record.timeout_windows_left.set(0, ctx.journal_mut()) {
                Ok(_) => {}
                Err(e) => return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}")),
            }
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(U256::from(new_timeout),)),
            )
        }
        ArbRetryableTx::ArbRetryableTxCalls::cancel(c) => {
            let record = state.retryables.retryable(c.ticketId);
            let timeout = match record.timeout.get(ctx.journal_mut()) {
                Ok(t) => t,
                Err(e) => return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}")),
            };
            if timeout == 0 {
                return revert_result(gas_limit, "ArbRetryableTx: ticket does not exist");
            }
            let beneficiary = match record.beneficiary.get(ctx.journal_mut()) {
                Ok(b) => b,
                Err(e) => return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}")),
            };
            if call_inputs.caller != beneficiary {
                return revert_result(
                    gas_limit,
                    "ArbRetryableTx: only the beneficiary may cancel a retryable",
                );
            }
            match state
                .retryables
                .delete_retryable(c.ticketId, ctx.journal_mut())
            {
                Ok(true) => ok_result(gas_limit, vec![]),
                Ok(false) => revert_result(gas_limit, "ArbRetryableTx: ticket does not exist"),
                Err(e) => revert_result(gas_limit, &format!("ArbRetryableTx: cancel error: {e}")),
            }
        }
        ArbRetryableTx::ArbRetryableTxCalls::redeem(c) => {
            let current_timestamp: u64 = match ctx.block().timestamp().try_into() {
                Ok(ts) => ts,
                Err(_) => {
                    return revert_result(
                        gas_limit,
                        "ArbRetryableTx: block timestamp does not fit in u64",
                    );
                }
            };
            let retryable = state.retryables.retryable(c.ticketId);

            let exists = match retryable.exists(current_timestamp, ctx.journal_mut()) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}")),
            };
            if !exists {
                return revert_result(gas_limit, "ArbRetryableTx: ticket does not exist");
            }

            let nonce = match retryable.num_tries.get(ctx.journal_mut()) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}")),
            };
            if let Err(e) = retryable
                .num_tries
                .set(nonce.saturating_add(1), ctx.journal_mut())
            {
                return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}"));
            }

            let from = match retryable.from.get(ctx.journal_mut()) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}")),
            };
            let to = match retryable.to(ctx.journal_mut()) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}")),
            };
            let value = match retryable.callvalue.get(ctx.journal_mut()) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}")),
            };
            let input = match retryable.calldata.get(ctx.journal_mut()) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}")),
            };

            if gas_limit < RETRY_TX_GAS_MINIMUM {
                return revert_result(
                    gas_limit,
                    "ArbRetryableTx: not enough gas to run redeem attempt",
                );
            }
            let donated_gas = gas_limit;

            let chain_id = match ctx.tx().chain_id() {
                Some(id) => U256::from(id),
                None => match state.chain_id.get(ctx.journal_mut()) {
                    Ok(id) => id,
                    Err(e) => {
                        return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}"));
                    }
                },
            };

            let retry_tx = TxRetry {
                chain_id,
                nonce,
                from,
                gas_fee_cap: U256::from(ctx.block().basefee()),
                gas_limit: donated_gas,
                to: match to {
                    Some(dest) => TxKind::Call(dest),
                    None => TxKind::Create,
                },
                value,
                input: Bytes::from(input),
                ticket_id: c.ticketId,
                refund_to: call_inputs.caller,
                max_refund: U256::MAX,
                submission_fee_refund: U256::ZERO,
            };
            let retry_tx_hash = retry_tx.tx_hash();

            ctx.journal_mut().log(Log::new_unchecked(
                call_inputs.bytecode_address,
                vec![
                    keccak256(REDEEM_SCHEDULED_EVENT_SIGNATURE),
                    c.ticketId,
                    retry_tx_hash,
                    u256_to_b256(U256::from(nonce)),
                ],
                Bytes::from(alloy_core::sol_types::SolValue::abi_encode(&(
                    donated_gas,
                    call_inputs.caller,
                    U256::MAX,
                    U256::ZERO,
                ))),
            ));

            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(retry_tx_hash,)),
            )
        }
    }
}

#[inline]
fn u256_to_b256(value: U256) -> B256 {
    B256::from(value.to_be_bytes::<32>())
}
