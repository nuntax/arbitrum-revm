use crate::api::exec::ArbContextTr;
use crate::storage::ArbosState;
use crate::transaction::ArbTxTr;
use revm::{
    context_interface::{
        Block, ContextTr, JournalTr, Transaction,
        journaled_state::{TransferError, account::JournaledAccountTr},
    },
    primitives::{Address, B256, TxKind, U256, keccak256},
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
    // A contract-creation retry (retryTo = nil) is run by revm as a CREATE frame, which itself
    // bumps the caller nonce and derives the deploy address from the PRE-increment nonce. Bumping
    // here too would double-increment and shift the CREATE address by one nonce (Nitro does a
    // single increment; the address uses the sender's nonce as-is). A CALL retry still needs the
    // manual bump below: protocol txs bypass revm's tx-level pre-execution and the CALL frame does
    // not bump the caller nonce.
    let is_create = matches!(tx.kind(), TxKind::Create);
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
    if !is_create {
        let mut caller_account = journal
            .load_account_mut(from)
            .map_err(|err| format!("[ARBITRUM] failed to load retry sender account: {err}"))?;
        if !caller_account.data.bump_nonce() {
            return Err("[ARBITRUM] retry tx sender nonce overflow".to_string());
        }
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

    // Was this ticket's escrow destructed by a same-block submit (pre-Stylus, zero callvalue)?
    // If so, a *successful* redeem resurrects it as a present-but-empty "zombie"; a failed one
    // leaves it absent. Read before borrowing the journal (see submit_retryable_tx.rs).
    let zombie_eligible = ctx.chain().pending_zombie_escrow_tickets.contains(&ticket_id);

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

        // Pre-Stylus zombie escrow (see submit_retryable_tx.rs / ArbChainContext): a
        // zero-callvalue retryable whose escrow was destructed by the same-block submit is
        // resurrected as a present-but-empty account, and go-ethereum keeps that zombie only
        // when this redeem SUCCEEDS. Materialize it now by marking the escrow Created + Touched,
        // so revm-database's `apply_account_state` writes it via `newly_created` *before* the
        // EIP-161 empty-clear, matching canonical (escrow present, nonce 0, balance 0, empty
        // code/storage). `ArbContextTr` pins `Journal::State = EvmState`, so `evm_state_mut()`
        // yields the raw account map; `create_account_checkpoint` is unusable here as it forces
        // nonce 1 under SpuriousDragon, whereas the canonical zombie escrow has nonce 0.
        if zombie_eligible {
            let escrow = retryable_escrow_address(ticket_id);
            journal.load_account(escrow).map_err(|err| {
                format!("[ARBITRUM] failed to load retryable escrow for zombie materialization: {err}")
            })?;
            if let Some(escrow_account) = journal.evm_state_mut().get_mut(&escrow) {
                escrow_account.mark_created();
                escrow_account.mark_touch();
            }
        }
    } else if callvalue > U256::ZERO {
        let escrow = retryable_escrow_address(ticket_id);
        let transfer_error = journal.transfer(from, escrow, callvalue).map_err(|err| {
            format!("[ARBITRUM] failed to return retryable callvalue to escrow: {err}")
        })?;
        map_transfer_error(transfer_error, "retry callvalue return transfer")?;
    }
    // A failed zero-callvalue redeem intentionally does nothing: the escrow was never
    // materialized (the submit only recorded eligibility), so it stays absent, matching Nitro,
    // where the failed redeem's resurrected zombie does not survive `Finalise`.

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
