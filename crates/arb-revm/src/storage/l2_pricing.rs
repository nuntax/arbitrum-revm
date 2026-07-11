use eyre::Result;
use revm::primitives::{Bytes, U256};

use super::{L2PricingOffset, StorageBacked, StorageSpace};
use crate::arb_journal::ArbJournal;

const ONE_IN_BIPS: i64 = 10_000;
const GAS_CONSTRAINTS_KEY: u8 = 0;
const MULTI_GAS_CONSTRAINTS_KEY: u8 = 1;
const SUB_STORAGE_VECTOR_LENGTH_OFFSET: u8 = 0;
const GAS_CONSTRAINT_TARGET_OFFSET: u8 = 0;
const GAS_CONSTRAINT_ADJUSTMENT_WINDOW_OFFSET: u8 = 1;
const GAS_CONSTRAINT_BACKLOG_OFFSET: u8 = 2;
const ARBOS_SINGLE_GAS_CONSTRAINTS_VERSION: u64 = 50;
const ARBOS_MULTI_GAS_CONSTRAINTS_VERSION: u64 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GasModel {
    Legacy,
    SingleGasConstraints,
    MultiGasConstraints,
}

struct GasConstraint {
    target: StorageBacked<u64>,
    adjustment_window: StorageBacked<u64>,
    backlog: StorageBacked<u64>,
}

impl GasConstraint {
    fn open(storage: &StorageSpace) -> Self {
        Self {
            target: storage.storage_backed(GAS_CONSTRAINT_TARGET_OFFSET),
            adjustment_window: storage.storage_backed(GAS_CONSTRAINT_ADJUSTMENT_WINDOW_OFFSET),
            backlog: storage.storage_backed(GAS_CONSTRAINT_BACKLOG_OFFSET),
        }
    }
}

/// ArbOS L2 pricing storage wrapper.
pub struct L2Pricing {
    pub speed_limit_per_second: StorageBacked<u64>,
    pub per_block_gas_limit: StorageBacked<u64>,
    pub per_tx_gas_limit: StorageBacked<u64>,
    pub base_fee_wei: StorageBacked<U256>,
    pub min_base_fee_wei: StorageBacked<U256>,
    pub gas_backlog: StorageBacked<u64>,
    pub pricing_inertia: StorageBacked<u64>,
    pub backlog_tolerance: StorageBacked<u64>,
    gas_constraints: StorageSpace,
    multi_gas_constraints: StorageSpace,
}

impl L2Pricing {
    pub fn open(storage: &StorageSpace) -> Self {
        let gas_constraints = storage.open_subspace_with_key(GAS_CONSTRAINTS_KEY);
        let multi_gas_constraints = storage.open_subspace_with_key(MULTI_GAS_CONSTRAINTS_KEY);
        Self {
            speed_limit_per_second: storage
                .storage_backed(L2PricingOffset::SpeedLimitPerSecond as u8),
            per_block_gas_limit: storage.storage_backed(L2PricingOffset::PerBlockGasLimit as u8),
            per_tx_gas_limit: storage.storage_backed(L2PricingOffset::PerTxGasLimit as u8),
            base_fee_wei: storage.storage_backed(L2PricingOffset::BaseFeeWei as u8),
            min_base_fee_wei: storage.storage_backed(L2PricingOffset::MinBaseFeeWei as u8),
            gas_backlog: storage.storage_backed(L2PricingOffset::GasBacklog as u8),
            pricing_inertia: storage.storage_backed(L2PricingOffset::PricingInertia as u8),
            backlog_tolerance: storage.storage_backed(L2PricingOffset::BacklogTolerance as u8),
            gas_constraints,
            multi_gas_constraints,
        }
    }

    pub fn update_pricing_model<J: ArbJournal>(
        &self,
        time_passed: u64,
        arbos_version: u64,
        journal: &mut J,
    ) -> Result<()> {
        let model = self.gas_model(arbos_version, journal)?;
        match model {
            GasModel::Legacy => self.update_pricing_model_legacy(time_passed, journal),
            GasModel::SingleGasConstraints => {
                self.update_pricing_model_single_constraints(time_passed, journal)
            }
            GasModel::MultiGasConstraints => {
                // TODO(parity): implement full multi-gas constraints pricing model.
                Ok(())
            }
        }
    }

