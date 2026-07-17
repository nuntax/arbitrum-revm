use crate::{
    ArbSpecId, api::exec::ArbContextTr, chain::ArbChainContext, precompiles::ArbPrecompiles,
    storage::ArbosState,
};
use revm::{
    Database, Inspector,
    bytecode::opcode,
    context::{Cfg, ContextError, ContextSetters, Evm, FrameStack},
    context_interface::ContextTr,
    handler::{
        EthFrame, EvmTr, FrameInitOrResult, ItemOrResult, PrecompileProvider,
        evm::FrameTr,
        instructions::{EthInstructions, InstructionProvider},
    },
    inspector::{InspectorEvmTr, JournalExt},
    interpreter::{
        CallScheme, FrameInput, Host, Instruction, InstructionContext, InstructionExecResult,
        InstructionResult, InterpreterResult, interpreter::EthInterpreter,
    },
    primitives::{Address, B256, U256},
};

/// Arbitrum `NUMBER` opcode: returns the L1 block number, not the L2 block number.
///
/// Mirrors Nitro's patched `opNumber` (`go-ethereum/core/vm/instructions.go`), which
/// reads `ProcessingHook.L1BlockNumber` rather than `BlockContext.BlockNumber`. The
/// value is carried block-scoped in [`ArbChainContext::l1_block_number`].
fn arb_block_number<CTX>(ctx: InstructionContext<'_, CTX, EthInterpreter>) -> InstructionExecResult
where
    CTX: ContextTr<Chain = ArbChainContext> + Host,
{
    let l1_block_number = ctx.host.chain().l1_block_number;
    if !ctx.interpreter.stack.push(U256::from(l1_block_number)) {
        return Err(revm::interpreter::InstructionResult::StackOverflow);
    }
    Ok(())
}

/// Arbitrum `BLOCKHASH` opcode: returns the hash of an **L1** block from the ArbOS
/// block-hash ring buffer, not an L2 header hash.
///
/// Mirrors Nitro's patched `GetHashFn` (`go-ethereum`), which reads `state.blockhashes`
/// (the last 256 L1 block hashes recorded by `Blockhashes.RecordNewL1Block`). The stock
/// revm instruction compares the requested number against the **L2** block number and so
/// returns 0 for any L1 number, diverging the first time a tx reads `BLOCKHASH(l1Num)`. The internal
/// ArbOS-storage read is unmetered (matching geth's free `GetHashFn`); only the fixed
/// 20-gas table cost applies.
fn arb_block_hash<CTX>(ctx: InstructionContext<'_, CTX, EthInterpreter>) -> InstructionExecResult
where
    CTX: ContextTr<Chain = ArbChainContext> + Host,
{
    let Some(([], number)) = ctx.interpreter.stack.popn_top::<0>() else {
        return Err(InstructionResult::StackUnderflow);
    };
    let requested = u64::try_from(*number).unwrap_or(u64::MAX);
    let hash = ArbosState::open()
        .block_hashes
        .block_hash(requested, ctx.host.journal_mut())
        .unwrap_or(B256::ZERO);
    *number = U256::from_be_bytes(hash.0);
    Ok(())
}

/// Arbitrum EVM wrapper over revm's generic [`Evm`] type.
#[derive(Debug, Clone)]
pub struct ArbEvm<
    CTX,
    INSP,
    I = EthInstructions<EthInterpreter, CTX>,
    P = ArbPrecompiles,
    F = EthFrame<EthInterpreter>,
>(pub Evm<CTX, INSP, I, P, F>);

impl<CTX, INSP> ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, ArbPrecompiles>
where
    CTX: ContextTr<Cfg: Cfg<Spec: Into<ArbSpecId> + Clone>, Chain = ArbChainContext> + Host,
{
    /// Creates a new Arbitrum EVM.
    pub fn new(ctx: CTX, inspector: INSP) -> Self {
        let spec: ArbSpecId = ctx.cfg().spec().into();
        let mut instruction = EthInstructions::new_mainnet_with_spec(spec.into());
        // Arbitrum overrides NUMBER (returns the L1 block number) and BLOCKHASH (returns the
        // ArbOS-stored L1 block hash).
        instruction.insert_instruction(opcode::NUMBER, Instruction::new(arb_block_number::<CTX>), 2);
        instruction.insert_instruction(opcode::BLOCKHASH, Instruction::new(arb_block_hash::<CTX>), 20);
        Self(Evm {
            ctx,
            inspector,
            instruction,
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
    CTX: ArbContextTr<Journal: JournalExt> + ContextSetters,
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
    CTX: ArbContextTr,
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
        // Track per-address open call-frame spans for the Stylus `reentrant` flag (Nitro
        // `TxProcessor.PushContract`). Only count when a frame is actually pushed (`Item`);
        // fast-path results (precompiles, failed pre-checks) never open a geth contract frame.
        let span = span_address(&frame_input.frame_input);
        match self.0.frame_init(frame_input)? {
            ItemOrResult::Item(_) => {}
            ItemOrResult::Result(result) => return Ok(ItemOrResult::Result(result)),
        }
        if let Some(address) = span {
            *self
                .0
                .ctx
                .chain_mut()
                .stylus_program_spans
                .entry(address)
                .or_insert(0) += 1;
        }
        // The pushed frame is the top of the stack, which is exactly what the inner
        // `frame_init` returned for the `Item` case.
        Ok(ItemOrResult::Item(self.0.frame_stack.get()))
    }

    fn frame_run(
        &mut self,
    ) -> Result<
        FrameInitOrResult<Self::Frame>,
        ContextError<<<Self::Context as ContextTr>::Db as Database>::Error>,
    > {
        // Arbitrum: if the current frame runs a Stylus program (bytecode carries the Stylus
        // discriminant), execute it as WASM instead of dispatching to the EVM interpreter.
        #[cfg(feature = "stylus")]
        if self
            .0
            .frame_stack
            .get()
            .interpreter
            .bytecode
            .bytes()
            .starts_with(crate::stylus::constants::STYLUS_DISCRIMINANT)
        {
            if let Some(action) = self.frame_run_stylus() {
                let frame = self.0.frame_stack.get();
                let context = &mut self.0.ctx;
                return frame.process_next_action(context, action).inspect(|next| {
                    if next.is_result() {
                        frame.set_finished(true);
                    }
                });
            }
        }
        self.0.frame_run()
    }

    fn frame_return_result(
        &mut self,
        result: <Self::Frame as FrameTr>::FrameResult,
    ) -> Result<
        Option<<Self::Frame as FrameTr>::FrameResult>,
        ContextError<<<Self::Context as ContextTr>::Db as Database>::Error>,
    > {
        // The inner call pops the top frame iff it is finished (Nitro `PopContract`);
        // close its span first. Fast-path results arriving here belong to inits that never
        // pushed a frame - then the top frame is the still-running parent (not finished).
        let span = {
            let frame = self.0.frame_stack.get();
            if frame.is_finished() {
                span_address(&frame.input)
            } else {
                None
            }
        };
        if let Some(address) = span
            && let Some(count) = self.0.ctx.chain_mut().stylus_program_spans.get_mut(&address)
        {
            *count = count.saturating_sub(1);
        }
        self.0.frame_return_result(result)
    }
}

/// The acting address whose context span a new frame opens, per Nitro's `PushContract`:
/// non-delegate call frames act as their target address; DELEGATECALL/CALLCODE frames act as
/// the parent's already-open address and are not counted; create frames are exempt (see
/// [`ArbChainContext::stylus_program_spans`]).
fn span_address(input: &FrameInput) -> Option<Address> {
    match input {
        FrameInput::Call(call)
            if !matches!(
                call.scheme,
                CallScheme::DelegateCall | CallScheme::CallCode
            ) =>
        {
            Some(call.target_address)
        }
        _ => None,
    }
}
