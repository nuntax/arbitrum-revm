use crate::{precompiles::ArbPrecompiles, ArbSpecId};
use revm::{
    context::{Cfg, ContextError, ContextSetters, Evm, FrameStack},
    context_interface::ContextTr,
    handler::{
        evm::FrameTr,
        instructions::{EthInstructions, InstructionProvider},
        EthFrame, EvmTr, FrameInitOrResult, ItemOrResult, PrecompileProvider,
    },
    inspector::{InspectorEvmTr, JournalExt},
    interpreter::{interpreter::EthInterpreter, InterpreterResult},
    Database, Inspector,
};

/// Arbitrum EVM wrapper over revm's generic [`Evm`] type.
#[derive(Debug, Clone)]
pub struct ArbEvm<
    CTX,
    INSP,
    I = EthInstructions<EthInterpreter, CTX>,
    P = ArbPrecompiles,
    F = EthFrame<EthInterpreter>,
>(pub Evm<CTX, INSP, I, P, F>);

impl<CTX: ContextTr<Cfg: Cfg<Spec: Into<ArbSpecId> + Clone>>, INSP>
    ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, ArbPrecompiles>
{
    /// Creates a new Arbitrum EVM.
    pub fn new(ctx: CTX, inspector: INSP) -> Self {
        let spec: ArbSpecId = ctx.cfg().spec().into();
        Self(Evm {
            ctx,
            inspector,
            instruction: EthInstructions::new_mainnet_with_spec(spec.into()),
            precompiles: ArbPrecompiles::new_with_spec(spec),
            frame_stack: FrameStack::new_prealloc(8),
        })
    }

    /// Consumes self and returns the inner context.
    pub fn into_context(self) -> CTX {
        self.0.ctx
    }
}

impl<CTX, INSP, I, P> ArbEvm<CTX, INSP, I, P> {
    /// Consumes self and returns a new EVM with a different inspector.
    pub fn with_inspector<OINSP>(self, inspector: OINSP) -> ArbEvm<CTX, OINSP, I, P> {
        ArbEvm(self.0.with_inspector(inspector))
    }

    /// Consumes self and returns a new EVM with a different precompile provider.
    pub fn with_precompiles<OP>(self, precompiles: OP) -> ArbEvm<CTX, INSP, I, OP> {
        ArbEvm(self.0.with_precompiles(precompiles))
    }

    /// Consumes self and returns the inner inspector.
    pub fn into_inspector(self) -> INSP {
        self.0.into_inspector()
    }
}

impl<CTX, INSP, I, P> InspectorEvmTr for ArbEvm<CTX, INSP, I, P>
where
    CTX: ContextTr<Journal: JournalExt> + ContextSetters,
    I: InstructionProvider<Context = CTX, InterpreterTypes = EthInterpreter>,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
    INSP: Inspector<CTX, I::InterpreterTypes>,
{
    type Inspector = INSP;

    fn all_inspector(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
        &Self::Inspector,
    ) {
        self.0.all_inspector()
    }

    fn all_mut_inspector(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
        &mut Self::Inspector,
    ) {
        self.0.all_mut_inspector()
    }
}

impl<CTX, INSP, I, P> EvmTr for ArbEvm<CTX, INSP, I, P, EthFrame<EthInterpreter>>
where
    CTX: ContextTr,
    I: InstructionProvider<Context = CTX, InterpreterTypes = EthInterpreter>,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type Context = CTX;
    type Instructions = I;
    type Precompiles = P;
    type Frame = EthFrame<EthInterpreter>;

    fn all(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
    ) {
        self.0.all()
    }

    fn all_mut(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
    ) {
        self.0.all_mut()
    }

    fn frame_init(
        &mut self,
        frame_input: <Self::Frame as FrameTr>::FrameInit,
    ) -> Result<
        ItemOrResult<&mut Self::Frame, <Self::Frame as FrameTr>::FrameResult>,
        ContextError<<<Self::Context as ContextTr>::Db as Database>::Error>,
    > {
        self.0.frame_init(frame_input)
    }

    fn frame_run(
        &mut self,
    ) -> Result<
        FrameInitOrResult<Self::Frame>,
        ContextError<<<Self::Context as ContextTr>::Db as Database>::Error>,
    > {
        self.0.frame_run()
    }

    fn frame_return_result(
        &mut self,
        result: <Self::Frame as FrameTr>::FrameResult,
    ) -> Result<
        Option<<Self::Frame as FrameTr>::FrameResult>,
        ContextError<<<Self::Context as ContextTr>::Db as Database>::Error>,
    > {
        self.0.frame_return_result(result)
    }
}
