use revm::primitives::Bytes;

/// Top-level ArbOS metadata slots.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArbosMetadataOffset {
    Version = 0x00,
    UpgradeVersion = 0x01,
    UpgradeTimestamp = 0x02,
    NetworkFeeAccount = 0x03,
    ChainId = 0x04,
    GenesisBlockNumber = 0x05,
    InfraFeeAccount = 0x06,
    BrotliCompressionLevel = 0x07,
    NativeTokenEnabledFromTimestamp = 0x08,
    TransactionFilteringEnabledFromTimestamp = 0x09,
    FilteredFundsRecipient = 0x0a,
    CollectTips = 0x0b,
}

/// Top-level ArbOS subspaces.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subspace {
    L1Pricing = 0,
    L2Pricing = 1,
    Retryables = 2,
    AddressTable = 3,
    ChainOwners = 4,
    SendMerkle = 5,
    BlockHashes = 6,
    ChainConfig = 7,
    Programs = 8,
    Features = 9,
    NativeTokenOwners = 10,
    TransactionFilterers = 11,
}

impl Subspace {
    pub fn as_bytes(self) -> Bytes {
        Bytes::from(vec![self as u8])
    }
}

/// L1 pricing offsets inside the L1 pricing subspace.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum L1PricingOffset {
    PayRewardsTo = 0,
    EquilibrationUnits = 1,
    Inertia = 2,
    PerUnitReward = 3,
    LastUpdateTime = 4,
    FundsDueForRewards = 5,
    UnitsSince = 6,
    PricePerUnit = 7,
    LastSurplus = 8,
    PerBatchGasCost = 9,
    AmortizedCostCapBips = 10,
    L1FeesAvailable = 11,
    GasFloorPerToken = 12,
}

/// L2 pricing offsets inside the L2 pricing subspace.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum L2PricingOffset {
    SpeedLimitPerSecond = 0,
    PerBlockGasLimit = 1,
    BaseFeeWei = 2,
    MinBaseFeeWei = 3,
    GasBacklog = 4,
    PricingInertia = 5,
    BacklogTolerance = 6,
    PerTxGasLimit = 7,
}
