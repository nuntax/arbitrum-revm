use crate::{
    api::exec::ArbContextTr,
    constants::{ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE, ARBOS_STATE_ADDRESS, ARB_RETRYABLE_TX_ADDRESS},
    storage::{ArbosState, RETRYABLE_LIFETIME_SECONDS},
};
use alloy_consensus::Transaction as AlloyTransaction;
use alloy_rlp::{Encodable, Header};
use arb_alloy_consensus::transactions::{
    ArbTxEnvelope, TxRetry, submit_retryable::SubmitRetryableTx,
};
use revm::{
    context_interface::{Block, ContextTr, JournalTr, Transaction, journaled_state::TransferError},
    primitives::{Address, B256, Bytes, Log, TxKind, U256, keccak256},
    state::EvmState,
};

const WORD_SIZE: usize = 32;
const SELECTOR_SIZE: usize = 4;
const SUBMIT_RETRYABLE_SELECTOR: [u8; 4] = [0xc9, 0xf9, 0x5d, 0x32];
const HEAD_WORDS: usize = 11;
const MIN_CALLDATA_LEN: usize = SELECTOR_SIZE + ((HEAD_WORDS + 1) * WORD_SIZE);
const TICKET_CREATED_EVENT_SIGNATURE: &[u8] = b"TicketCreated(bytes32)";
const REDEEM_SCHEDULED_EVENT_SIGNATURE: &[u8] =
    b"RedeemScheduled(bytes32,bytes32,uint64,uint64,address,uint256,uint256)";

const RETRYABLE_ESCROW_TAG: &[u8] = b"retryable escrow";
const TX_GAS: u64 = 21_000;
/// Nitro `params.ArbosVersion_Stylus`. Below this version, ArbOS's `util.TransferBalance`
/// resurrects a zero-value transfer's destructed `from` account as an empty "zombie"
/// (see the auto-redeem escrow handling in [`apply_submit_retryable_tx`]).
const ARBOS_VERSION_STYLUS: u64 = 30;

/// Outcome of applying a submit-retryable tx.
///
/// Mirrors Nitro `StartTxHook`'s `return true, ZeroGas, err, nil` semantics: a retryable
/// submission can legitimately fail (e.g. the L1 deposit can't cover the max submission
/// fee, or the sender can't fund the callvalue escrow). Nitro records such a tx as failed
/// (status 0, gasUsed 0) **without reverting** the state changes made before the failure
/// (notably the deposit mint) and continues the block. So these are not fatal errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmitRetryableOutcome {
    /// Ticket created. Receipt = success. `gas_used` is the value Nitro reports for the
    /// submit tx's receipt: `0` when no auto-redeem is scheduled (Nitro `StartTxHook` returns
    /// `ZeroGas` on the gas-too-low / can't-fund-gas path, `tx_processor.go:346`), or the
    /// retryable's `usergas` when the auto-redeem *is* scheduled (Nitro returns
    /// `SingleDimGas(usergas)`, `tx_processor.go:435`). The reserved `usergas` is charged as
    /// the submit's gasUsed only in the latter case; the separate auto-redeem retry tx that
    /// then runs carries its own receipt.
    Created { gas_used: u64 },
    /// Submission failed a funds check after applying its partial state changes. Receipt
    /// = failed, gasUsed 0; the partial state (deposit mint, fee refunds) is kept.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubmitRetryableCall {
    pub request_id: B256,
    pub l1_base_fee: U256,
    pub deposit_value: U256,
    pub retry_value: U256,
    pub gas_fee_cap: U256,
    pub gas_limit: u64,
    pub max_submission_fee: U256,
    pub fee_refund_address: Address,
    pub beneficiary: Address,
    pub retry_to: Option<Address>,
    pub retry_data: Bytes,
}

