use super::*;
use crate::arb_journal::ArbPrecompileCtx;
use crate::constants::BATCH_POSTER_ADDRESS;

pub(super) fn run_arb_aggregator<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let call = match ArbAggregator::ArbAggregatorCalls::abi_decode(input) {
        Ok(c) => c,
        Err(e) => {
            return revert_result(gas_limit, &format!("ArbAggregator: invalid calldata: {e}"));
        }
    };

    let state = ArbosState::open();

    match call {
        ArbAggregator::ArbAggregatorCalls::getBatchPosters(_) => {
            let posters = match state
                .l1_pricing
                .batch_poster_table
                .poster_address_set
                .all_members(ctx.journal_mut())
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
                ctx.journal_mut(),
                false,
            ) {
                Ok(s) => s,
                Err(e) => {
                    return revert_result(gas_limit, &format!("ArbAggregator: error: {e}"));
                }
            };
            let pay_to = match poster_state.pay_to(ctx.journal_mut()) {
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
            let caller = ctx.tx_caller();
            let caller_is_owner = match state.chain_owners.is_member(caller, ctx.journal_mut()) {
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
                .is_member(c.newBatchPoster, ctx.journal_mut())
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
                ctx.journal_mut(),
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
                ctx.journal_mut(),
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
            let old_fee_collector = match poster_state.pay_to(ctx.journal_mut()) {
                Ok(a) => a,
                Err(e) => return revert_result(gas_limit, &format!("ArbAggregator: error: {e}")),
            };
            let caller = ctx.tx_caller();
            if caller != c.batchPoster && caller != old_fee_collector {
                let caller_is_owner = match state.chain_owners.is_member(caller, ctx.journal_mut())
                {
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
            match poster_state.set_pay_to(c.newFeeCollector, ctx.journal_mut()) {
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
