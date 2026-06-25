use revm::primitives::hardfork::SpecId;

/// Arbitrum execution specification, keyed by ArbOS version.
///
/// Each variant is a named ArbOS version from Nitro's `params/config_arbitrum.go`.
/// The variant discriminant **is** the ArbOS version number, so ordering and the
/// EVM-hardfork mapping fall out of the numeric value. This is the Arbitrum analogue
/// of `op_revm::OpSpecId`: it is the `Cfg::Spec` carried through execution and is the
/// single source of truth for "which ArbOS version's rules apply".
///
/// EVM-hardfork mapping (from Nitro `params/config.go` `IsShanghai`/`IsCancun`/`IsPrague`):
/// ArbOS 1–10 → London, 11+ → Shanghai, 20+ → Cancun, 40+ → Prague.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Default)]
#[allow(non_camel_case_types)]
pub enum ArbSpecId {
    /// Nitro genesis baseline (London-equivalent EVM rules).
    ARBOS_1 = 1,
    ARBOS_2 = 2,
    ARBOS_3 = 3,
    ARBOS_4 = 4,
    ARBOS_5 = 5,
    ARBOS_6 = 6,
    ARBOS_7 = 7,
    ARBOS_8 = 8,
    ARBOS_9 = 9,
    ARBOS_10 = 10,
    /// First Shanghai-era version; also `FixRedeemGas`.
    ARBOS_11 = 11,
    /// Cancun-era.
    ARBOS_20 = 20,
    /// Stylus.
    ARBOS_30 = 30,
    /// Stylus fixes.
    ARBOS_31 = 31,
    /// Stylus charging fixes.
    ARBOS_32 = 32,
    /// First Prague-era version.
    #[default]
    ARBOS_40 = 40,
    ARBOS_41 = 41,
    /// "Dia".
    ARBOS_50 = 50,
    ARBOS_51 = 51,
    ARBOS_60 = 60,
}

impl ArbSpecId {
    /// Current default Arbitrum spec. Kept as an alias for the previous single
    /// `NITRO` variant so existing call sites continue to compile.
    pub const NITRO: ArbSpecId = ArbSpecId::ARBOS_40;

    /// The numeric ArbOS version this spec represents.
    pub const fn arbos_version(self) -> u64 {
        self as u8 as u64
    }

    /// Converts this Arbitrum spec into the base Ethereum spec id used by revm,
    /// per Nitro's ArbOS-version → hardfork schedule.
    pub const fn into_eth_spec(self) -> SpecId {
        let v = self as u8;
        if v >= 40 {
            SpecId::PRAGUE
        } else if v >= 20 {
            SpecId::CANCUN
        } else if v >= 11 {
            SpecId::SHANGHAI
        } else {
            SpecId::LONDON
        }
    }

    /// Selects the spec for an arbitrary ArbOS version number (e.g. read from
    /// ArbOS state), clamping to the nearest defined version at or below `version`.
    pub const fn from_arbos_version(version: u64) -> Self {
        match version {
            0 | 1 => Self::ARBOS_1,
            2 => Self::ARBOS_2,
            3 => Self::ARBOS_3,
            4 => Self::ARBOS_4,
            5 => Self::ARBOS_5,
            6 => Self::ARBOS_6,
            7 => Self::ARBOS_7,
            8 => Self::ARBOS_8,
            9 => Self::ARBOS_9,
            10 => Self::ARBOS_10,
            11..=19 => Self::ARBOS_11,
            20..=29 => Self::ARBOS_20,
            30 => Self::ARBOS_30,
            31 => Self::ARBOS_31,
            32..=39 => Self::ARBOS_32,
            40 => Self::ARBOS_40,
            41..=49 => Self::ARBOS_41,
            50 => Self::ARBOS_50,
            51..=59 => Self::ARBOS_51,
            _ => Self::ARBOS_60,
        }
    }

    /// Returns true when `self` is at or after `other` (ordered by ArbOS version).
    pub const fn is_enabled_in(self, other: Self) -> bool {
        (self as u8) >= (other as u8)
    }
}

impl From<ArbSpecId> for SpecId {
    fn from(spec: ArbSpecId) -> Self {
        spec.into_eth_spec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbos_version_maps_to_correct_hardfork() {
        // Nitro params/config.go: 1-10 London, 11+ Shanghai, 20+ Cancun, 40+ Prague.
        assert_eq!(ArbSpecId::from_arbos_version(1).into_eth_spec(), SpecId::LONDON);
        assert_eq!(
            ArbSpecId::from_arbos_version(10).into_eth_spec(),
            SpecId::LONDON
        );
        assert_eq!(
            ArbSpecId::from_arbos_version(11).into_eth_spec(),
            SpecId::SHANGHAI
        );
        assert_eq!(
            ArbSpecId::from_arbos_version(19).into_eth_spec(),
            SpecId::SHANGHAI
        );
        assert_eq!(
            ArbSpecId::from_arbos_version(20).into_eth_spec(),
            SpecId::CANCUN
        );
        assert_eq!(
            ArbSpecId::from_arbos_version(32).into_eth_spec(),
            SpecId::CANCUN
        );
        assert_eq!(
            ArbSpecId::from_arbos_version(40).into_eth_spec(),
            SpecId::PRAGUE
        );
        assert_eq!(
            ArbSpecId::from_arbos_version(60).into_eth_spec(),
            SpecId::PRAGUE
        );
        // Unknown future versions clamp to the latest known.
        assert_eq!(
            ArbSpecId::from_arbos_version(999).into_eth_spec(),
            SpecId::PRAGUE
        );
    }

    #[test]
    fn version_roundtrips_and_orders() {
        assert_eq!(ArbSpecId::from_arbos_version(30).arbos_version(), 30);
        assert!(ArbSpecId::ARBOS_40.is_enabled_in(ArbSpecId::ARBOS_20));
        assert!(!ArbSpecId::ARBOS_11.is_enabled_in(ArbSpecId::ARBOS_40));
        assert_eq!(ArbSpecId::NITRO, ArbSpecId::ARBOS_40);
    }
}
