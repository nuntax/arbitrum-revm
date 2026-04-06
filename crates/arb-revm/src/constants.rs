use revm::primitives::{Address, address};

/// ArbOS system actor address used for internal calls in Nitro.
pub const ARBOS_ACTS_ADDRESS: Address = address!("0x00000000000000000000000000000000000A4B05");

/// Root ArbOS state account used by Nitro.
pub const ARBOS_STATE_ADDRESS: Address = address!("0xA4B05FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF");

/// Sequencer batch poster sentinel account used by ArbOS pricing.
pub const BATCH_POSTER_ADDRESS: Address = address!("0xA4B000000000000000000073657175656e636572");

/// Address aliasing offset applied to retryable/L1-originated senders.
pub const ADDRESS_ALIAS_OFFSET_HEX: &str = "1111000000000000000000000000000000001111";
