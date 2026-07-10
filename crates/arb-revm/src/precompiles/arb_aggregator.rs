use super::*;
use crate::arb_journal::{ArbCall, ArbJournal, ArbPrecompileCtx, MeteredJournal};
use crate::constants::BATCH_POSTER_ADDRESS;

pub(super) fn run_arb_aggregator<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
    arb_call: &ArbCall,
) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let call = match ArbAggregator::ArbAggregatorCalls::abi_decode(input) {
        Ok(c) => c,
        Err(_) => return gated_revert_result(gas_limit),
    };

    let state = ArbosState::open();
    // Nitro's ArbAggregator uses `c.caller`, the immediate CALL caller (msg.sender of the
    // precompile call), NOT the tx origin. Through a proxy that delegatecalls into an impl which
    // then calls the precompile, these differ.
    let caller = arb_call.caller;

    // Nitro meters every ArbOS-storage read/write performed inside these methods through the
    // precompile burner (StorageReadCost=800 / StorageWriteCost=20000), on top of the per-call
    // args/result/OpenArbosState charge that `arbos_call_extra_gas` already folds in. Route the
    // storage ops through `MeteredJournal` and add its total to the call's gas.
    let mut journal = MeteredJournal::new(ctx.journal_mut());
    let mut result = dispatch_arb_aggregator(&state, call, caller, gas_limit, &mut journal);

    let burned = journal.burned;
    if !result.gas.record_regular_cost(burned) {
        result.result = revm::interpreter::InstructionResult::OutOfGas;
        result.output = revm::primitives::Bytes::new();
    }
    result
}

fn dispatch_arb_aggregator<J: ArbJournal>(
    state: &ArbosState,
    call: ArbAggregator::ArbAggregatorCalls,
    caller: Address,
    gas_limit: u64,
    journal: &mut J,
) -> InterpreterResult {
    match call {
        ArbAggregator::ArbAggregatorCalls::getBatchPosters(_) => {
            let posters = match state
                .l1_pricing
                .batch_poster_table
                .poster_address_set
                .all_members(journal)
            {
                Ok(p) => p,
                Err(e) => return revert_result(gas_limit, &format!("ArbAggregator: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(posters,)),
            )
        }
        ArbAggregator::ArbAggregatorCalls::getFeeCollector(c) => {
            let poster_state = match state.l1_pricing.batch_poster_table.open_poster_checked(
                c.batchPoster,
                journal,
                false,
            ) {
                Ok(s) => s,
                Err(e) => {
                    return revert_result(gas_limit, &format!("ArbAggregator: error: {e}"));
                }
            };
            let pay_to = match poster_state.pay_to(journal) {
                Ok(a) => a,
                Err(e) => return revert_result(gas_limit, &format!("ArbAggregator: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(pay_to,)),
            )
        }
        ArbAggregator::ArbAggregatorCalls::getPreferredAggregator(_) => {
            // Deprecated in Nitro: always the sequencer batch poster and "default=true".
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(BATCH_POSTER_ADDRESS, true)),
            )
        }
        ArbAggregator::ArbAggregatorCalls::getDefaultAggregator(_) => ok_result(
            gas_limit,
            alloy_core::sol_types::SolValue::abi_encode(&(BATCH_POSTER_ADDRESS,)),
        ),
        ArbAggregator::ArbAggregatorCalls::getTxBaseFee(_) => ok_result(
            gas_limit,
            alloy_core::sol_types::SolValue::abi_encode(&(U256::ZERO,)),
        ),
        ArbAggregator::ArbAggregatorCalls::addBatchPoster(c) => {
            let caller_is_owner = match state.chain_owners.is_member(caller, journal) {
                Ok(v) => v,
                Err(e) => {
                    return revert_result(gas_limit, &format!("ArbAggregator: error: {e}"));
                }
            };
            if !caller_is_owner {
                return revert_result(gas_limit, "ArbAggregator: must be called by chain owner");
            }
            let already_registered = match state
                .l1_pricing
                .batch_poster_table
                .poster_address_set
                .is_member(c.newBatchPoster, journal)
            {
                Ok(v) => v,
                Err(e) => {
                    return revert_result(gas_limit, &format!("ArbAggregator: error: {e}"));
                }
            };
            if already_registered {
                return ok_result(gas_limit, vec![]);
            }
            match state.l1_pricing.batch_poster_table.add_poster(
                c.newBatchPoster,
                c.newBatchPoster,
                journal,
            ) {
                Ok(_) => ok_result(gas_limit, vec![]),
                Err(e) => revert_result(
                    gas_limit,
                    &format!("ArbAggregator: addBatchPoster error: {e}"),
                ),
            }
        }
        ArbAggregator::ArbAggregatorCalls::setFeeCollector(c) => {
            let poster_state = match state.l1_pricing.batch_poster_table.open_poster_checked(
                c.batchPoster,
                journal,
                false,
            ) {
                Ok(s) => s,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbAggregator: setFeeCollector error: {e}"),
                    );
                }
            };
            let old_fee_collector = match poster_state.pay_to(journal) {
                Ok(a) => a,
                Err(e) => return revert_result(gas_limit, &format!("ArbAggregator: error: {e}")),
            };
            if caller != c.batchPoster && caller != old_fee_collector {
                let caller_is_owner = match state.chain_owners.is_member(caller, journal) {
                    Ok(v) => v,
                    Err(e) => {
                        return revert_result(gas_limit, &format!("ArbAggregator: error: {e}"));
                    }
                };
                if !caller_is_owner {
                    return revert_result(
                        gas_limit,
                        "ArbAggregator: only poster, fee collector, or chain owner may set fee collector",
                    );
                }
            }
            match poster_state.set_pay_to(c.newFeeCollector, journal) {
                Ok(_) => ok_result(gas_limit, vec![]),
                Err(e) => revert_result(
                    gas_limit,
                    &format!("ArbAggregator: setFeeCollector error: {e}"),
                ),
            }
        }
        ArbAggregator::ArbAggregatorCalls::setTxBaseFee(_) => {
            // Deprecated no-op.
            ok_result(gas_limit, vec![])
        }
    }
}
