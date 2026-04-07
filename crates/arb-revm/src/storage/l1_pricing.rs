use eyre::{eyre, Result};
use revm::{
    context_interface::{
        context::SStoreResult,
        journaled_state::{StateLoad, TransferError},
        JournalTr,
    },
    primitives::{Address, I256, U256},
};

use super::{BatchPosterTable, L1PricingOffset, StorageBacked, StorageSpace};
use crate::constants::L1_PRICER_FUNDS_POOL_ADDRESS;

const ONE_IN_BIPS: u64 = 10_000;
const ARBOS_VERSION_WITH_LAST_SURPLUS: u64 = 2;
const ARBOS_VERSION_WITH_AMORTIZED_COST_CAP: u64 = 3;
const ARBOS_VERSION_WITH_L1_FEES_AVAILABLE: u64 = 10;
const ARBOS_VERSION_WITH_SIGNED_LAST_SURPLUS: u64 = 7;
const ARBOS_VERSION_BATCH_REPORT_V2_FLOOR_GAS: u64 = 50;
const FLOOR_GAS_ADDITIONAL_TOKENS: u64 = 172;
const TX_DATA_ZERO_GAS: u64 = 4;
const TX_DATA_NON_ZERO_GAS_EIP2028: u64 = 16;
const KECCAK256_GAS: u64 = 30;
const KECCAK256_WORD_GAS: u64 = 6;
const SSTORE_SET_GAS_EIP2200: u64 = 20_000;
const TX_GAS: u64 = 21_000;

/// ArbOS L1 pricing storage wrapper.
#[derive(Debug)]
pub struct L1Pricing {
    pub batch_poster_table: BatchPosterTable,
    pub pay_rewards_to: StorageBacked<Address>,
    pub equilibration_units: StorageBacked<U256>,
    pub inertia: StorageBacked<u64>,
    pub per_unit_reward: StorageBacked<u64>,
    pub last_update_time: StorageBacked<u64>,
    pub funds_due_for_rewards: StorageBacked<I256>,
    pub units_since_update: StorageBacked<u64>,
    pub price_per_unit: StorageBacked<U256>,
    pub last_surplus: StorageBacked<I256>,
    pub per_batch_gas_cost: StorageBacked<i64>,
    pub amortized_cost_cap_bips: StorageBacked<u64>,
    pub l1_fees_available: StorageBacked<U256>,
    pub gas_floor_per_token: StorageBacked<u64>,
    pub storage: StorageSpace,
}

impl L1Pricing {
    pub fn open(storage: &StorageSpace) -> Self {
        Self {
            batch_poster_table: BatchPosterTable::open(&storage.open_subspace_with_key(0)),
            pay_rewards_to: storage.storage_backed(L1PricingOffset::PayRewardsTo as u8),
            equilibration_units: storage.storage_backed(L1PricingOffset::EquilibrationUnits as u8),
            inertia: storage.storage_backed(L1PricingOffset::Inertia as u8),
            per_unit_reward: storage.storage_backed(L1PricingOffset::PerUnitReward as u8),
            last_update_time: storage.storage_backed(L1PricingOffset::LastUpdateTime as u8),
            funds_due_for_rewards: storage
                .storage_backed(L1PricingOffset::FundsDueForRewards as u8),
            units_since_update: storage.storage_backed(L1PricingOffset::UnitsSince as u8),
            price_per_unit: storage.storage_backed(L1PricingOffset::PricePerUnit as u8),
            last_surplus: storage.storage_backed(L1PricingOffset::LastSurplus as u8),
            per_batch_gas_cost: storage.storage_backed(L1PricingOffset::PerBatchGasCost as u8),
            amortized_cost_cap_bips: storage
                .storage_backed(L1PricingOffset::AmortizedCostCapBips as u8),
            l1_fees_available: storage.storage_backed(L1PricingOffset::L1FeesAvailable as u8),
            gas_floor_per_token: storage.storage_backed(L1PricingOffset::GasFloorPerToken as u8),
            storage: storage.clone(),
        }
    }

    pub fn get_l1_pricing_surplus<J: JournalTr>(&self, journal: &mut J) -> Result<I256> {
        let refunds_due = self.batch_poster_table.total_funds_due(journal)?;
        let rewards_due = self.funds_due_for_rewards.get(journal)?;
        let available = self.l1_fees_available.get(journal)?;
        let needed = I256::from(refunds_due)
            .checked_add(rewards_due)
            .ok_or_else(|| eyre!("overflow calculating ArbOS L1 pricing surplus"))?;
        I256::from(available)
            .checked_sub(needed)
            .ok_or_else(|| eyre!("underflow calculating ArbOS L1 pricing surplus"))
    }

