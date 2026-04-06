use revm::primitives::hardfork::SpecId;

/// Arbitrum EVM specification identifier.
///
/// This starts intentionally small and can be expanded as Nitro fork boundaries are
/// modeled in detail.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Default)]
#[allow(non_camel_case_types)]
pub enum ArbSpecId {
    /// Initial Nitro-compatible execution mode.
    #[default]
    NITRO = 100,
}

impl ArbSpecId {
    /// Converts this Arbitrum spec id into a base Ethereum spec id used by revm.
    pub const fn into_eth_spec(self) -> SpecId {
        match self {
            Self::NITRO => SpecId::PRAGUE,
        }
    }

    /// Returns true when `self` is enabled in `other`.
    pub const fn is_enabled_in(self, other: Self) -> bool {
        other as u8 <= self as u8
    }
}

impl From<ArbSpecId> for SpecId {
    fn from(spec: ArbSpecId) -> Self {
        spec.into_eth_spec()
    }
}