#[cfg(test)]
pub(crate) fn encode_submit_retryable_calldata(call: &SubmitRetryableCall) -> Bytes {
    let retry_to = call.retry_to.unwrap_or(Address::ZERO);
    let mut args = Vec::new();
    args.extend_from_slice(call.request_id.as_slice());
    args.extend_from_slice(&call.l1_base_fee.to_be_bytes::<32>());
    args.extend_from_slice(&call.deposit_value.to_be_bytes::<32>());
    args.extend_from_slice(&call.retry_value.to_be_bytes::<32>());
    args.extend_from_slice(&call.gas_fee_cap.to_be_bytes::<32>());
    args.extend_from_slice(&U256::from(call.gas_limit).to_be_bytes::<32>());
    args.extend_from_slice(&call.max_submission_fee.to_be_bytes::<32>());
    args.extend_from_slice(&address_word(call.fee_refund_address));
    args.extend_from_slice(&address_word(call.beneficiary));
    args.extend_from_slice(&address_word(retry_to));
    args.extend_from_slice(&U256::from(HEAD_WORDS * WORD_SIZE).to_be_bytes::<32>());
    args.extend_from_slice(&U256::from(call.retry_data.len()).to_be_bytes::<32>());
    args.extend_from_slice(call.retry_data.as_ref());
    let remainder = call.retry_data.len() % WORD_SIZE;
    if remainder != 0 {
        args.extend(core::iter::repeat(0_u8).take(WORD_SIZE - remainder));
    }

    let mut out = Vec::with_capacity(SELECTOR_SIZE + args.len());
    out.extend_from_slice(&SUBMIT_RETRYABLE_SELECTOR);
    out.extend_from_slice(&args);
    out.into()
}

pub(crate) fn decode_submit_retryable_calldata(
    input: &[u8],
) -> Result<SubmitRetryableCall, String> {
    if input.len() < MIN_CALLDATA_LEN {
        return Err(format!(
            "[ARBITRUM] submit-retryable calldata too short: got {}, need at least {}",
            input.len(),
            MIN_CALLDATA_LEN
        ));
    }
    if input[..SELECTOR_SIZE] != SUBMIT_RETRYABLE_SELECTOR {
        return Err(format!(
            "[ARBITRUM] submit-retryable selector mismatch: got 0x{}",
            hex::encode(&input[..SELECTOR_SIZE])
        ));
    }

    let args = &input[SELECTOR_SIZE..];
    let request_id = B256::from_slice(word(args, 0)?);
    let l1_base_fee = word_to_u256(word(args, 1)?);
    let deposit_value = word_to_u256(word(args, 2)?);
    let retry_value = word_to_u256(word(args, 3)?);
    let gas_fee_cap = word_to_u256(word(args, 4)?);
    let gas_limit = word_to_u64(word(args, 5)?)?;
    let max_submission_fee = word_to_u256(word(args, 6)?);
    let fee_refund_address = word_to_address(word(args, 7)?);
    let beneficiary = word_to_address(word(args, 8)?);
    let retry_to = word_to_optional_address(word(args, 9)?);

    let retry_data_offset = word_to_usize(word(args, 10)?)?;
    if retry_data_offset % WORD_SIZE != 0 || retry_data_offset < HEAD_WORDS * WORD_SIZE {
        return Err(format!(
            "[ARBITRUM] invalid submit-retryable data offset {retry_data_offset}"
        ));
    }
    if retry_data_offset + WORD_SIZE > args.len() {
        return Err("[ARBITRUM] submit-retryable data offset out of bounds".into());
    }
    let retry_data_len = word_to_usize(&args[retry_data_offset..retry_data_offset + WORD_SIZE])?;
    let retry_data_start = retry_data_offset + WORD_SIZE;
    let retry_data_end = retry_data_start.saturating_add(retry_data_len);
    if retry_data_end > args.len() {
        return Err(format!(
            "[ARBITRUM] submit-retryable retryData out of bounds: end={retry_data_end}, len={}",
            args.len()
        ));
    }

    Ok(SubmitRetryableCall {
        request_id,
        l1_base_fee,
        deposit_value,
        retry_value,
        gas_fee_cap,
        gas_limit,
        max_submission_fee,
        fee_refund_address,
        beneficiary,
        retry_to,
        retry_data: Bytes::copy_from_slice(&args[retry_data_start..retry_data_end]),
    })
}