    pub fn add_to_l1_fees_available<J: JournalTr>(
        &self,
        delta: U256,
        journal: &mut J,
    ) -> Result<StateLoad<SStoreResult>> {
        let current = self.l1_fees_available.get(journal)?;
        let next = current
            .checked_add(delta)
            .ok_or_else(|| eyre!("overflow adding to ArbOS l1_fees_available"))?;
        self.l1_fees_available.set(next, journal)
    }

    pub fn update_for_batch_poster_spending<J: JournalTr>(
        &self,
        arbos_version: u64,
        update_time: u64,
        current_time: u64,
        batch_poster: Address,
        mut wei_spent: U256,
        l1_base_fee: U256,
        journal: &mut J,
    ) -> Result<()> {
        let poster_state =
            self.batch_poster_table
                .open_poster_checked(batch_poster, journal, true)?;

        let mut funds_due_for_rewards = self.funds_due_for_rewards.get(journal)?;
        let use_l1_fees_available = arbos_version >= ARBOS_VERSION_WITH_L1_FEES_AVAILABLE;
        let mut funds_available = if use_l1_fees_available {
            self.l1_fees_available.get(journal)?
        } else {
            pool_balance(journal)?
        };

        let mut last_update_time = self.last_update_time.get(journal)?;
        if last_update_time == 0 && update_time > 0 {
            last_update_time = update_time.saturating_sub(1);
        }
        if update_time > current_time || update_time < last_update_time {
            return Err(eyre!(
                "invalid ArbOS batch report timestamp: update_time={update_time} current_time={current_time} last_update_time={last_update_time}"
            ));
        }

        let mut allocation_numerator = update_time.saturating_sub(last_update_time);
        let mut allocation_denominator = current_time.saturating_sub(last_update_time);
        if allocation_denominator == 0 {
            allocation_numerator = 1;
            allocation_denominator = 1;
        }

        let units_since_update = self.units_since_update.get(journal)?;
        let units_allocated =
            units_since_update.saturating_mul(allocation_numerator) / allocation_denominator;
        let remaining_units = units_since_update.saturating_sub(units_allocated);
        self.units_since_update.set(remaining_units, journal)?;

        if arbos_version >= ARBOS_VERSION_WITH_AMORTIZED_COST_CAP {
            let amortized_cost_cap_bips = self.amortized_cost_cap_bips.get(journal)?;
            if amortized_cost_cap_bips != 0 {
                let wei_spent_cap = mul_u256_by_bips_saturating(
                    mul_u256_u64_saturating(l1_base_fee, units_allocated),
                    amortized_cost_cap_bips,
                );
                if wei_spent_cap < wei_spent {
                    // Nitro caps assigned amortized cost; excess is a poster-side loss.
                    wei_spent = wei_spent_cap;
                }
            }
        }

        let due_to_poster = poster_state.funds_due(journal)?;
        let next_due_to_poster = add_u256_saturating(due_to_poster, wei_spent);
        poster_state.set_funds_due(next_due_to_poster, journal)?;

        let per_unit_reward = self.per_unit_reward.get(journal)?;
        let payment_for_rewards_target =
            mul_u256_u64_saturating(U256::from(per_unit_reward), units_allocated);
        funds_due_for_rewards =
            add_i256_u256_saturating(funds_due_for_rewards, payment_for_rewards_target);
        self.funds_due_for_rewards.set_saturating_with_warning(
            funds_due_for_rewards,
            "L1 pricer funds due for rewards",
            journal,
        )?;

        let payment_for_rewards = core::cmp::min(payment_for_rewards_target, funds_available);
        funds_due_for_rewards = u256_to_i256_saturating(sub_u256_floor_zero(
            i256_nonnegative_to_u256(funds_due_for_rewards),
            payment_for_rewards,
        ));
        self.funds_due_for_rewards.set_saturating_with_warning(
            funds_due_for_rewards,
            "L1 pricer funds due for rewards",
            journal,
        )?;
        if payment_for_rewards > U256::ZERO {
            let pay_rewards_to = self.pay_rewards_to.get(journal)?;
            transfer_from_pool(pay_rewards_to, payment_for_rewards, journal)?;
        }
        if use_l1_fees_available {
            funds_available = sub_u256_floor_zero(funds_available, payment_for_rewards);
            self.l1_fees_available.set(funds_available, journal)?;
        } else {
            funds_available = pool_balance(journal)?;
        }

        let mut balance_due_to_poster = poster_state.funds_due(journal)?;
        let balance_to_transfer = core::cmp::min(balance_due_to_poster, funds_available);
        if balance_to_transfer > U256::ZERO {
            let pay_to = poster_state.pay_to(journal)?;
            transfer_from_pool(pay_to, balance_to_transfer, journal)?;
            balance_due_to_poster = sub_u256_floor_zero(balance_due_to_poster, balance_to_transfer);
            poster_state.set_funds_due(balance_due_to_poster, journal)?;
            if use_l1_fees_available {
                funds_available = sub_u256_floor_zero(funds_available, balance_to_transfer);
                self.l1_fees_available.set(funds_available, journal)?;
            }
        }
        self.last_update_time.set(update_time, journal)?;

        if units_allocated == 0 {
            return Ok(());
        }

        let total_funds_due = self.batch_poster_table.total_funds_due(journal)?;
        funds_due_for_rewards = self.funds_due_for_rewards.get(journal)?;
        let needed_funds = add_i256_u256_saturating(funds_due_for_rewards, total_funds_due);
        let current_available = if use_l1_fees_available {
            self.l1_fees_available.get(journal)?
        } else {
            pool_balance(journal)?
        };
        let surplus = u256_to_i256_saturating(current_available)
            .checked_sub(needed_funds)
            .unwrap_or(I256::ZERO);

        let inertia = self.inertia.get(journal)?;
        if inertia == 0 {
            return Ok(());
        }

        let equilibration_units = self.equilibration_units.get(journal)?;
        if equilibration_units == U256::ZERO {
            return Ok(());
        }

        let inertia_units = equilibration_units / U256::from(inertia);
        let alloc_plus_inert = add_u256_saturating(inertia_units, U256::from(units_allocated));
        if alloc_plus_inert == U256::ZERO {
            return Ok(());
        }

        let old_surplus = self.last_surplus.get(journal)?;
        let equilibration_units_i = u256_to_i256_saturating(equilibration_units);
        if equilibration_units_i == I256::ZERO {
            return Ok(());
        }
        let units_allocated_i = u256_to_i256_saturating(U256::from(units_allocated));
        let alloc_plus_inert_i = u256_to_i256_saturating(alloc_plus_inert);
        if alloc_plus_inert_i == I256::ZERO {
            return Ok(());
        }

        let desired_derivative = (-surplus)
            .checked_div(equilibration_units_i)
            .unwrap_or(I256::ZERO);
        let actual_derivative = surplus
            .checked_sub(old_surplus)
            .and_then(|delta| delta.checked_div(units_allocated_i))
            .unwrap_or(I256::ZERO);
        let change_derivative = desired_derivative
            .checked_sub(actual_derivative)
            .unwrap_or(I256::ZERO);
        let price_change = change_derivative
            .checked_mul(units_allocated_i)
            .and_then(|v| v.checked_div(alloc_plus_inert_i))
            .unwrap_or(I256::ZERO);

        if arbos_version < ARBOS_VERSION_WITH_LAST_SURPLUS {
            // Pre-ArbOS-2 does not track last surplus.
        } else if arbos_version < ARBOS_VERSION_WITH_SIGNED_LAST_SURPLUS {
            self.last_surplus.set_pre_version7(surplus, journal)?;
        } else {
            self.last_surplus.set_saturating_with_warning(
                surplus,
                "L1 pricer last surplus",
                journal,
            )?;
        }

        let price_per_unit = self.price_per_unit.get(journal)?;
        let next_price_per_unit = if price_change == I256::ZERO {
            price_per_unit
        } else {
            let price_per_unit_i = u256_to_i256_saturating(price_per_unit);
            let next_price_i = price_per_unit_i
                .checked_add(price_change)
                .unwrap_or_else(i256_max);
            if next_price_i <= I256::ZERO {
                U256::ZERO
            } else {
                i256_nonnegative_to_u256(next_price_i)
            }
        };
        self.price_per_unit.set(next_price_per_unit, journal)?;

        Ok(())
    }

