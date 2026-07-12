use super::{ArbosState, gated_revert_result, ok_result, revert_result};
use crate::arb_journal::{ArbCall, ArbJournal, ArbPrecompileCtx};
use crate::storage::StorageSpace;
use alloy_core::sol;
use alloy_core::sol_types::{SolInterface, SolValue};
use revm::interpreter::InterpreterResult;
use revm::primitives::{Address, B256, Bytes, Log, U256, keccak256};

/// ArbOS version at which ArbFilteredTransactionsManager becomes active.
/// Nitro reference: params.ArbosVersion_TransactionFiltering = ArbosVersion_60 = 60.
const ARBOS_VERSION_TRANSACTION_FILTERING: u64 = 60;

/// Dedicated backing account for the filtered-transactions KV store.
/// Nitro: `types.FilteredTransactionsStateAddress`. The account is created (nonce=1) at the ArbOS-60
/// upgrade (see internal_tx.rs); the entries live directly under it with an empty storage subspace.
const FILTERED_TRANSACTIONS_STATE_ADDRESS: Address = Address::new([
    0xA4, 0xB0, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01,
]);

/// Nitro `filteredTransactions.presentHash` = common.BytesToHash([]byte{1}) = 1.
const PRESENT_VALUE: U256 = U256::from_limbs([1, 0, 0, 0]);

sol! {
    interface ArbFilteredTransactionsManager {
        event FilteredTransactionAdded(bytes32 indexed txHash);
        event FilteredTransactionDeleted(bytes32 indexed txHash);
        function addFilteredTransaction(bytes32 txHash) external;
        function deleteFilteredTransaction(bytes32 txHash) external;
        function isTransactionFiltered(bytes32 txHash) external view returns (bool);
    }
}

/// ArbFilteredTransactionsManager, filtered tx list management for authorised callers.
///
/// Active from ArbOS v60 (Nitro precompiles/ArbFilteredTransactionsManager.go). Before that version
/// Nitro returns empty bytes (the precompile "doesn't exist yet"). add/delete require the immediate
/// caller to be a registered transaction filterer (an ArbOwner-managed set); isTransactionFiltered is
/// a public view. Entries are a flat KV map on the dedicated state account: key = the tx hash mapped
/// through ArbOS paging (`StorageSpace::slot_for_hash`, empty subspace = Nitro's `mapAddress`), value
/// = 1. The state write is NOT metered into frame gas (raw ArbOS journal, like ArbOwner); the
/// framework's per-call `arbos_call_extra_gas` is the whole precompile cost.
pub(super) fn run_arb_filtered_transactions_manager<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
    call: &ArbCall,
) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let state = ArbosState::open();
    let j = ctx.journal_mut();
    let arbos_version = match state.arbos_version.get(j) {
        Ok(v) => v,
        Err(e) => {
            return revert_result(
                gas_limit,
                &format!("ArbFilteredTransactionsManager: storage error: {e}"),
            );
        }
    };

    // Nitro behaviour: return empty bytes before the version gate is reached.
    if arbos_version < ARBOS_VERSION_TRANSACTION_FILTERING {
        return ok_result(gas_limit, vec![]);
    }

    let parsed =
        match ArbFilteredTransactionsManager::ArbFilteredTransactionsManagerCalls::abi_decode(input)
        {
            Ok(c) => c,
            Err(_) => return gated_revert_result(gas_limit),
        };

    // `hasAccess`: the immediate caller must be a registered transaction filterer. Nitro returns
    // `c.BurnOut()` (consume all gas, revert) when it isn't; mirror with an all-gas revert.
    macro_rules! require_filterer {
        () => {
            match state.transaction_filterers.is_member(call.caller, j) {
                Ok(true) => {}
                Ok(false) => return gated_revert_result(gas_limit),
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbFilteredTransactionsManager: access check error: {e}"),
                    );
                }
            }
        };
    }

    let filtered = StorageSpace::new(FILTERED_TRANSACTIONS_STATE_ADDRESS);

    match parsed {
        ArbFilteredTransactionsManager::ArbFilteredTransactionsManagerCalls::addFilteredTransaction(
            c,
        ) => {
            require_filterer!();
            if let Err(e) = filtered.set(c.txHash, PRESENT_VALUE, j) {
                return revert_result(
                    gas_limit,
                    &format!("ArbFilteredTransactionsManager: add error: {e}"),
                );
            }
            j.emit_log(Log::new_unchecked(
                call.bytecode_address,
                vec![
                    keccak256("FilteredTransactionAdded(bytes32)"),
                    B256::from(c.txHash),
                ],
                Bytes::new(),
            ));
            ok_result(gas_limit, vec![])
        }
        ArbFilteredTransactionsManager::ArbFilteredTransactionsManagerCalls::deleteFilteredTransaction(
            c,
        ) => {
            require_filterer!();
            // Nitro `store.Clear` writes 0, which makes geth delete the trie entry (not store zeros).
            if let Err(e) = filtered.set(c.txHash, U256::ZERO, j) {
                return revert_result(
                    gas_limit,
                    &format!("ArbFilteredTransactionsManager: delete error: {e}"),
                );
            }
            j.emit_log(Log::new_unchecked(
                call.bytecode_address,
                vec![
                    keccak256("FilteredTransactionDeleted(bytes32)"),
                    B256::from(c.txHash),
                ],
                Bytes::new(),
            ));
            ok_result(gas_limit, vec![])
        }
        ArbFilteredTransactionsManager::ArbFilteredTransactionsManagerCalls::isTransactionFiltered(
            c,
        ) => {
            let value = match filtered.get(c.txHash, j) {
                Ok(v) => v.data,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbFilteredTransactionsManager: read error: {e}"),
                    );
                }
            };
            let is_filtered = value == PRESENT_VALUE;
            ok_result(gas_limit, SolValue::abi_encode(&(is_filtered,)))
        }
    }
}
