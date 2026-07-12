use crate::{
    ArbSpecId, ArbosState,
    api::exec::ArbContextTr,
    constants::{
        ARB_RETRYABLE_TX_ADDRESS, ARBITRUM_CONTRACT_TX_TYPE, ARBITRUM_DEPOSIT_TX_TYPE,
        ARBITRUM_INTERNAL_TX_TYPE, ARBITRUM_RETRY_TX_TYPE, ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE,
        ARBOS_ACTS_ADDRESS,
        BATCH_POSTER_ADDRESS, CURRENT_TX_L1_FEE_ADDR, FILTERED_TRANSACTIONS_STATE_ADDRESS,
        L1_PRICER_FUNDS_POOL_ADDRESS,
    },
    deposit_tx, internal_tx,
    l1_cost::{compute_poster_info, encode_tx_bytes},
    retry_tx,
    storage::StorageSpace,
    submit_retryable_tx,
    transaction::ArbTxTr,
};
use revm::{
    context_interface::{
        Block, Cfg, ContextTr, JournalTr, Transaction,
        journaled_state::account::JournaledAccountTr,
        result::{FromStringError, HaltReason, InvalidTransaction},
    },
    handler::{
        EthFrame, EvmTr, FrameResult, Handler, MainnetHandler, evm::FrameTr, handler::EvmTrError,
    },
    inspector::{Inspector, InspectorEvmTr, InspectorHandler},
    interpreter::{
        CallOutcome, Gas, InitialAndFloorGas, InstructionResult, InterpreterResult,
        interpreter::EthInterpreter, interpreter_action::FrameInit,
    },
    primitives::{Address, Bytes, U256, hardfork::SpecId, keccak256},
};

/// ArbOS version at which the on-chain transaction filter is active (Nitro
/// ArbosVersion_TransactionFiltering = 60).
const ARBOS_VERSION_TRANSACTION_FILTERING: u64 = 60;

/// Arbitrum handler that composes mainnet logic and overrides Arbitrum-specific
/// transaction semantics.
#[derive(Debug, Clone)]
pub struct ArbHandler<EVM, ERROR, FRAME> {
    /// Mainnet behavior reused where Arbitrum does not diverge.
    pub mainnet: MainnetHandler<EVM, ERROR, FRAME>,
}

impl<EVM, ERROR, FRAME> ArbHandler<EVM, ERROR, FRAME> {
    /// Creates a new Arbitrum handler.
    pub fn new() -> Self {
        Self {
            mainnet: MainnetHandler::default(),
        }
    }
}

impl<EVM, ERROR, FRAME> Default for ArbHandler<EVM, ERROR, FRAME> {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn is_internal_tx<EVM: EvmTr>(evm: &mut EVM) -> bool {
    evm.ctx().tx().tx_type() == ARBITRUM_INTERNAL_TX_TYPE
}

#[inline]
fn is_deposit_tx<EVM: EvmTr>(evm: &mut EVM) -> bool {
    evm.ctx().tx().tx_type() == ARBITRUM_DEPOSIT_TX_TYPE
}

#[inline]
fn is_submit_retryable_tx<EVM: EvmTr>(evm: &mut EVM) -> bool {
    evm.ctx().tx().tx_type() == ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE
}

#[inline]
fn is_retry_tx<EVM: EvmTr>(evm: &mut EVM) -> bool {
    evm.ctx().tx().tx_type() == ARBITRUM_RETRY_TX_TYPE
}

#[inline]
fn is_protocol_short_circuit_tx<EVM: EvmTr>(evm: &mut EVM) -> bool {
    is_internal_tx(evm) || is_deposit_tx(evm) || is_submit_retryable_tx(evm)
}

#[inline]
fn is_protocol_env_bypass_tx<EVM: EvmTr>(evm: &mut EVM) -> bool {
    is_protocol_short_circuit_tx(evm) || is_retry_tx(evm)
}

#[inline]
fn is_allowed_internal_caller(caller: Address) -> bool {
    caller == ARBOS_ACTS_ADDRESS
}

#[inline]
fn collect_tips_enabled(spec: ArbSpecId, delayed_inbox: bool, collect_tips_flag: u64) -> bool {
    // Nitro tx_processor.go CollectTips():
    // - never collect tips on delayed inbox messages
    // - v9: collect all tips
    // - v10..v59: drop all tips
    // - v60+: collect based on storage flag
    if delayed_inbox {
        return false;
    }
    if spec == ArbSpecId::ARBOS_9 {
        return true;
    }
    if !spec.is_enabled_in(ArbSpecId::ARBOS_60) {
        return false;
    }
    collect_tips_flag != 0
}

/// True when the current (non-protocol) tx's hash was pre-registered in the on-chain
/// transaction filter. Nitro `RevertedTxHook` skips such a tx's execution but consumes all its gas.
/// The read is free (raw journal), matching Nitro's `IsFilteredFree`.
fn is_filtered_normal_tx<EVM>(evm: &mut EVM) -> bool
where
    EVM: EvmTr<Context: ArbContextTr>,
{
    let tx_hash = match evm.ctx().tx().encoded_2718_bytes() {
        Some(bytes) => keccak256(bytes),
        None => return false,
    };
    let arbos = ArbosState::open();
    let journal = evm.ctx_mut().journal_mut();
    if arbos.arbos_version.get(journal).unwrap_or(0) < ARBOS_VERSION_TRANSACTION_FILTERING {
        return false;
    }
    StorageSpace::new(FILTERED_TRANSACTIONS_STATE_ADDRESS)
        .get(tx_hash, journal)
        .map(|v| v.data == U256::ONE)
        .unwrap_or(false)
}

impl<EVM, ERROR, FRAME> Handler for ArbHandler<EVM, ERROR, FRAME>
where
    EVM: EvmTr<Context: ArbContextTr, Frame = FRAME>,
    ERROR: EvmTrError<EVM> + FromStringError,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    type Evm = EVM;
    type Error = ERROR;
    type HaltReason = HaltReason;