/// Applies Nitro-style submit-retryable path:
/// 1. Mint `deposit_value` to sender.
/// 2. Validate max-submission-fee bound.
/// 3. Move retry callvalue into deterministic escrow.
/// 4. Create retryable record in ArbOS storage and enqueue it for timeout reaping.
pub(crate) fn apply_submit_retryable_tx<CTX: ArbContextTr>(
    ctx: &mut CTX,
) -> Result<SubmitRetryableOutcome, String> {
    let call = decode_submit_retryable_calldata(ctx.tx().input().as_ref())?;
    let from = ctx.tx().caller();
    let chain_id = ctx.tx().chain_id().ok_or_else(|| {
        "[ARBITRUM] submit-retryable tx missing chain_id (required for ticket hash)".to_string()
    })?;
    let ticket_id = compute_submit_retryable_ticket_id(chain_id, from, &call);

    let submission_fee = retryable_submission_fee(call.retry_data.len(), call.l1_base_fee);
    if call.max_submission_fee < submission_fee {
        // Nitro `tx_processor.go`: "should be impossible as this is checked at L1"; still a
        // recorded failure, not a fatal error.
        return Ok(SubmitRetryableOutcome::Failed);
    }

    let current_timestamp: u64 = ctx
        .block()
        .timestamp()
        .try_into()
        .map_err(|_| "[ARBITRUM] block.timestamp does not fit in u64".to_string())?;
    let timeout = current_timestamp.saturating_add(RETRYABLE_LIFETIME_SECONDS);
    let effective_base_fee = U256::from(ctx.block().basefee());

    let journal = ctx.journal_mut();
    journal
        .load_account(ARBOS_STATE_ADDRESS)
        .map_err(|err| format!("[ARBITRUM] failed to warm ArbOS state account: {err}"))?;

    journal
        .balance_incr(from, call.deposit_value)
        .map_err(|err| format!("[ARBITRUM] failed to mint retryable deposit value: {err}"))?;

    let arbos_state = ArbosState::open();
    let network_fee_account = arbos_state
        .network_fee_account
        .get(journal)
        .map_err(|err| format!("[ARBITRUM] failed to read network fee account: {err}"))?;

    let balance_after_mint = journal
        .load_account(from)
        .map_err(|err| format!("[ARBITRUM] failed to load sender account: {err}"))?
        .info
        .balance;
    if balance_after_mint < call.max_submission_fee {
        // Nitro `tx_processor.go:258`: the (aliased) sender's balance after the deposit mint
        // can't cover the max submission fee. Nitro records the tx as failed (status 0,
        // gasUsed 0) and keeps the block going, *without* reverting the mint. Real Arbitrum
        // One batch-1 retryables hit this (e.g. tx 0xff56fb78…, canonical receipt status 0).
        return Ok(SubmitRetryableOutcome::Failed);
    }

    let mut available_refund = call.deposit_value;
    let _ = take_funds(&mut available_refund, call.retry_value);
    let transfer_error = journal
        .transfer(from, network_fee_account, submission_fee)
        .map_err(|err| format!("[ARBITRUM] failed to transfer submission fee: {err}"))?;
    map_transfer_error(transfer_error, "submit-retryable submission fee transfer")?;
    let withheld_submission_fee = take_funds(&mut available_refund, submission_fee);

    let submission_fee_refund = take_funds(
        &mut available_refund,
        call.max_submission_fee.saturating_sub(submission_fee),
    );
    if submission_fee_refund > U256::ZERO {
        let transfer_error = journal
            .transfer(from, call.fee_refund_address, submission_fee_refund)
            .map_err(|err| format!("[ARBITRUM] failed to transfer submission fee refund: {err}"))?;
        map_transfer_error(
            transfer_error,
            "submit-retryable submission fee refund transfer",
        )?;
    }

    if call.retry_value > U256::ZERO {
        let escrow = retryable_escrow_address(ticket_id);
        let transfer_error = journal
            .transfer(from, escrow, call.retry_value)
            .map_err(|err| format!("[ARBITRUM] failed to escrow retryable callvalue: {err}"))?;
        if let Err(original_error) =
            map_transfer_error(transfer_error, "submit-retryable escrow transfer")
        {
            // Refund submission fee to sender, then push whatever was withheld from
            // the L1 deposit side to the fee refund address.
            if submission_fee > U256::ZERO {
                let refund_err = journal
                    .transfer(network_fee_account, from, submission_fee)
                    .map_err(|err| {
                        format!("[ARBITRUM] failed to revert submission fee transfer: {err}")
                    })?;
                map_transfer_error(
                    refund_err,
                    "submit-retryable submission fee revert transfer",
                )?;
            }
            if withheld_submission_fee > U256::ZERO {
                let refund_err = journal
                    .transfer(from, call.fee_refund_address, withheld_submission_fee)
                    .map_err(|err| {
                        format!(
                           "[ARBITRUM] failed to transfer withheld submission fee refund: {err}"
                        )
                    })?;
                map_transfer_error(
                    refund_err,
                    "submit-retryable withheld submission fee refund transfer",
                )?;
            }
            // Nitro `tx_processor.go:307`: callvalue escrow couldn't be funded. The
            // submission-fee refund dance above mirrors Nitro; the tx is then a recorded
            // failure (state kept), not a fatal error.
            let _ = original_error;
            return Ok(SubmitRetryableOutcome::Failed);
        }
    }

    let retryable = arbos_state.retryables.retryable(ticket_id);
    retryable
        .num_tries
        .set(0, journal)
        .map_err(|err| format!("[ARBITRUM] failed to write retryable numTries: {err}"))?;
    retryable
        .from
        .set(from, journal)
        .map_err(|err| format!("[ARBITRUM] failed to write retryable from: {err}"))?;
    retryable
        .set_to(call.retry_to, journal)
        .map_err(|err| format!("[ARBITRUM] failed to write retryable to: {err}"))?;
    retryable
        .callvalue
        .set(call.retry_value, journal)
        .map_err(|err| format!("[ARBITRUM] failed to write retryable callvalue: {err}"))?;
    retryable
        .beneficiary
        .set(call.beneficiary, journal)
        .map_err(|err| format!("[ARBITRUM] failed to write retryable beneficiary: {err}"))?;
    retryable
        .timeout
        .set(timeout, journal)
        .map_err(|err| format!("[ARBITRUM] failed to write retryable timeout: {err}"))?;
    retryable
        .timeout_windows_left
        .set(0, journal)
        .map_err(|err| format!("[ARBITRUM] failed to write retryable timeout windows: {err}"))?;
    retryable
        .calldata
        .set_fresh(call.retry_data.as_ref(), journal)
        .map_err(|err| format!("[ARBITRUM] failed to write retryable calldata: {err}"))?;

    arbos_state
        .retryables
        .ensure_timeout_queue_initialized(journal)
        .map_err(|err| format!("[ARBITRUM] failed to initialize retryable timeout queue: {err}"))?;
    arbos_state
        .retryables
        .timeout_queue
        .put(ticket_id, journal)
        .map_err(|err| format!("[ARBITRUM] failed to enqueue retryable timeout: {err}"))?;
    journal.log(ticket_created_log(ticket_id));

    let usergas = call.gas_limit;
    let balance = journal
        .load_account(from)
        .map_err(|err| {
            format!("[ARBITRUM] failed to reload sender account after retryable creation: {err}")
        })?
        .info
        .balance;

    let max_gas_cost = call.gas_fee_cap.saturating_mul(U256::from(usergas));
    let max_fee_per_gas_too_low = call.gas_fee_cap < effective_base_fee;
    if balance < max_gas_cost || usergas < TX_GAS || max_fee_per_gas_too_low {
        let gas_cost_refund = take_funds(&mut available_refund, max_gas_cost);
        if gas_cost_refund > U256::ZERO {
            let transfer_error = journal
                .transfer(from, call.fee_refund_address, gas_cost_refund)
                .map_err(|err| format!("[ARBITRUM] failed to transfer gas-cost refund: {err}"))?;
            map_transfer_error(transfer_error, "submit-retryable gas-cost refund transfer")?;
        }
        // Ticket created but no auto-redeem (gas too low / can't fund gas): still a success,
        // but Nitro returns `ZeroGas` for the submit receipt (no `usergas` is charged since the
        // auto-redeem it would have paid for is not scheduled).
        return Ok(SubmitRetryableOutcome::Created { gas_used: 0 });
    }

    let gascost = effective_base_fee.saturating_mul(U256::from(usergas));
    let mut network_cost = gascost;
    let infra_fee_account = arbos_state
        .infra_fee_account
        .get(journal)
        .map_err(|err| format!("[ARBITRUM] failed to read infra fee account: {err}"))?;
    if infra_fee_account != Address::ZERO {
        let min_base_fee = arbos_state
            .l2_pricing
            .min_base_fee_wei
            .get(journal)
            .map_err(|err| format!("[ARBITRUM] failed to read min base fee: {err}"))?;
        let infra_fee = core::cmp::min(min_base_fee, effective_base_fee);
        let infra_cost_requested = infra_fee.saturating_mul(U256::from(usergas));
        let infra_cost = take_funds(&mut network_cost, infra_cost_requested);
        if infra_cost > U256::ZERO {
            let transfer_error = journal
                .transfer(from, infra_fee_account, infra_cost)
                .map_err(|err| format!("[ARBITRUM] failed to transfer infra gas cost: {err}"))?;
            map_transfer_error(transfer_error, "submit-retryable infra gas cost transfer")?;
        }
    }
    if network_cost > U256::ZERO {
        let transfer_error = journal
            .transfer(from, network_fee_account, network_cost)
            .map_err(|err| format!("[ARBITRUM] failed to transfer network gas cost: {err}"))?;
        map_transfer_error(transfer_error, "submit-retryable network gas cost transfer")?;
    }

    let withheld_gas_funds = take_funds(&mut available_refund, gascost);
    let gas_price_refund_requested = call
        .gas_fee_cap
        .saturating_sub(effective_base_fee)
        .saturating_mul(U256::from(usergas));
    let gas_price_refund = take_funds(&mut available_refund, gas_price_refund_requested);
    if gas_price_refund > U256::ZERO {
        let transfer_error = journal
            .transfer(from, call.fee_refund_address, gas_price_refund)
            .map_err(|err| format!("[ARBITRUM] failed to transfer gas-price refund: {err}"))?;
        map_transfer_error(transfer_error, "submit-retryable gas-price refund transfer")?;
    }

    let max_refund = available_refund
        .saturating_add(withheld_gas_funds)
        .saturating_add(withheld_submission_fee);
    retryable.num_tries.set(1, journal).map_err(|err| {
        format!("[ARBITRUM] failed to mark retryable auto-redeem scheduled: {err}")
    })?;

    // ArbOS pre-Stylus "zombie escrow" quirk. The auto-redeem we just scheduled runs in this
    // same block; Nitro's redeem (`arbos/tx_processor.go`) does
    // `util.TransferBalance(escrow, from, callvalue)`, and for `callvalue == 0` with
    // `ArbOSVersion < Stylus` the `from`-side of that transfer takes the
    // `CreateZombieIfDeleted(escrow)` branch (`arbos/util/transfer.go`). The escrow was
    // destructed by the submit's own zero-value escrow touch earlier this block, so it *can* be
    // resurrected as a present-but-empty account, but go-ethereum's `Finalise` keeps that
    // zombie only when the redeem SUCCEEDS (the `DeleteRetryable` path). A redeem that reverts
    // (e.g. out of gas) leaves the escrow ABSENT, confirmed on Arb One via `eth_getProof`: a
    // successful redeem leaves the escrow present-empty, an OOG redeem leaves it non-existent
    // (an exclusion proof, not a zeroed leaf). Since the
    // outcome isn't known until the redeem runs, we can't materialize the escrow here; instead
    // record this ticket as zombie-eligible for the current block and let the redeem hook
    // (`retry_tx.rs`) materialize the escrow iff it succeeds. Block-scoping the set (cleared
    // each StartBlock) excludes later-block manual redeems, which see no same-block destruct and
    // so never resurrect the escrow. Retryables with `callvalue > 0` don't hit this: their
    // escrow holds the value across the submit and is emptied-then-cleared when the redeem moves
    // it back out.
    let track_zombie_escrow = if call.retry_value == U256::ZERO {
        let arbos_version = arbos_state
            .arbos_version
            .get(journal)
            .map_err(|err| format!("[ARBITRUM] failed to read ArbOS version: {err}"))?;
        arbos_version < ARBOS_VERSION_STYLUS
    } else {
        false
    };

    let retry_tx_hash = TxRetry {
        chain_id: U256::from(chain_id),
        nonce: 0,
        from,
        gas_fee_cap: effective_base_fee,
        gas_limit: usergas,
        to: match call.retry_to {
            Some(address) => TxKind::Call(address),
            None => TxKind::Create,
        },
        value: call.retry_value,
        input: call.retry_data.clone(),
        ticket_id,
        refund_to: call.fee_refund_address,
        max_refund,
        submission_fee_refund: submission_fee,
    }
    .tx_hash();
    journal.log(redeem_scheduled_log(
        ticket_id,
        retry_tx_hash,
        usergas,
        call.fee_refund_address,
        max_refund,
        submission_fee,
    ));

    // Journal borrow released above; record zombie-escrow eligibility for the same-block redeem.
    if track_zombie_escrow {
        ctx.chain_mut().pending_zombie_escrow_tickets.push(ticket_id);
    }

    // Auto-redeem scheduled: Nitro returns `SingleDimGas(usergas)`, so the submit tx's receipt
    // gasUsed is the reserved `usergas`.
    Ok(SubmitRetryableOutcome::Created { gas_used: usergas })
}

