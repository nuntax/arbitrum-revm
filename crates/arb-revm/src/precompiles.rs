use crate::ArbSpecId;
use revm::{
    context::Cfg,
    context_interface::ContextTr,
    handler::{EthPrecompiles, PrecompileProvider},
    interpreter::InterpreterResult,
    precompile::{PrecompileSpecId, Precompiles},
    primitives::{Address, address},
};

/// ArbSys precompile address.
pub const ARBSYS_ADDRESS: Address = address!("0x0000000000000000000000000000000000000064");

/// ArbGasInfo precompile address.
pub const ARBGASINFO_ADDRESS: Address = address!("0x000000000000000000000000000000000000006c");

/// Arbitrum precompile provider.
///
/// The initial implementation delegates to the standard Ethereum precompile set
/// and adds chain-specific precompiles incrementally.
#[derive(Debug, Clone)]
pub struct ArbPrecompiles {
    /// Inner Ethereum precompile provider.
    pub inner: EthPrecompiles,
}

impl ArbPrecompiles {
    /// Creates a provider for a given Arbitrum spec.
    pub fn new_with_spec(spec: ArbSpecId) -> Self {
        Self {
            inner: EthPrecompiles::new(spec.into()),
        }
    }
}

impl Default for ArbPrecompiles {
    fn default() -> Self {
        Self::new_with_spec(ArbSpecId::default())
    }
}

impl<CTX> PrecompileProvider<CTX> for ArbPrecompiles
where
    CTX: ContextTr<Cfg: Cfg<Spec = ArbSpecId>>,
{
    type Output = InterpreterResult;

    fn set_spec(&mut self, spec: <CTX::Cfg as Cfg>::Spec) -> bool {
        let eth_spec = spec.into();
        if eth_spec == self.inner.spec {
            return false;
        }

        self.inner.precompiles = Precompiles::new(PrecompileSpecId::from_spec_id(eth_spec));
        self.inner.spec = eth_spec;
        true
    }

    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &revm::interpreter::CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        self.inner.run(context, inputs)
    }

    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        self.inner.warm_addresses()
    }

    fn contains(&self, address: &Address) -> bool {
        self.inner.contains(address)
    }
}
