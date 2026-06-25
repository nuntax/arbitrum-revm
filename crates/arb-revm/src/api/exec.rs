use crate::{
    ArbSpecId, chain::ArbChainContext, evm::ArbEvm, handler::ArbHandler, transaction::ArbTxTr,
};
use revm::{
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
    context::{ContextSetters, result::ExecResultAndState},
    context_interface::{
        Cfg, ContextTr, Database, JournalTr,
        result::{EVMError, ExecutionResult, InvalidTransaction},
    },
    handler::{
        EthFrame, Handler, PrecompileProvider, SystemCallTx,
        instructions::EthInstructions,
        system_call::{SystemCallCommitEvm, SystemCallEvm},
    },
    inspector::{
        InspectCommitEvm, InspectEvm, InspectSystemCallEvm, Inspector, InspectorHandler, JournalExt,
    },
    interpreter::{InterpreterResult, interpreter::EthInterpreter},
    primitives::{Address, Bytes},
    state::EvmState,
};

/// Context trait bound used by Arbitrum execution APIs.
pub trait ArbContextTr:
    ContextTr<
        Journal: JournalTr<State = EvmState>,
        Tx: ArbTxTr,
        Cfg: Cfg<Spec = ArbSpecId>,
        Chain = ArbChainContext,
    >
{
}

impl<T> ArbContextTr for T where
    T: ContextTr<
            Journal: JournalTr<State = EvmState>,
            Tx: ArbTxTr,
            Cfg: Cfg<Spec = ArbSpecId>,
            Chain = ArbChainContext,
        >
{
}

/// Error type for Arbitrum EVM execution.
pub type ArbError<CTX> = EVMError<<<CTX as ContextTr>::Db as Database>::Error, InvalidTransaction>;

impl<CTX, INSP, PRECOMPILE> ExecuteEvm
    for ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: ArbContextTr + ContextSetters,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type Tx = <CTX as ContextTr>::Tx;
    type Block = <CTX as ContextTr>::Block;
    type State = EvmState;
    type Error = ArbError<CTX>;
    type ExecutionResult = ExecutionResult<revm::context_interface::result::HaltReason>;

    fn set_block(&mut self, block: Self::Block) {
        self.0.ctx.set_block(block);
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.0.ctx.set_tx(tx);
        let mut h = ArbHandler::<_, _, EthFrame<EthInterpreter>>::new();
        h.run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.0.ctx.journal_mut().finalize()
    }

    fn replay(
        &mut self,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        let mut h = ArbHandler::<_, _, EthFrame<EthInterpreter>>::new();
        h.run(self).map(|result| {
            let state = self.finalize();
            ExecResultAndState::new(result, state)
        })
    }
}

impl<CTX, INSP, PRECOMPILE> ExecuteCommitEvm
    for ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: ArbContextTr<Db: DatabaseCommit> + ContextSetters,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    fn commit(&mut self, state: Self::State) {
        self.0.ctx.db_mut().commit(state);
    }
}

impl<CTX, INSP, PRECOMPILE> InspectEvm
    for ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: ArbContextTr<Journal: JournalExt> + ContextSetters,
    INSP: Inspector<CTX, EthInterpreter>,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.0.inspector = inspector;
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.0.ctx.set_tx(tx);
        let mut h = ArbHandler::<_, _, EthFrame<EthInterpreter>>::new();
        h.inspect_run(self)
    }
}

impl<CTX, INSP, PRECOMPILE> InspectCommitEvm
    for ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: ArbContextTr<Journal: JournalExt, Db: DatabaseCommit> + ContextSetters,
    INSP: Inspector<CTX, EthInterpreter>,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
}

impl<CTX, INSP, PRECOMPILE> SystemCallEvm
    for ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: ArbContextTr<Tx: SystemCallTx> + ContextSetters,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    fn system_call_one_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.0.ctx.set_tx(CTX::Tx::new_system_tx_with_caller(
            caller,
            system_contract_address,
            data,
        ));
        let mut h = ArbHandler::<_, _, EthFrame<EthInterpreter>>::new();
        h.run_system_call(self)
    }
}

impl<CTX, INSP, PRECOMPILE> InspectSystemCallEvm
    for ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: ArbContextTr<Journal: JournalExt, Tx: SystemCallTx> + ContextSetters,
    INSP: Inspector<CTX, EthInterpreter>,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    fn inspect_one_system_call_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.0.ctx.set_tx(CTX::Tx::new_system_tx_with_caller(
            caller,
            system_contract_address,
            data,
        ));
        let mut h = ArbHandler::<_, _, EthFrame<EthInterpreter>>::new();
        h.inspect_run_system_call(self)
    }
}

impl<CTX, INSP, PRECOMPILE> SystemCallCommitEvm
    for ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: ArbContextTr<Db: DatabaseCommit, Tx: SystemCallTx> + ContextSetters,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    fn system_call_with_caller_commit(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.system_call_with_caller(caller, system_contract_address, data)
            .map(|output| {
                self.commit(output.state);
                output.result
            })
    }
}