fn ticket_created_log(ticket_id: B256) -> Log {
    Log::new_unchecked(
        ARB_RETRYABLE_TX_ADDRESS,
        vec![keccak256(TICKET_CREATED_EVENT_SIGNATURE), ticket_id],
        Bytes::new(),
    )
}

fn redeem_scheduled_log(
    ticket_id: B256,
    retry_tx_hash: B256,
    donated_gas: u64,
    gas_donor: Address,
    max_refund: U256,
    submission_fee_refund: U256,
) -> Log {
    Log::new_unchecked(
        ARB_RETRYABLE_TX_ADDRESS,
        vec![
            keccak256(REDEEM_SCHEDULED_EVENT_SIGNATURE),
            ticket_id,
            retry_tx_hash,
            B256::from(U256::ZERO.to_be_bytes::<32>()),
        ],
        Bytes::from(alloy_core::sol_types::SolValue::abi_encode(&(
            donated_gas,
            gas_donor,
            max_refund,
            submission_fee_refund,
        ))),
    )
}

pub fn submit_retryable_auto_redeem_scheduled(state: &EvmState, ticket_id: B256) -> bool {
    let arbos_state = ArbosState::open();
    let retryable = arbos_state.retryables.retryable(ticket_id);
    let (account, slot) = retryable.num_tries.account_and_key();
    let slot_key = U256::from_be_bytes(slot.0);
    state
        .get(&account)
        .and_then(|account_state| account_state.storage.get(&slot_key))
        .map(|slot| slot.present_value() > U256::ZERO)
        .unwrap_or(false)
}

