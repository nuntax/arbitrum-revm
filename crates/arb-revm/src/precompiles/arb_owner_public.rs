use super::*;
use crate::arb_journal::{ArbJournal, ArbPrecompileCtx, MeteredJournal};
use crate::storage::{stylus_param_layout as layout, unpack_uint};
use revm::interpreter::InstructionResult;
use revm::primitives::{B256, Bytes, Log, keccak256};

pub(super) fn run_arb_owner_public<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let call = match ArbOwnerPublic::ArbOwnerPublicCalls::abi_decode(input) {
        Ok(c) => c,
        Err(_) => return gated_revert_result(gas_limit),
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
                    );
                }
            };
            let max_fragments = unpack_uint(
                &word,
                layout::MAX_FRAGMENT_COUNT.0,
                layout::MAX_FRAGMENT_COUNT.1,
            ) as u8;
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(u16::from(max_fragments),)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::rectifyChainOwner(c) => {
            let mut journal = MeteredJournal::new(ctx.journal_mut());
            match state.chain_owners.rectify_mapping(c.account, &mut journal) {
                Ok(()) => {
                    let mut account_topic = [0_u8; 32];
                    account_topic[12..].copy_from_slice(c.account.as_slice());
                    journal.emit_log(Log::new_unchecked(
                        ARB_OWNER_PUBLIC,
                        vec![keccak256("ChainOwnerRectified(address)")],
                        Bytes::copy_from_slice(B256::from(account_topic).as_slice()),
                    ));
                    let mut result = ok_result(gas_limit, vec![]);
                    if !result.gas.record_regular_cost(journal.burned) {
                        result.result = InstructionResult::OutOfGas;
                        result.output = Bytes::new();
                    }
                    result
                }
                Err(e) => {
                    let mut result = revert_result(
                        gas_limit,
                        &format!("ArbOwnerPublic: rectifyChainOwner error: {e}"),
                    );
                    if !result.gas.record_regular_cost(journal.burned) {
                        result.result = InstructionResult::OutOfGas;
                        result.output = Bytes::new();
                    }
                    result
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_core::sol_types::SolCall;
    use arbitrum_alloy_precompiles::addresses::ARB_OWNER_PUBLIC;
    use revm::{
        context_interface::{ContextTr, JournalTr},
        database_interface::EmptyDB,
        interpreter::InstructionResult,
        primitives::{Address, address, keccak256},
    };

    use super::{ArbOwnerPublic, run_arb_owner_public};
    use crate::{
        ArbosState,
        api::default_ctx::{ArbContext, DefaultArb},
    };

    const OWNER_1: Address = address!("d345e41ae2cb00311956aa7109fc801ae8c81a52");
    const OWNER_2: Address = address!("98e4db7e07e584f89a2f6043e7b7c89dc27769ed");
    const OWNER_3: Address = address!("cf57572261c7c2bcf21ffd220ea7d1a27d40a827");

    #[test]
    fn rectify_chain_owner_repairs_history_and_emits_canonical_event() {
        let mut ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();
        let owners = &ArbosState::open().chain_owners;
        for owner in [OWNER_1, OWNER_2, OWNER_3] {
            owners.add(owner, ctx.journal_mut()).unwrap();
        }
        owners.remove(OWNER_1, 10, ctx.journal_mut()).unwrap();
        owners.remove(OWNER_2, 10, ctx.journal_mut()).unwrap();
        owners.clear_list(ctx.journal_mut()).unwrap();

        let input = ArbOwnerPublic::rectifyChainOwnerCall { account: OWNER_3 }.abi_encode();
        let result = run_arb_owner_public(&mut ctx, &input, 100_000);

        assert_eq!(result.result, InstructionResult::Return);
        assert!(result.output.is_empty());
        assert_eq!(result.gas.total_gas_spent(), 70_806);
        assert_eq!(
            owners.all_members(ctx.journal_mut()).unwrap(),
            vec![OWNER_3]
        );

        let logs = ctx.journal_mut().logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].address, ARB_OWNER_PUBLIC);
        assert_eq!(
            logs[0].data.topics(),
            &[keccak256("ChainOwnerRectified(address)")]
        );
        let mut expected_data = [0_u8; 32];
        expected_data[12..].copy_from_slice(OWNER_3.as_slice());
        assert_eq!(logs[0].data.data.as_ref(), expected_data);

        assert_eq!(owners.size.get(ctx.journal_mut()).unwrap(), 1);
    }
}