    fn validate(&self, evm: &mut Self::Evm) -> Result<InitialAndFloorGas, Self::Error> {
        if is_protocol_short_circuit_tx(evm) {
            self.validate_env(evm)?;
            evm.ctx_mut().chain_mut().intrinsic_gas = 0;
            return Ok(InitialAndFloorGas::new(0, 0));
        }
        if is_retry_tx(evm) {
            self.validate_env(evm)?;
            let init_and_floor = {
                let ctx = evm.ctx();
                let spec = ctx.cfg().spec().into();
                revm::handler::validation::validate_initial_tx_gas(
                    ctx.tx(),
                    spec,
                    ctx.cfg().is_eip7623_disabled(),
                    ctx.cfg().is_amsterdam_eip8037_enabled(),
                    ctx.cfg().tx_gas_limit_cap(),
                )?
            };
            evm.ctx_mut().chain_mut().intrinsic_gas = init_and_floor.initial_total_gas();
            return Ok(init_and_floor);
        }
        let result = self.mainnet.validate(evm)?;
        // Store intrinsic gas for use in pre_execution (gas limit enforcement)
        // and reward_beneficiary (reconstructing Nitro's computeGas).
        evm.ctx_mut().chain_mut().intrinsic_gas = result.initial_total_gas();
        Ok(result)
    }

    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        if is_internal_tx(evm) {
            let caller = evm.ctx().tx().caller();
            if !is_allowed_internal_caller(caller) {
                return Err(InvalidTransaction::Str(
                    "[ARBITRUM] internal tx caller must be ArbOS".into(),
                )
                .into());
            }
            match evm.ctx().tx().kind() {
                revm::primitives::TxKind::Call(target) if target == ARBOS_ACTS_ADDRESS => {}
                _ => {
                    return Err(InvalidTransaction::Str(
                        "[ARBITRUM] internal tx target must be ArbOS".into(),
                    )
                    .into());
                }
            }
            // Nitro marks internal txs as skipTransactionChecks/skipNonceChecks.
            // We mirror that by bypassing generic mainnet tx env checks.
            return Ok(());
        }
        if is_deposit_tx(evm) {
            // Nitro deposit txs are protocol-delivered and skip generic tx env checks.
            match evm.ctx().tx().kind() {
                revm::primitives::TxKind::Call(_) => return Ok(()),
                revm::primitives::TxKind::Create => {
                    return Err(InvalidTransaction::Str(
                        "[ARBITRUM] deposit tx must target a call address".into(),
                    )
                    .into());
                }
            }
        }
        if is_submit_retryable_tx(evm) {
            // Nitro submit-retryable txs are protocol-delivered and skip generic tx env checks.
            match evm.ctx().tx().kind() {
                revm::primitives::TxKind::Call(target) if target == ARB_RETRYABLE_TX_ADDRESS => {
                    return Ok(());
                }
                revm::primitives::TxKind::Call(_) => {
                    return Err(InvalidTransaction::Str(
                        "[ARBITRUM] submit-retryable tx must target ArbRetryableTx precompile"
                            .into(),
                    )
                    .into());
                }
                revm::primitives::TxKind::Create => {
                    return Err(InvalidTransaction::Str(
                        "[ARBITRUM] submit-retryable tx must target a call address".into(),
                    )
                    .into());
                }
            }
        }
        if is_retry_tx(evm) {
            // Nitro retry txs are protocol-scheduled and bypass generic env checks.
            return Ok(());
        }
        self.mainnet.validate_env(evm)
    }

    fn pre_execution(
        &self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &mut InitialAndFloorGas,
    ) -> Result<u64, Self::Error> {
        if is_protocol_env_bypass_tx(evm) {
            // Protocol txs (internal/deposit/submit-retryable/retry) carry no L1 poster cost and
            // skip the GasChargingHook below, but they still execute EVM and must obey EIP-2929.
            // `load_accounts` is what pre-warms the precompile addresses (revm pre_execution.rs)
            // Nitro's geth runs `Prepare` (which warms all active precompiles) for EVERY tx type, so
            // skipping it here charged a COLD (2600) access instead of WARM (100) the first time a
            // protocol tx touches a precompile. Warm accounts (precompiles + set journal spec)
            // before returning; we still skip caller-deduction + GasChargingHook (protocol txs are
            // gas-prepaid and poster-cost-free).
            self.load_accounts(evm)?;
            evm.ctx_mut().chain_mut().reset_poster_state();
            // Protocol txs carry no L1 poster fee; publish 0 so a getCurrentTxL1GasFees call within
            // this tx reads 0 rather than a value leaked from a prior tx (belt-and-braces: revm also
            // clears transient storage per tx).
            JournalTr::tstore(
                evm.ctx_mut().journal_mut(),
                CURRENT_TX_L1_FEE_ADDR,
                U256::ZERO,
                U256::ZERO,
            );
            return Ok(0);
        }

        // Run the standard pre-execution steps, but through this handler so our
        // overridden validate_against_state_and_deduct_caller logic is applied.
        self.validate_against_state_and_deduct_caller(evm, init_and_floor_gas)?;
        self.load_accounts(evm)?;
        let mainnet_cost = self.apply_eip7702_auth_list(evm, init_and_floor_gas)?;

        // --- GasChargingHook (Nitro: tx_processor.go GasChargingHook) ---
        // Two-phase approach to avoid simultaneous mutable+immutable borrows:
        //   Phase 1 (immutable): encode tx bytes and snapshot block/tx scalars.
        //   Phase 2 (mutable): read L1/L2 storage params, compute cost, update storage.
        {
            // Phase 1: snapshot values from the immutable context.
            let coinbase = evm.ctx().block().beneficiary();
            // Use effective_gas_price (= min(max_fee, basefee + priority_fee)) for the poster
            // cost, consistent with what validate_against_state_and_deduct_caller and
            // reimburse_caller use. For legacy txs gas_price() == effective_gas_price().
            let basefee_u128 = evm.ctx().block().basefee() as u128;
            let gas_price = U256::from(evm.ctx().tx().effective_gas_price(basefee_u128));
            let tx_gas_limit = evm.ctx().tx().gas_limit();
            let tx_bytes = encode_tx_bytes(evm.ctx().tx());

            // Phase 2: mutable context for storage access.
            let ctx = evm.ctx_mut();
            let intrinsic_gas = ctx.chain().intrinsic_gas;
            let mut paid_gas_price = U256::from(ctx.chain().paid_gas_price);
            ctx.chain_mut().reset_poster_state();

            let arbos_state = ArbosState::open();
            let journal = ctx.journal_mut();

            let spec = ArbSpecId::from_arbos_version(arbos_state.arbos_version.get(journal).unwrap_or(0));
            if paid_gas_price.is_zero() {
                let collect_tips_flag = arbos_state.collect_tips.get(journal).unwrap_or(0);
                let delayed_inbox = coinbase != BATCH_POSTER_ADDRESS;
                let collect_tips =
                    collect_tips_enabled(spec, delayed_inbox, collect_tips_flag);
                paid_gas_price = if collect_tips {
                    gas_price
                } else {
                    U256::from(basefee_u128)
                };
            }
            // Nitro falls back to basefee when the paid gas price resolves to zero.
            if paid_gas_price.is_zero() {
                paid_gas_price = U256::from(basefee_u128);
            }

            let price_per_unit = arbos_state
                .l1_pricing
                .price_per_unit
                .get(journal)
                .unwrap_or(U256::ZERO);
            let brotli_level = arbos_state
                .brotli_compression_level
                .get(journal)
                .unwrap_or(0) as u32;

            let info =
                compute_poster_info(&tx_bytes, coinbase, price_per_unit, paid_gas_price, brotli_level);

            if info.calldata_units > 0 {
                let units_since = arbos_state
                    .l1_pricing
                    .units_since_update
                    .get(journal)
                    .unwrap_or(0);
                let _ = arbos_state
                    .l1_pricing
                    .units_since_update
                    .set(units_since.saturating_add(info.calldata_units), journal);
            }

            // --- Per-tx / per-block gas limit enforcement ---
            // Mirrors Nitro's GasChargingHook gas cap (tx_processor.go:549-568).
            // Gas available for computation after intrinsic and poster costs:
            let after_overhead = tx_gas_limit
                .saturating_sub(intrinsic_gas)
                .saturating_sub(info.poster_gas);

            // A zero gas limit means the value is unset (uninitialized ArbOS
            // storage reads back as 0, and `get()` succeeds rather than erroring,
            // so `unwrap_or` never fires). Nitro's L2 pricing always carries a
            // positive limit, so treat 0 as "no cap" to avoid holding the entire
            // gas budget and starving the compute frame.
            let limit_or_unlimited = |limit: u64| if limit == 0 { u64::MAX } else { limit };
            let max_compute = if !spec.is_enabled_in(ArbSpecId::ARBOS_50) {
                limit_or_unlimited(
                    arbos_state
                        .l2_pricing
                        .per_block_gas_limit
                        .get(journal)
                        .unwrap_or(u64::MAX),
                )
            } else {
                // ArbOS >= 50: per-tx limit, reduced by intrinsic (already charged).
                limit_or_unlimited(
                    arbos_state
                        .l2_pricing
                        .per_tx_gas_limit
                        .get(journal)
                        .unwrap_or(u64::MAX),
                )
                .saturating_sub(intrinsic_gas)
            };

            let hold_gas = after_overhead.saturating_sub(max_compute);

            // Store for EndTxHook.
            ctx.chain_mut().poster_gas = info.poster_gas;
            ctx.chain_mut().poster_fee = info.poster_fee;
            ctx.chain_mut().hold_gas = hold_gas;

            // Publish the current tx's L1 poster fee for ArbGasInfo.getCurrentTxL1GasFees (Nitro:
            // txProcessor.PosterFee). Transient storage is the only channel the node-path precompile
            // (over EvmInternals, which hides the chain context) can read; not consensus state.
            JournalTr::tstore(
                ctx.journal_mut(),
                CURRENT_TX_L1_FEE_ADDR,
                U256::ZERO,
                info.poster_fee,
            );
        }

        // In revm, pre_execution's return value is interpreted as an EIP-7702 refund delta,
        // not as pre-EVM gas burn. Nitro's poster/hold gas is instead enforced by reducing
        // the first-frame gas limit in execution().
        Ok(mainnet_cost)
    }

    fn execution(
        &mut self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &InitialAndFloorGas,
    ) -> Result<FrameResult, Self::Error> {
        if is_internal_tx(evm) {
            internal_tx::apply_internal_tx(evm.ctx_mut())
                .map_err(|msg| ERROR::from_string(msg))?;
            return Ok(internal_success_frame_result());
        }
        if is_deposit_tx(evm) {
            let outcome = deposit_tx::apply_deposit_tx(evm.ctx_mut())
                .map_err(|msg| ERROR::from_string(msg))?;
            return Ok(match outcome {
                // Normal deposit: success receipt.
                deposit_tx::DepositOutcome::Applied => internal_success_frame_result(),
                // Filtered deposit: Nitro records a failed tx (status 0, gasUsed 0) but keeps the
                // redirected transfer (same shape as a funds-failed submit-retryable).
                deposit_tx::DepositOutcome::Filtered => submit_retryable_failed_frame_result(),
            });
        }
        if is_submit_retryable_tx(evm) {
            let outcome = submit_retryable_tx::apply_submit_retryable_tx(evm.ctx_mut())
                .map_err(|msg| ERROR::from_string(msg))?;
            return Ok(match outcome {
                // Ticket created: success receipt. gasUsed is `usergas` only when the auto-redeem
                // was scheduled, else 0 (Nitro `StartTxHook`); the outcome carries the value.
                submit_retryable_tx::SubmitRetryableOutcome::Created { gas_used } => {
                    submit_retryable_success_frame_result(gas_used)
                }
                // Funds check failed: Nitro records a failed tx (status 0, gasUsed 0) and
                // keeps the partial state (deposit mint, fee refunds) instead of halting.
                submit_retryable_tx::SubmitRetryableOutcome::Failed => {
                    submit_retryable_failed_frame_result()
                }
            });
        }
        if is_retry_tx(evm) {
            retry_tx::apply_retry_tx_pre_execution(evm.ctx_mut())
                .map_err(|msg| ERROR::from_string(msg))?;
            return self.mainnet.execution(evm, init_and_floor_gas);
        }

        // Nitro charges poster gas before EVM compute starts, and caps the compute gas
        // at the per-block/per-tx limit by withholding `hold_gas`. In revm both are
        // expressed by reducing the initial frame gas limit. Crucially, `hold_gas` is
        // only a *cap*, Nitro returns whatever the tx doesn't actually use, so it must
        // not end up in `gasUsed`. (poster_gas IS charged and stays.)
        let (tx_gas_limit, poster_gas, hold_gas) = {
            let ctx = evm.ctx();
            (
                ctx.tx().gas_limit(),
                ctx.chain().poster_gas,
                ctx.chain().hold_gas,
            )
        };
        let total_initial = init_and_floor_gas
            .initial_total_gas()
            .saturating_add(poster_gas)
            .saturating_add(hold_gas);
        if total_initial > tx_gas_limit {
            return Err(InvalidTransaction::CallGasCostMoreThanGasLimit {
                initial_gas: total_initial,
                gas_limit: tx_gas_limit,
            }
            .into());
        }

        // Nitro `RevertedTxHook`: a normal (non-protocol) tx whose hash was pre-registered in the
        // on-chain filter is NOT executed but consumes all its gas (status 0). The nonce was already
        // bumped by caller validation. Applies from ArbOS 60 (TransactionFiltering).
        if is_filtered_normal_tx(evm) {
            // Consume the whole tx gas budget (Nitro sets gasRemaining = 0). gasUsed is taken from
            // the returned frame's spent gas, so spend the full tx limit here (intrinsic included).
            let mut gas = Gas::new(tx_gas_limit);
            gas.spend_all();
            let output = InterpreterResult::new(InstructionResult::Revert, Bytes::new(), gas);
            return Ok(FrameResult::Call(CallOutcome::new(output, 0..0)));
        }

        let first_frame_input = self
            .mainnet
            .first_frame_input(evm, tx_gas_limit.saturating_sub(total_initial), 0)?;
        let mut frame_result = self.mainnet.run_exec_loop(evm, first_frame_input)?;
        self.mainnet.last_frame_result(evm, 0, &mut frame_result)?;
        // Return the withheld cap gas: it bounded compute but is not part of gasUsed.
        // (frame gas.spent() = intrinsic + poster + hold + compute; this removes hold.)
        frame_result.gas_mut().erase_cost(hold_gas);
        Ok(frame_result)
    }

    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
        _init_and_floor_gas: &mut InitialAndFloorGas,
    ) -> Result<(), Self::Error> {
        if is_protocol_env_bypass_tx(evm) {
            // Nitro internal/deposit/submit-retryable/retry txs are protocol actions,
            // not regular fee-paying user transactions.
            return Ok(());
        }

        let basefee_u128 = evm.ctx().block().basefee() as u128;
        let (
            caller,
            gas_limit,
            value,
            max_fee_per_gas,
            effective_gas_price,
            is_call,
            tx_nonce,
            is_eip3607_disabled,
            is_nonce_check_disabled,
        ) = {
            let ctx = evm.ctx();
            let tx = ctx.tx();
            (
                tx.caller(),
                tx.gas_limit(),
                tx.value(),
                tx.max_fee_per_gas(),
                tx.effective_gas_price(basefee_u128),
                tx.kind().is_call(),
                tx.nonce(),
                ctx.cfg().is_eip3607_disabled(),
                // ArbitrumContractTx (L1->L2 unsigned contract call) is not nonce-checked: its
                // nonce field is always 0 and uniqueness comes from the L1 requestId. Nitro skips
                // the check but still bumps the sender nonce (kept below via `bump_nonce`).
                ctx.cfg().is_nonce_check_disabled()
                    || tx.tx_type() == ARBITRUM_CONTRACT_TX_TYPE,
            )
        };

        let (paid_gas_price, max_gas_price_for_balance_check) = {
            let ctx = evm.ctx_mut();
            let arbos_state = ArbosState::open();
            let journal = ctx.journal_mut();
            let spec = ArbSpecId::from_arbos_version(arbos_state.arbos_version.get(journal).unwrap_or(0));
            let collect_tips_flag = arbos_state.collect_tips.get(journal).unwrap_or(0);
            let delayed_inbox = ctx.block().beneficiary() != BATCH_POSTER_ADDRESS;
            let collect_tips =
                collect_tips_enabled(spec, delayed_inbox, collect_tips_flag);

            let paid = if collect_tips {
                effective_gas_price
            } else {
                ctx.block().basefee() as u128
            };
            // Nitro/go-ethereum always validate the caller balance against the
            // declared gas fee cap (gasLimit * gasFeeCap + value), regardless of
            // how much is ultimately charged. Only the deducted amount tracks the
            // effective/basefee price. Collapsing this to `paid` would let an
            // unfunded caller through whenever basefee is zero.
            let max_for_check = max_fee_per_gas.max(paid);
            (paid, max_for_check)
        };

        {
            let journal = evm.ctx_mut().journal_mut();
            let mut caller_acc = journal.load_account_with_code_mut(caller)?.data;

            revm::handler::pre_execution::validate_account_nonce_and_code(
                &caller_acc.account().info,
                tx_nonce,
                is_eip3607_disabled,
                is_nonce_check_disabled,
            )?;

            let caller_balance = *caller_acc.balance();
            let max_fee = U256::from(max_gas_price_for_balance_check)
                .saturating_mul(U256::from(gas_limit))
                .saturating_add(value);
            if caller_balance < max_fee {
                return Err(InvalidTransaction::LackOfFundForMaxFee {
                    fee: Box::new(max_fee),
                    balance: Box::new(caller_balance),
                }
                .into());
            }

            let paid_gas_cost = U256::from(paid_gas_price).saturating_mul(U256::from(gas_limit));
            let new_balance = caller_balance.saturating_sub(paid_gas_cost);
            caller_acc.set_balance(new_balance);
            if is_call {
                caller_acc.bump_nonce();
            }
        }
        evm.ctx_mut().chain_mut().paid_gas_price = paid_gas_price;
        Ok(())
    }

    fn last_frame_result(
        &mut self,
        evm: &mut Self::Evm,
        original_reservoir: u64,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        if is_protocol_short_circuit_tx(evm) {
            let status = frame_result.interpreter_result().result;
            // A submit-retryable may legitimately fail its funds checks: Nitro records it as
            // a failed tx (status 0, gasUsed 0) and continues the block, so a non-ok result
            // here is expected, not fatal. Internal and deposit txs are protocol-guaranteed
            // and must never fail, a non-ok result for those is a real bug.
            if !status.is_ok() && !is_submit_retryable_tx(evm) {
                let label = if is_internal_tx(evm) { "internal" } else { "deposit" };
                return Err(ERROR::from_string(
                    format!("[ARBITRUM] {label} transaction execution failed"),
                ));
            }
            return Ok(());
        }
        self.mainnet.last_frame_result(evm, original_reservoir, frame_result)
    }

    fn reimburse_caller(
        &self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        if is_protocol_env_bypass_tx(evm) {
            return Ok(());
        }
        let basefee_u128 = evm.ctx().block().basefee() as u128;
        let caller = {
            let ctx = evm.ctx();
            let tx = ctx.tx();
            tx.caller()
        };

        let paid_gas_price = evm.ctx().chain().paid_gas_price;
        let paid_gas_price = if paid_gas_price == 0 {
            basefee_u128
        } else {
            paid_gas_price
        };

        let gas = frame_result.interpreter_result().gas;
        let returned_gas = gas.remaining().saturating_add(gas.refunded().max(0) as u64);
        let refund_wei = U256::from(paid_gas_price).saturating_mul(U256::from(returned_gas));

        evm.ctx_mut()
            .journal_mut()
            .load_account_mut(caller)?
            .incr_balance(refund_wei);
        Ok(())
    }

    fn refund(
        &self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        eip7702_refund: i64,
    ) {
        // Nitro caps SSTORE-style refunds against (gasUsed - posterGas), where poster gas is
        // nonrefundable. Revm's default cap uses gasUsed only, so we specialize it here.
        // Use tx-level total_gas_spent (which includes intrinsic, unlike frame-level).
        let gas = frame_result.gas_mut();
        gas.record_refund(eip7702_refund);

        let tx_gas_spent = {
            let tx_limit = evm.ctx().tx().gas_limit();
            let remaining = gas.remaining();
            tx_limit.saturating_sub(remaining)
        };
        let spec: SpecId = evm.ctx().cfg().spec().into();
        let max_refund_quotient = if spec.is_enabled_in(SpecId::LONDON) {
            5
        } else {
            2
        };
        let nonrefundable = evm.ctx().chain().poster_gas;
        let refundable_spent = tx_gas_spent.saturating_sub(nonrefundable);
        let max_refund = refundable_spent / max_refund_quotient;
        let current_refund = gas.refunded().max(0) as u64;
        gas.set_refund(current_refund.min(max_refund) as i64);
    }

    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        // Protocol short-circuit txs (internal/deposit/submit-retryable) fully bypass.
        // Retry txs get their own EndTxHook below.
        if is_protocol_short_circuit_tx(evm) {
            return Ok(());
        }

        if is_retry_tx(evm) {
            return self.retry_end_tx_hook(evm, frame_result);
        }

        // --- EndTxHook (Nitro: tx_processor.go EndTxHook, normal-tx path) ---
        //
        // gas.spent() = execution gas consumed (not counting intrinsic or poster/hold).
        // Because pre_execution returned poster_gas + hold_gas, those were already
        // subtracted from the EVM gas budget.
        //
        // Nitro's computeGas = gasUsed - posterGas.
        // Held gas is refunded before `gasUsed` is finalized in Nitro, so it does NOT
        // contribute to computeGas.
        //
        // Fee distribution:
        //   infraFeeAccount  ← min(minBaseFee, baseFee) * computeGas  (ArbOS >= 5)
        //   networkFeeAccount ← (baseFee * computeGas) - infraComputeCost
        //   L1PricerFundsPool ← posterFee                              (ArbOS >= 2)
        //   coinbase          ← posterFee                              (ArbOS < 2)
        //
        // AddToL1FeesAvailable(posterFee) is called when ArbOS >= 10.
        // GrowBacklog grows by computeGas only (not gasUsed).
        let gas = frame_result.interpreter_result().gas;
        let tx_gas_limit = {
            let ctx = evm.ctx();
            ctx.tx().gas_limit()
        };
        let remaining = gas.remaining();
        let refund = gas.refunded().max(0) as u64;
        // Exclude the EIP-7825/8037 reservoir (see the retry path): the capped-off excess above
        // TX_GAS_LIMIT_CAP is not real work. Zero on the normal path at our specs, but kept
        // symmetric with the retry path so a >2^24 gas tx never inflates computeGas.
        let gas_used = tx_gas_limit
            .saturating_sub(remaining)
            .saturating_sub(refund)
            .saturating_sub(gas.reservoir());
        // gas.refunded() is the capped EIP-3529 refund that reimburse_caller returns to the
        // caller. Nitro's gasUsed = GasLimit - gasLeft (where gasLeft includes the refund),
        // which maps to revm's spent_sub_refunded() at this stage.
        // otherwise we over-mint by basefee * refunded on every tx with SSTORE/SELFDESTRUCT.

        let (poster_fee, poster_gas, basefee, basefee_u128, coinbase, paid_gas_price) = {
            let ctx = evm.ctx_mut();
            let poster_fee = ctx.chain().poster_fee;
            let poster_gas = ctx.chain().poster_gas;
            let basefee = U256::from(ctx.block().basefee());
            let basefee_u128 = ctx.block().basefee() as u128;
            let coinbase = ctx.block().beneficiary();
            let paid_gas_price = ctx.chain().paid_gas_price;
            (
                poster_fee,
                poster_gas,
                basefee,
                basefee_u128,
                coinbase,
                paid_gas_price,
            )
        };

        // computeGas mirrors Nitro EndTxHook: computeGas = gasUsed - posterGas.
        let compute_gas = gas_used.saturating_sub(poster_gas);
        let compute_cost = basefee.saturating_mul(U256::from(compute_gas));

        let arbos_state = ArbosState::open();
        let journal = evm.ctx_mut().journal_mut();

        let arbos_version = arbos_state.arbos_version.get(journal).unwrap_or(0);
        let spec = ArbSpecId::from_arbos_version(arbos_version);
        let paid_gas_price = if paid_gas_price == 0 {
            basefee_u128
        } else {
            paid_gas_price
        };
        let tip_per_gas = paid_gas_price.saturating_sub(basefee_u128);
        let tip_amount = U256::from(tip_per_gas).saturating_mul(U256::from(compute_gas));

        // Split compute cost between the infra and network fee accounts.
        //
        // Nitro (tx_processor.go EndTxHook) carves out the infra portion, both the
        // mint AND the `computeCost -= infraComputeCost` subtraction, *only* when an
        // infra fee account is configured. When it is unset (e.g. infraFeeAccount == 0),
        // the entire compute cost goes to the network fee account. Subtracting the infra
        // portion unconditionally (as before) silently burned it and under-credited the
        // network fee account.
        let infra_account = if spec.is_enabled_in(ArbSpecId::ARBOS_5) {
            arbos_state.infra_fee_account.get(journal).unwrap_or_default()
        } else {
            Address::ZERO
        };
        let mut network_compute_cost = compute_cost;
        if infra_account != Address::ZERO {
            let min_base_fee = arbos_state
                .l2_pricing
                .min_base_fee_wei
                .get(journal)
                .unwrap_or(U256::ZERO);
            let infra_fee = min_base_fee.min(basefee);
            let infra_compute_cost = infra_fee
                .saturating_mul(U256::from(compute_gas))
                .min(network_compute_cost);
            if infra_compute_cost > U256::ZERO {
                let _ = journal.balance_incr(infra_account, infra_compute_cost);
            }
            network_compute_cost = network_compute_cost.saturating_sub(infra_compute_cost);
        }

        // Mint remaining compute cost → networkFeeAccount
        if network_compute_cost > U256::ZERO {
            let net_account = arbos_state
                .network_fee_account
                .get(journal)
                .unwrap_or_default();
            if net_account != Address::ZERO {
                let _ = journal.balance_incr(net_account, network_compute_cost);
            }
        }

        // Credit tips on compute gas to tip recipient (network fee account in Nitro).
        if tip_amount > U256::ZERO {
            let tip_recipient = arbos_state
                .network_fee_account
                .get(journal)
                .unwrap_or_default();
            if tip_recipient != Address::ZERO {
                let _ = journal.balance_incr(tip_recipient, tip_amount);
            }
        }

        // Mint poster fee → L1PricerFundsPool (ArbOS >= 2) or coinbase (pre-v2).
        if poster_fee > U256::ZERO {
            let poster_dest = if spec.is_enabled_in(ArbSpecId::ARBOS_2) {
                L1_PRICER_FUNDS_POOL_ADDRESS
            } else {
                coinbase
            };
            let _ = journal.balance_incr(poster_dest, poster_fee);

            // AddToL1FeesAvailable (ArbOS >= 10)
            if spec.is_enabled_in(ArbSpecId::ARBOS_10) {
                let _ = arbos_state
                    .l1_pricing
                    .add_to_l1_fees_available(poster_fee, journal);
            }
        }

        // Grow L2 backlog by computeGas only (poster gas is L1 accounting, not compute time).
        let _ = arbos_state
            .l2_pricing
            .grow_backlog(compute_gas, arbos_version, journal);

        // Do NOT call mainnet.reward_beneficiary, Arbitrum replaces that entire path.
        Ok(())
    }
}

