use crate::api::exec::ArbContextTr;
use revm::{
    context_interface::{ContextTr, JournalTr, Transaction, journaled_state::TransferError},
    primitives::TxKind,
};

/// Applies Nitro-style Arbitrum deposit transaction semantics:
/// 1. Mint `value` into `from`.
/// 2. Transfer `value` from `from` to `to`.
///
/// This intentionally bypasses EVM call execution and gas accounting.
pub(crate) fn apply_deposit_tx<CTX: ArbContextTr>(ctx: &mut CTX) -> Result<(), String> {
    let from = ctx.tx().caller();
    let to = match ctx.tx().kind() {
        TxKind::Call(target) => target,
        TxKind::Create => return Err("[ARBITRUM] deposit tx must be a call".into()),
    };
    let value = ctx.tx().value();

    let journal = ctx.journal_mut();
    journal
        .balance_incr(from, value)
        .map_err(|err| format!("[ARBITRUM] failed to mint deposit value: {err}"))?;

    let transfer_error = journal
        .transfer(from, to, value)
        .map_err(|err| format!("[ARBITRUM] failed to apply deposit transfer: {err}"))?;

    match transfer_error {
        None => Ok(()),
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
