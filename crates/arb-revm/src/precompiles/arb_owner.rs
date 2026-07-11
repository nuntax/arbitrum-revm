use super::*;
use crate::arb_journal::{ArbCall, ArbJournal, ArbPrecompileCtx};
use crate::storage::{pack_uint, stylus_param_layout as layout};
use revm::interpreter::InstructionResult;
use revm::primitives::{B256, Bytes, Log, keccak256};

const FEATURE_ENABLE_DELAY_SECONDS: u64 = 7 * 24 * 60 * 60;
const ARBOS_VERSION_PER_TX_GAS_LIMIT: u64 = 50;
// Gas-constraint pricing model version gates (Nitro go-ethereum/params/config_arbitrum.go).
const ARBOS_MULTI_CONSTRAINT_FIX: u64 = 51;
const ARBOS_MULTI_GAS_CONSTRAINTS_VERSION: u64 = 60;
// Max single-gas constraints, enforced only in [MultiConstraintFix, MultiGasConstraintsVersion).
const GAS_CONSTRAINTS_MAX_NUM: u64 = 20;
const MIN_INIT_GAS_UNITS: u64 = 128;
const MIN_CACHED_INIT_GAS_UNITS: u64 = 32;
const COST_SCALAR_PERCENT_UNITS: u64 = 2;
const MAX_UINT24: u32 = 0x00ff_ffff;

