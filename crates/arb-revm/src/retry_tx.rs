use crate::api::exec::ArbContextTr;
use crate::storage::ArbosState;
use crate::transaction::ArbTxTr;
use revm::{
    context_interface::{
        Block, ContextTr, JournalTr, Transaction,
        journaled_state::{TransferError, account::JournaledAccountTr},
    },
    primitives::{Address, B256, U256, keccak256},
};

const RETRYABLE_ESCROW_TAG: &[u8] = b"retryable escrow";

/// Nitro-style pre-execution hook for `ArbitrumRetryTx`:
/// move retryable callvalue from escrow into retry sender before execution.
pub(crate) fn apply_retry_tx_pre_execution<CTX: ArbContextTr>(ctx: &mut CTX) -> Result<(), String> {
    let tx = ctx.tx();
    let retry_meta = tx
        .retry_meta()
        .ok_or_else(|| "[ARBITRUM] retry tx missing retry metadata".to_string())?;
    let ticket_id = retry_meta.ticket_id;
    let from = tx.caller();
    let current_timestamp: u64 = ctx
        .block()
        .timestamp()
        .try_into()
        .map_err(|_| "[ARBITRUM] block.timestamp does not fit in u64".to_string())?;

    let arbos_state = ArbosState::open();
    let journal = ctx.journal_mut();
    let retryable = arbos_state.retryables.retryable(ticket_id);
    let exists = retryable
        .exists(current_timestamp, journal)
        .map_err(|err| format!("[ARBITRUM] failed to read retryable existence: {err}"))?;
    if !exists {
        return Err(format!(
            "[ARBITRUM] retryable with ticket id 0x{} not found",
            hex::encode(ticket_id)
        ));
    }

    let callvalue = retryable
        .callvalue
        .get(journal)
        .map_err(|err| format!("[ARBITRUM] failed to read retryable callvalue: {err}"))?;
    if callvalue > U256::ZERO {
        let escrow = retryable_escrow_address(ticket_id);
        let transfer_error = journal
            .transfer(escrow, from, callvalue)
            .map_err(|err| format!("[ARBITRUM] failed to transfer retryable callvalue: {err}"))?;
        map_transfer_error(transfer_error, "retry callvalue transfer")?;
    }
    let mut caller_account = journal
        .load_account_mut(from)
        .map_err(|err| format!("[ARBITRUM] failed to load retry sender account: {err}"))?;
    if !caller_account.data.bump_nonce() {
        return Err("[ARBITRUM] retry tx sender nonce overflow".to_string());
    }

    Ok(())
}

/// Nitro-style post-execution hook for `ArbitrumRetryTx`.
///
/// On success, delete retryable. On failure, return callvalue to escrow.
pub(crate) fn apply_retry_tx_post_execution<CTX: ArbContextTr>(
    ctx: &mut CTX,
    success: bool,
) -> Result<(), String> {
    let tx = ctx.tx();
    let retry_meta = tx
        .retry_meta()
        .ok_or_else(|| "[ARBITRUM] retry tx missing retry metadata".to_string())?;
    let ticket_id = retry_meta.ticket_id;
    let from = tx.caller();

    let arbos_state = ArbosState::open();
    let journal = ctx.journal_mut();
    let retryable = arbos_state.retryables.retryable(ticket_id);
    let callvalue = retryable
        .callvalue
        .get(journal)
        .map_err(|err| format!("[ARBITRUM] failed to read retryable callvalue: {err}"))?;

    if success {
        let _ = arbos_state
            .retryables
            .delete_retryable(ticket_id, journal)
            .map_err(|err| format!("[ARBITRUM] failed to delete retryable after success: {err}"))?;
    } else if callvalue > U256::ZERO {
        let escrow = retryable_escrow_address(ticket_id);
        let transfer_error = journal.transfer(from, escrow, callvalue).map_err(|err| {
            format!("[ARBITRUM] failed to return retryable callvalue to escrow: {err}")
        })?;
        map_transfer_error(transfer_error, "retry callvalue return transfer")?;
    }

    Ok(())
}

fn retryable_escrow_address(ticket_id: B256) -> Address {
    let mut preimage = Vec::with_capacity(RETRYABLE_ESCROW_TAG.len() + ticket_id.len());
    preimage.extend_from_slice(RETRYABLE_ESCROW_TAG);
    preimage.extend_from_slice(ticket_id.as_slice());
    let hash = keccak256(preimage);
    Address::from_slice(&hash[12..])
}

fn map_transfer_error(transfer_error: Option<TransferError>, label: &str) -> Result<(), String> {
    match transfer_error {
        None => Ok(()),
        Some(TransferError::OutOfFunds) => Err(format!("{label} failed: out of funds")),
        Some(TransferError::OverflowPayment) => Err(format!("{label} failed: overflow payment")),
        Some(TransferError::CreateCollision) => Err(format!("{label} failed: create collision")),
    }
}
