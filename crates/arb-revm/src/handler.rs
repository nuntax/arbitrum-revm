use crate::{
    api::exec::ArbContextTr,
    constants::{ARBITRUM_DEPOSIT_TX_TYPE, ARBITRUM_INTERNAL_TX_TYPE, ARBOS_ACTS_ADDRESS},
    deposit_tx, internal_tx,
};
use revm::{
    context_interface::{
        result::{FromStringError, HaltReason, InvalidTransaction},
        ContextTr, Transaction,
    },
    handler::{
        evm::FrameTr, handler::EvmTrError, EthFrame, EvmTr, FrameResult, Handler, MainnetHandler,
    },
    inspector::{Inspector, InspectorEvmTr, InspectorHandler},
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, CallOutcome, Gas,
        InitialAndFloorGas, InstructionResult, InterpreterResult,
    },
    primitives::{Address, Bytes},
};

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
fn is_protocol_short_circuit_tx<EVM: EvmTr>(evm: &mut EVM) -> bool {
    is_internal_tx(evm) || is_deposit_tx(evm)
}

#[inline]
fn is_allowed_internal_caller(caller: Address) -> bool {
    caller == ARBOS_ACTS_ADDRESS
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
            return Ok(InitialAndFloorGas::new(0, 0));
        }
        self.mainnet.validate(evm)
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
        self.mainnet.validate_env(evm)
    }

    fn pre_execution(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error> {
        if is_protocol_short_circuit_tx(evm) {
            return Ok(0);
        }
        self.mainnet.pre_execution(evm)
    }

    fn execution(
        &mut self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &InitialAndFloorGas,
    ) -> Result<FrameResult, Self::Error> {
        if is_internal_tx(evm) {
            internal_tx::apply_internal_tx(evm.ctx_mut())
                .map_err(|msg| ERROR::from_string(msg.into()))?;
            return Ok(internal_success_frame_result());
        }
        if is_deposit_tx(evm) {
            deposit_tx::apply_deposit_tx(evm.ctx_mut())
                .map_err(|msg| ERROR::from_string(msg.into()))?;
            return Ok(internal_success_frame_result());
        }
        self.mainnet.execution(evm, init_and_floor_gas)
    }

    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<(), Self::Error> {
        if is_protocol_short_circuit_tx(evm) {
            // Nitro internal/deposit txs are protocol actions, not fee-paying user transactions.
            return Ok(());
        }
        self.mainnet.validate_against_state_and_deduct_caller(evm)
    }

    fn last_frame_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        if is_protocol_short_circuit_tx(evm) {
            let status = frame_result.interpreter_result().result;
            if !status.is_ok() {
                let label = if is_internal_tx(evm) {
                    "internal"
                } else {
                    "deposit"
                };
                return Err(ERROR::from_string(
                    format!("[ARBITRUM] {label} transaction execution failed").into(),
                ));
            }
            return Ok(());
        }
        self.mainnet.last_frame_result(evm, frame_result)
    }

    fn reimburse_caller(
        &self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        if is_protocol_short_circuit_tx(evm) {
            return Ok(());
        }
        self.mainnet.reimburse_caller(evm, frame_result)
    }

    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        if is_protocol_short_circuit_tx(evm) {
            return Ok(());
        }
        self.mainnet.reward_beneficiary(evm, frame_result)
    }
}

fn internal_success_frame_result() -> FrameResult {
    FrameResult::Call(CallOutcome::new(
        InterpreterResult::new(InstructionResult::Stop, Bytes::new(), Gas::new(0)),
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
}

#[cfg(test)]
mod tests {
    use super::{ARBITRUM_DEPOSIT_TX_TYPE, ARBITRUM_INTERNAL_TX_TYPE};
    use crate::{
        constants::ARBOS_ACTS_ADDRESS, internal_tx, ArbBuilder, ArbChainContext, ArbSpecId,
        ArbTransaction,
    };
    use revm::{
        context::{CfgEnv, TxEnv},
        context_interface::result::{EVMError, InvalidTransaction},
        database::InMemoryDB,
        primitives::{Address, TxKind, U256},
        Context, ExecuteEvm, MainContext,
    };

    fn make_evm(
        db: InMemoryDB,
    ) -> impl ExecuteEvm<
        Tx = ArbTransaction<TxEnv>,
        Error = EVMError<
            <InMemoryDB as revm::Database>::Error,
            revm::context_interface::result::InvalidTransaction,
        >,
    > {
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
        let ctx = Context::mainnet()
            .with_tx(ArbTransaction::<TxEnv>::default())
            .with_cfg(cfg)
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
