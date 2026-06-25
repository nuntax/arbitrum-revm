use super::*;
use crate::storage::{stylus_param_layout as layout, unpack_uint};

pub(super) fn run_arb_owner_public<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
) -> InterpreterResult
where
    CTX: ContextTr<Journal: JournalTr>,
{
    let call = match ArbOwnerPublic::ArbOwnerPublicCalls::abi_decode(input) {
        Ok(c) => c,
        Err(e) => {
            return revert_result(gas_limit, &format!("ArbOwnerPublic: invalid calldata: {e}"));
        }
    };

    let state = ArbosState::open();

    match call {
        ArbOwnerPublic::ArbOwnerPublicCalls::getAllChainOwners(_) => {
            let owners = match state.chain_owners.all_members(ctx.journal_mut()) {
                Ok(o) => o,
                Err(e) => return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(owners,)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::isChainOwner(c) => {
            let is_owner = match state.chain_owners.is_member(c.account, ctx.journal_mut()) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(is_owner,)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::isNativeTokenOwner(c) => {
            let is_owner = match state
                .native_token_owners
                .is_member(c.account, ctx.journal_mut())
            {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(is_owner,)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getAllNativeTokenOwners(_) => {
            let owners = match state.native_token_owners.all_members(ctx.journal_mut()) {
                Ok(o) => o,
                Err(e) => return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(owners,)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getNativeTokenManagementFrom(_) => {
            let ts = match state
                .native_token_enabled_from_timestamp
                .get(ctx.journal_mut())
            {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(ts,)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getTransactionFilteringFrom(_) => {
            let ts = match state
                .transaction_filtering_enabled_from_timestamp
                .get(ctx.journal_mut())
            {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(ts,)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::isTransactionFilterer(c) => {
            let is_filterer = match state
                .transaction_filterers
                .is_member(c.filterer, ctx.journal_mut())
            {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(is_filterer,)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getAllTransactionFilterers(_) => {
            let filterers = match state.transaction_filterers.all_members(ctx.journal_mut()) {
                Ok(f) => f,
                Err(e) => return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(filterers,)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getFilteredFundsRecipient(_) => {
            let recipient = match state.filtered_funds_recipient.get(ctx.journal_mut()) {
                Ok(a) => a,
                Err(e) => return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(recipient,)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getNetworkFeeAccount(_) => {
            let account = match state.network_fee_account.get(ctx.journal_mut()) {
                Ok(a) => a,
                Err(e) => return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(account,)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getInfraFeeAccount(_) => {
            let arbos_version = match state.arbos_version.get(ctx.journal_mut()) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}")),
            };
            let account = if arbos_version < 6 {
                match state.network_fee_account.get(ctx.journal_mut()) {
                    Ok(a) => a,
                    Err(e) => {
                        return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}"));
                    }
                }
            } else {
                match state.infra_fee_account.get(ctx.journal_mut()) {
                    Ok(a) => a,
                    Err(e) => {
                        return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}"));
                    }
                }
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(account,)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getBrotliCompressionLevel(_) => {
            let level = match state.brotli_compression_level.get(ctx.journal_mut()) {
                Ok(a) => a,
                Err(e) => return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(level,)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getScheduledUpgrade(_) => {
            let arbos_version = match state.arbos_version.get(ctx.journal_mut()) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}")),
            };
            let version = match state.upgrade_version.get(ctx.journal_mut()) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}")),
            };
            let timestamp = match state.upgrade_timestamp.get(ctx.journal_mut()) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}")),
            };
            let (version, timestamp) = if arbos_version >= version {
                (0_u64, 0_u64)
            } else {
                (version, timestamp)
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(version, timestamp)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::isCalldataPriceIncreaseEnabled(_) => {
            let enabled = match state
                .features
                .is_calldata_price_increase_enabled(ctx.journal_mut())
            {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(enabled,)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getParentGasFloorPerToken(_) => {
            let floor = match state.l1_pricing.gas_floor_per_token.get(ctx.journal_mut()) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbOwnerPublic: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(floor,)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getMaxStylusContractFragments(_) => {
            let word = match state.programs.read_params_word(ctx.journal_mut()) {
                Ok(w) => w,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwnerPublic: getMaxStylusContractFragments error: {e}"),
                    )
                }
            };
            let max_fragments =
                unpack_uint(&word, layout::MAX_FRAGMENT_COUNT.0, layout::MAX_FRAGMENT_COUNT.1)
                    as u8;
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(u16::from(max_fragments),)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::rectifyChainOwner(_) => revert_result(
            gas_limit,
            "ArbOwnerPublic: rectifyChainOwner not yet implemented",
        ),
    }
}
