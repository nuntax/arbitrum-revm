use eyre::Result;
use revm::primitives::{Bytes, U256};

use super::{L2PricingOffset, StorageBacked, StorageSpace};
use crate::arb_journal::ArbJournal;

const ONE_IN_BIPS: i64 = 10_000;
const GAS_CONSTRAINTS_KEY: u8 = 0;
const MULTI_GAS_CONSTRAINTS_KEY: u8 = 1;
const MULTI_GAS_FEES_KEY: u8 = 2;
const SUB_STORAGE_VECTOR_LENGTH_OFFSET: u8 = 0;
const GAS_CONSTRAINT_TARGET_OFFSET: u8 = 0;
const GAS_CONSTRAINT_ADJUSTMENT_WINDOW_OFFSET: u8 = 1;
const GAS_CONSTRAINT_BACKLOG_OFFSET: u8 = 2;
const MULTI_GAS_CONSTRAINT_BACKLOG_OFFSET: u8 = 2;
const MULTI_GAS_CONSTRAINT_WEIGHTED_RESOURCES_OFFSET: u8 = 4;
pub const NUM_RESOURCE_KINDS: usize = 9;
const RESOURCE_KIND_SINGLE_DIM: u8 = 6;
const ARBOS_SINGLE_GAS_CONSTRAINTS_VERSION: u64 = 50;
const ARBOS_MULTI_CONSTRAINT_FIX_VERSION: u64 = 51;
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

struct MultiGasConstraint {
    target: StorageBacked<u64>,
    adjustment_window: StorageBacked<u32>,
    backlog: StorageBacked<u64>,
    max_weight: StorageBacked<u64>,
    weighted_resources: [StorageBacked<u64>; NUM_RESOURCE_KINDS],
}

impl MultiGasConstraint {
    fn open(storage: &StorageSpace) -> Self {
        Self {
            target: storage.storage_backed(0),
            adjustment_window: storage.storage_backed(1),
            backlog: storage.storage_backed(MULTI_GAS_CONSTRAINT_BACKLOG_OFFSET),
            max_weight: storage.storage_backed(3),
            weighted_resources: core::array::from_fn(|kind| {
                storage.storage_backed(MULTI_GAS_CONSTRAINT_WEIGHTED_RESOURCES_OFFSET + kind as u8)
            }),
        }
    }
}

struct MultiGasFees {
    next: [StorageBacked<U256>; NUM_RESOURCE_KINDS],
    current: [StorageBacked<U256>; NUM_RESOURCE_KINDS],
}

impl MultiGasFees {
    fn open(storage: &StorageSpace) -> Self {
        Self {
            next: core::array::from_fn(|kind| storage.storage_backed(kind as u8)),
            current: core::array::from_fn(|kind| {
                storage.storage_backed((NUM_RESOURCE_KINDS + kind) as u8)
            }),
        }
    }

    fn commit_next_to_current<J: ArbJournal>(&self, journal: &mut J) -> Result<()> {
        for kind in 0..NUM_RESOURCE_KINDS {
            let next = self.next[kind].get(journal)?;
            self.current[kind].set(next, journal)?;
        }
        Ok(())
    }
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
    multi_gas_fees: MultiGasFees,
}