pub fn build_scheduled_retry_from_submit(
    submit: &SubmitRetryableTx,
    effective_base_fee: U256,
) -> Result<ArbTxEnvelope, String> {
    let call = decode_submit_retryable_calldata(submit.input().as_ref())?;
    let submission_fee = retryable_submission_fee(call.retry_data.len(), call.l1_base_fee);
    let mut available_refund = call.deposit_value;
    let _ = take_funds(&mut available_refund, call.retry_value);
    let withheld_submission_fee = take_funds(&mut available_refund, submission_fee);
    let _ = take_funds(
        &mut available_refund,
        call.max_submission_fee.saturating_sub(submission_fee),
    );

    let gascost = effective_base_fee.saturating_mul(U256::from(call.gas_limit));
    let withheld_gas_funds = take_funds(&mut available_refund, gascost);
    let _ = take_funds(
        &mut available_refund,
        call.gas_fee_cap
            .saturating_sub(effective_base_fee)
            .saturating_mul(U256::from(call.gas_limit)),
    );
    available_refund = available_refund.saturating_add(withheld_gas_funds);
    available_refund = available_refund.saturating_add(withheld_submission_fee);

    let retry_to = match call.retry_to {
        Some(address) => TxKind::Call(address),
        None => TxKind::Create,
    };

    Ok(ArbTxEnvelope::from(TxRetry {
        chain_id: U256::from(
            submit
                .chain_id()
                .ok_or_else(|| "[ARBITRUM] submit-retryable missing chain_id".to_string())?,
        ),
        nonce: 0,
        from: submit.from(),
        gas_fee_cap: effective_base_fee,
        gas_limit: call.gas_limit,
        to: retry_to,
        value: call.retry_value,
        input: call.retry_data,
        ticket_id: submit.tx_hash(),
        refund_to: call.fee_refund_address,
        max_refund: available_refund,
        submission_fee_refund: submission_fee,
    }))
}