impl<EVM, ERROR, FRAME> ArbHandler<EVM, ERROR, FRAME>
where
    EVM: EvmTr<Context: ArbContextTr, Frame = FRAME>,
    ERROR: EvmTrError<EVM> + FromStringError,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    /// EndTxHook for `ArbitrumRetryTx`.
    ///
    /// Mirrors Nitro's tx_processor.go EndTxHook retry branch (lines 589-720).
    ///
    /// On success:
    ///   - refund `submission_fee_refund` (up to `max_refund`) from networkFeeAccount → refund_to
    ///   - reduce `max_refund` by `submission_fee_refund` and by `effectiveBaseFee * exec_gas`
    ///   - refund remaining unused gas (`gas_left * effectiveBaseFee`) from infra/network accounts
    ///
    /// On failure:
    ///   - no submission fee refund; just reduce `max_refund` by `submission_fee_refund`
    ///   - refund unused gas the same way
    ///
    /// Always calls `GrowBacklog(exec_gas)`.
    fn retry_end_tx_hook(
        &self,
        evm: &mut EVM,
        frame_result: &mut FrameResult,
    ) -> Result<(), ERROR> {
        let gas = frame_result.interpreter_result().gas;
        let success = frame_result.interpreter_result().result.is_ok();

        // effective_base_fee is the tx gas_price (= GasFeeCap for retry txs).
        let tx_gas_limit: u64;
        let effective_base_fee: U256;
        let refund_to: Address;
        let from: Address;
        let max_refund: U256;
        let submission_fee_refund: U256;
        {
            let ctx = evm.ctx();
            let tx = ctx.tx();
            tx_gas_limit = tx.gas_limit();
            effective_base_fee = U256::from(tx.gas_price());
            // `from` is the retryable's sender: any refund beyond the L1-deposit budget
            // (`max_refund`) goes here rather than to `refund_to` (Nitro `refund()` helper).
            from = tx.caller();
            let retry_meta = tx.retry_meta().ok_or_else(|| {
                ERROR::from_string("[ARBITRUM] retry EndTxHook: missing retry metadata".into())
            })?;
            refund_to = retry_meta.refund_to;
            max_refund = retry_meta.max_refund;
            submission_fee_refund = retry_meta.submission_fee_refund;
        }
        // Nitro: gasUsed = gasLimit - gasLeft (where gasLeft = remaining + refund).
        // Frame-level `spent_sub_refunded()` misses intrinsic because
        // `first_frame_input` subtracts it from the budget. We reconstruct the
        // tx-level gas directly from the raw gas tracker fields.
        let remaining = gas.remaining();
        let refund = gas.refunded().max(0) as u64;
        // EIP-7825/8037: under Osaka revm sets aside (gas_limit - TX_GAS_LIMIT_CAP) in the gas
        // reservoir; it is never real work, and revm's ExecutionResult.gas_used excludes it. Nitro
        // exempts Arbitrum from the cap, so its gasUsed also excludes it. Subtract it here or the
        // L2 backlog over-grows by exactly the reservoir for any tx whose gas limit exceeds 2^24.
        let reservoir = gas.reservoir();
        let gas_used = tx_gas_limit
            .saturating_sub(remaining)
            .saturating_sub(refund)
            .saturating_sub(reservoir);
        let gas_left = tx_gas_limit.saturating_sub(gas_used);

        let arbos_state = ArbosState::open();
        let journal = evm.ctx_mut().journal_mut();

        let arbos_version = arbos_state.arbos_version.get(journal).unwrap_or(0);
        let spec = ArbSpecId::from_arbos_version(arbos_version);
        let min_base_fee = arbos_state
            .l2_pricing
            .min_base_fee_wei
            .get(journal)
            .unwrap_or(U256::ZERO);

        let net_account = arbos_state
            .network_fee_account
            .get(journal)
            .unwrap_or_default();
        let infra_account = arbos_state
            .infra_fee_account
            .get(journal)
            .unwrap_or_default();

        // Track remaining refund budget (the L1 deposit still available to reimburse `refund_to`).
        let mut remaining_refund = max_refund;

        if success {
            // Refund the submission fee from the network fee account (see `retry_fee_refund`).
            retry_fee_refund(
                journal,
                net_account,
                submission_fee_refund,
                refund_to,
                from,
                &mut remaining_refund,
            );
        } else {
            // The submission fee is still taken from the L1 deposit, just not refunded.
            remaining_refund = remaining_refund.saturating_sub(submission_fee_refund);
        }

        // Deduct single-gas cost (execution cost) from the deposit budget (no transfer).
        let single_gas_cost = effective_base_fee.saturating_mul(U256::from(gas_used));
        remaining_refund = remaining_refund.saturating_sub(single_gas_cost);

        // Refund the unused gas. Nitro carves the infra share out of the gas bucket first, then
        // refunds infra and network buckets separately, each via `refund()` (deposit cap +
        // excess to `from`). Not pre-capped by the budget; `retry_fee_refund` applies the cap.
        let mut network_refund = effective_base_fee.saturating_mul(U256::from(gas_left));

        if spec.is_enabled_in(ArbSpecId::ARBOS_5) && infra_account != Address::ZERO {
            let infra_fee = min_base_fee.min(effective_base_fee);
            let infra_refund = infra_fee
                .saturating_mul(U256::from(gas_left))
                .min(network_refund);
            network_refund = network_refund.saturating_sub(infra_refund);
            retry_fee_refund(
                journal,
                infra_account,
                infra_refund,
                refund_to,
                from,
                &mut remaining_refund,
            );
        }

        retry_fee_refund(journal, net_account, network_refund, refund_to, from, &mut remaining_refund);

        // GrowBacklog by gas_used (Nitro retry path: gasLimit - gasLeft), using the
        // active L2 pricing model.
        let _ = arbos_state
            .l2_pricing
            .grow_backlog(gas_used, arbos_version, journal);

        // Retry post-exec lifecycle (clear on success, restore escrow on failure) must run in
        // EndTxHook because execution delegates to `mainnet.execution`, which does not call
        // `ArbHandler::last_frame_result`.
        retry_tx::apply_retry_tx_post_execution(evm.ctx_mut(), success)
            .map_err(|msg| ERROR::from_string(msg))?;

        Ok(())
    }
}