    pub fn apply_batch_posting_report<J: JournalTr>(
        &self,
        arbos_version: u64,
        batch_timestamp: u64,
        current_time: u64,
        batch_poster: Address,
        batch_data_gas: u64,
        l1_base_fee: U256,
        journal: &mut J,
    ) -> Result<()> {
        let per_batch_gas = self.per_batch_gas_cost.get(journal)?;
        let gas_spent = signed_i64_to_u64_floor_zero(per_batch_gas).saturating_add(batch_data_gas);
        let wei_spent = l1_base_fee
            .checked_mul(U256::from(gas_spent))
            .unwrap_or(U256::MAX);
        self.update_for_batch_poster_spending(
            arbos_version,
            batch_timestamp,
            current_time,
            batch_poster,
            wei_spent,
            l1_base_fee,
            journal,
        )
    }

    pub fn apply_batch_posting_report_v2<J: JournalTr>(
        &self,
        arbos_version: u64,
        batch_timestamp: u64,
        current_time: u64,
        batch_poster: Address,
        batch_calldata_length: u64,
        batch_calldata_non_zeros: u64,
        batch_extra_gas: u64,
        l1_base_fee: U256,
        journal: &mut J,
    ) -> Result<()> {
        let per_batch_gas = self.per_batch_gas_cost.get(journal)?;
        let mut gas_spent =
            legacy_batch_cost_for_stats(batch_calldata_length, batch_calldata_non_zeros)
                .saturating_add(batch_extra_gas)
                .saturating_add(signed_i64_to_u64_floor_zero(per_batch_gas));

        if arbos_version >= ARBOS_VERSION_BATCH_REPORT_V2_FLOOR_GAS {
            let gas_floor_per_token = self.gas_floor_per_token.get(journal)?;
            let floor_tokens = batch_calldata_length
                .saturating_add(batch_calldata_non_zeros.saturating_mul(3))
                .saturating_add(FLOOR_GAS_ADDITIONAL_TOKENS);
            let floor_gas_spent = gas_floor_per_token
                .saturating_mul(floor_tokens)
                .saturating_add(TX_GAS);
            if floor_gas_spent > gas_spent {
                gas_spent = floor_gas_spent;
            }
        }

        let wei_spent = l1_base_fee
            .checked_mul(U256::from(gas_spent))
            .unwrap_or(U256::MAX);
        self.update_for_batch_poster_spending(
            arbos_version,
            batch_timestamp,
            current_time,
            batch_poster,
            wei_spent,
            l1_base_fee,
            journal,
        )
    }
}