    fn update_pricing_model_legacy<J: ArbJournal>(&self, time_passed: u64, journal: &mut J) -> Result<()> {
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

    fn update_pricing_model_single_constraints<J: ArbJournal>(
        &self,
        time_passed: u64,
        journal: &mut J,
    ) -> Result<()> {
        let constraints_len = self.gas_constraints_len(journal)?;
        let mut total_exponent_bips: i64 = 0;

        for i in 0..constraints_len {
            let constraint = self.open_gas_constraint(i);
            let target = constraint.target.get(journal)?;
            let gas_to_shrink = time_passed.saturating_mul(target);
            let backlog = constraint.backlog.get(journal)?;
            let backlog = backlog.saturating_sub(gas_to_shrink);
            constraint.backlog.set(backlog, journal)?;

            if backlog > 0 {
                let inertia = constraint.adjustment_window.get(journal)?;
                let divisor = inertia.saturating_mul(target);
                if divisor > 0 {
                    let dividend = (backlog as i128)
                        .saturating_mul(ONE_IN_BIPS as i128)
                        .min(i64::MAX as i128) as i64;
                    let exponent = dividend / divisor as i64;
                    total_exponent_bips = total_exponent_bips.saturating_add(exponent);
                }
            }
        }

        let min_base_fee = self.min_base_fee_wei.get(journal)?;
        let next_base_fee = if total_exponent_bips > 0 {
            big_mul_by_bips(min_base_fee, approx_exp_basis_points(total_exponent_bips, 4))
        } else {
            min_base_fee
        };
        self.base_fee_wei.set(next_base_fee, journal)?;
        Ok(())
    }

    pub fn grow_backlog<J: ArbJournal>(
        &self,
        used_gas: u64,
        arbos_version: u64,
        journal: &mut J,
    ) -> Result<()> {
        let model = self.gas_model(arbos_version, journal)?;
        match model {
            GasModel::Legacy => {
                let current = self.gas_backlog.get(journal)?;
                self.gas_backlog
                    .set(current.saturating_add(used_gas), journal)?;
                Ok(())
            }
            GasModel::SingleGasConstraints => {
                let constraints_len = self.gas_constraints_len(journal)?;
                for i in 0..constraints_len {
                    let constraint = self.open_gas_constraint(i);
                    let backlog = constraint.backlog.get(journal)?;
                    constraint
                        .backlog
                        .set(backlog.saturating_add(used_gas), journal)?;
                }
                Ok(())
            }
            GasModel::MultiGasConstraints => {
                // TODO(parity): implement full multi-gas constraints backlog updates.
                Ok(())
            }
        }
    }

    pub fn shrink_backlog<J: ArbJournal>(&self, gas: u64, journal: &mut J) -> Result<()> {
        let current = self.gas_backlog.get(journal)?;
        self.gas_backlog.set(current.saturating_sub(gas), journal)?;
        Ok(())
    }

    fn gas_model<J: ArbJournal>(&self, arbos_version: u64, journal: &mut J) -> Result<GasModel> {
        if arbos_version >= ARBOS_MULTI_GAS_CONSTRAINTS_VERSION
            && self.multi_gas_constraints_len(journal)? > 0
        {
            return Ok(GasModel::MultiGasConstraints);
        }
        if arbos_version >= ARBOS_SINGLE_GAS_CONSTRAINTS_VERSION
            && self.gas_constraints_len(journal)? > 0
        {
            return Ok(GasModel::SingleGasConstraints);
        }
        Ok(GasModel::Legacy)
    }

    fn gas_constraints_len<J: ArbJournal>(&self, journal: &mut J) -> Result<u64> {
        self.gas_constraints
            .storage_backed::<u64>(SUB_STORAGE_VECTOR_LENGTH_OFFSET)
            .get(journal)
    }

    fn multi_gas_constraints_len<J: ArbJournal>(&self, journal: &mut J) -> Result<u64> {
        self.multi_gas_constraints
            .storage_backed::<u64>(SUB_STORAGE_VECTOR_LENGTH_OFFSET)
            .get(journal)
    }

    fn open_gas_constraint(&self, index: u64) -> GasConstraint {
        let slot_id = index.to_be_bytes();
        let storage = self
            .gas_constraints
            .open_subspace(Bytes::copy_from_slice(&slot_id));
        GasConstraint::open(&storage)
    }

    /// Appends a gas constraint to the `gasConstraints` sub-storage vector, mirroring Nitro
    /// `L2PricingState.AddGasConstraint` (`storage.SubStorageVector.Push` then set the three
    /// fields). Push opens the element sub-storage at the big-endian current length, then bumps
    /// the length slot (offset 0 of the vector root).
    pub fn add_gas_constraint<J: ArbJournal>(
        &self,
        target: u64,
        adjustment_window: u64,
        backlog: u64,
        journal: &mut J,
    ) -> Result<()> {
        let length = self.gas_constraints_len(journal)?;
        let constraint = self.open_gas_constraint(length);
        self.gas_constraints
            .storage_backed::<u64>(SUB_STORAGE_VECTOR_LENGTH_OFFSET)
            .set(length + 1, journal)?;
        constraint.target.set(target, journal)?;
        constraint.adjustment_window.set(adjustment_window, journal)?;
        constraint.backlog.set(backlog, journal)?;
        Ok(())
    }

    /// Clears every gas constraint, mirroring Nitro `L2PricingState.ClearGasConstraints`: pop each
    /// element (decrement the length slot) from the tail and zero its three fields.
    pub fn clear_gas_constraints<J: ArbJournal>(&self, journal: &mut J) -> Result<()> {
        let length = self.gas_constraints_len(journal)?;
        for _ in 0..length {
            let current = self.gas_constraints_len(journal)?;
            let constraint = self.open_gas_constraint(current - 1);
            self.gas_constraints
                .storage_backed::<u64>(SUB_STORAGE_VECTOR_LENGTH_OFFSET)
                .set(current - 1, journal)?;
            constraint.target.set(0, journal)?;
            constraint.adjustment_window.set(0, journal)?;
            constraint.backlog.set(0, journal)?;
        }
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
