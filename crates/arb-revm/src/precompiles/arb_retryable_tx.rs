use super::*;
use crate::arb_journal::{ArbCall, ArbJournal, ArbPrecompileCtx};
use alloy_core::sol_types::SolError;
use arbitrum_alloy_consensus::transactions::TxRetry;
use revm::{
    interpreter::{Gas, InstructionResult, InterpreterResult},
    primitives::{Address, B256, Bytes, Log, TxKind, keccak256},
};

const REDEEM_SCHEDULED_EVENT_SIGNATURE: &[u8] =
    b"RedeemScheduled(bytes32,bytes32,uint64,uint64,address,uint256,uint256)";
const LIFETIME_EXTENDED_EVENT_SIGNATURE: &[u8] = b"LifetimeExtended(bytes32,uint256)";
const RETRY_TX_GAS_MINIMUM: u64 = 21_000;
// EVM log gas (go-ethereum params/protocol_params.go); version-independent.
const LOG_GAS: u64 = 375; // params.LogGas
const LOG_TOPIC_GAS: u64 = 375; // params.LogTopicGas
const LOG_DATA_GAS: u64 = 8; // params.LogDataGas
// Cost of emitting the RedeemScheduled event. Unlike the ArbOS-version-dependent backlog cost, this
// is a genuine constant: the event shape never changes -- 4 topics (sig + 3 indexed: ticketId,
// retryTxHash, sequenceNum) and 128 bytes of data (4 non-indexed words: donatedGas, gasDonor,
// maxRefund, submissionFeeRefund). Nitro computes it via `con.RedeemScheduledGasCost(...zeros...)`,
// which returns the same value because only the fixed size matters, not the arg values.
const REDEEM_SCHEDULED_EVENT_DATA_BYTES: u64 = 128; // 4 * 32-byte words
const REDEEM_SCHEDULED_EVENT_GAS: u64 =
    LOG_GAS + 4 * LOG_TOPIC_GAS + REDEEM_SCHEDULED_EVENT_DATA_BYTES * LOG_DATA_GAS; // 2899
const LIFETIME_EXTENDED_EVENT_GAS: u64 = LOG_GAS + 2 * LOG_TOPIC_GAS + 32 * LOG_DATA_GAS; // 1381
const REDEEM_COPY_GAS: u64 = 3; // params.CopyGas (gasCostToReturnResult)
// Nitro ArbOS pricer version boundaries (go-ethereum/params/config_arbitrum.go). At and after
// `SingleGasConstraints` the redeem's backlog reservation includes an extra `GasModelToUse` read;
// at and after `MultiGasConstraints` the ShrinkBacklog cost is a fixed amount charged manually and
// the SSTORE itself runs unmetered (Redeem calls `SetUnmeteredGasAccounting`).
const ARBOS_VERSION_MULTI_GAS_CONSTRAINTS: u64 = 60;
const ARBOS_VERSION_MULTI_CONSTRAINT_FIX: u64 = 51;
// ArbOS flat storage gas (Nitro arbos/storage): every read = StorageReadCost, every write =
// StorageWriteCost (SstoreSetGasEIP2200, flat, not EIP-2929). The retryable-size burn at
// ArbRetryableTx.go:60 uses params.SloadGas (the *COPY* multiplier = 50), NOT StorageReadCost.
const REDEEM_STORAGE_READ: u64 = 800; // StorageReadCost = SloadGasEIP2200
const REDEEM_SIZE_SLOAD_GAS: u64 = 50; // params.SloadGas (COPY multiplier), ArbRetryableTx.go:60
// ArbOS-storage gas Nitro burns for the retryable reads BEFORE reading GasLeft for the donation,
// for a ZERO-calldata retryable (W=0). arb_revm's ArbosState reads are free, so we replicate it
// to match the donated gas. Empirically calibrated against the testnode (ArbOS 40) redeem oracle:
// ≈ 10 storage reads (8000) + numTries SstoreSet (20000) + RetryableSizeBytes burn 50*7 (350) + 3.
// Calldata of W words adds REDEEM_SIZE_SLOAD_GAS*W (line-60 size burn) + 800*(W-1) (content reads).
const REDEEM_READ_BURNS_BASE: u64 = 28_353;
// Nitro's Redeem, on a missing/expired ticket, reads the retryable `timeout` twice before
// reverting (RetryableSizeBytes -> OpenRetryable, then the direct OpenRetryable), each a flat
// StorageReadCost. arb_revm's ArbosState reads are free, so charge the equivalent so the not-found
// path burns the same computation gas as canonical.
const REDEEM_NOT_FOUND_READ_BURNS: u64 = 2 * REDEEM_STORAGE_READ;
// Nitro `ArbRetryableTx.Keepalive` has to prepay an update proportional to the serialized
// retryable size, append a duplicate timeout-queue entry, extend the window counter, pay the
// future reaper, and emit `LifetimeExtended`. ArbOS storage is unmetered in the Rust model, so
// mirror the corresponding burner charges explicitly.
const KEEPALIVE_RETRYABLE_FIXED_BYTES: u64 = 7 * 32;
const KEEPALIVE_UPDATE_COST_PER_WORD: u64 = 200; // params.SstoreSetGasEIP2200 / 100
const KEEPALIVE_QUEUE_AND_WINDOW_WRITES: u64 = 3 * 20_000;
const KEEPALIVE_REAP_PRICE: u64 = 58_000;
// `NoTicketWithID()` custom-error selector, the revert reason Nitro returns for a missing ticket at
// ArbOS >= 3 (oldNotFoundError). Matching it also matches the revert-output copy gas.
const NO_TICKET_WITH_ID_SELECTOR: [u8; 4] = [0x80, 0x69, 0x84, 0x56];