fn pool_balance<J: JournalTr>(journal: &mut J) -> Result<U256> {
    let account = journal.load_account(L1_PRICER_FUNDS_POOL_ADDRESS)?;
    Ok(account.info.balance)
}

fn transfer_from_pool<J: JournalTr>(
    recipient: Address,
    amount: U256,
    journal: &mut J,
) -> Result<()> {
    if amount == U256::ZERO {
        return Ok(());
    }
    let transfer_error = journal.transfer(L1_PRICER_FUNDS_POOL_ADDRESS, recipient, amount)?;
    match transfer_error {
        None => Ok(()),
        Some(TransferError::OutOfFunds) => Err(eyre!(
            "insufficient L1 pricer funds pool balance for transfer of {amount}"
        )),
        Some(TransferError::OverflowPayment) => Err(eyre!(
            "overflow while crediting recipient {recipient} from L1 pricer funds pool"
        )),
        Some(TransferError::CreateCollision) => Err(eyre!(
            "create collision transferring from L1 pricer funds pool"
        )),
    }
}

fn add_u256_saturating(lhs: U256, rhs: U256) -> U256 {
    lhs.checked_add(rhs).unwrap_or(U256::MAX)
}

fn sub_u256_floor_zero(lhs: U256, rhs: U256) -> U256 {
    lhs.checked_sub(rhs).unwrap_or(U256::ZERO)
}

fn mul_u256_u64_saturating(value: U256, multiplier: u64) -> U256 {
    value
        .checked_mul(U256::from(multiplier))
        .unwrap_or(U256::MAX)
}

fn mul_u256_by_bips_saturating(value: U256, bips: u64) -> U256 {
    mul_u256_u64_saturating(value, bips) / U256::from(ONE_IN_BIPS)
}

fn u256_to_i256_saturating(value: U256) -> I256 {
    let max_i256_u256 = (U256::ONE << 255) - U256::ONE;
    if value > max_i256_u256 {
        i256_max()
    } else {
        I256::from(value)
    }
}

fn i256_nonnegative_to_u256(value: I256) -> U256 {
    if value <= I256::ZERO {
        U256::ZERO
    } else {
        U256::from(value)
    }
}

fn add_i256_u256_saturating(lhs: I256, rhs: U256) -> I256 {
    lhs.checked_add(u256_to_i256_saturating(rhs))
        .unwrap_or_else(i256_max)
}

fn words_for_bytes(byte_len: u64) -> u64 {
    byte_len.saturating_add(31) / 32
}

fn signed_i64_to_u64_floor_zero(value: i64) -> u64 {
    if value <= 0 {
        0
    } else {
        value as u64
    }
}

fn legacy_batch_cost_for_stats(length: u64, non_zeros: u64) -> u64 {
    let zeros = length.saturating_sub(non_zeros);
    let calldata_gas = TX_DATA_ZERO_GAS
        .saturating_mul(zeros)
        .saturating_add(TX_DATA_NON_ZERO_GAS_EIP2028.saturating_mul(non_zeros));
    let keccak_words = words_for_bytes(length);
    calldata_gas
        .saturating_add(KECCAK256_GAS)
        .saturating_add(keccak_words.saturating_mul(KECCAK256_WORD_GAS))
        .saturating_add(2_u64.saturating_mul(SSTORE_SET_GAS_EIP2200))
}

fn i256_max() -> I256 {
    I256::from((U256::ONE << 255) - U256::ONE)
}
