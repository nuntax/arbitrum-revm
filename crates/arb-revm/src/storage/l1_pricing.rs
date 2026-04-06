use eyre::{Result, eyre};
use revm::{
    context_interface::{JournalTr, context::SStoreResult, journaled_state::StateLoad},
    primitives::{Address, I256, U256},
};

use super::{BatchPosterTable, L1PricingOffset, StorageBacked, StorageSpace};

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
    pub per_batch_gas_cost: StorageBacked<u64>,
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
}
