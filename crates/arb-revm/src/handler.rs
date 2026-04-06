use crate::api::exec::ArbContextTr;
use revm::{
    context_interface::result::HaltReason,
    handler::{
        EthFrame, EvmTr, FrameResult, Handler, MainnetHandler, evm::FrameTr, handler::EvmTrError,
    },
    interpreter::{interpreter::EthInterpreter, interpreter_action::FrameInit},
};

/// Arbitrum handler that composes the default mainnet handler.
#[derive(Debug, Clone)]
pub struct ArbHandler<EVM, ERROR, FRAME> {
    /// Mainnet behavior reused by default.
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

impl<EVM, ERROR, FRAME> Handler for ArbHandler<EVM, ERROR, FRAME>
where
    EVM: EvmTr<Context: ArbContextTr, Frame = FRAME>,
    ERROR: EvmTrError<EVM>,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    type Evm = EVM;
    type Error = ERROR;
    type HaltReason = HaltReason;
}

impl<EVM, ERROR> revm::inspector::InspectorHandler
    for ArbHandler<EVM, ERROR, EthFrame<EthInterpreter>>
where
    EVM: revm::inspector::InspectorEvmTr<
            Context: ArbContextTr,
            Frame = EthFrame<EthInterpreter>,
            Inspector: revm::Inspector<<<Self as Handler>::Evm as EvmTr>::Context, EthInterpreter>,
        >,
    ERROR: EvmTrError<EVM>,
{
    type IT = EthInterpreter;
}
