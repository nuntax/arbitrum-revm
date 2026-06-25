use crate::{
    ArbSpecId, chain::ArbChainContext, evm::ArbEvm, precompiles::ArbPrecompiles,
    transaction::ArbTxTr,
};
use revm::{
    Context, Database,
    context::Cfg,
    context_interface::{Block, JournalTr},
    handler::instructions::EthInstructions,
    interpreter::interpreter::EthInterpreter,
    state::EvmState,
};

/// Default Arbitrum EVM type.
pub type DefaultArbEvm<CTX, INSP = ()> =
    ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, ArbPrecompiles>;

/// Builder trait for creating Arbitrum EVMs from contexts.
pub trait ArbBuilder: Sized {
    /// Context type used by the EVM.
    type Context;

    /// Builds an Arbitrum EVM without an inspector.
    fn build_arb(self) -> DefaultArbEvm<Self::Context>;

    /// Builds an Arbitrum EVM with an inspector.
    fn build_arb_with_inspector<INSP>(self, inspector: INSP) -> DefaultArbEvm<Self::Context, INSP>;
}

impl<BLOCK, TX, CFG, DB, JOURNAL> ArbBuilder
    for Context<BLOCK, TX, CFG, DB, JOURNAL, ArbChainContext>
where
    BLOCK: Block,
    TX: ArbTxTr,
    CFG: Cfg<Spec = ArbSpecId>,
    DB: Database,
    JOURNAL: JournalTr<Database = DB, State = EvmState>,
{
    type Context = Self;

    fn build_arb(self) -> DefaultArbEvm<Self::Context> {
        ArbEvm::new(self, ())
    }

    fn build_arb_with_inspector<INSP>(self, inspector: INSP) -> DefaultArbEvm<Self::Context, INSP> {
        ArbEvm::new(self, inspector)
    }
}
