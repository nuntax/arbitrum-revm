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
    // The ArbOS version is a field Nitro caches when it opens the state, so reading it is free.
    // Every other read below goes through Nitro's burner at 800 gas (`StorageReadCost`), which
    // `MeteredJournal` reproduces; the state-open read itself is added by the dispatcher.
    let arbos_version = match state.arbos_version.get(ctx.journal_mut()) {
        Ok(v) => v,
        Err(e) => return fatal_result(gas_limit, &format!("ArbOwnerPublic: storage error: {e}")),
    };
    let mut journal = MeteredJournal::new(ctx.journal_mut());
    let j = &mut journal;

    macro_rules! get {
        ($expr:expr) => {
            match $expr {
                Ok(v) => ok_result(
                    gas_limit,
                    alloy_core::sol_types::SolValue::abi_encode_params(&(v,)),
                ),
                Err(e) => {
                    return fatal_result(gas_limit, &format!("ArbOwnerPublic: storage error: {e}"));
                }
            }
        };
    }

    let mut result = match call {
        ArbOwnerPublic::ArbOwnerPublicCalls::getCollectTips(_) => {
            get!(state.collect_tips.get(j).map(|v| v != 0))
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getAllChainOwners(_) => {
            get!(state.chain_owners.all_members(j))
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::isChainOwner(c) => {
            get!(state.chain_owners.is_member(c.addr, j))
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::isNativeTokenOwner(c) => {
            get!(state.native_token_owners.is_member(c.addr, j))
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getAllNativeTokenOwners(_) => {
            get!(state.native_token_owners.all_members(j))
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getNativeTokenManagementFrom(_) => {
            get!(state.native_token_enabled_from_timestamp.get(j))
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getTransactionFilteringFrom(_) => {
            get!(state.transaction_filtering_enabled_from_timestamp.get(j))
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::isTransactionFilterer(c) => {
            get!(state.transaction_filterers.is_member(c.filterer, j))
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getAllTransactionFilterers(_) => {
            get!(state.transaction_filterers.all_members(j))
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getFilteredFundsRecipient(_) => {
            get!(state.filtered_funds_recipient.get(j))
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getNetworkFeeAccount(_) => {
            get!(state.network_fee_account.get(j))
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getInfraFeeAccount(_) => {
            // Nitro: before ArbOS 6 the public getter answers with the network fee account.
            if arbos_version < 6 {
                get!(state.network_fee_account.get(j))
            } else {
                get!(state.infra_fee_account.get(j))
            }
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getBrotliCompressionLevel(_) => {
            get!(state.brotli_compression_level.get(j))
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getScheduledUpgrade(_) => {
            let scheduled = state.upgrade_version.get(j).and_then(|version| {
                state
                    .upgrade_timestamp
                    .get(j)
                    .map(|timestamp| (version, timestamp))
            });
            match scheduled {
                Ok((version, timestamp)) => {
                    let (version, timestamp) = if arbos_version >= version {
                        (0_u64, 0_u64)
                    } else {
                        (version, timestamp)
                    };
                    ok_result(
                        gas_limit,
                        alloy_core::sol_types::SolValue::abi_encode_params(&(version, timestamp)),
                    )
                }
                Err(e) => {
                    return fatal_result(gas_limit, &format!("ArbOwnerPublic: storage error: {e}"));
                }
            }
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::isCalldataPriceIncreaseEnabled(_) => {
            get!(state.features.is_calldata_price_increase_enabled(j))
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getParentGasFloorPerToken(_) => {
            get!(state.l1_pricing.gas_floor_per_token.get(j))
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::getMaxStylusContractFragments(_) => {
            // Nitro reads this through `Programs().Params()`, which bills its physical reads as a
            // single warm access (100 gas) rather than 800 per slot.
            let word = match state.programs.read_params_word(j.inner_mut()) {
                Ok(w) => w,
                Err(e) => {
                    return fatal_result(gas_limit, &format!("ArbOwnerPublic: storage error: {e}"));
                }
            };
            j.charge(100);
            let max_fragments = unpack_uint(
                &word,
                layout::MAX_FRAGMENT_COUNT.0,
                layout::MAX_FRAGMENT_COUNT.1,
            ) as u8;
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode_params(&(u16::from(max_fragments),)),
            )
        }
        ArbOwnerPublic::ArbOwnerPublicCalls::rectifyChainOwner(c) => {
            match state.chain_owners.rectify_mapping(c.ownerToRectify, j) {
                Ok(()) => {
                    let mut account_topic = [0_u8; 32];
                    account_topic[12..].copy_from_slice(c.ownerToRectify.as_slice());
                    j.emit_log(Log::new_unchecked(
                        ARB_OWNER_PUBLIC,
                        vec![keccak256("ChainOwnerRectified(address)")],
                        Bytes::copy_from_slice(B256::from(account_topic).as_slice()),
                    ));
                    ok_result(gas_limit, vec![])
                }
                // Nitro's `RectifyMapping` reports a plain Go error ("not an owner", "already
                // correctly mapped"): an empty revert.
                Err(_) => ordinary_error_result(gas_limit),
            }
        }
    };
    if !result.gas.record_regular_cost(journal.burned) {
        result.result = InstructionResult::OutOfGas;
        result.output = Bytes::new();
    }
    result
}

#[cfg(test)]
mod tests {
    use alloy_core::sol_types::{SolCall, SolValue};
    use arbitrum_alloy_precompiles::addresses::ARB_OWNER_PUBLIC;
    use revm::{
        context_interface::{ContextTr, JournalTr},
        database_interface::EmptyDB,
        interpreter::InstructionResult,
        primitives::{Address, U256, address, keccak256},
    };

    use super::{ArbOwnerPublic, ArbPrecompilesEnum, run_arb_owner_public};
    use crate::{
        ArbosState,
        api::default_ctx::{ArbContext, DefaultArb},
        arb_journal::ArbCall,
    };

    const OWNER_1: Address = address!("d345e41ae2cb00311956aa7109fc801ae8c81a52");
    const OWNER_2: Address = address!("98e4db7e07e584f89a2f6043e7b7c89dc27769ed");
    const OWNER_3: Address = address!("cf57572261c7c2bcf21ffd220ea7d1a27d40a827");

    #[test]
    fn get_scheduled_upgrade_charges_both_storage_reads() {
        let mut ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();
        let state = ArbosState::open();
        state.arbos_version.set(61, ctx.journal_mut()).unwrap();
        state.upgrade_version.set(62, ctx.journal_mut()).unwrap();
        state
            .upgrade_timestamp
            .set(1_234, ctx.journal_mut())
            .unwrap();

        let input = ArbOwnerPublic::getScheduledUpgradeCall {}.abi_encode();
        let call = ArbCall {
            input: &input,
            gas_limit: 100_000,
            caller: Address::ZERO,
            value: U256::ZERO,
            bytecode_address: ARB_OWNER_PUBLIC,
            acting_address: ARB_OWNER_PUBLIC,
            is_static: true,
        };
        let result = ArbPrecompilesEnum::ArbOwnerPublic.run_dispatch(&mut ctx, &call);

        assert_eq!(result.result, InstructionResult::Return);
        assert_eq!(
            <(u64, u64)>::abi_decode(&result.output).unwrap(),
            (62, 1_234)
        );
        // OpenArbosState (800), upgrade version and timestamp reads (2 * 800), and two output
        // words (2 * 3).
        assert_eq!(result.gas.total_gas_spent(), 2_406);
    }

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

        let input = ArbOwnerPublic::rectifyChainOwnerCall {
            ownerToRectify: OWNER_3,
        }
        .abi_encode();
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