pub(crate) fn compute_submit_retryable_ticket_id(
    chain_id: u64,
    from: Address,
    call: &SubmitRetryableCall,
) -> B256 {
    let retry_to = match call.retry_to {
        Some(address) => TxKind::Call(address),
        None => TxKind::Create,
    };
    let chain_id = U256::from(chain_id);
    let gas_limit = U256::from(call.gas_limit);
    let request_id_bytes = call.request_id.0;
    let fields_len = chain_id.length()
        + request_id_bytes.length()
        + from.length()
        + call.l1_base_fee.length()
        + call.deposit_value.length()
        + call.gas_fee_cap.length()
        + gas_limit.length()
        + retry_to.length()
        + call.retry_value.length()
        + call.beneficiary.length()
        + call.max_submission_fee.length()
        + call.fee_refund_address.length()
        + call.retry_data.length();

    let mut out = Vec::new();
    out.push(ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE);
    Header {
        list: true,
        payload_length: fields_len,
    }
    .encode(&mut out);
    chain_id.encode(&mut out);
    request_id_bytes.encode(&mut out);
    from.encode(&mut out);
    call.l1_base_fee.encode(&mut out);
    call.deposit_value.encode(&mut out);
    call.gas_fee_cap.encode(&mut out);
    gas_limit.encode(&mut out);
    retry_to.encode(&mut out);
    call.retry_value.encode(&mut out);
    call.beneficiary.encode(&mut out);
    call.max_submission_fee.encode(&mut out);
    call.fee_refund_address.encode(&mut out);
    call.retry_data.encode(&mut out);
    keccak256(out)
}

