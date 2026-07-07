use super::*;
use crate::arb_journal::{ArbJournal, ArbPrecompileCtx, MeteredJournal};

const ASSUMED_SIMPLE_TX_SIZE: u64 = 140;
const TX_DATA_NON_ZERO_GAS_EIP2028: u64 = 16;
const STORAGE_WRITE_COST: u64 = 20_000;

pub(super) fn run_arb_gas_info<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let call = match ArbGasInfo::ArbGasInfoCalls::abi_decode(input) {
        Ok(c) => c,
        Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: invalid calldata: {e}")),
    };

    let state = ArbosState::open();
    let l2_gas_price = U256::from(ctx.block_basefee());

    // ArbOS version is a cached field on Nitro's opened `ArbosState` (no per-read storage charge),
    // so read it through the raw journal to keep it unmetered. Every other ArbOS-storage read below
    // goes through `MeteredJournal`, which bills 800 gas per read (Nitro's precompile burner /
    // `StorageReadCost`) on top of the per-call OpenArbosState charge already folded in by
    // `arbos_call_extra_gas`. Without this the getters undercharged their storage reads by 800 per read, mispricing the receipt gasUsed.
    let arbos_version = match state.arbos_version.get(ctx.journal_mut()) {
        Ok(v) => v,
        Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
    };
    let mut journal = MeteredJournal::new(ctx.journal_mut());
    let j = &mut journal;

    let mut result = match call {
        ArbGasInfo::ArbGasInfoCalls::getPricesInWei(_) => {
            // (perL2Tx, perL1CalldataUnit, perStorageAllocation,
            //  perArbGasBase, perArbGasCongestion, perArbGasTotal)
            let min_base_fee = match state.l2_pricing.min_base_fee_wei.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            let l1_price = match state.l1_pricing.price_per_unit.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            let wei_for_l1_calldata =
                l1_price.saturating_mul(U256::from(TX_DATA_NON_ZERO_GAS_EIP2028));
            let per_l2_tx = wei_for_l1_calldata.saturating_mul(U256::from(ASSUMED_SIMPLE_TX_SIZE));
            let per_arb_gas_base = if arbos_version < 4 {
                l2_gas_price
            } else {
                core::cmp::min(l2_gas_price, min_base_fee)
            };
            let per_arb_gas_congestion = l2_gas_price.saturating_sub(per_arb_gas_base);
            let per_storage_allocation =
                l2_gas_price.saturating_mul(U256::from(STORAGE_WRITE_COST));
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(
                    per_l2_tx,
                    wei_for_l1_calldata,
                    per_storage_allocation,
                    per_arb_gas_base,
                    per_arb_gas_congestion,
                    l2_gas_price,
                )),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getPricesInWeiWithAggregator(_) => {
            // Deprecated aggregator path; return same as getPricesInWei.
            let min_base_fee = match state.l2_pricing.min_base_fee_wei.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            let l1_price = match state.l1_pricing.price_per_unit.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            let wei_for_l1_calldata =
                l1_price.saturating_mul(U256::from(TX_DATA_NON_ZERO_GAS_EIP2028));
            let per_l2_tx = wei_for_l1_calldata.saturating_mul(U256::from(ASSUMED_SIMPLE_TX_SIZE));
            let per_arb_gas_base = if arbos_version < 4 {
                l2_gas_price
            } else {
                core::cmp::min(l2_gas_price, min_base_fee)
            };
            let per_arb_gas_congestion = l2_gas_price.saturating_sub(per_arb_gas_base);
            let per_storage_allocation =
                l2_gas_price.saturating_mul(U256::from(STORAGE_WRITE_COST));
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(
                    per_l2_tx,
                    wei_for_l1_calldata,
                    per_storage_allocation,
                    per_arb_gas_base,
                    per_arb_gas_congestion,
                    l2_gas_price,
                )),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getPricesInArbGas(_) => {
            // (perL2Tx, perL1Calldata, perStorageAllocation)
            let l1_price = match state.l1_pricing.price_per_unit.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            let wei_for_l1_calldata =
                l1_price.saturating_mul(U256::from(TX_DATA_NON_ZERO_GAS_EIP2028));
            let wei_per_l2_tx =
                wei_for_l1_calldata.saturating_mul(U256::from(ASSUMED_SIMPLE_TX_SIZE));
            let gas_for_l1_calldata = if l2_gas_price > U256::ZERO {
                wei_for_l1_calldata / l2_gas_price
            } else {
                U256::ZERO
            };
            let gas_per_l2_tx = if arbos_version < 4 {
                U256::from(ASSUMED_SIMPLE_TX_SIZE)
            } else if l2_gas_price > U256::ZERO {
                wei_per_l2_tx / l2_gas_price
            } else {
                U256::ZERO
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(
                    gas_per_l2_tx,
                    gas_for_l1_calldata,
                    U256::from(STORAGE_WRITE_COST),
                )),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getPricesInArbGasWithAggregator(_) => {
            let l1_price = match state.l1_pricing.price_per_unit.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            let wei_for_l1_calldata =
                l1_price.saturating_mul(U256::from(TX_DATA_NON_ZERO_GAS_EIP2028));
            let wei_per_l2_tx =
                wei_for_l1_calldata.saturating_mul(U256::from(ASSUMED_SIMPLE_TX_SIZE));
            let gas_for_l1_calldata = if l2_gas_price > U256::ZERO {
                wei_for_l1_calldata / l2_gas_price
            } else {
                U256::ZERO
            };
            let gas_per_l2_tx = if arbos_version < 4 {
                U256::from(ASSUMED_SIMPLE_TX_SIZE)
            } else if l2_gas_price > U256::ZERO {
                wei_per_l2_tx / l2_gas_price
            } else {
                U256::ZERO
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(
                    gas_per_l2_tx,
                    gas_for_l1_calldata,
                    U256::from(STORAGE_WRITE_COST),
                )),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getGasAccountingParams(_) => {
            let speed_limit = match state.l2_pricing.speed_limit_per_second.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            let per_block = match state.l2_pricing.per_block_gas_limit.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(
                    U256::from(speed_limit),
                    U256::from(per_block),
                    U256::from(per_block),
                )),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getMaxTxGasLimit(_) => {
            let limit = match state.l2_pricing.per_tx_gas_limit.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(U256::from(limit),)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getMinimumGasPrice(_) => {
            let min_fee = match state.l2_pricing.min_base_fee_wei.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(min_fee,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getL1BaseFeeEstimate(_) => {
            let l1_fee = match state.l1_pricing.price_per_unit.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(l1_fee,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getL1BaseFeeEstimateInertia(_) => {
            let inertia = match state.l1_pricing.inertia.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(inertia,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getL1RewardRate(_) => {
            let rate = match state.l1_pricing.per_unit_reward.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(rate,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getL1RewardRecipient(_) => {
            let recipient = match state.l1_pricing.pay_rewards_to.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(recipient,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getL1GasPriceEstimate(_) => {
            // In Nitro, L1 gas price estimate == price_per_unit.
            let price = match state.l1_pricing.price_per_unit.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(price,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getCurrentTxL1GasFees(_) => {
            // Nitro returns txProcessor.PosterFee, the L1 poster fee charged to the current tx,
            // set in GasChargingHook. The gas-charging handler publishes it to a transient-storage
            // slot (CURRENT_TX_L1_FEE_ADDR); read it back here so both the in-EVM and node execution
            // paths agree (the node-path EvmInternals handle cannot expose the chain context).
            let fee = j.transient_load(crate::constants::CURRENT_TX_L1_FEE_ADDR, U256::ZERO);
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(fee,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getGasBacklog(_) => {
            let backlog = match state.l2_pricing.gas_backlog.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(backlog,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getPricingInertia(_) => {
            let inertia = match state.l2_pricing.pricing_inertia.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(inertia,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getGasBacklogTolerance(_) => {
            let tolerance = match state.l2_pricing.backlog_tolerance.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(tolerance,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getL1PricingSurplus(_) => {
            let surplus = match state.l1_pricing.get_l1_pricing_surplus(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(surplus,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getPerBatchGasCharge(_) => {
            let charge = match state.l1_pricing.per_batch_gas_cost.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(charge,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getAmortizedCostCapBips(_) => {
            let cap = match state.l1_pricing.amortized_cost_cap_bips.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(cap,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getL1FeesAvailable(_) => {
            let available = match state.l1_pricing.l1_fees_available.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(available,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getL1PricingEquilibrationUnits(_) => {
            let units = match state.l1_pricing.equilibration_units.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(units,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getLastL1PricingUpdateTime(_) => {
            let ts = match state.l1_pricing.last_update_time.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(ts,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getL1PricingFundsDueForRewards(_) => {
            let due = match state.l1_pricing.funds_due_for_rewards.get(j) {
                Ok(v) => {
                    // funds_due_for_rewards is I256; return abs or 0 if negative.
                    if v >= revm::primitives::I256::ZERO {
                        U256::from(v)
                    } else {
                        U256::ZERO
                    }
                }
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(due,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getL1PricingUnitsSinceUpdate(_) => {
            let units = match state.l1_pricing.units_since_update.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(units,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getLastL1PricingSurplus(_) => {
            let surplus = match state.l1_pricing.last_surplus.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(surplus,)),
            )
        }
        ArbGasInfo::ArbGasInfoCalls::getMaxBlockGasLimit(_) => {
            let limit = match state.l2_pricing.per_block_gas_limit.get(j) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbGasInfo: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(limit,)),
            )
        }
    };

    // Fold the per-read storage gas the method burned (via MeteredJournal) into the call's result,
    // matching Nitro's precompile burner. Reverts already consume all gas, so this only affects the
    // success paths.
    let burned = journal.burned;
    if !result.gas.record_regular_cost(burned) {
        result.result = revm::interpreter::InstructionResult::OutOfGas;
        result.output = revm::primitives::Bytes::new();
    }
    result
}