fn backlog_cost_lookup_burn(arbos_version: u64) -> u64 {
    if (ARBOS_VERSION_MULTI_CONSTRAINT_FIX..ARBOS_VERSION_MULTI_GAS_CONSTRAINTS)
        .contains(&arbos_version)
    {
        REDEEM_STORAGE_READ
    } else {
        0
    }
}

pub(super) fn run_arb_retryable_tx<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
    call_inputs: &ArbCall,
) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let call = match ArbRetryableTx::ArbRetryableTxCalls::abi_decode(input) {
        Ok(c) => c,
        Err(_) => return gated_revert_result(gas_limit),
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
        ArbRetryableTx::ArbRetryableTxCalls::submitRetryable(_) => InterpreterResult {
            // This method exists only to represent retryable submissions to explorers. Nitro
            // recognizes the selector, charges the normal non-pure precompile wrapper costs, and
            // reverts with the custom `NotCallable()` error. It must not take the unknown-selector
            // path, which consumes the call's entire gas supply.
            result: InstructionResult::Revert,
            gas: Gas::new(gas_limit),
            output: Bytes::from(ArbRetryableTx::NotCallable {}.abi_encode()),
        },
        ArbRetryableTx::ArbRetryableTxCalls::keepalive(c) => {
            let record = state.retryables.retryable(c.ticketId);
            let current_time = ctx.block_timestamp();
            let arbos_version = match state.arbos_version.get(ctx.journal_mut()) {
                Ok(version) => version,
                Err(error) => {
                    return fatal_result(
                        gas_limit,
                        &format!("ArbRetryableTx: version read failed: {error}"),
                    );
                }
            };

            // Nitro calls `RetryableSizeBytes` first. It opens the ticket and therefore reads the
            // raw timeout (and, only for expired v60+ tickets, the extension-window count).
            let raw_timeout = match record.timeout.get(ctx.journal_mut()) {
                Ok(timeout) => timeout,
                Err(error) => {
                    return fatal_result(
                        gas_limit,
                        &format!("ArbRetryableTx: retryable timeout read failed: {error}"),
                    );
                }
            };
            let mut read_count = 1_u64;
            let mut ticket_exists = raw_timeout != 0;
            if ticket_exists && raw_timeout < current_time {
                if arbos_version >= ARBOS_VERSION_MULTI_GAS_CONSTRAINTS {
                    let windows = match record.timeout_windows_left.get(ctx.journal_mut()) {
                        Ok(windows) => windows,
                        Err(error) => {
                            return fatal_result(
                                gas_limit,
                                &format!(
                                    "ArbRetryableTx: retryable extension-window read failed: {error}"
                                ),
                            );
                        }
                    };
                    read_count += 1;
                    ticket_exists = raw_timeout
                        .saturating_add(windows.saturating_mul(RETRYABLE_LIFETIME_SECONDS))
                        >= current_time;
                } else {
                    ticket_exists = false;
                }
            }
            if !ticket_exists {
                return keepalive_not_found_result(gas_limit, read_count, arbos_version);
            }

            let calldata_size = match record.calldata.size(ctx.journal_mut()) {
                Ok(size) => size,
                Err(error) => {
                    return fatal_result(
                        gas_limit,
                        &format!("ArbRetryableTx: retryable calldata size read failed: {error}"),
                    );
                }
            };
            read_count += 1;
            let retryable_size = KEEPALIVE_RETRYABLE_FIXED_BYTES.saturating_add(
                32 * words_for_bytes(calldata_size.try_into().unwrap_or(usize::MAX)),
            );
            let update_cost = words_for_bytes(retryable_size.try_into().unwrap_or(usize::MAX))
                .saturating_mul(KEEPALIVE_UPDATE_COST_PER_WORD);

            // `RetryableState.Keepalive` opens the ticket again, then `CalculateTimeout` reads the
            // raw timeout and window count. Keep the access order explicit, including v60's
            // expired-ticket window read, because it determines the metered gas.
            let timeout_for_open = match record.timeout.get(ctx.journal_mut()) {
                Ok(timeout) => timeout,
                Err(error) => {
                    return fatal_result(
                        gas_limit,
                        &format!("ArbRetryableTx: retryable timeout read failed: {error}"),
                    );
                }
            };
            read_count += 1;
            let mut open_ticket_exists = timeout_for_open != 0;
            if open_ticket_exists && timeout_for_open < current_time {
                if arbos_version >= ARBOS_VERSION_MULTI_GAS_CONSTRAINTS {
                    let windows = match record.timeout_windows_left.get(ctx.journal_mut()) {
                        Ok(windows) => windows,
                        Err(error) => {
                            return fatal_result(
                                gas_limit,
                                &format!(
                                    "ArbRetryableTx: retryable extension-window read failed: {error}"
                                ),
                            );
                        }
                    };
                    read_count += 1;
                    open_ticket_exists = timeout_for_open
                        .saturating_add(windows.saturating_mul(RETRYABLE_LIFETIME_SECONDS))
                        >= current_time;
                } else {
                    open_ticket_exists = false;
                }
            }
            if !open_ticket_exists {
                return keepalive_not_found_result(gas_limit, read_count, arbos_version);
            }

            let timeout = match record.timeout.get(ctx.journal_mut()) {
                Ok(timeout) => timeout,
                Err(error) => {
                    return fatal_result(
                        gas_limit,
                        &format!("ArbRetryableTx: retryable timeout read failed: {error}"),
                    );
                }
            };
            let windows = match record.timeout_windows_left.get(ctx.journal_mut()) {
                Ok(windows) => windows,
                Err(error) => {
                    return fatal_result(
                        gas_limit,
                        &format!("ArbRetryableTx: retryable extension-window read failed: {error}"),
                    );
                }
            };
            read_count += 2;
            let effective_timeout =
                timeout.saturating_add(windows.saturating_mul(RETRYABLE_LIFETIME_SECONDS));
            let pre_mutation_cost = read_count
                .saturating_mul(REDEEM_STORAGE_READ)
                .saturating_add(update_cost);
            if effective_timeout > current_time.saturating_add(RETRYABLE_LIFETIME_SECONDS) {
                return keepalive_ordinary_error_result(gas_limit, pre_mutation_cost);
            }

            // Queue::Put reads the write pointer and writes both the pointer and a new entry; the
            // subsequent Increment reads and writes `timeoutWindowsLeft`.
            read_count += 2;
            let body_cost = read_count
                .saturating_mul(REDEEM_STORAGE_READ)
                .saturating_add(update_cost)
                .saturating_add(KEEPALIVE_QUEUE_AND_WINDOW_WRITES)
                .saturating_add(KEEPALIVE_REAP_PRICE)
                .saturating_add(LIFETIME_EXTENDED_EVENT_GAS);
            if gas_limit < body_cost {
                return InterpreterResult {
                    result: InstructionResult::OutOfGas,
                    gas: Gas::new_spent_with_reservoir(gas_limit, 0),
                    output: Bytes::new(),
                };
            }

            if let Err(error) = state
                .retryables
                .timeout_queue
                .put(c.ticketId, ctx.journal_mut())
            {
                return fatal_result(
                    gas_limit,
                    &format!("ArbRetryableTx: retryable timeout queue write failed: {error}"),
                );
            }
            if let Err(error) = record
                .timeout_windows_left
                .set(windows.saturating_add(1), ctx.journal_mut())
            {
                return fatal_result(
                    gas_limit,
                    &format!("ArbRetryableTx: retryable extension-window write failed: {error}"),
                );
            }

            let new_timeout = effective_timeout.saturating_add(RETRYABLE_LIFETIME_SECONDS);
            ctx.journal_mut().emit_log(Log::new_unchecked(
                call_inputs.bytecode_address,
                vec![keccak256(LIFETIME_EXTENDED_EVENT_SIGNATURE), c.ticketId],
                Bytes::from(alloy_core::sol_types::SolValue::abi_encode(&(U256::from(
                    new_timeout,
                ),))),
            ));

            let mut gas = Gas::new(gas_limit);
            let _ = gas.record_regular_cost(body_cost);
            InterpreterResult {
                result: InstructionResult::Return,
                gas,
                output: Bytes::from(alloy_core::sol_types::SolValue::abi_encode(&(U256::from(
                    new_timeout,
                ),))),
            }
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
            let redeem_input_len = input.len();
            let call_input_extra = ARBOS_STATE_OPEN_GAS
                + COPY_GAS * words_for_bytes(redeem_input_len.saturating_sub(4));
            let current_timestamp: u64 = ctx.block_timestamp();
            let retryable = state.retryables.retryable(c.ticketId);

            let arbos_version = match state.arbos_version.get(ctx.journal_mut()) {
                Ok(version) => version,
                Err(e) => {
                    return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}"));
                }
            };

            let exists = match retryable.exists(current_timestamp, arbos_version, ctx.journal_mut())
            {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbRetryableTx: error: {e}")),
            };
            if !exists {
                if arbos_version >= 3 {
                    let mut gas = Gas::new(gas_limit);
                    let _ = gas.record_regular_cost(REDEEM_NOT_FOUND_READ_BURNS);
                    return InterpreterResult {
                        result: InstructionResult::Revert,
                        output: Bytes::from_static(&NO_TICKET_WITH_ID_SELECTOR),
                        gas,
                    };
                }
                // Pre-v3: the legacy `Error("ticketId not found")` string, with the same read burns.
                let mut result = revert_result(gas_limit, "ticketId not found");
                let _ = result.gas.record_regular_cost(REDEEM_NOT_FOUND_READ_BURNS);
                return result;
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

            // Donation, per Nitro ArbRetryableTx.Redeem: gasToDonate = GasLeft - futureGasCosts,
            // where GasLeft is already reduced by the ArbOS-storage gas burned reading the retryable
            // (RetryableSizeBytes, OpenRetryable x2, IncrementNumTries, MakeTx fields). arb_revm's
            // ArbosState reads are free, so subtract the equivalent burns so the donated gas, hence
            // the retry tx hash, the RedeemScheduled event, and the ShrinkBacklog below, matches.
            // 40-49: legacy backlog cost (20800); 50-59: single-gas-constraints (+800
            // GasModelToUse). At v51-59 BacklogUpdateCost itself also reads the constraints-vector
            // length once to calculate the future cost. Nitro burns that read immediately, in
            // addition to the cost it returns for the later ShrinkBacklog call.
            let arbos_version = match state.arbos_version.get(ctx.journal_mut()) {
                Ok(version) => version,
                Err(error) => {
                    return fatal_result(
                        gas_limit,
                        &format!("ArbRetryableTx: version read failed: {error}"),
                    );
                }
            };
            let backlog_update_cost = match state
                .l2_pricing
                .backlog_update_cost(arbos_version, ctx.journal_mut())
            {
                Ok(cost) => cost,
                Err(e) => {
                    return fatal_result(
                        gas_limit,
                        &format!("ArbRetryableTx: backlog-cost error: {e}"),
                    );
                }
            };
            let future_gas_costs =
                REDEEM_SCHEDULED_EVENT_GAS + REDEEM_COPY_GAS + backlog_update_cost;
            let calldata_words = words_for_bytes(input.len());
            let read_burns = REDEEM_READ_BURNS_BASE
                + REDEEM_SIZE_SLOAD_GAS * calldata_words
                + REDEEM_STORAGE_READ * calldata_words.saturating_sub(1)
                + backlog_cost_lookup_burn(arbos_version);
            let reserved = future_gas_costs.saturating_add(read_burns);
            if gas_limit < reserved {
                return revert_result(gas_limit, "ArbRetryableTx: not enough gas for redeem");
            }
            let donated_gas = gas_limit - reserved;
            if donated_gas < RETRY_TX_GAS_MINIMUM {
                // Nitro has already metered RetryableSizeBytes, OpenRetryable,
                // IncrementNumTries, and MakeTx before it discovers that the remaining donation
                // cannot fund a retry transaction. This is an ordinary precompile error; at
                // ArbOS 11 and later, the shared wrapper preserves that burned gas and returns
                // empty revert data.
                let mut result = ordinary_error_result(gas_limit);
                // `read_burns` includes the generic `makeContext` charge, which
                // `run_active_dispatch` folds in after this handler returns.
                let _ = result
                    .gas
                    .record_regular_cost(read_burns.saturating_sub(call_input_extra));
                return result;
            }

            let chain_id = match ctx.tx_chain_id() {
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
                gas_fee_cap: U256::from(ctx.block_basefee()),
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

            ctx.journal_mut().emit_log(Log::new_unchecked(
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

            // Nitro shrinks the L2 gas backlog by the donated gas: it is not consumed by the redeem
            // tx itself (the retry re-grows it). Without this the backlog slot, and thus the state
            // root, is too high. Model-aware: legacy uses the single gas_backlog, SingleGas/MultiGas
            // constraints use the per-constraint backlogs (Nitro `ShrinkBacklog`).
            let actual_backlog_update_cost =
                match state
                    .l2_pricing
                    .shrink_backlog(donated_gas, arbos_version, ctx.journal_mut())
                {
                    Ok(cost) => cost,
                    Err(e) => {
                        return fatal_result(
                            gas_limit,
                            &format!("ArbRetryableTx: backlog error: {e}"),
                        );
                    }
                };

            // The redeem prepays a full StorageWrite (20000) for the trailing ShrinkBacklog SSTORE.
            // At v40-59 the actual cost depends on the active model and on every post-shrink
            // backlog value (zero writes cost 5000 instead of 20000). At v50, before
            // MultiConstraintFix, the historical fixed reservation can even be smaller than the
            // actual constraint traversal; Nitro then runs out of gas and reverts the redeem.
            // v60+ charges and consumes the same fixed 20800 with storage metering disabled.
            if actual_backlog_update_cost > backlog_update_cost {
                // Leave exactly the generic call overhead unspent here. `run_active_dispatch`
                // consumes it after the body returns, producing Nitro's all-gas execution revert
                // rather than exposing revm's distinct OutOfGas halt at the CALL boundary.
                let mut gas = Gas::new(gas_limit);
                let _ = gas.record_regular_cost(gas_limit.saturating_sub(call_input_extra));
                return InterpreterResult {
                    result: InstructionResult::Revert,
                    output: Bytes::new(),
                    gas,
                };
            }
            let backlog_overreserve = backlog_update_cost - actual_backlog_update_cost;

            // gasUsed is INDEPENDENT of the donation: Nitro charges read_burns + donation + post-run
            // costs, and read_burns cancels against the donation reservation. precompiles/mod.rs::run
            // re-adds arbos_call_extra_gas (ArbosState open + arg/result copy) on top of our result,
            // so subtract it to avoid double-charging.
            let modrs_extra = call_input_extra + COPY_GAS * words_for_bytes(32);
            let consumed = gas_limit
                .saturating_sub(backlog_overreserve)
                .saturating_sub(modrs_extra);
            let mut gas = Gas::new(gas_limit);
            let _ = gas.record_regular_cost(consumed);
            InterpreterResult {
                result: InstructionResult::Return,
                output: Bytes::from(alloy_core::sol_types::SolValue::abi_encode(&(
                    retry_tx_hash,
                ))),
                gas,
            }
        }
    }
}

fn keepalive_not_found_result(
    gas_limit: u64,
    read_count: u64,
    arbos_version: u64,
) -> InterpreterResult {
    let mut gas = Gas::new(gas_limit);
    let _ = gas.record_regular_cost(read_count.saturating_mul(REDEEM_STORAGE_READ));
    if arbos_version >= 3 {
        InterpreterResult {
            result: InstructionResult::Revert,
            gas,
            output: Bytes::from_static(&NO_TICKET_WITH_ID_SELECTOR),
        }
    } else {
        InterpreterResult {
            result: InstructionResult::PrecompileError,
            gas,
            output: Bytes::new(),
        }
    }
}

fn keepalive_ordinary_error_result(gas_limit: u64, consumed: u64) -> InterpreterResult {
    let mut gas = Gas::new(gas_limit);
    let _ = gas.record_regular_cost(consumed);
    InterpreterResult {
        result: InstructionResult::PrecompileError,
        gas,
        output: Bytes::new(),
    }
}

#[inline]
fn u256_to_b256(value: U256) -> B256 {
    B256::from(value.to_be_bytes::<32>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backlog_cost_lookup_burn_matches_nitro_boundaries() {
        assert_eq!(backlog_cost_lookup_burn(50), 0);
        assert_eq!(backlog_cost_lookup_burn(51), REDEEM_STORAGE_READ);
        assert_eq!(backlog_cost_lookup_burn(59), REDEEM_STORAGE_READ);
        assert_eq!(backlog_cost_lookup_burn(60), 0);
        assert_eq!(backlog_cost_lookup_burn(61), 0);
    }

    #[test]
    fn redeem_scheduled_event_gas_is_fixed() {
        // Derived from the fixed event shape; must equal the historical hardcoded 2899.
        assert_eq!(REDEEM_SCHEDULED_EVENT_GAS, 2_899);
    }
}