/// Nitro's retryable `refund()` helper (`arbos/tx_processor.go`): move the full `amount` out of
/// `refund_from`, split between `refund_to` and `from`.
///
/// `budget` is the remaining L1-deposit allowance: `min(amount, budget)` is reimbursed to
/// `refund_to` (and decrements `budget`), while the EXCESS, which can't be charged against the
/// deposit, is returned to `from` (the retryable's sender). Both transfers come from
/// `refund_from` (a fee account). Balances are read first to avoid `OutOfFunds`.
fn retry_fee_refund<J: JournalTr>(
    journal: &mut J,
    refund_from: Address,
    amount: U256,
    refund_to: Address,
    from: Address,
    budget: &mut U256,
) {
    if amount == U256::ZERO {
        return;
    }
    let to_refund_addr = amount.min(*budget);
    *budget = budget.saturating_sub(to_refund_addr);
    if refund_from == Address::ZERO {
        return;
    }
    if to_refund_addr > U256::ZERO {
        let actual = available_balance(journal, refund_from, to_refund_addr);
        if actual > U256::ZERO {
            let _ = journal.transfer(refund_from, refund_to, actual);
        }
    }
    let excess = amount.saturating_sub(to_refund_addr);
    if excess > U256::ZERO {
        let actual = available_balance(journal, refund_from, excess);
        if actual > U256::ZERO {
            let _ = journal.transfer(refund_from, from, actual);
        }
    }
}

/// Returns the transferable amount: `min(amount, src_balance)`.
///
/// Callers should then call `journal.transfer(src, dst, actual)` to move funds
/// atomically. We read balance first to avoid `OutOfFunds` errors from `transfer`.
fn available_balance<J: JournalTr>(journal: &mut J, src: Address, amount: U256) -> U256 {
    let Ok(account_load) = journal.load_account(src) else {
        return U256::ZERO;
    };
    amount.min(account_load.data.info.balance)
}

fn internal_success_frame_result() -> FrameResult {
    FrameResult::Call(CallOutcome::new(
        InterpreterResult::new(InstructionResult::Stop, Bytes::new(), Gas::new(0)),
        0..0,
    ))
}

fn submit_retryable_success_frame_result(gas_used: u64) -> FrameResult {
    FrameResult::Call(CallOutcome::new(
        InterpreterResult::new(
            InstructionResult::Stop,
            Bytes::new(),
            Gas::new_spent_with_reservoir(gas_used, 0),
        ),
        0..0,
    ))
}

