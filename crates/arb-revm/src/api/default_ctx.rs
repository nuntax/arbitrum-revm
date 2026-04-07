use crate::{ArbChainContext, ArbSpecId, ArbTransaction};
use revm::{
    context::{BlockEnv, CfgEnv, TxEnv},
    database_interface::EmptyDB,
    Context, Journal, MainContext,
};

/// Default Arbitrum context type.
pub type ArbContext<DB> =
    Context<BlockEnv, ArbTransaction<TxEnv>, CfgEnv<ArbSpecId>, DB, Journal<DB>, ArbChainContext>;

/// Trait for creating a default Arbitrum context.
pub trait DefaultArb {
    /// Returns a default Arbitrum context.
    fn arb() -> ArbContext<EmptyDB>;

    /// Returns a default Arbitrum context with explicit message-scoped chain inputs.
    fn arb_with_chain_context(chain: ArbChainContext) -> ArbContext<EmptyDB>;
}

impl DefaultArb for ArbContext<EmptyDB> {
    fn arb() -> Self {
        Self::arb_with_chain_context(ArbChainContext::default())
    }

    fn arb_with_chain_context(chain: ArbChainContext) -> ArbContext<EmptyDB> {
        Context::mainnet()
            .with_tx(ArbTransaction::builder().build_fill())
            .with_cfg(CfgEnv::new_with_spec(ArbSpecId::NITRO))
            .with_chain(chain)
    }
}