fn retryable_escrow_address(ticket_id: B256) -> Address {
    let mut preimage = Vec::with_capacity(RETRYABLE_ESCROW_TAG.len() + ticket_id.len());
    preimage.extend_from_slice(RETRYABLE_ESCROW_TAG);
    preimage.extend_from_slice(ticket_id.as_slice());
    let hash = keccak256(preimage);
    Address::from_slice(&hash[12..])
}

fn retryable_submission_fee(calldata_len: usize, l1_base_fee: U256) -> U256 {
    let calldata_len_u128 = u128::try_from(calldata_len).unwrap_or(u128::MAX);
    let units = 1400_u128.saturating_add(6_u128.saturating_mul(calldata_len_u128));
    l1_base_fee * U256::from(units)
}

fn map_transfer_error(transfer_error: Option<TransferError>, label: &str) -> Result<(), String> {
    match transfer_error {
        None => Ok(()),
        Some(TransferError::OutOfFunds) => Err(format!("[ARBITRUM] {label} failed: out of funds")),
        Some(TransferError::OverflowPayment) => {
            Err(format!("[ARBITRUM] {label} failed: overflow payment"))
        }
        Some(TransferError::CreateCollision) => {
            Err(format!("[ARBITRUM] {label} failed: create collision"))
        }
    }
}