impl L2Pricing {
    pub fn open(storage: &StorageSpace) -> Self {
        let gas_constraints = storage.open_subspace_with_key(GAS_CONSTRAINTS_KEY);
        let multi_gas_constraints = storage.open_subspace_with_key(MULTI_GAS_CONSTRAINTS_KEY);
        let multi_gas_fees = MultiGasFees::open(&storage.open_subspace_with_key(MULTI_GAS_FEES_KEY));
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
            multi_gas_fees,
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
                self.update_pricing_model_multi_constraints(time_passed, journal)
            }
        }
    }

    fn update_pricing_model_multi_constraints<J: ArbJournal>(
        &self,
        time_passed: u64,
        journal: &mut J,
    ) -> Result<()> {
        let constraints_len = self.multi_gas_constraints_len(journal)?;

        // Pay down each constraint's backlog by elapsed time * target.
        for i in 0..constraints_len {
            let constraint = self.open_multi_gas_constraint(i);
            let target = constraint.target.get(journal)?;
            let backlog = constraint.backlog.get(journal)?;
            constraint
                .backlog
                .set(backlog.saturating_sub(time_passed.saturating_mul(target)), journal)?;
        }

        let exponents = self.multi_gas_constraint_exponents(journal)?;
        let min_base_fee = self.min_base_fee_wei.get(journal)?;
        let mut max_base_fee = min_base_fee;
        for (kind, exponent) in exponents.into_iter().enumerate() {
            let base_fee = if exponent > 0 {
                big_mul_by_bips(min_base_fee, approx_exp_basis_points(exponent, 4))
            } else {
                min_base_fee
            };
            self.multi_gas_fees.next[kind].set(base_fee, journal)?;
            max_base_fee = max_base_fee.max(base_fee);
        }
        self.base_fee_wei.set(max_base_fee, journal)?;
        Ok(())
    }

    fn multi_gas_constraint_exponents<J: ArbJournal>(
        &self,
        journal: &mut J,
    ) -> Result<[i64; NUM_RESOURCE_KINDS]> {
        let constraints_len = self.multi_gas_constraints_len(journal)?;
        let mut exponents = [0_i64; NUM_RESOURCE_KINDS];
        for i in 0..constraints_len {
            let constraint = self.open_multi_gas_constraint(i);
            let target = constraint.target.get(journal)?;
            let backlog = constraint.backlog.get(journal)?;
            if backlog == 0 {
                continue;
            }
            let adjustment_window = u64::from(constraint.adjustment_window.get(journal)?);
            let max_weight = constraint.max_weight.get(journal)?;
            let divisor = adjustment_window
                .saturating_mul(target)
                .saturating_mul(max_weight);
            if divisor == 0 {
                continue;
            }
            for (kind, exponent) in exponents.iter_mut().enumerate() {
                // Nitro excludes the synthetic single-dimensional resource from base-fee pricing.
                if kind == usize::from(RESOURCE_KIND_SINGLE_DIM) {
                    continue;
                }
                let weight = constraint.weighted_resources[kind].get(journal)?;
                if weight == 0 {
                    continue;
                }
                let weighted_backlog = backlog.saturating_mul(weight);
                let dividend = (weighted_backlog as i128)
                    .saturating_mul(ONE_IN_BIPS as i128)
                    .min(i64::MAX as i128) as i64;
                let contribution = dividend / divisor.min(i64::MAX as u64) as i64;
                *exponent = exponent.saturating_add(contribution);
            }
        }
        Ok(exponents)
    }

    /// Nitro rotates the next-block multi-resource fees into the current-block slots before the
    /// start-block transaction. It is a no-op unless multi constraints are the active model.
    pub fn commit_multi_gas_fees<J: ArbJournal>(
        &self,
        arbos_version: u64,
        journal: &mut J,
    ) -> Result<()> {
        if self.gas_model(arbos_version, journal)? == GasModel::MultiGasConstraints {
            self.multi_gas_fees.commit_next_to_current(journal)?;
        }
        Ok(())
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
                // Nitro grows these backlogs from the transaction's complete `MultiGas` vector,
                // not from the scalar gas total. Upstream revm does not expose that vector yet;
                // using `used_gas` here (or silently doing nothing) would commit a deterministic
                // but invalid state root on a chain that enables this optional model.
                Err(eyre::eyre!(
                    "active ArbOS multi-gas constraints require per-resource EVM gas accounting"
                ))
            }
        }
    }

    /// Shrink the L2 gas backlog by `gas`, mirroring [`grow_backlog`] under the active gas model
    /// (Nitro `L2PricingState.ShrinkBacklog` -> `updateBacklog(Shrink, ..)`). This MUST be model-
    /// aware: under `SingleGasConstraints` the live backlog lives in the per-constraint slots, not
    /// the legacy `gas_backlog`, so a legacy-only shrink is a no-op on the real backlog and leaves
    /// it too high after a retryable redeem (the redeem's `ShrinkBacklog(gasToDonate)` releases the
    /// reservation the retry re-grows). Robinhood (SingleGasConstraints) diverged exactly here.
    ///
    /// [`grow_backlog`]: Self::grow_backlog
    pub fn shrink_backlog<J: ArbJournal>(
        &self,
        gas: u64,
        arbos_version: u64,
        journal: &mut J,
    ) -> Result<u64> {
        const READ: u64 = crate::arb_journal::STORAGE_READ_COST;
        const WRITE: u64 = crate::arb_journal::STORAGE_WRITE_COST;
        const WRITE_ZERO: u64 = crate::arb_journal::STORAGE_WRITE_ZERO_COST;

        let model = self.gas_model(arbos_version, journal)?;
        // Redeem manually charges this fixed amount and disables storage metering before calling
        // ShrinkBacklog at v60+, regardless of which pricing model is active.
        let fixed_cost = (arbos_version >= ARBOS_MULTI_GAS_CONSTRAINTS_VERSION)
            .then_some(READ + WRITE);
        match model {
            GasModel::Legacy => {
                let current = self.gas_backlog.get(journal)?;
                let updated = current.saturating_sub(gas);
                self.gas_backlog.set(updated, journal)?;
                Ok(fixed_cost.unwrap_or_else(|| {
                    let model_read = if arbos_version >= ARBOS_SINGLE_GAS_CONSTRAINTS_VERSION {
                        READ
                    } else {
                        0
                    };
                    model_read + READ + if updated == 0 { WRITE_ZERO } else { WRITE }
                }))
            }
            GasModel::SingleGasConstraints => {
                let constraints_len = self.gas_constraints_len(journal)?;
                let mut metered_cost = 2 * READ; // GasModelToUse length + traversal length.
                for i in 0..constraints_len {
                    let constraint = self.open_gas_constraint(i);
                    let backlog = constraint.backlog.get(journal)?;
                    let updated = backlog.saturating_sub(gas);
                    constraint
                        .backlog
                        .set(updated, journal)?;
                    metered_cost = metered_cost.saturating_add(
                        READ + if updated == 0 { WRITE_ZERO } else { WRITE },
                    );
                }
                Ok(fixed_cost.unwrap_or(metered_cost))
            }
            GasModel::MultiGasConstraints => {
                // ArbRetryableTx.Redeem passes MultiGas(SingleDim, gas) to ShrinkBacklog. Apply
                // that exact weighted resource delta to every configured multi-gas constraint.
                // Other resource amounts are zero for this call, so only the SingleDim weight can
                // affect the state (Nitro multi_gas_constraint.go `updateBacklog`).
                let constraints_len = self.multi_gas_constraints_len(journal)?;
                for i in 0..constraints_len {
                    let constraint = self.open_multi_gas_constraint(i);
                    let backlog = constraint.backlog.get(journal)?;
                    let weight = constraint.weighted_resources
                        [usize::from(RESOURCE_KIND_SINGLE_DIM)]
                    .get(journal)?;
                    let weighted_gas = gas.saturating_mul(weight);
                    constraint
                        .backlog
                        .set(backlog.saturating_sub(weighted_gas), journal)?;
                }
                Ok(fixed_cost.expect("multi-gas model is inactive before ArbOS 60"))
            }
        }
    }

    /// Nitro `L2PricingState.BacklogUpdateCost`, used by ArbRetryableTx.Redeem to reserve the
    /// trailing ShrinkBacklog gas before deciding how much gas to donate.
    pub fn backlog_update_cost<J: ArbJournal>(
        &self,
        arbos_version: u64,
        journal: &mut J,
    ) -> Result<u64> {
        let constraints_len = if (ARBOS_MULTI_CONSTRAINT_FIX_VERSION
            ..ARBOS_MULTI_GAS_CONSTRAINTS_VERSION)
            .contains(&arbos_version)
        {
            self.gas_constraints_len(journal)?
        } else {
            0
        };
        Ok(backlog_update_cost_for_constraints(
            arbos_version,
            constraints_len,
        ))
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

    fn open_multi_gas_constraint(&self, index: u64) -> MultiGasConstraint {
        let slot_id = index.to_be_bytes();
        let storage = self
            .multi_gas_constraints
            .open_subspace(Bytes::copy_from_slice(&slot_id));
        MultiGasConstraint::open(&storage)
    }

    /// Appends one Nitro multi-dimensional gas constraint using the fixed v60+ storage layout.
    pub fn add_multi_gas_constraint<J: ArbJournal>(
        &self,
        target: u64,
        adjustment_window: u32,
        backlog: u64,
        weights: [u64; NUM_RESOURCE_KINDS],
        journal: &mut J,
    ) -> Result<()> {
        let length = self.multi_gas_constraints_len(journal)?;
        let constraint = self.open_multi_gas_constraint(length);
        self.multi_gas_constraints
            .storage_backed::<u64>(SUB_STORAGE_VECTOR_LENGTH_OFFSET)
            .set(length.saturating_add(1), journal)?;
        constraint.target.set(target, journal)?;
        constraint
            .adjustment_window
            .set(adjustment_window, journal)?;
        constraint.backlog.set(backlog, journal)?;
        let max_weight = weights.into_iter().max().unwrap_or(0);
        constraint.max_weight.set(max_weight, journal)?;
        for (slot, weight) in constraint.weighted_resources.iter().zip(weights) {
            slot.set(weight, journal)?;
        }
        Ok(())
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

fn backlog_update_cost_for_constraints(arbos_version: u64, constraints_len: u64) -> u64 {
    const READ: u64 = crate::arb_journal::STORAGE_READ_COST;
    const WRITE: u64 = crate::arb_journal::STORAGE_WRITE_COST;

    if arbos_version >= ARBOS_MULTI_GAS_CONSTRAINTS_VERSION {
        return READ + WRITE;
    }

    let gas_model_read = if arbos_version >= ARBOS_SINGLE_GAS_CONSTRAINTS_VERSION {
        READ
    } else {
        0
    };
    if arbos_version >= ARBOS_MULTI_CONSTRAINT_FIX_VERSION && constraints_len > 0 {
        return gas_model_read
            .saturating_add(READ)
            .saturating_add(constraints_len.saturating_mul(READ + WRITE));
    }
    gas_model_read.saturating_add(READ + WRITE)
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

#[cfg(test)]
mod tests {
    use super::{
        NUM_RESOURCE_KINDS, RESOURCE_KIND_SINGLE_DIM, backlog_update_cost_for_constraints,
    };
    use crate::{
        ArbosState,
        api::default_ctx::{ArbContext, DefaultArb},
    };
    use revm::{context_interface::ContextTr, database_interface::EmptyDB};

    fn fresh() -> ArbContext<EmptyDB> {
        <ArbContext<EmptyDB> as DefaultArb>::arb()
    }

    #[test]
    fn backlog_update_cost_matches_every_nitro_version_boundary() {
        assert_eq!(backlog_update_cost_for_constraints(49, 7), 20_800);
        // v50 predates MultiConstraintFix: configured constraints do not alter the reservation.
        assert_eq!(backlog_update_cost_for_constraints(50, 7), 21_600);
        // v51-59: model read + traversal read + one read/write pair per configured constraint.
        assert_eq!(backlog_update_cost_for_constraints(51, 0), 21_600);
        assert_eq!(backlog_update_cost_for_constraints(51, 1), 22_400);
        assert_eq!(backlog_update_cost_for_constraints(59, 3), 64_000);
        // v60+ is the fixed static cost regardless of the active model or constraint count.
        assert_eq!(backlog_update_cost_for_constraints(60, 0), 20_800);
        assert_eq!(backlog_update_cost_for_constraints(60, 100), 20_800);
        assert_eq!(backlog_update_cost_for_constraints(61, 100), 20_800);
    }

    #[test]
    fn retryable_single_dim_shrink_updates_multi_constraint_weights() {
        let mut ctx = fresh();
        let journal = ctx.journal_mut();
        let pricing = ArbosState::open().l2_pricing;
        let mut weights = [0_u64; NUM_RESOURCE_KINDS];
        weights[usize::from(RESOURCE_KIND_SINGLE_DIM)] = 3;
        pricing
            .add_multi_gas_constraint(100, 10, 1_000, weights, journal)
            .unwrap();

        pricing.shrink_backlog(100, 60, journal).unwrap();
        let constraint = pricing.open_multi_gas_constraint(0);
        assert_eq!(constraint.backlog.get(journal).unwrap(), 700);
    }

    #[test]
    fn retryable_shrink_cost_matches_nitro_models_and_version_boundaries() {
        let mut legacy_ctx = fresh();
        let legacy_journal = legacy_ctx.journal_mut();
        let legacy = ArbosState::open().l2_pricing;
        legacy.gas_backlog.set(1_000, legacy_journal).unwrap();
        assert_eq!(legacy.shrink_backlog(100, 49, legacy_journal).unwrap(), 20_800);

        let mut v50_ctx = fresh();
        let v50_journal = v50_ctx.journal_mut();
        let v50 = ArbosState::open().l2_pricing;
        v50.add_gas_constraint(100, 10, 1_000, v50_journal).unwrap();
        // v50's historical reservation is 21600, but model lookup + traversal + a nonzero write
        // actually cost 22400. Nitro therefore runs out of gas in this configuration.
        assert_eq!(v50.shrink_backlog(100, 50, v50_journal).unwrap(), 22_400);

        let mut v51_ctx = fresh();
        let v51_journal = v51_ctx.journal_mut();
        let v51 = ArbosState::open().l2_pricing;
        for backlog in [50, 100, 1_000] {
            v51.add_gas_constraint(100, 10, backlog, v51_journal).unwrap();
        }
        // Two constraints become zero (5800 each) and one remains nonzero (20800), plus the two
        // vector-length reads. The 64000 maximum reservation refunds the exact 30000 difference.
        let actual = v51.shrink_backlog(100, 51, v51_journal).unwrap();
        assert_eq!(actual, 34_000);
        assert_eq!(backlog_update_cost_for_constraints(51, 3) - actual, 30_000);

        // v60+ charges the fixed cost and disables storage metering for the same state writes.
        assert_eq!(v51.shrink_backlog(100, 60, v51_journal).unwrap(), 20_800);
    }

    #[test]
    fn normal_tx_never_silently_skips_active_multi_constraint_growth() {
        let mut ctx = fresh();
        let journal = ctx.journal_mut();
        let pricing = ArbosState::open().l2_pricing;
        let mut weights = [0_u64; NUM_RESOURCE_KINDS];
        weights[1] = 1;
        pricing
            .add_multi_gas_constraint(100, 10, 1_000, weights, journal)
            .unwrap();

        let error = pricing.grow_backlog(100, 60, journal).unwrap_err();
        assert!(error.to_string().contains("per-resource EVM gas accounting"));
        assert_eq!(
            pricing
                .open_multi_gas_constraint(0)
                .backlog
                .get(journal)
                .unwrap(),
            1_000
        );
    }

    #[test]
    fn multi_constraint_exponents_match_nitro_vector() {
        let mut ctx = fresh();
        let journal = ctx.journal_mut();
        let pricing = ArbosState::open().l2_pricing;

        let mut first = [0_u64; NUM_RESOURCE_KINDS];
        first[1] = 1; // Computation
        first[3] = 2; // StorageAccessRead
        pricing
            .add_multi_gas_constraint(100_000, 10, 20_000, first, journal)
            .unwrap();

        let mut second = [0_u64; NUM_RESOURCE_KINDS];
        second[5] = 1; // StorageGrowth
        pricing
            .add_multi_gas_constraint(50_000, 5, 15_000, second, journal)
            .unwrap();

        let exponents = pricing.multi_gas_constraint_exponents(journal).unwrap();
        assert_eq!(exponents[1], 100);
        assert_eq!(exponents[3], 200);
        assert_eq!(exponents[5], 600);
        assert_eq!(exponents[usize::from(RESOURCE_KIND_SINGLE_DIM)], 0);
    }
}
