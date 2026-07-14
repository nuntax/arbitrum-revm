use crate::api::exec::ArbContextTr;
use crate::transaction::ArbTxTr;
use crate::transaction_filter::{filtered_funds_recipient_or_default, is_tx_hash_filtered};
use revm::{
    context_interface::{ContextTr, JournalTr, Transaction, journaled_state::TransferError},
    primitives::{TxKind, keccak256},
};

/// Result of applying a deposit: whether it was redirected by the on-chain transaction filter.
pub(crate) enum DepositOutcome {
    /// Normal deposit: value credited to the intended recipient. Success receipt.
    Applied,
    /// The deposit's hash was pre-registered by a transaction filterer, so the value was redirected
    /// to the filtered-funds recipient. Nitro records a failed tx (status 0, gasUsed 0) but keeps
    /// the redirected transfer.
    Filtered,
}

/// Applies Nitro-style Arbitrum deposit transaction semantics:
/// 1. Mint `value` into `from`.
/// 2. Transfer `value` from `from` to `to`.
///
/// When the deposit's transaction hash is in the on-chain filter (ArbOS >= 60), the recipient is
/// redirected to the filtered-funds recipient (or the network fee account as fallback) and the tx is
/// reported as filtered, mirroring Nitro `tx_processor.go` StartTxHook for `ArbitrumDepositTx`.
///
/// This intentionally bypasses EVM call execution and gas accounting.
pub(crate) fn apply_deposit_tx<CTX: ArbContextTr>(ctx: &mut CTX) -> Result<DepositOutcome, String> {
    let from = ctx.tx().caller();
    let mut to = match ctx.tx().kind() {
        TxKind::Call(target) => target,
        TxKind::Create => return Err("[ARBITRUM] deposit tx must be a call".into()),
    };
    let value = ctx.tx().value();

    // Filtered-transaction enforcement: a deposit whose hash was pre-registered via
    // ArbFilteredTransactionsManager has its funds diverted to the filtered-funds recipient. The
    // filter read is free (no gas), matching Nitro's `IsFilteredFree`.
    let tx_hash = ctx
        .tx()
        .tx_hash()
        .or_else(|| ctx.tx().encoded_2718_bytes().map(keccak256));
    let mut filtered = false;
    if let Some(tx_hash) = tx_hash {
        let is_filtered = is_tx_hash_filtered(tx_hash, None, ctx.journal_mut())
            .map_err(|e| format!("[ARBITRUM] deposit filter read failed: {e}"))?;
        if is_filtered {
            to = filtered_funds_recipient_or_default(ctx.journal_mut())
                .map_err(|e| format!("[ARBITRUM] filtered-funds recipient read failed: {e}"))?;
            filtered = true;
        }
    }

    let journal = ctx.journal_mut();
    journal
        .balance_incr(from, value)
        .map_err(|err| format!("[ARBITRUM] failed to mint deposit value: {err}"))?;

    let transfer_error = journal
        .transfer(from, to, value)
        .map_err(|err| format!("[ARBITRUM] failed to apply deposit transfer: {err}"))?;

    match transfer_error {
        None => Ok(if filtered {
            DepositOutcome::Filtered
        } else {
            DepositOutcome::Applied
        }),
        Some(TransferError::OutOfFunds) => {
            Err("[ARBITRUM] deposit transfer failed: out of funds".into())
        }
        Some(TransferError::OverflowPayment) => {
            Err("[ARBITRUM] deposit transfer failed: overflow payment".into())
        }
        Some(TransferError::CreateCollision) => {
            Err("[ARBITRUM] deposit transfer failed: create collision".into())
        }
    }
}
