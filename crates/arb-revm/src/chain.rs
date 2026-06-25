use revm::primitives::U256;

/// Arbitrum chain-scoped execution context carried alongside block/tx/cfg.
///
/// This must stay minimal and should not duplicate values already present in
/// block env or transaction/message env.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArbChainContext {
    /// Sequencer feed sequence number for this message.
    pub sequence_number: Option<u64>,
    /// L1 block number for this L2 block. On Arbitrum the EVM `NUMBER` opcode
    /// (`block.number`) returns this, NOT the L2 block number — Nitro patches
    /// `opNumber` to read `ProcessingHook.L1BlockNumber` while keeping the L2
    /// number for chain rules. Block-scoped: set once when the block is built.
    pub l1_block_number: u64,
    /// Intrinsic gas cost for the current transaction (from validate()).
    /// Used in EndTxHook to reconstruct the full gasUsed seen by Nitro.
    pub intrinsic_gas: u64,
    /// L2-gas equivalent of the L1 poster cost for the current transaction.
    /// Set during GasChargingHook (pre_execution), consumed in EndTxHook (reward_beneficiary).
    pub poster_gas: u64,
    /// Gas units held back by the per-block/per-tx gas limit cap.
    /// Returned from pre_execution so it's charged but not available for compute.
    pub hold_gas: u64,
    /// Wei-denominated L1 poster fee for the current transaction.
    /// Set during GasChargingHook (pre_execution), consumed in EndTxHook (reward_beneficiary).
    pub poster_fee: U256,
    /// Snapshot of the tx gas price actually paid by the caller for this tx.
    /// Must remain stable for prepay/refund/reward hooks even if ArbOS config mutates mid-tx.
    pub paid_gas_price: u128,
    /// Stylus memory-page high-water tracking for the current tx (Nitro `statedb` StylusPages).
    /// `open` = pages currently active; `ever` = max ever active. Used by the Stylus memory
    /// model to price page growth across the tx's (possibly nested) Stylus calls.
    pub stylus_pages_open: u16,
    pub stylus_pages_ever: u16,
}

impl ArbChainContext {
    /// Creates a lean chain context.
    pub fn new(sequence_number: Option<u64>) -> Self {
        Self {
            sequence_number,
            l1_block_number: 0,
            intrinsic_gas: 0,
            poster_gas: 0,
            hold_gas: 0,
            poster_fee: U256::ZERO,
            paid_gas_price: 0,
            stylus_pages_open: 0,
            stylus_pages_ever: 0,
        }
    }

    /// Sets the L1 block number returned by the `NUMBER` opcode.
    pub fn with_l1_block_number(mut self, l1_block_number: u64) -> Self {
        self.l1_block_number = l1_block_number;
        self
    }

    /// Resets per-tx gas accounting state. Called at the start of each transaction.
    pub fn reset_poster_state(&mut self) {
        self.poster_gas = 0;
        self.hold_gas = 0;
        self.poster_fee = U256::ZERO;
        self.paid_gas_price = 0;
        self.stylus_pages_open = 0;
        self.stylus_pages_ever = 0;
    }

    /// Sets the sequence number.
    pub fn with_sequence_number(mut self, sequence_number: Option<u64>) -> Self {
        self.sequence_number = sequence_number;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::ArbChainContext;

    #[test]
    fn builds_chain_context_from_non_block_inputs() {
        let ctx = ArbChainContext::new(Some(42));
        assert_eq!(ctx.sequence_number, Some(42));
    }
}
