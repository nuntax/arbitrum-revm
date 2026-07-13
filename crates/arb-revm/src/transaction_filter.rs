use crate::{
    arb_journal::ArbJournal,
    constants::FILTERED_TRANSACTIONS_STATE_ADDRESS,
    storage::{ArbosMetadataOffset, StorageSpace},
};
use revm::primitives::{Address, B256, U256};

/// ArbOS version that introduced the on-chain transaction filter.
pub(crate) const ARBOS_VERSION_TRANSACTION_FILTERING: u64 = 60;

/// Returns whether `tx_hash` is registered in ArbOS's on-chain transaction filter.
///
/// Nitro's `IsFilteredFree` returns false before ArbOS 60 because the backing state is absent.
/// The caller deliberately performs this through the raw ArbOS journal, so the read carries no
/// EVM gas cost.
pub(crate) fn is_tx_hash_filtered<J: ArbJournal>(
    tx_hash: B256,
    journal: &mut J,
) -> Result<bool, String> {
    // This check runs on every regular and retry transaction. Opening the whole typed ArbOS tree
    // derives slots for every unrelated subspace; only the version metadata word is required here.
    if StorageSpace::arbos()
        .storage_backed::<u64>(ArbosMetadataOffset::Version as u8)
        .get(journal)
        .map_err(|err| format!("failed to read ArbOS version: {err:?}"))?
        < ARBOS_VERSION_TRANSACTION_FILTERING
    {
        return Ok(false);
    }
    Ok(StorageSpace::new(FILTERED_TRANSACTIONS_STATE_ADDRESS)
        .get(tx_hash, journal)
        .map_err(|err| format!("failed to read transaction filter: {err:?}"))?
        .data
        == U256::ONE)
}

/// Nitro `FilteredFundsRecipientOrDefault`: use the configured recipient, falling back to the
/// network-fee account when it is unset.
pub(crate) fn filtered_funds_recipient_or_default<J: ArbJournal>(
    journal: &mut J,
) -> Result<Address, String> {
    let arbos = StorageSpace::arbos();
    let recipient = arbos
        .storage_backed::<Address>(ArbosMetadataOffset::FilteredFundsRecipient as u8)
        .get(journal)
        .map_err(|err| format!("failed to read filtered-funds recipient: {err:?}"))?;
    if recipient == Address::ZERO {
        arbos
            .storage_backed::<Address>(ArbosMetadataOffset::NetworkFeeAccount as u8)
            .get(journal)
            .map_err(|err| format!("failed to read network-fee account: {err:?}"))
    } else {
        Ok(recipient)
    }
}
