use eyre::{Result, eyre};
use revm::{
    context_interface::{
        context::SStoreResult,
        journaled_state::{StateLoad, TransferError},
    },
    primitives::{Address, I256, U256},
};

use super::{BatchPosterTable, L1PricingOffset, StorageBacked, StorageSpace};
use crate::arb_journal::ArbJournal;
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

    pub fn get_l1_pricing_surplus<J: ArbJournal>(&self, journal: &mut J) -> Result<I256> {
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

    pub fn add_to_l1_fees_available<J: ArbJournal>(
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

    pub fn update_for_batch_poster_spending<J: ArbJournal>(
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
        if arbos_version < ARBOS_VERSION_WITH_LAST_SURPLUS {
            // The pre-v2 implementation used a stricter upper bound and historically ignored an
            // invalid timestamp instead of returning an error. Both details are consensus-visible.
            if last_update_time == 0 && current_time > 0 {
                last_update_time = update_time.wrapping_sub(1);
            }
            if update_time >= current_time || update_time < last_update_time {
                return Ok(());
            }
        } else {
            if last_update_time == 0 && update_time > 0 {
                last_update_time = update_time.saturating_sub(1);
            }
            if update_time > current_time || update_time < last_update_time {
                return Err(eyre!(
                    "invalid ArbOS batch report timestamp: update_time={update_time} current_time={current_time} last_update_time={last_update_time}"
                ));
            }
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

        let desired_derivative = div_i256_like_go_bigint(-surplus, equilibration_units_i);
        let actual_derivative = surplus
            .checked_sub(old_surplus)
            .map(|delta| div_i256_like_go_bigint(delta, units_allocated_i))
            .unwrap_or(I256::ZERO);
        let change_derivative = desired_derivative
            .checked_sub(actual_derivative)
            .unwrap_or(I256::ZERO);
        let price_change = change_derivative
            .checked_mul(units_allocated_i)
            .map(|v| div_i256_like_go_bigint(v, alloc_plus_inert_i))
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

    pub fn apply_batch_posting_report<J: ArbJournal>(
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

    pub fn apply_batch_posting_report_v2<J: ArbJournal>(
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

fn pool_balance<J: ArbJournal>(journal: &mut J) -> Result<U256> {
    Ok(journal.account_balance(L1_PRICER_FUNDS_POOL_ADDRESS)?)
}

fn transfer_from_pool<J: ArbJournal>(
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
    if value <= 0 { 0 } else { value as u64 }
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

/// Go's `big.Int.Div` performs Euclidean division, while Rust signed division truncates
/// toward zero. Nitro uses `big.Int.Div`, so we mirror that behavior to avoid +/-1 drift
/// in L1 price updates when numerators are negative.
fn div_i256_like_go_bigint(dividend: I256, divisor: I256) -> I256 {
    if divisor <= I256::ZERO {
        return I256::ZERO;
    }
    let quotient = dividend.checked_div(divisor).unwrap_or(I256::ZERO);
    let product = quotient.checked_mul(divisor).unwrap_or(I256::ZERO);
    let remainder = dividend.checked_sub(product).unwrap_or(I256::ZERO);
    if remainder < I256::ZERO {
        quotient
            .checked_sub(I256::from(U256::ONE))
            .unwrap_or(quotient)
    } else {
        quotient
    }
}

#[cfg(test)]
mod tests {
    use revm::{
        context::journaled_state::account::JournaledAccountTr,
        context_interface::{ContextTr, JournalTr},
        database_interface::EmptyDB,
        primitives::{Address, I256, U256},
    };

    use super::L1Pricing;
    use crate::{
        api::default_ctx::{ArbContext, DefaultArb},
        arb_journal::ArbJournal,
        constants::L1_PRICER_FUNDS_POOL_ADDRESS,
        storage::StorageSpace,
    };

    const GWEI: u64 = 1_000_000_000;
    const POSTER: Address = Address::with_last_byte(0x03);
    const POSTER_PAY_TO: Address = Address::with_last_byte(0x04);
    const REWARD_RECIPIENT: Address = Address::with_last_byte(0x89);

    fn set_balance(ctx: &mut ArbContext<EmptyDB>, address: Address, balance: U256) {
        let mut account = ctx.journal_mut().load_account_mut(address).unwrap();
        account.data.set_balance(balance);
        ctx.journal_mut().touch_account(address);
    }

    #[test]
    fn reward_poster_and_pool_allocation_matches_nitro_vectors() {
        struct Case {
            unit_reward: u64,
            units_per_second: u64,
            funds_collected_per_second: u64,
            funds_spent: u64,
            amortization_cap_bips: u64,
            expected_reward: u64,
            expected_poster: u64,
            expected_pool: u64,
        }

        let cases = [
            Case {
                unit_reward: 10,
                units_per_second: 78,
                funds_collected_per_second: 7_800,
                funds_spent: 3_000,
                amortization_cap_bips: u64::MAX,
                expected_reward: 780,
                expected_poster: 3_000,
                expected_pool: 19_620,
            },
            Case {
                unit_reward: 10,
                units_per_second: 78,
                funds_collected_per_second: 1_313,
                funds_spent: 3_000,
                amortization_cap_bips: u64::MAX,
                expected_reward: 780,
                expected_poster: 3_000,
                expected_pool: 159,
            },
            Case {
                unit_reward: 10,
                units_per_second: 78,
                funds_collected_per_second: 31,
                funds_spent: 3_000,
                amortization_cap_bips: u64::MAX,
                expected_reward: 93,
                expected_poster: 0,
                expected_pool: 0,
            },
            Case {
                unit_reward: 10,
                units_per_second: 78,
                funds_collected_per_second: 7_800,
                funds_spent: 3_000,
                amortization_cap_bips: 100,
                expected_reward: 780,
                expected_poster: 3_000,
                expected_pool: 19_620,
            },
            Case {
                unit_reward: 0,
                units_per_second: 78,
                funds_collected_per_second: 7_800 * GWEI,
                funds_spent: 3_000 * GWEI,
                amortization_cap_bips: 100,
                expected_reward: 0,
                expected_poster: 7_800_000_000,
                expected_pool: 23_392_200_000_000,
            },
        ];

        for case in cases {
            let mut ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();
            let pricing = L1Pricing::open(&StorageSpace::arbos().open_subspace_with_key(0xa1));
            let journal = ctx.journal_mut();
            pricing
                .per_unit_reward
                .set(case.unit_reward, journal)
                .unwrap();
            pricing
                .pay_rewards_to
                .set(REWARD_RECIPIENT, journal)
                .unwrap();
            pricing
                .amortized_cost_cap_bips
                .set(case.amortization_cap_bips, journal)
                .unwrap();
            pricing
                .units_since_update
                .set(case.units_per_second * 3, journal)
                .unwrap();
            let collected = U256::from(case.funds_collected_per_second) * U256::from(3);
            pricing.l1_fees_available.set(collected, journal).unwrap();
            pricing
                .batch_poster_table
                .add_poster(POSTER, POSTER_PAY_TO, journal)
                .unwrap();
            set_balance(&mut ctx, L1_PRICER_FUNDS_POOL_ADDRESS, collected);

            pricing
                .update_for_batch_poster_spending(
                    10,
                    1,
                    3,
                    POSTER,
                    U256::from(case.funds_spent),
                    U256::from(10 * GWEI),
                    ctx.journal_mut(),
                )
                .unwrap();

            assert_eq!(
                ctx.journal_mut().account_balance(REWARD_RECIPIENT).unwrap(),
                U256::from(case.expected_reward)
            );
            assert_eq!(
                ctx.journal_mut().account_balance(POSTER_PAY_TO).unwrap(),
                U256::from(case.expected_poster)
            );
            assert_eq!(
                ctx.journal_mut()
                    .account_balance(L1_PRICER_FUNDS_POOL_ADDRESS)
                    .unwrap(),
                U256::from(case.expected_pool)
            );
            assert_eq!(
                pricing.units_since_update.get(ctx.journal_mut()).unwrap(),
                case.units_per_second * 2
            );
            assert_eq!(
                pricing.l1_fees_available.get(ctx.journal_mut()).unwrap(),
                U256::from(case.expected_pool)
            );
            assert_eq!(
                pricing
                    .funds_due_for_rewards
                    .get(ctx.journal_mut())
                    .unwrap(),
                I256::from_raw(U256::from(
                    case.unit_reward * case.units_per_second - case.expected_reward
                ))
            );
        }
    }

    #[test]
    fn pre_v2_invalid_batch_report_time_is_ignored_but_v2_plus_errors() {
        let mut ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();
        let pricing = L1Pricing::open(&StorageSpace::arbos().open_subspace_with_key(0xa2));
        pricing
            .batch_poster_table
            .add_poster(POSTER, POSTER, ctx.journal_mut())
            .unwrap();
        pricing.last_update_time.set(10, ctx.journal_mut()).unwrap();
        pricing
            .units_since_update
            .set(99, ctx.journal_mut())
            .unwrap();

        pricing
            .update_for_batch_poster_spending(
                1,
                9,
                9,
                POSTER,
                U256::ONE,
                U256::from(GWEI),
                ctx.journal_mut(),
            )
            .unwrap();
        assert_eq!(pricing.last_update_time.get(ctx.journal_mut()).unwrap(), 10);
        assert_eq!(
            pricing.units_since_update.get(ctx.journal_mut()).unwrap(),
            99
        );
        assert_eq!(
            pricing
                .batch_poster_table
                .open_poster_checked(POSTER, ctx.journal_mut(), false)
                .unwrap()
                .funds_due(ctx.journal_mut())
                .unwrap(),
            U256::ZERO
        );

        for version in [2, 3, 10] {
            assert!(
                pricing
                    .update_for_batch_poster_spending(
                        version,
                        9,
                        12,
                        POSTER,
                        U256::ONE,
                        U256::from(GWEI),
                        ctx.journal_mut(),
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn l1_price_equilibrates_up_down_and_constant_like_nitro() {
        const EQUILIBRATION_UNITS: u64 = 16 * 10_000_000;

        for (initial, equilibrium) in [
            (1_000_000_000_u64, 5_000_000_000_u64),
            (5_000_000_000_u64, 1_000_000_000_u64),
            (2_000_000_000_u64, 2_000_000_000_u64),
        ] {
            let mut ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();
            let pricing = L1Pricing::open(&StorageSpace::arbos().open_subspace_with_key(0xa3));
            pricing.per_unit_reward.set(0, ctx.journal_mut()).unwrap();
            pricing
                .price_per_unit
                .set(U256::from(initial), ctx.journal_mut())
                .unwrap();
            pricing
                .equilibration_units
                .set(U256::from(EQUILIBRATION_UNITS), ctx.journal_mut())
                .unwrap();
            pricing.inertia.set(10, ctx.journal_mut()).unwrap();

            for i in 0..10_u64 {
                let old_units = pricing.units_since_update.get(ctx.journal_mut()).unwrap();
                pricing
                    .units_since_update
                    .set(old_units + EQUILIBRATION_UNITS, ctx.journal_mut())
                    .unwrap();
                let current_price = pricing.price_per_unit.get(ctx.journal_mut()).unwrap();
                let old_pool = ctx
                    .journal_mut()
                    .account_balance(L1_PRICER_FUNDS_POOL_ADDRESS)
                    .unwrap();
                set_balance(
                    &mut ctx,
                    L1_PRICER_FUNDS_POOL_ADDRESS,
                    old_pool + current_price * U256::from(EQUILIBRATION_UNITS),
                );
                pricing
                    .update_for_batch_poster_spending(
                        3,
                        10 * (i + 1),
                        10 * (i + 1) + 5,
                        POSTER,
                        U256::from(equilibrium) * U256::from(EQUILIBRATION_UNITS),
                        U256::from(equilibrium),
                        ctx.journal_mut(),
                    )
                    .unwrap();
            }

            let actual: u64 = pricing
                .price_per_unit
                .get(ctx.journal_mut())
                .unwrap()
                .try_into()
                .unwrap();
            let expected_movement = equilibrium.abs_diff(initial);
            let actual_movement = actual.abs_diff(initial);
            assert_eq!(actual.cmp(&initial), equilibrium.cmp(&initial));
            assert!(
                u128::from(expected_movement) * 100 <= u128::from(actual_movement) * 101
                    && u128::from(actual_movement) * 100 <= u128::from(expected_movement) * 101,
                "initial={initial} equilibrium={equilibrium} actual={actual}"
            );
        }
    }
}
