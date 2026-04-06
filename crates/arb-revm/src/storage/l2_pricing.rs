use eyre::Result;
use revm::{context_interface::JournalTr, primitives::U256};

use super::{L2PricingOffset, StorageBacked, StorageSpace};

const ONE_IN_BIPS: i64 = 10_000;

/// ArbOS L2 pricing storage wrapper.
pub struct L2Pricing {
    pub speed_limit_per_second: StorageBacked<u64>,
    pub per_block_gas_limit: StorageBacked<u64>,
    pub base_fee_wei: StorageBacked<U256>,
    pub min_base_fee_wei: StorageBacked<U256>,
    pub gas_backlog: StorageBacked<u64>,
    pub pricing_inertia: StorageBacked<u64>,
    pub backlog_tolerance: StorageBacked<u64>,
}

impl L2Pricing {
    pub fn open(storage: &StorageSpace) -> Self {
        Self {
            speed_limit_per_second: storage
                .storage_backed(L2PricingOffset::SpeedLimitPerSecond as u8),
            per_block_gas_limit: storage.storage_backed(L2PricingOffset::PerBlockGasLimit as u8),
            base_fee_wei: storage.storage_backed(L2PricingOffset::BaseFeeWei as u8),
            min_base_fee_wei: storage.storage_backed(L2PricingOffset::MinBaseFeeWei as u8),
            gas_backlog: storage.storage_backed(L2PricingOffset::GasBacklog as u8),
            pricing_inertia: storage.storage_backed(L2PricingOffset::PricingInertia as u8),
            backlog_tolerance: storage.storage_backed(L2PricingOffset::BacklogTolerance as u8),
        }
    }

    pub fn update_pricing_model<J: JournalTr>(
        &self,
        time_passed: u64,
        journal: &mut J,
    ) -> Result<()> {
        let speed_limit = self.speed_limit_per_second.get(journal)?;
        let gas_to_shrink = time_passed.saturating_mul(speed_limit);
        let current_backlog = self.gas_backlog.get(journal)?;
        let new_backlog = current_backlog.saturating_sub(gas_to_shrink);
        self.gas_backlog.set(new_backlog, journal)?;

        let inertia = self.pricing_inertia.get(journal)?;
        let tolerance = self.backlog_tolerance.get(journal)?;
        let min_base_fee = self.min_base_fee_wei.get(journal)?;

        let threshold = tolerance.saturating_mul(speed_limit);
        let next_base_fee = if new_backlog > threshold {
            let excess = new_backlog - threshold;
            let divisor = inertia.saturating_mul(speed_limit);
            let exponent_bips = if divisor > 0 {
                let scaled = (excess as i128)
                    .saturating_mul(ONE_IN_BIPS as i128)
                    .min(i64::MAX as i128) as i64;
                scaled / divisor as i64
            } else {
                0
            };
            big_mul_by_bips(min_base_fee, approx_exp_basis_points(exponent_bips, 4))
        } else {
            min_base_fee
        };

        self.base_fee_wei.set(next_base_fee, journal)?;
        Ok(())
    }

    pub fn grow_backlog<J: JournalTr>(&self, used_gas: u64, journal: &mut J) -> Result<()> {
        let current = self.gas_backlog.get(journal)?;
        self.gas_backlog
            .set(current.saturating_add(used_gas), journal)?;
        Ok(())
    }

    pub fn shrink_backlog<J: JournalTr>(&self, gas: u64, journal: &mut J) -> Result<()> {
        let current = self.gas_backlog.get(journal)?;
        self.gas_backlog.set(current.saturating_sub(gas), journal)?;
        Ok(())
    }
}

fn approx_exp_basis_points(value: i64, accuracy: u64) -> i64 {
    let negative = value < 0;
    let x = if negative { -value } else { value } as u64;
    let base = ONE_IN_BIPS as u64;
    let mut result = base + x / accuracy;

    for i in (1..accuracy).rev() {
        result = base + result.saturating_mul(x) / (i * base);
    }

    if negative {
        ((base * base) / result).min(i64::MAX as u64) as i64
    } else {
        result.min(i64::MAX as u64) as i64
    }
}

fn big_mul_by_bips(value: U256, bips: i64) -> U256 {
    if bips < 0 {
        return U256::ZERO;
    }
    value.saturating_mul(U256::from(bips as u64)) / U256::from(ONE_IN_BIPS as u64)
}