/// Frame result for a submit-retryable that failed a funds check: a reverted (status-0)
/// receipt with zero gas, mirroring Nitro's `return true, ZeroGas, err, nil`. The state
/// changes already applied in `apply_submit_retryable_tx` are not reverted (there is no
/// frame checkpoint to unwind), matching Nitro keeping the deposit mint on failure.
fn submit_retryable_failed_frame_result() -> FrameResult {
    FrameResult::Call(CallOutcome::new(
        InterpreterResult::new(InstructionResult::Revert, Bytes::new(), Gas::new(0)),
        0..0,
    ))
}

impl<EVM, ERROR> InspectorHandler for ArbHandler<EVM, ERROR, EthFrame<EthInterpreter>>
where
    EVM: InspectorEvmTr<
            Context: ArbContextTr,
            Frame = EthFrame<EthInterpreter>,
            Inspector: Inspector<<<Self as Handler>::Evm as EvmTr>::Context, EthInterpreter>,
        >,
    ERROR: EvmTrError<EVM> + FromStringError,
{
    type IT = EthInterpreter;

    /// Mirror [`ArbHandler::execution`] (protocol short-circuits + poster/hold gas
    /// reservation) but drive the **inspecting** frame loop. The default
    /// `inspect_execution` skips the poster-gas reservation, so the inspector path would
    /// otherwise leave `posterGas` in the EVM gas pool, inflating the `GAS` opcode and
    /// diverging gas-sensitive contracts from the non-inspect `transact` path.
    fn inspect_execution(
        &mut self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &InitialAndFloorGas,
    ) -> Result<FrameResult, Self::Error> {
        if is_internal_tx(evm) {
            internal_tx::apply_internal_tx(evm.ctx_mut())
                .map_err(|msg| ERROR::from_string(msg))?;
            return Ok(internal_success_frame_result());
        }
        if is_deposit_tx(evm) {
            let outcome = deposit_tx::apply_deposit_tx(evm.ctx_mut())
                .map_err(|msg| ERROR::from_string(msg))?;
            return Ok(match outcome {
                // Normal deposit: success receipt.
                deposit_tx::DepositOutcome::Applied => internal_success_frame_result(),
                // Filtered deposit: Nitro records a failed tx (status 0, gasUsed 0) but keeps the
                // redirected transfer (same shape as a funds-failed submit-retryable).
                deposit_tx::DepositOutcome::Filtered => submit_retryable_failed_frame_result(),
            });
        }
        if is_submit_retryable_tx(evm) {
            let outcome = submit_retryable_tx::apply_submit_retryable_tx(evm.ctx_mut())
                .map_err(|msg| ERROR::from_string(msg))?;
            return Ok(match outcome {
                submit_retryable_tx::SubmitRetryableOutcome::Created { gas_used } => {
                    submit_retryable_success_frame_result(gas_used)
                }
                submit_retryable_tx::SubmitRetryableOutcome::Failed => {
                    submit_retryable_failed_frame_result()
                }
            });
        }
        if is_retry_tx(evm) {
            retry_tx::apply_retry_tx_pre_execution(evm.ctx_mut())
                .map_err(|msg| ERROR::from_string(msg))?;
        }

        // poster_gas/hold_gas are 0 for retry txs (they bypass the GasChargingHook), so the
        // same reservation formula covers them; reservoir is 0 at our specs (no EIP-7623).
        let (tx_gas_limit, poster_gas, hold_gas) = {
            let ctx = evm.ctx();
            (ctx.tx().gas_limit(), ctx.chain().poster_gas, ctx.chain().hold_gas)
        };
        let total_initial = init_and_floor_gas
            .initial_total_gas()
            .saturating_add(poster_gas)
            .saturating_add(hold_gas);
        if total_initial > tx_gas_limit {
            return Err(InvalidTransaction::CallGasCostMoreThanGasLimit {
                initial_gas: total_initial,
                gas_limit: tx_gas_limit,
            }
            .into());
        }

        let first_frame_input = self
            .mainnet
            .first_frame_input(evm, tx_gas_limit.saturating_sub(total_initial), 0)?;
        let mut frame_result = self.inspect_run_exec_loop(evm, first_frame_input)?;
        self.mainnet.last_frame_result(evm, 0, &mut frame_result)?;
        frame_result.gas_mut().erase_cost(hold_gas);
        Ok(frame_result)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ARBITRUM_DEPOSIT_TX_TYPE, ARBITRUM_INTERNAL_TX_TYPE, ARBITRUM_RETRY_TX_TYPE,
        ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE,
    };
    use crate::{
        ArbBuilder, ArbChainContext, ArbSpecId, ArbTransaction,
        constants::{ARB_RETRYABLE_TX_ADDRESS, ARBOS_ACTS_ADDRESS, ARBOS_STATE_ADDRESS},
        internal_tx,
        storage::{ArbosState, RETRYABLE_LIFETIME_SECONDS},
        submit_retryable_tx::{
            SubmitRetryableCall, compute_submit_retryable_ticket_id,
            encode_submit_retryable_calldata,
        },
        transaction::RetryTxMeta,
    };
    use alloy_core::sol_types::SolCall;
    use arbitrum_alloy_precompiles::{
        ArbOwner,
        addresses::{ARB_OWNER, ARB_SYS},
    };
    use revm::{
        Context, ExecuteCommitEvm, ExecuteEvm, MainContext,
        context::{BlockEnv, CfgEnv, TxEnv},
        context_interface::result::{EVMError, InvalidTransaction},
        database::InMemoryDB,
        primitives::{Address, B256, TxKind, U256, keccak256},
        state::AccountInfo,
    };

    fn make_evm(
        db: InMemoryDB,
    ) -> impl ExecuteCommitEvm<Tx = ArbTransaction<TxEnv>, Error = EVMError<
            <InMemoryDB as revm::Database>::Error,
            revm::context_interface::result::InvalidTransaction,
        >> {
        let cfg = CfgEnv::new_with_spec(ArbSpecId::NITRO)
            .with_chain_id(42161)
            .with_disable_priority_fee_check(true);
        let ctx = Context::mainnet()
            .with_tx(ArbTransaction::<TxEnv>::default())
            .with_cfg(cfg)
            .with_chain(ArbChainContext::default())
            .with_db(db);
        ctx.build_arb()
    }

    fn make_call_tx(tx_type: u8, caller: Address, to: Address) -> ArbTransaction<TxEnv> {
        let mut tx = TxEnv::default();
        tx.tx_type = tx_type;
        tx.caller = caller;
        tx.kind = TxKind::Call(to);
        tx.gas_limit = 100_000;
        tx.gas_price = 1;
        tx.nonce = 0;
        tx.chain_id = Some(42161);
        ArbTransaction::new(tx)
    }

    fn make_deposit_tx(caller: Address, to: Address, value: U256) -> ArbTransaction<TxEnv> {
        let mut tx = TxEnv::default();
        tx.tx_type = ARBITRUM_DEPOSIT_TX_TYPE;
        tx.caller = caller;
        tx.kind = TxKind::Call(to);
        tx.value = value;
        tx.gas_limit = 0;
        tx.gas_price = 0;
        tx.nonce = 0;
        tx.chain_id = Some(42161);
        ArbTransaction::new(tx)
    }

    fn make_submit_retryable_tx(
        caller: Address,
        call: SubmitRetryableCall,
    ) -> ArbTransaction<TxEnv> {
        let mut tx = TxEnv::default();
        tx.tx_type = ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE;
        tx.caller = caller;
        tx.kind = TxKind::Call(ARB_RETRYABLE_TX_ADDRESS);
        tx.value = U256::ZERO;
        tx.data = encode_submit_retryable_calldata(&call);
        tx.gas_limit = call.gas_limit;
        tx.gas_price = call.gas_fee_cap.try_into().unwrap_or(u128::MAX);
        tx.nonce = 0;
        tx.chain_id = Some(42161);
        ArbTransaction::new(tx)
    }

    fn make_retry_tx(
        caller: Address,
        to: Address,
        ticket_id: B256,
        value: U256,
        gas_limit: u64,
    ) -> ArbTransaction<TxEnv> {
        let mut tx = TxEnv::default();
        tx.tx_type = ARBITRUM_RETRY_TX_TYPE;
        tx.caller = caller;
        tx.kind = TxKind::Call(to);
        tx.value = value;
        tx.gas_limit = gas_limit;
        tx.gas_price = 0;
        tx.nonce = 0;
        tx.chain_id = Some(42161);
        ArbTransaction::new(tx).with_retry_meta(RetryTxMeta {
            ticket_id,
            refund_to: caller,
            max_refund: U256::MAX,
            submission_fee_refund: U256::ZERO,
        })
    }

    fn encode_start_block_calldata(
        l1_base_fee: U256,
        l1_block_number: u64,
        l2_block_number: u64,
        time_last_block: u64,
    ) -> revm::primitives::Bytes {
        let mut out = Vec::with_capacity(4 + (32 * 4));
        out.extend_from_slice(&internal_tx::start_block_selector());
        out.extend_from_slice(&l1_base_fee.to_be_bytes::<32>());

        let mut word = [0_u8; 32];
        word[24..].copy_from_slice(&l1_block_number.to_be_bytes());
        out.extend_from_slice(&word);

        word = [0_u8; 32];
        word[24..].copy_from_slice(&l2_block_number.to_be_bytes());
        out.extend_from_slice(&word);

        word = [0_u8; 32];
        word[24..].copy_from_slice(&time_last_block.to_be_bytes());
        out.extend_from_slice(&word);

        out.into()
    }

    fn encode_batch_posting_report_calldata(
        batch_timestamp: U256,
        batch_poster_address: Address,
        batch_number: u64,
        batch_data_gas: u64,
        l1_base_fee_wei: U256,
    ) -> revm::primitives::Bytes {
        let mut out = Vec::with_capacity(4 + (32 * 5));
        out.extend_from_slice(&internal_tx::batch_posting_report_selector());
        out.extend_from_slice(&batch_timestamp.to_be_bytes::<32>());

        let mut word = [0_u8; 32];
        word[12..].copy_from_slice(batch_poster_address.as_slice());
        out.extend_from_slice(&word);

        word = [0_u8; 32];
        word[24..].copy_from_slice(&batch_number.to_be_bytes());
        out.extend_from_slice(&word);

        word = [0_u8; 32];
        word[24..].copy_from_slice(&batch_data_gas.to_be_bytes());
        out.extend_from_slice(&word);

        out.extend_from_slice(&l1_base_fee_wei.to_be_bytes::<32>());
        out.into()
    }

    fn encode_batch_posting_report_v2_calldata(
        batch_timestamp: U256,
        batch_poster_address: Address,
        batch_number: u64,
        batch_calldata_length: u64,
        batch_calldata_non_zeros: u64,
        batch_extra_gas: u64,
        l1_base_fee_wei: U256,
    ) -> revm::primitives::Bytes {
        let mut out = Vec::with_capacity(4 + (32 * 7));
        out.extend_from_slice(&internal_tx::batch_posting_report_v2_selector());
        out.extend_from_slice(&batch_timestamp.to_be_bytes::<32>());

        let mut word = [0_u8; 32];
        word[12..].copy_from_slice(batch_poster_address.as_slice());
        out.extend_from_slice(&word);

        word = [0_u8; 32];
        word[24..].copy_from_slice(&batch_number.to_be_bytes());
        out.extend_from_slice(&word);

        word = [0_u8; 32];
        word[24..].copy_from_slice(&batch_calldata_length.to_be_bytes());
        out.extend_from_slice(&word);

        word = [0_u8; 32];
        word[24..].copy_from_slice(&batch_calldata_non_zeros.to_be_bytes());
        out.extend_from_slice(&word);

        word = [0_u8; 32];
        word[24..].copy_from_slice(&batch_extra_gas.to_be_bytes());
        out.extend_from_slice(&word);

        out.extend_from_slice(&l1_base_fee_wei.to_be_bytes::<32>());
        out.into()
    }

    fn encode_send_tx_to_l1_calldata(destination: Address, data: &[u8]) -> revm::primitives::Bytes {
        let selector_hash = keccak256("sendTxToL1(address,bytes)");
        let mut out = Vec::with_capacity(4 + (32 * 3) + data.len() + 32);
        out.extend_from_slice(&selector_hash[..4]);

        let mut destination_word = [0_u8; 32];
        destination_word[12..].copy_from_slice(destination.as_slice());
        out.extend_from_slice(&destination_word);
        out.extend_from_slice(&U256::from(64_u64).to_be_bytes::<32>());

        out.extend_from_slice(&U256::from(data.len()).to_be_bytes::<32>());
        out.extend_from_slice(data);
        let remainder = data.len() % 32;
        if remainder != 0 {
            out.extend(std::iter::repeat_n(0_u8, 32 - remainder));
        }
        out.into()
    }

    fn encode_set_network_fee_account_calldata(account: Address) -> revm::primitives::Bytes {
        let mut out = Vec::with_capacity(4 + 32);
        out.extend_from_slice(&ArbOwner::setNetworkFeeAccountCall::SELECTOR);
        out.extend_from_slice(&[0_u8; 12]);
        out.extend_from_slice(account.as_slice());
        out.into()
    }

    fn make_regular_call_tx(
        caller: Address,
        to: Address,
        value: U256,
        data: revm::primitives::Bytes,
    ) -> ArbTransaction<TxEnv> {
        let mut tx = TxEnv::default();
        tx.tx_type = 0x02;
        tx.caller = caller;
        tx.kind = TxKind::Call(to);
        tx.value = value;
        tx.data = data;
        tx.gas_limit = 1_000_000;
        tx.gas_price = 0;
        tx.nonce = 0;
        tx.chain_id = Some(42161);
        ArbTransaction::new(tx)
    }

    fn encode_redeem_calldata(ticket_id: B256) -> revm::primitives::Bytes {
        let selector_hash = keccak256("redeem(bytes32)");
        let mut out = Vec::with_capacity(4 + 32);
        out.extend_from_slice(&selector_hash[..4]);
        out.extend_from_slice(ticket_id.as_slice());
        out.into()
    }

    #[test]
    fn internal_tx_skips_fee_deduction_for_caller() {
        let mut evm = make_evm(InMemoryDB::default());
        let mut tx = make_call_tx(
            ARBITRUM_INTERNAL_TX_TYPE,
            ARBOS_ACTS_ADDRESS,
            ARBOS_ACTS_ADDRESS,
        );
        tx.base.data = encode_start_block_calldata(U256::ZERO, 0, 0, 0);
        let result = evm.transact_one(tx);
        assert!(
            result.is_ok(),
            "internal tx should not fail on caller funds"
        );
    }

    #[test]
    fn non_internal_tx_still_requires_funds() {
        let mut evm = make_evm(InMemoryDB::default());
        let tx = make_call_tx(0x02, Address::with_last_byte(1), Address::ZERO);
        let err = match evm.transact_one(tx) {
            Ok(_) => panic!("non-internal tx should fail for unfunded caller"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            EVMError::Transaction(InvalidTransaction::LackOfFundForMaxFee { .. })
        ));
    }

    #[test]
    fn deposit_tx_skips_fee_deduction_for_caller() {
        let mut evm = make_evm(InMemoryDB::default());
        let tx = make_deposit_tx(
            Address::with_last_byte(0x11),
            Address::with_last_byte(0x22),
            U256::from(7_u64),
        );
        let result = evm.transact_one(tx);
        assert!(result.is_ok(), "deposit tx should not fail on caller funds");
    }

    #[test]
    fn deposit_tx_mints_and_transfers_value() {
        let cfg = CfgEnv::new_with_spec(ArbSpecId::NITRO)
            .with_chain_id(42161)
            .with_disable_priority_fee_check(true);
        let mut block = BlockEnv::default();
        block.timestamp = U256::ZERO;
        let ctx = Context::mainnet()
            .with_tx(ArbTransaction::<TxEnv>::default())
            .with_cfg(cfg)
            .with_block(block)
            .with_chain(ArbChainContext::default())
            .with_db(InMemoryDB::default());
        let mut evm = ctx.build_arb();

        let from = Address::with_last_byte(0x33);
        let to = Address::with_last_byte(0x44);
        let value = U256::from(9_u64);
        let tx = make_deposit_tx(from, to, value);

        let out = evm.transact(tx).expect("deposit tx should execute");
        let to_account = out
            .state
            .get(&to)
            .expect("recipient account should be present in state diff");
        assert_eq!(to_account.info.balance, value);

        let from_account = out
            .state
            .get(&from)
            .expect("sender account should be present in state diff");
        assert_eq!(from_account.info.balance, U256::ZERO);
        assert_eq!(from_account.info.nonce, 0);
    }

    fn retryable_escrow_address(ticket_id: B256) -> Address {
        let mut preimage = Vec::with_capacity("retryable escrow".len() + ticket_id.len());
        preimage.extend_from_slice(b"retryable escrow");
        preimage.extend_from_slice(ticket_id.as_slice());
        let hash = keccak256(preimage);
        Address::from_slice(&hash[12..])
    }

    #[test]
    fn submit_retryable_tx_skips_fee_deduction_for_caller() {
        let mut evm = make_evm(InMemoryDB::default());
        let call = SubmitRetryableCall {
            request_id: B256::with_last_byte(0x77),
            l1_base_fee: U256::from(1_u64),
            deposit_value: U256::from(3_000_u64),
            retry_value: U256::from(4_u64),
            gas_fee_cap: U256::from(1_u64),
            gas_limit: 200_000,
            max_submission_fee: U256::from(2_000_u64),
            fee_refund_address: Address::with_last_byte(0x45),
            beneficiary: Address::with_last_byte(0x46),
            retry_to: Some(Address::with_last_byte(0x47)),
            retry_data: revm::primitives::bytes!("01020304"),
        };
        let tx = make_submit_retryable_tx(Address::with_last_byte(0x44), call);
        let result = evm.transact_one(tx);
        assert!(
            result.is_ok(),
            "submit-retryable tx should not fail on caller funds"
        );
    }

    #[test]
    fn submit_retryable_tx_creates_retryable_record_and_escrows_callvalue() {
        let cfg = CfgEnv::new_with_spec(ArbSpecId::NITRO)
            .with_chain_id(42161)
            .with_disable_priority_fee_check(true);
        let ctx = Context::mainnet()
            .with_tx(ArbTransaction::<TxEnv>::default())
            .with_cfg(cfg)
            .with_chain(ArbChainContext::default())
            .with_db(InMemoryDB::default());
        let mut evm = ctx.build_arb();

        let caller = Address::with_last_byte(0x51);
        let beneficiary = Address::with_last_byte(0x52);
        let retry_to = Address::with_last_byte(0x53);
        let fee_refund = Address::with_last_byte(0x54);
        let retry_value = U256::from(8_u64);
        let call = SubmitRetryableCall {
            request_id: B256::with_last_byte(0xAB),
            l1_base_fee: U256::from(1_u64),
            deposit_value: U256::from(3_000_u64),
            retry_value,
            gas_fee_cap: U256::from(1_u64),
            gas_limit: 250_000,
            max_submission_fee: U256::from(2_000_u64),
            fee_refund_address: fee_refund,
            beneficiary,
            retry_to: Some(retry_to),
            retry_data: revm::primitives::bytes!("deadbeef"),
        };
        let tx = make_submit_retryable_tx(caller, call.clone());
        let out = evm
            .transact(tx)
            .expect("submit-retryable tx should execute");

        let ticket_id = compute_submit_retryable_ticket_id(42161, caller, &call);
        let escrow = retryable_escrow_address(ticket_id);
        let escrow_account = out
            .state
            .get(&escrow)
            .expect("escrow account should be present in state diff");
        assert_eq!(escrow_account.info.balance, retry_value);

        let arbos_state = ArbosState::open();
        let retryable = arbos_state.retryables.retryable(ticket_id);
        let (_, timeout_slot) = retryable.timeout.account_and_key();
        let (_, from_slot) = retryable.from.account_and_key();
        let (_, beneficiary_slot) = retryable.beneficiary.account_and_key();
        let (_, callvalue_slot) = retryable.callvalue.account_and_key();
        let (_, to_slot) = retryable.to_raw.account_and_key();
        let (_, num_tries_slot) = retryable.num_tries.account_and_key();

        let arbos_account = out
            .state
            .get(&ARBOS_STATE_ADDRESS)
            .expect("ArbOS state account should be present in state diff");

        let timeout_slot_key = U256::from_be_bytes(timeout_slot.0);
        let from_slot_key = U256::from_be_bytes(from_slot.0);
        let beneficiary_slot_key = U256::from_be_bytes(beneficiary_slot.0);
        let callvalue_slot_key = U256::from_be_bytes(callvalue_slot.0);
        let to_slot_key = U256::from_be_bytes(to_slot.0);
        let num_tries_slot_key = U256::from_be_bytes(num_tries_slot.0);

        let timeout_written = arbos_account
            .storage
            .get(&timeout_slot_key)
            .expect("retryable timeout slot should be written")
            .present_value();
        assert!(
            timeout_written >= U256::from(RETRYABLE_LIFETIME_SECONDS),
            "retryable timeout should be at least one lifetime in the future"
        );

        let from_written = arbos_account
            .storage
            .get(&from_slot_key)
            .expect("retryable from slot should be written")
            .present_value();
        let mut from_word = [0_u8; 32];
        from_word[12..].copy_from_slice(caller.as_slice());
        assert_eq!(from_written, U256::from_be_bytes(from_word));

        let beneficiary_written = arbos_account
            .storage
            .get(&beneficiary_slot_key)
            .expect("retryable beneficiary slot should be written")
            .present_value();
        let mut beneficiary_word = [0_u8; 32];
        beneficiary_word[12..].copy_from_slice(beneficiary.as_slice());
        assert_eq!(beneficiary_written, U256::from_be_bytes(beneficiary_word));

        let callvalue_written = arbos_account
            .storage
            .get(&callvalue_slot_key)
            .expect("retryable callvalue slot should be written")
            .present_value();
        assert_eq!(callvalue_written, retry_value);

        let to_written = arbos_account
            .storage
            .get(&to_slot_key)
            .expect("retryable to slot should be written")
            .present_value();
        let mut to_word = [0_u8; 32];
        to_word[12..].copy_from_slice(retry_to.as_slice());
        assert_eq!(to_written, U256::from_be_bytes(to_word));

        let num_tries_written = arbos_account
            .storage
            .get(&num_tries_slot_key)
            .expect("retryable numTries slot should be written")
            .present_value();
        assert_eq!(
            num_tries_written,
            U256::ZERO,
            "insufficient submit-time balance should leave auto-redeem unscheduled"
        );

        let fee_refund_account = out
            .state
            .get(&fee_refund)
            .expect("fee refund account should be present in state diff");
        assert_eq!(
            fee_refund_account.info.balance,
            U256::from(1_568_u64),
            "submit-retryable should refund excess submission fee plus gas-price refund"
        );
        assert_eq!(
            out.result.tx_gas_used(),
            0,
            "submit-retryable with no scheduled auto-redeem reports gasUsed 0 (Nitro ZeroGas)"
        );
        assert_eq!(
            out.result.logs().len(),
            1,
            "unscheduled submit-retryable should emit TicketCreated only"
        );
        let ticket_created_sig = keccak256("TicketCreated(bytes32)");
        let ticket_created = out
            .result
            .logs()
            .iter()
            .find(|log| {
                log.address == ARB_RETRYABLE_TX_ADDRESS
                    && log.topics().len() == 2
                    && log.topics()[0] == ticket_created_sig
            })
            .expect("submit-retryable should emit TicketCreated");
        assert_eq!(ticket_created.topics()[1], ticket_id);
    }

    #[test]
    fn submit_retryable_tx_with_underfunded_submission_fee_fails_without_halting() {
        // Nitro `tx_processor.go:258`: when the deposit mint can't cover the max submission
        // fee, the tx is recorded as failed (status 0, gasUsed 0) and the block continues,
        // keeping the deposit mint. It must NOT surface as a fatal EVM error (which would
        // halt the driver, the real Arbitrum One batch-1 retryable 0xff56fb78… hits this).
        let cfg = CfgEnv::new_with_spec(ArbSpecId::NITRO)
            .with_chain_id(42161)
            .with_disable_priority_fee_check(true);
        let ctx = Context::mainnet()
            .with_tx(ArbTransaction::<TxEnv>::default())
            .with_cfg(cfg)
            .with_chain(ArbChainContext::default())
            .with_db(InMemoryDB::default());
        let mut evm = ctx.build_arb();

        let caller = Address::with_last_byte(0x71);
        // deposit_value (1_000) < max_submission_fee (2_000); the caller starts at 0, so the
        // balance after the mint is exactly the deposit and falls short of the fee.
        let call = SubmitRetryableCall {
            request_id: B256::with_last_byte(0x72),
            l1_base_fee: U256::from(1_u64),
            deposit_value: U256::from(1_000_u64),
            retry_value: U256::from(5_u64),
            gas_fee_cap: U256::from(1_u64),
            gas_limit: 250_000,
            max_submission_fee: U256::from(2_000_u64),
            fee_refund_address: Address::with_last_byte(0x73),
            beneficiary: Address::with_last_byte(0x74),
            retry_to: Some(Address::with_last_byte(0x75)),
            retry_data: revm::primitives::bytes!("deadbeef"),
        };
        let ticket_id = compute_submit_retryable_ticket_id(42161, caller, &call);
        let tx = make_submit_retryable_tx(caller, call);
        let out = evm.transact(tx).expect("under-funded submit-retryable must not be a fatal error");

        assert!(!out.result.is_success(), "receipt status must be failure");
        assert_eq!(out.result.tx_gas_used(), 0, "failed submit-retryable consumes zero gas");
        assert!(out.result.logs().is_empty(), "no TicketCreated on a failed submission");

        // The deposit mint is kept (Nitro does not revert it on this failure path).
        let caller_account =
            out.state.get(&caller).expect("caller account present in state diff");
        assert_eq!(caller_account.info.balance, U256::from(1_000_u64));

        // No retryable record was created: its numTries slot is unwritten.
        let arbos_state = ArbosState::open();
        let (_, num_tries_slot) = arbos_state.retryables.retryable(ticket_id).num_tries.account_and_key();
        let unwritten = out
            .state
            .get(&ARBOS_STATE_ADDRESS)
            .and_then(|acct| acct.storage.get(&U256::from_be_bytes(num_tries_slot.0)))
            .is_none();
        assert!(unwritten, "failed submission must not create a retryable record");
    }

    #[test]
    fn submit_retryable_tx_emits_redeem_scheduled_when_auto_redeem_is_funded() {
        let cfg = CfgEnv::new_with_spec(ArbSpecId::NITRO)
            .with_chain_id(42161)
            .with_disable_priority_fee_check(true);
        let ctx = Context::mainnet()
            .with_tx(ArbTransaction::<TxEnv>::default())
            .with_cfg(cfg)
            .with_chain(ArbChainContext::default())
            .with_db(InMemoryDB::default());
        let mut evm = ctx.build_arb();

        let caller = Address::with_last_byte(0x61);
        let call = SubmitRetryableCall {
            request_id: B256::with_last_byte(0x62),
            l1_base_fee: U256::from(1_u64),
            deposit_value: U256::from(100_000_u64),
            retry_value: U256::from(9_u64),
            gas_fee_cap: U256::from(1_u64),
            gas_limit: 21_000,
            max_submission_fee: U256::from(2_500_u64),
            fee_refund_address: Address::with_last_byte(0x63),
            beneficiary: Address::with_last_byte(0x64),
            retry_to: Some(Address::with_last_byte(0x65)),
            retry_data: revm::primitives::bytes!("c0ffee"),
        };
        let out = evm
            .transact(make_submit_retryable_tx(caller, call.clone()))
            .expect("submit-retryable tx should execute");

        let ticket_id = compute_submit_retryable_ticket_id(42161, caller, &call);
        let ticket_created_sig = keccak256("TicketCreated(bytes32)");
        let redeem_scheduled_sig =
            keccak256("RedeemScheduled(bytes32,bytes32,uint64,uint64,address,uint256,uint256)");
        assert_eq!(
            out.result.logs().len(),
            2,
            "funded submit-retryable should emit TicketCreated and RedeemScheduled"
        );
        assert!(
            out.result.logs().iter().any(|log| {
                log.address == ARB_RETRYABLE_TX_ADDRESS
                    && log.topics().len() == 2
                    && log.topics()[0] == ticket_created_sig
                    && log.topics()[1] == ticket_id
            }),
            "missing TicketCreated log"
        );
        assert!(
            out.result.logs().iter().any(|log| {
                log.address == ARB_RETRYABLE_TX_ADDRESS
                    && log.topics().len() == 4
                    && log.topics()[0] == redeem_scheduled_sig
                    && log.topics()[1] == ticket_id
            }),
            "missing RedeemScheduled log"
        );
        assert_eq!(
            out.result.tx_gas_used(),
            21_000,
            "submit-retryable with a scheduled auto-redeem charges the reserved usergas as gasUsed"
        );
    }

    #[test]
    fn arb_retryable_redeem_emits_redeem_scheduled_and_returns_retry_hash() {
        let cfg = CfgEnv::new_with_spec(ArbSpecId::NITRO)
            .with_chain_id(42161)
            .with_disable_priority_fee_check(true);
        let ctx = Context::mainnet()
            .with_tx(ArbTransaction::<TxEnv>::default())
            .with_cfg(cfg)
            .with_chain(ArbChainContext::default())
            .with_db(InMemoryDB::default());
        let mut evm = ctx.build_arb();

        let submitter = Address::with_last_byte(0x81);
        let redeemer = Address::with_last_byte(0x82);
        let retry_to = Address::with_last_byte(0x83);
        let retry_data = revm::primitives::bytes!("c0ffee");
        let retry_value = U256::from(9_u64);

        let call = SubmitRetryableCall {
            request_id: B256::with_last_byte(0x84),
            l1_base_fee: U256::from(1_u64),
            deposit_value: U256::from(4_000_u64),
            retry_value,
            gas_fee_cap: U256::from(1_u64),
            gas_limit: 250_000,
            max_submission_fee: U256::from(2_000_u64),
            fee_refund_address: Address::with_last_byte(0x85),
            beneficiary: Address::with_last_byte(0x86),
            retry_to: Some(retry_to),
            retry_data: retry_data.clone(),
        };
        let submit_tx = make_submit_retryable_tx(submitter, call.clone());
        evm.transact_commit(submit_tx)
            .expect("submit retryable should commit");

        let ticket_id = compute_submit_retryable_ticket_id(42161, submitter, &call);
        let redeem_call = encode_redeem_calldata(ticket_id);
        let redeem_tx =
            make_regular_call_tx(redeemer, ARB_RETRYABLE_TX_ADDRESS, U256::ZERO, redeem_call);
        let out = evm.transact(redeem_tx).expect("redeem call should execute");
        assert!(out.result.is_success(), "redeem should succeed");

        let redeem_event_sig =
            keccak256("RedeemScheduled(bytes32,bytes32,uint64,uint64,address,uint256,uint256)");
        let redeem_log = out
            .result
            .logs()
            .iter()
            .find(|log| {
                log.address == ARB_RETRYABLE_TX_ADDRESS
                    && log.topics().len() == 4
                    && log.topics()[0] == redeem_event_sig
            })
            .expect("redeem should emit RedeemScheduled event");

        assert_eq!(redeem_log.topics()[1], ticket_id);
        let mut nonce_topic = [0_u8; 32];
        nonce_topic[24..].copy_from_slice(&0_u64.to_be_bytes());
        assert_eq!(redeem_log.topics()[3], B256::from(nonce_topic));

        let output = out
            .result
            .output()
            .expect("redeem should return bytes32 retry tx hash");
        assert_eq!(output.as_ref(), redeem_log.topics()[2].as_slice());
    }

    #[test]
    fn arb_sys_send_tx_to_l1_emits_l2_to_l1_event_and_burns_value() {
        let cfg = CfgEnv::new_with_spec(ArbSpecId::NITRO)
            .with_chain_id(42161)
            .with_disable_priority_fee_check(true);
        let ctx = Context::mainnet()
            .with_tx(ArbTransaction::<TxEnv>::default())
            .with_cfg(cfg)
            .with_chain(ArbChainContext::default())
            .with_db(InMemoryDB::default());
        let mut evm = ctx.build_arb();
        let caller = Address::with_last_byte(0x71);
        let destination = Address::with_last_byte(0x72);
        let funded_balance = U256::from(1_000_u64);
        let callvalue = U256::from(7_u64);
        let payload = revm::primitives::bytes!("11223344");

        let funding_tx = make_deposit_tx(Address::with_last_byte(0x70), caller, funded_balance);
        evm.transact_commit(funding_tx)
            .expect("funding deposit should commit");

        let tx = make_regular_call_tx(
            caller,
            ARB_SYS,
            callvalue,
            encode_send_tx_to_l1_calldata(destination, payload.as_ref()),
        );
        let out = evm.transact(tx).expect("sendTxToL1 tx should execute");
        assert!(out.result.is_success(), "sendTxToL1 should succeed");

        let caller_account = out
            .state
            .get(&caller)
            .expect("caller account should be present in state diff");
        assert_eq!(
            caller_account.info.balance,
            funded_balance.saturating_sub(callvalue),
            "caller should pay exactly the send callvalue when gas price is zero"
        );

        let logs = out.result.logs();
        assert_eq!(logs.len(), 1, "first send should emit only L2ToL1Tx");
        let l2_to_l1 = &logs[0];
        assert_eq!(l2_to_l1.address, ARB_SYS);

        let topics = l2_to_l1.topics();
        assert_eq!(topics.len(), 4);
        assert_eq!(
            topics[0],
            keccak256(
                "L2ToL1Tx(address,address,uint256,uint256,uint256,uint256,uint256,uint256,bytes)"
            ),
        );

        let mut destination_topic = [0_u8; 32];
        destination_topic[12..].copy_from_slice(destination.as_slice());
        assert_eq!(topics[1], B256::from(destination_topic));
    }

    #[test]
    fn arb_sys_send_tx_to_l1_reverts_when_native_token_owners_exist_and_value_sent() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(ARBOS_STATE_ADDRESS, AccountInfo::default());
        let state = ArbosState::open();
        let (_, version_slot) = state.arbos_version.account_and_key();
        let (_, owner_size_slot) = state.native_token_owners.size.account_and_key();
        db.insert_account_storage(
            ARBOS_STATE_ADDRESS,
            U256::from_be_bytes(version_slot.0),
            U256::from(41_u64),
        )
        .expect("should seed ArbOS version");
        db.insert_account_storage(
            ARBOS_STATE_ADDRESS,
            U256::from_be_bytes(owner_size_slot.0),
            U256::from(1_u64),
        )
        .expect("should seed native token owners size");

        let cfg = CfgEnv::new_with_spec(ArbSpecId::NITRO)
            .with_chain_id(42161)
            .with_disable_priority_fee_check(true);
        let ctx = Context::mainnet()
            .with_tx(ArbTransaction::<TxEnv>::default())
            .with_cfg(cfg)
            .with_chain(ArbChainContext::default())
            .with_db(db);
        let mut evm = ctx.build_arb();
        let caller = Address::with_last_byte(0x73);
        let destination = Address::with_last_byte(0x74);
        let funding_tx =
            make_deposit_tx(Address::with_last_byte(0x75), caller, U256::from(100_u64));
        evm.transact_commit(funding_tx)
            .expect("funding deposit should commit");

        let tx = make_regular_call_tx(
            caller,
            ARB_SYS,
            U256::from(1_u64),
            encode_send_tx_to_l1_calldata(destination, &[]),
        );
        let out = evm.transact(tx).expect("sendTxToL1 tx should execute");
        assert!(
            !out.result.is_success(),
            "sendTxToL1 should revert when native token owners exist and value is non-zero"
        );

        let revert_data = out
            .result
            .output()
            .expect("revert should include encoded error data");
        let expected = b"not allowed to send value when native token owners exist";
        assert!(
            revert_data
                .as_ref()
                .windows(expected.len())
                .any(|window| window == expected),
            "revert payload should include Nitro-compatible restriction message"
        );
    }

    #[test]
    fn arb_owner_reverts_for_non_owner_caller() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(ARBOS_STATE_ADDRESS, AccountInfo::default());
        let cfg = CfgEnv::new_with_spec(ArbSpecId::NITRO)
            .with_chain_id(42161)
            .with_disable_priority_fee_check(true);
        let ctx = Context::mainnet()
            .with_tx(ArbTransaction::<TxEnv>::default())
            .with_cfg(cfg)
            .with_chain(ArbChainContext::default())
            .with_db(db);
        let mut evm = ctx.build_arb();
        let caller = Address::with_last_byte(0x81);
        let replacement = Address::with_last_byte(0x82);

        let tx = make_regular_call_tx(
            caller,
            ARB_OWNER,
            U256::ZERO,
            encode_set_network_fee_account_calldata(replacement),
        );
        let out = evm.transact(tx).expect("ArbOwner call should execute");
        assert!(
            !out.result.is_success(),
            "ArbOwner should reject callers that are not chain owners"
        );

        let revert_data = out
            .result
            .output()
            .expect("revert should include encoded error data");
        let expected = b"unauthorized caller to access-controlled method";
        assert!(
            revert_data
                .as_ref()
                .windows(expected.len())
                .any(|window| window == expected),
            "revert payload should include Nitro-compatible owner-access message"
        );
    }

    #[test]
    fn internal_tx_rejects_non_arbos_caller() {
        let mut evm = make_evm(InMemoryDB::default());
        let tx = make_call_tx(
            ARBITRUM_INTERNAL_TX_TYPE,
            Address::with_last_byte(9),
            ARBOS_ACTS_ADDRESS,
        );
        let err = match evm.transact_one(tx) {
            Ok(_) => panic!("internal tx should reject non-ArbOS caller"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            EVMError::Transaction(InvalidTransaction::Str(_))
        ));
    }

    #[test]
    fn internal_tx_rejects_non_arbos_target() {
        let mut evm = make_evm(InMemoryDB::default());
        let tx = make_call_tx(ARBITRUM_INTERNAL_TX_TYPE, ARBOS_ACTS_ADDRESS, Address::ZERO);
        let err = match evm.transact_one(tx) {
            Ok(_) => panic!("internal tx should reject non-ArbOS target"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            EVMError::Transaction(InvalidTransaction::Str(_))
        ));
    }

    #[test]
    fn submit_retryable_tx_rejects_non_retryable_precompile_target() {
        let mut evm = make_evm(InMemoryDB::default());
        let tx = make_call_tx(
            ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE,
            Address::with_last_byte(0x31),
            Address::ZERO,
        );
        let err = match evm.transact_one(tx) {
            Ok(_) => panic!("submit-retryable tx should reject non-precompile target"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            EVMError::Transaction(InvalidTransaction::Str(_))
        ));
    }

    #[test]
    fn retry_tx_success_deletes_retryable() {
        let mut evm = make_evm(InMemoryDB::default());
        let caller = Address::with_last_byte(0x61);
        let beneficiary = Address::with_last_byte(0x62);
        let retry_to = Address::with_last_byte(0x63);
        let retry_value = U256::from(9_u64);
        let call = SubmitRetryableCall {
            request_id: B256::with_last_byte(0x91),
            l1_base_fee: U256::from(1_u64),
            deposit_value: U256::from(4_000_u64),
            retry_value,
            gas_fee_cap: U256::from(1_u64),
            gas_limit: 250_000,
            max_submission_fee: U256::from(2_000_u64),
            fee_refund_address: Address::with_last_byte(0x64),
            beneficiary,
            retry_to: Some(retry_to),
            retry_data: revm::primitives::bytes!("010203"),
        };
        let submit_tx = make_submit_retryable_tx(caller, call.clone());
        evm.transact_commit(submit_tx)
            .expect("submit retryable should commit");

        let ticket_id = compute_submit_retryable_ticket_id(42161, caller, &call);
        let retry_tx = make_retry_tx(caller, retry_to, ticket_id, retry_value, 1_000_000);
        let _ = evm
            .transact_commit(retry_tx)
            .expect("retry tx should execute");

        let retry_tx_again = make_retry_tx(caller, retry_to, ticket_id, retry_value, 1_000_000);
        let err = match evm.transact_one(retry_tx_again) {
            Ok(_) => panic!("second retry should fail because retryable was deleted"),
            Err(err) => err,
        };
        assert!(
            matches!(err, EVMError::Custom(_)),
            "expected retryable-not-found path to bubble as custom error"
        );
    }

    #[test]
    fn start_block_internal_tx_rejects_l2_block_mismatch() {
        let mut evm = make_evm(InMemoryDB::default());
        let mut tx = make_call_tx(
            ARBITRUM_INTERNAL_TX_TYPE,
            ARBOS_ACTS_ADDRESS,
            ARBOS_ACTS_ADDRESS,
        );
        tx.base.data = encode_start_block_calldata(U256::ZERO, 9, 999, 0);
        let err = match evm.transact_one(tx) {
            Ok(_) => panic!("startBlock should reject mismatched l2 block number"),
            Err(err) => err,
        };
        assert!(matches!(err, EVMError::Custom(_)));
    }

    #[test]
    fn batch_posting_report_internal_tx_executes() {
        let mut evm = make_evm(InMemoryDB::default());
        let mut tx = make_call_tx(
            ARBITRUM_INTERNAL_TX_TYPE,
            ARBOS_ACTS_ADDRESS,
            ARBOS_ACTS_ADDRESS,
        );
        tx.base.data = encode_batch_posting_report_calldata(
            U256::ZERO,
            Address::with_last_byte(0x42),
            1,
            123_456,
            U256::from(7),
        );

        let result = evm.transact_one(tx);
        assert!(
            result.is_ok(),
            "batchPostingReport internal tx should execute"
        );
    }

    #[test]
    fn batch_posting_report_v2_internal_tx_executes() {
        let mut evm = make_evm(InMemoryDB::default());
        let mut tx = make_call_tx(
            ARBITRUM_INTERNAL_TX_TYPE,
            ARBOS_ACTS_ADDRESS,
            ARBOS_ACTS_ADDRESS,
        );
        tx.base.data = encode_batch_posting_report_v2_calldata(
            U256::ZERO,
            Address::with_last_byte(0x43),
            2,
            10_000,
            7_500,
            50_000,
            U256::from(5),
        );

        let result = evm.transact_one(tx);
        assert!(
            result.is_ok(),
            "batchPostingReportV2 internal tx should execute"
        );
    }
}