fn take_funds(available: &mut U256, requested: U256) -> U256 {
    let taken = core::cmp::min(*available, requested);
    *available = available.saturating_sub(taken);
    taken
}

fn word(input: &[u8], index: usize) -> Result<&[u8], String> {
    let start = index.saturating_mul(WORD_SIZE);
    let end = start.saturating_add(WORD_SIZE);
    input
        .get(start..end)
        .ok_or_else(|| format!("[ARBITRUM] missing ABI word at index {index}"))
}

fn word_to_u256(word: &[u8]) -> U256 {
    let mut out = [0_u8; 32];
    out.copy_from_slice(word);
    U256::from_be_bytes(out)
}

fn word_to_u64(word: &[u8]) -> Result<u64, String> {
    let value = word_to_u256(word);
    value
        .try_into()
        .map_err(|_| "[ARBITRUM] submit-retryable u64 field does not fit in u64".to_string())
}

fn word_to_usize(word: &[u8]) -> Result<usize, String> {
    let value = word_to_u256(word);
    let value_u64: u64 = value.try_into().map_err(|_| {
        "[ARBITRUM] submit-retryable byte length field does not fit in u64".to_string()
    })?;
    usize::try_from(value_u64)
        .map_err(|_| "[ARBITRUM] submit-retryable byte length does not fit in usize".to_string())
}

fn word_to_address(word: &[u8]) -> Address {
    Address::from_slice(&word[12..32])
}

fn word_to_optional_address(word: &[u8]) -> Option<Address> {
    if word.iter().all(|byte| *byte == 0) {
        None
    } else {
        Some(word_to_address(word))
    }
}

#[cfg(test)]
fn address_word(address: Address) -> [u8; 32] {
    let mut word = [0_u8; 32];
    word[12..].copy_from_slice(address.as_slice());
    word
}

#[cfg(test)]
mod ticket_id_parity {
    use super::*;
    use arb_alloy_consensus::transactions::submit_retryable::SubmitRetryableTx;
    use revm::primitives::{Address, B256, Bytes, TxKind, U256};

    #[test]
    fn compute_matches_alloy_tx_hash() {
        let sender = Address::with_last_byte(0x11);
        let retry_to = Address::with_last_byte(0x22);
        let request_id = B256::with_last_byte(0x33);
        let submit = SubmitRetryableTx::new(
            U256::from(42161_u64),
            request_id,
            sender,
            U256::from(7_u64),
            U256::from(200_000_000_000_000_u64),
            U256::from(1_000_000_000_u64),
            U256::from(100_000_u64),
            TxKind::Call(retry_to),
            U256::ZERO,
            retry_to,
            U256::from(100_000_000_000_000_u64),
            retry_to,
            Bytes::new(),
        );
        let alloy_hash = submit.tx_hash();
        let decoded = decode_submit_retryable_calldata(submit.input().as_ref()).unwrap();
        let computed = compute_submit_retryable_ticket_id(42161, sender, &decoded);
        assert_eq!(computed, alloy_hash, "compute={computed:?} alloy={alloy_hash:?}");
    }
}
