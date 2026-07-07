use revm::primitives::{Address, address};

/// ArbOS system actor address used for internal calls in Nitro.
pub const ARBOS_ACTS_ADDRESS: Address = address!("0x00000000000000000000000000000000000A4B05");

/// Root ArbOS state account used by Nitro.
pub const ARBOS_STATE_ADDRESS: Address = address!("0xA4B05FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF");

/// Sequencer batch poster sentinel account used by ArbOS pricing.
pub const BATCH_POSTER_ADDRESS: Address = address!("0xA4B000000000000000000073657175656e636572");

/// ArbOS L1 pricer funds pool account (Nitro `types.L1PricerFundsPoolAddress`).
pub const L1_PRICER_FUNDS_POOL_ADDRESS: Address =
    address!("0xA4B00000000000000000000000000000000000f6");

/// ArbRetryableTx precompile address used for retryable submission/redeem flows.
pub const ARB_RETRYABLE_TX_ADDRESS: Address =
    address!("0x000000000000000000000000000000000000006e");

/// Transient-storage (EIP-1153) slot ArbOS uses to carry the current transaction's L1 poster fee
/// from the gas-charging handler to `ArbGasInfo.getCurrentTxL1GasFees`. This is NOT consensus state
/// (transient storage is per-tx and never enters the state root); the address/key are internal and
/// collision-free because no EVM bytecode executes at the ArbGasInfo precompile address, so nothing
/// else can TSTORE here. Mirrors Nitro returning `txProcessor.PosterFee` (a Go field), which the
/// node-path `EvmInternals` handle cannot otherwise expose to a precompile.
pub const CURRENT_TX_L1_FEE_ADDR: Address = address!("0x000000000000000000000000000000000000006C");

/// EIP-2935 history storage contract address.
pub const HISTORY_STORAGE_ADDRESS: Address = address!("0x0000F90827F1C53a10cb7A02335B175320002935");

/// ArbOS version that enables EIP-2935 parent hash processing in StartBlock.
pub const ARBOS_VERSION_EIP2935: u64 = 40;

/// Address aliasing offset applied to retryable/L1-originated senders.
pub const ADDRESS_ALIAS_OFFSET_HEX: &str = "1111000000000000000000000000000000001111";

/// Nitro typed transaction discriminator for ArbOS internal transactions.
pub const ARBITRUM_INTERNAL_TX_TYPE: u8 = 0x6a;

/// Nitro typed transaction discriminator for L1->L2 ETH deposit transactions.
pub const ARBITRUM_DEPOSIT_TX_TYPE: u8 = 0x64;

/// Nitro typed transaction discriminator for submit-retryable transactions.
pub const ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE: u8 = 0x69;

/// Nitro typed transaction discriminator for retry (redeem) transactions.
pub const ARBITRUM_RETRY_TX_TYPE: u8 = 0x68;