// The feature-timestamp guards are written out per Nitro's ArbOwner checks rather than
// factored, so the parity mapping stays obvious.
#[allow(clippy::nonminimal_bool)]
pub(super) fn run_arb_owner<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
    call_inputs: &ArbCall,
) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let call = match ArbOwner::ArbOwnerCalls::abi_decode(input) {
        Ok(c) => c,
        Err(_) => return gated_revert_result(gas_limit),
    };

    let state = ArbosState::open();
    let block_timestamp: u64 = ctx.block_timestamp();
    let j = ctx.journal_mut();
    let is_owner = match state.chain_owners.is_member(call_inputs.caller, j) {
        Ok(v) => v,
        Err(e) => return revert_result(gas_limit, &format!("ArbOwner: owner check error: {e}")),
    };
    if !is_owner {
        return revert_result(
            gas_limit,
            "ArbOwner: unauthorized caller to access-controlled method",
        );
    }

    macro_rules! set_or_revert {
        ($expr:expr, $label:literal) => {
            match $expr {
                Ok(_) => ok_result(gas_limit, vec![]),
                Err(e) => revert_result(gas_limit, &format!("ArbOwner: {} error: {}", $label, e)),
            }
        };
    }

    let result = match call {
        ArbOwner::ArbOwnerCalls::addChainOwner(c) => {
            set_or_revert!(state.chain_owners.add(c.newOwner, j), "addChainOwner")
        }
        ArbOwner::ArbOwnerCalls::removeChainOwner(c) => match state
            .chain_owners
            .is_member(c.owner, j)
        {
            Ok(true) => set_or_revert!(state.chain_owners.remove(c.owner, j), "removeChainOwner"),
            Ok(false) => revert_result(gas_limit, "ArbOwner: tried to remove non-owner"),
            Err(e) => revert_result(gas_limit, &format!("ArbOwner: removeChainOwner error: {e}")),
        },
        ArbOwner::ArbOwnerCalls::addNativeTokenOwner(c) => {
            let enabled_from = match state.native_token_enabled_from_timestamp.get(j) {
                Ok(v) => v,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: addNativeTokenOwner error: {e}"),
                    );
                }
            };
            if enabled_from == 0 || enabled_from > block_timestamp {
                return revert_result(
                    gas_limit,
                    "ArbOwner: native token feature is not enabled yet",
                );
            }
            set_or_revert!(
                state.native_token_owners.add(c.newOwner, j),
                "addNativeTokenOwner"
            )
        }
        ArbOwner::ArbOwnerCalls::removeNativeTokenOwner(c) => {
            match state.native_token_owners.is_member(c.owner, j) {
                Ok(true) => set_or_revert!(
                    state.native_token_owners.remove(c.owner, j),
                    "removeNativeTokenOwner"
                ),
                Ok(false) => revert_result(
                    gas_limit,
                    "ArbOwner: tried to remove non native token owner",
                ),
                Err(e) => revert_result(
                    gas_limit,
                    &format!("ArbOwner: removeNativeTokenOwner error: {e}"),
                ),
            }
        }
        ArbOwner::ArbOwnerCalls::setNativeTokenManagementFrom(c) => {
            let current = match state.native_token_enabled_from_timestamp.get(j) {
                Ok(v) => v,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: setNativeTokenManagementFrom error: {e}"),
                    );
                }
            };
            if c.timestamp != 0 {
                let min_time = block_timestamp.saturating_add(FEATURE_ENABLE_DELAY_SECONDS);
                if (current == 0 && c.timestamp < min_time)
                    || (current > min_time && c.timestamp < min_time)
                {
                    return revert_result(
                        gas_limit,
                        "ArbOwner: feature must be enabled at least 7 days in the future",
                    );
                }
                if current > block_timestamp && current <= min_time && c.timestamp < current {
                    return revert_result(
                        gas_limit,
                        "ArbOwner: feature cannot be moved earlier than current scheduled enable time",
                    );
                }
            }
            set_or_revert!(
                state
                    .native_token_enabled_from_timestamp
                    .set(c.timestamp, j),
                "setNativeTokenManagementFrom"
            )
        }
        ArbOwner::ArbOwnerCalls::setTransactionFilteringFrom(c) => {
            let current = match state.transaction_filtering_enabled_from_timestamp.get(j) {
                Ok(v) => v,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: setTransactionFilteringFrom error: {e}"),
                    );
                }
            };
            if c.timestamp != 0 {
                let min_time = block_timestamp.saturating_add(FEATURE_ENABLE_DELAY_SECONDS);
                if (current == 0 && c.timestamp < min_time)
                    || (current > min_time && c.timestamp < min_time)
                {
                    return revert_result(
                        gas_limit,
                        "ArbOwner: feature must be enabled at least 7 days in the future",
                    );
                }
                if current > block_timestamp && current <= min_time && c.timestamp < current {
                    return revert_result(
                        gas_limit,
                        "ArbOwner: feature cannot be moved earlier than current scheduled enable time",
                    );
                }
            }
            set_or_revert!(
                state
                    .transaction_filtering_enabled_from_timestamp
                    .set(c.timestamp, j),
                "setTransactionFilteringFrom"
            )
        }
        ArbOwner::ArbOwnerCalls::addTransactionFilterer(c) => {
            let enabled_from = match state.transaction_filtering_enabled_from_timestamp.get(j) {
                Ok(v) => v,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: addTransactionFilterer error: {e}"),
                    );
                }
            };
            if enabled_from == 0 || enabled_from > block_timestamp {
                return revert_result(
                    gas_limit,
                    "ArbOwner: transaction filtering feature is not enabled yet",
                );
            }
            set_or_revert!(
                state.transaction_filterers.add(c.filterer, j),
                "addTransactionFilterer"
            )
        }
        ArbOwner::ArbOwnerCalls::removeTransactionFilterer(c) => {
            match state.transaction_filterers.is_member(c.filterer, j) {
                Ok(true) => set_or_revert!(
                    state.transaction_filterers.remove(c.filterer, j),
                    "removeTransactionFilterer"
                ),
                Ok(false) => revert_result(
                    gas_limit,
                    "ArbOwner: tried to remove non existing transaction filterer",
                ),
                Err(e) => revert_result(
                    gas_limit,
                    &format!("ArbOwner: removeTransactionFilterer error: {e}"),
                ),
            }
        }
        ArbOwner::ArbOwnerCalls::setFilteredFundsRecipient(c) => {
            set_or_revert!(
                state.filtered_funds_recipient.set(c.newRecipient, j),
                "setFilteredFundsRecipient"
            )
        }
        ArbOwner::ArbOwnerCalls::setNetworkFeeAccount(c) => {
            set_or_revert!(
                state.network_fee_account.set(c.newNetworkFeeAccount, j),
                "setNetworkFeeAccount"
            )
        }
        ArbOwner::ArbOwnerCalls::setInfraFeeAccount(c) => {
            set_or_revert!(
                state.infra_fee_account.set(c.newInfraFeeAccount, j),
                "setInfraFeeAccount"
            )
        }
        ArbOwner::ArbOwnerCalls::setL2BaseFee(c) => {
            set_or_revert!(
                state.l2_pricing.base_fee_wei.set(c.priceInWei, j),
                "setL2BaseFee"
            )
        }
        ArbOwner::ArbOwnerCalls::setMinimumL2BaseFee(c) => {
            set_or_revert!(
                state.l2_pricing.min_base_fee_wei.set(c.priceInWei, j),
                "setMinimumL2BaseFee"
            )
        }
        ArbOwner::ArbOwnerCalls::setSpeedLimit(c) => {
            if c.limit == 0 {
                return revert_result(gas_limit, "ArbOwner: speed limit must be nonzero");
            }
            set_or_revert!(
                state.l2_pricing.speed_limit_per_second.set(c.limit, j),
                "setSpeedLimit"
            )
        }
        ArbOwner::ArbOwnerCalls::setMaxTxGasLimit(c) => {
            let arbos_version = match state.arbos_version.get(j) {
                Ok(v) => v,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: setMaxTxGasLimit error: {e}"),
                    );
                }
            };
            if arbos_version < ARBOS_VERSION_PER_TX_GAS_LIMIT {
                set_or_revert!(
                    state.l2_pricing.per_block_gas_limit.set(c.limit, j),
                    "setMaxTxGasLimit"
                )
            } else {
                set_or_revert!(
                    state.l2_pricing.per_tx_gas_limit.set(c.limit, j),
                    "setMaxTxGasLimit"
                )
            }
        }
        ArbOwner::ArbOwnerCalls::setMaxBlockGasLimit(c) => {
            set_or_revert!(
                state.l2_pricing.per_block_gas_limit.set(c.limit, j),
                "setMaxBlockGasLimit"
            )
        }
        ArbOwner::ArbOwnerCalls::setL2GasPricingInertia(c) => {
            if c.sec == 0 {
                return revert_result(gas_limit, "ArbOwner: price inertia must be nonzero");
            }
            set_or_revert!(
                state.l2_pricing.pricing_inertia.set(c.sec, j),
                "setL2GasPricingInertia"
            )
        }
        ArbOwner::ArbOwnerCalls::setL2GasBacklogTolerance(c) => {
            set_or_revert!(
                state.l2_pricing.backlog_tolerance.set(c.sec, j),
                "setL2GasBacklogTolerance"
            )
        }
        ArbOwner::ArbOwnerCalls::setGasBacklog(c) => {
            set_or_revert!(
                state.l2_pricing.gas_backlog.set(c.backlog, j),
                "setGasBacklog"
            )
        }
        ArbOwner::ArbOwnerCalls::setL1BaseFeeEstimateInertia(c) => {
            set_or_revert!(
                state.l1_pricing.inertia.set(c.inertia, j),
                "setL1BaseFeeEstimateInertia"
            )
        }
        ArbOwner::ArbOwnerCalls::setL1PricingEquilibrationUnits(c) => {
            set_or_revert!(
                state
                    .l1_pricing
                    .equilibration_units
                    .set(c.equilibrationUnits, j),
                "setL1PricingEquilibrationUnits"
            )
        }
        ArbOwner::ArbOwnerCalls::setL1PricingInertia(c) => {
            set_or_revert!(
                state.l1_pricing.inertia.set(c.inertia, j),
                "setL1PricingInertia"
            )
        }
        ArbOwner::ArbOwnerCalls::setL1PricingRewardRecipient(c) => {
            set_or_revert!(
                state.l1_pricing.pay_rewards_to.set(c.recipient, j),
                "setL1PricingRewardRecipient"
            )
        }
        ArbOwner::ArbOwnerCalls::setL1PricingRewardRate(c) => {
            set_or_revert!(
                state.l1_pricing.per_unit_reward.set(c.weiPerUnit, j),
                "setL1PricingRewardRate"
            )
        }
        ArbOwner::ArbOwnerCalls::setL1PricePerUnit(c) => {
            set_or_revert!(
                state.l1_pricing.price_per_unit.set(c.pricePerUnit, j),
                "setL1PricePerUnit"
            )
        }
        ArbOwner::ArbOwnerCalls::setParentGasFloorPerToken(c) => {
            set_or_revert!(
                state
                    .l1_pricing
                    .gas_floor_per_token
                    .set(c.gasFloorPerToken, j),
                "setParentGasFloorPerToken"
            )
        }
        ArbOwner::ArbOwnerCalls::setPerBatchGasCharge(c) => {
            set_or_revert!(
                state.l1_pricing.per_batch_gas_cost.set(c.cost, j),
                "setPerBatchGasCharge"
            )
        }
        ArbOwner::ArbOwnerCalls::setAmortizedCostCapBips(c) => {
            set_or_revert!(
                state.l1_pricing.amortized_cost_cap_bips.set(c.cap, j),
                "setAmortizedCostCapBips"
            )
        }
        ArbOwner::ArbOwnerCalls::setBrotliCompressionLevel(c) => {
            set_or_revert!(
                state.brotli_compression_level.set(c.level, j),
                "setBrotliCompressionLevel"
            )
        }
        ArbOwner::ArbOwnerCalls::setCalldataPriceIncrease(c) => {
            set_or_revert!(
                state.features.set_calldata_price_increase(c.enable, j),
                "setCalldataPriceIncrease"
            )
        }
        ArbOwner::ArbOwnerCalls::scheduleArbOSUpgrade(c) => {
            match state.upgrade_version.set(c.newVersion, j) {
                Ok(_) => {}
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: scheduleArbOSUpgrade error: {e}"),
                    );
                }
            }
            set_or_revert!(
                state.upgrade_timestamp.set(c.timestamp, j),
                "scheduleArbOSUpgrade"
            )
        }
        ArbOwner::ArbOwnerCalls::setChainConfig(_) => {
            // Chain config update requires JSON deserialization; stub out for now.
            revert_result(gas_limit, "ArbOwner: setChainConfig not yet implemented")
        }
        ArbOwner::ArbOwnerCalls::setGasPricingConstraints(c) => {
            // Nitro ArbOwner.SetGasPricingConstraints: clear the existing constraints, then install
            // the new ones. Mirrors the ArbOS-50 single-gas-constraint pricing model (each is
            // {target, adjustmentWindow, backlog}); the base-fee model already consumes these.
            let arbos_version = match state.arbos_version.get(j) {
                Ok(v) => v,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: setGasPricingConstraints error: {e}"),
                    );
                }
            };
            if let Err(e) = state.l2_pricing.clear_gas_constraints(j) {
                return revert_result(
                    gas_limit,
                    &format!("ArbOwner: setGasPricingConstraints error: {e}"),
                );
            }
            // The count limit applies only in [MultiConstraintFix, MultiGasConstraintsVersion)
            // (Nitro: retryable-redeem gas accounting bounds it; lifted at ArbOS 60).
            if (ARBOS_MULTI_CONSTRAINT_FIX..ARBOS_MULTI_GAS_CONSTRAINTS_VERSION)
                .contains(&arbos_version)
                && c.constraints.len() as u64 > GAS_CONSTRAINTS_MAX_NUM
            {
                return revert_result(gas_limit, "ArbOwner: too many constraints");
            }
            let mut failed: Option<String> = None;
            for constraint in &c.constraints {
                let target = constraint[0];
                let adjustment_window = constraint[1];
                let backlog = constraint[2];
                if target == 0 || adjustment_window == 0 {
                    failed = Some(format!(
                        "invalid constraint with target {target} and adjustment window {adjustment_window}"
                    ));
                    break;
                }
                if let Err(e) =
                    state
                        .l2_pricing
                        .add_gas_constraint(target, adjustment_window, backlog, j)
                {
                    failed = Some(format!("failed to add constraint: {e}"));
                    break;
                }
            }
            match failed {
                None => ok_result(gas_limit, vec![]),
                Some(msg) => {
                    revert_result(gas_limit, &format!("ArbOwner: setGasPricingConstraints {msg}"))
                }
            }
        }
        ArbOwner::ArbOwnerCalls::releaseL1PricerSurplusFunds(_) => revert_result(
            gas_limit,
            "ArbOwner: releaseL1PricerSurplusFunds not yet implemented",
        ),
        // Stylus / WASM settings, stored as packed bytes in a single 32-byte
        // storage word at index 0 of the programs.params subspace.
        // Each setter performs a read-modify-write on the packed word.
        // Nitro reference: arbos/programs/params.go Save() / Params().
        ArbOwner::ArbOwnerCalls::setInkPrice(c) => {
            if c.inkPrice == 0 || c.inkPrice > MAX_UINT24 {
                return revert_result(gas_limit, "ArbOwner: ink price must be a positive uint24");
            }
            let mut word = match state.programs.read_params_word(j) {
                Ok(w) => w,
                Err(e) => {
                    return revert_result(gas_limit, &format!("ArbOwner: setInkPrice read: {e}"))
                }
            };
            pack_uint(&mut word, layout::INK_PRICE.0, layout::INK_PRICE.1, c.inkPrice);
            set_or_revert!(state.programs.write_params_word(word, j), "setInkPrice")
        }
        ArbOwner::ArbOwnerCalls::setWasmMaxStackDepth(c) => {
            let mut word = match state.programs.read_params_word(j) {
                Ok(w) => w,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: setWasmMaxStackDepth read: {e}"),
                    )
                }
            };
            pack_uint(
                &mut word,
                layout::MAX_STACK_DEPTH.0,
                layout::MAX_STACK_DEPTH.1,
                c.depth,
            );
            set_or_revert!(
                state.programs.write_params_word(word, j),
                "setWasmMaxStackDepth"
            )
        }
        ArbOwner::ArbOwnerCalls::setWasmFreePages(c) => {
            let mut word = match state.programs.read_params_word(j) {
                Ok(w) => w,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: setWasmFreePages read: {e}"),
                    )
                }
            };
            pack_uint(
                &mut word,
                layout::FREE_PAGES.0,
                layout::FREE_PAGES.1,
                c.pages.into(),
            );
            set_or_revert!(state.programs.write_params_word(word, j), "setWasmFreePages")
        }
        ArbOwner::ArbOwnerCalls::setWasmPageGas(c) => {
            let mut word = match state.programs.read_params_word(j) {
                Ok(w) => w,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: setWasmPageGas read: {e}"),
                    )
                }
            };
            pack_uint(
                &mut word,
                layout::PAGE_GAS.0,
                layout::PAGE_GAS.1,
                c.gas.into(),
            );
            set_or_revert!(state.programs.write_params_word(word, j), "setWasmPageGas")
        }
        ArbOwner::ArbOwnerCalls::setWasmPageLimit(c) => {
            let mut word = match state.programs.read_params_word(j) {
                Ok(w) => w,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: setWasmPageLimit read: {e}"),
                    )
                }
            };
            pack_uint(
                &mut word,
                layout::PAGE_LIMIT.0,
                layout::PAGE_LIMIT.1,
                c.limit.into(),
            );
            set_or_revert!(state.programs.write_params_word(word, j), "setWasmPageLimit")
        }
        ArbOwner::ArbOwnerCalls::setWasmMinInitGas(c) => {
            let gas_units =
                c.gas.saturating_add(MIN_INIT_GAS_UNITS.saturating_sub(1)) / MIN_INIT_GAS_UNITS;
            let cached_units = c
                .cached
                .saturating_add(MIN_CACHED_INIT_GAS_UNITS.saturating_sub(1))
                / MIN_CACHED_INIT_GAS_UNITS;
            let mut word = match state.programs.read_params_word(j) {
                Ok(w) => w,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: setWasmMinInitGas read: {e}"),
                    )
                }
            };
            pack_uint(
                &mut word,
                layout::MIN_INIT_GAS.0,
                layout::MIN_INIT_GAS.1,
                gas_units as u32,
            );
            pack_uint(
                &mut word,
                layout::MIN_CACHED_INIT_GAS.0,
                layout::MIN_CACHED_INIT_GAS.1,
                cached_units as u32,
            );
            set_or_revert!(
                state.programs.write_params_word(word, j),
                "setWasmMinInitGas"
            )
        }
        ArbOwner::ArbOwnerCalls::setWasmInitCostScalar(c) => {
            let units = c
                .percent
                .saturating_add(COST_SCALAR_PERCENT_UNITS.saturating_sub(1))
                / COST_SCALAR_PERCENT_UNITS;
            let mut word = match state.programs.read_params_word(j) {
                Ok(w) => w,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: setWasmInitCostScalar read: {e}"),
                    )
                }
            };
            pack_uint(
                &mut word,
                layout::INIT_COST_SCALAR.0,
                layout::INIT_COST_SCALAR.1,
                units as u32,
            );
            set_or_revert!(
                state.programs.write_params_word(word, j),
                "setWasmInitCostScalar"
            )
        }
        ArbOwner::ArbOwnerCalls::setWasmExpiryDays(c) => {
            let mut word = match state.programs.read_params_word(j) {
                Ok(w) => w,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: setWasmExpiryDays read: {e}"),
                    )
                }
            };
            pack_uint(
                &mut word,
                layout::EXPIRY_DAYS.0,
                layout::EXPIRY_DAYS.1,
                c.days.into(),
            );
            set_or_revert!(
                state.programs.write_params_word(word, j),
                "setWasmExpiryDays"
            )
        }
        ArbOwner::ArbOwnerCalls::setWasmKeepaliveDays(c) => {
            let mut word = match state.programs.read_params_word(j) {
                Ok(w) => w,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: setWasmKeepaliveDays read: {e}"),
                    )
                }
            };
            pack_uint(
                &mut word,
                layout::KEEPALIVE_DAYS.0,
                layout::KEEPALIVE_DAYS.1,
                c.keepaliveDays.into(),
            );
            set_or_revert!(
                state.programs.write_params_word(word, j),
                "setWasmKeepaliveDays"
            )
        }
        ArbOwner::ArbOwnerCalls::setWasmBlockCacheSize(c) => {
            let mut word = match state.programs.read_params_word(j) {
                Ok(w) => w,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: setWasmBlockCacheSize read: {e}"),
                    )
                }
            };
            pack_uint(
                &mut word,
                layout::BLOCK_CACHE_SIZE.0,
                layout::BLOCK_CACHE_SIZE.1,
                c.count.into(),
            );
            set_or_revert!(
                state.programs.write_params_word(word, j),
                "setWasmBlockCacheSize"
            )
        }
        ArbOwner::ArbOwnerCalls::setWasmMaxSize(c) => {
            let mut word = match state.programs.read_params_word(j) {
                Ok(w) => w,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: setWasmMaxSize read: {e}"),
                    )
                }
            };
            pack_uint(
                &mut word,
                layout::MAX_WASM_SIZE.0,
                layout::MAX_WASM_SIZE.1,
                c.maxWasmSize,
            );
            set_or_revert!(state.programs.write_params_word(word, j), "setWasmMaxSize")
        }
        ArbOwner::ArbOwnerCalls::addWasmCacheManager(c) => set_or_revert!(
            state.programs.cache_managers.add(c.manager, j),
            "addWasmCacheManager"
        ),
        ArbOwner::ArbOwnerCalls::removeWasmCacheManager(c) => {
            match state.programs.cache_managers.is_member(c.manager, j) {
                Ok(true) => set_or_revert!(
                    state.programs.cache_managers.remove(c.manager, j),
                    "removeWasmCacheManager"
                ),
                Ok(false) => revert_result(gas_limit, "ArbOwner: tried to remove non-manager"),
                Err(e) => revert_result(
                    gas_limit,
                    &format!("ArbOwner: removeWasmCacheManager error: {e}"),
                ),
            }
        }
        ArbOwner::ArbOwnerCalls::setMaxStylusContractFragments(c) => {
            let mut word = match state.programs.read_params_word(j) {
                Ok(w) => w,
                Err(e) => {
                    return revert_result(
                        gas_limit,
                        &format!("ArbOwner: setMaxStylusContractFragments read: {e}"),
                    )
                }
            };
            pack_uint(
                &mut word,
                layout::MAX_FRAGMENT_COUNT.0,
                layout::MAX_FRAGMENT_COUNT.1,
                u32::from(c.maxFragments),
            );
            set_or_revert!(
                state.programs.write_params_word(word, j),
                "setMaxStylusContractFragments"
            )
        }
    };

    // Nitro wraps every successful, state-mutating ArbOwner method with an `OwnerActs` event
    // (precompiles/precompile.go `emitOwnerActs` via `OwnerPrecompile.Call`): the event records the
    // 4-byte method selector, the owner, and the full calldata. The owner is NOT charged gas for it
    // (Nitro returns `ZeroGas`; our `ok_result` already records zero precompile gas). Without it the
    // produced block's receipts/logs-bloom (and thus the block hash) diverge from Nitro on any
    // owner action. `event OwnerActs(bytes4 indexed method, address indexed owner, bytes data)`.
    if result.result == InstructionResult::Return && !call_inputs.is_static && input.len() >= 4 {
        let mut method_topic = [0u8; 32];
        method_topic[..4].copy_from_slice(&input[..4]);
        let mut owner_topic = [0u8; 32];
        owner_topic[12..].copy_from_slice(call_inputs.caller.as_slice());

        // ABI-encode the single `bytes data` argument: offset (0x20) | length | content (32-padded).
        let mut data = Vec::with_capacity(64 + input.len().next_multiple_of(32));
        data.extend_from_slice(&U256::from(32u64).to_be_bytes::<32>());
        data.extend_from_slice(&U256::from(input.len()).to_be_bytes::<32>());
        data.extend_from_slice(input);
        data.resize(64 + input.len().next_multiple_of(32), 0);

        ctx.journal_mut().emit_log(Log::new_unchecked(
            call_inputs.bytecode_address,
            vec![
                keccak256("OwnerActs(bytes4,address,bytes)"),
                B256::from(method_topic),
                B256::from(owner_topic),
            ],
            Bytes::from(data),
        ));
    }

    result
}
