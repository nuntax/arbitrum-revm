use revm::primitives::U256;

/// Arbitrum chain-scoped execution context carried alongside block/tx/cfg.
///
/// This is intentionally lean and message-scoped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArbChainContext {
    /// Parent-chain block number associated with this L2 message.
    pub l1_block_number: Option<u64>,
    /// Parent-chain base fee associated with this L2 message.
    pub l1_base_fee_wei: Option<U256>,
    /// Delayed inbox message count for this message.
    pub delayed_messages_read: Option<u64>,
    /// Sequencer feed sequence number for this message.
    pub sequence_number: Option<u64>,
}

impl ArbChainContext {
    /// Creates a context from message-scoped inputs.
    pub fn new(
        l1_block_number: Option<u64>,
        l1_base_fee_wei: Option<U256>,
        delayed_messages_read: Option<u64>,
        sequence_number: Option<u64>,
    ) -> Self {
        Self {
            l1_block_number,
            l1_base_fee_wei,
            delayed_messages_read,
            sequence_number,
        }
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
    use revm::primitives::U256;

    #[test]
    fn builds_chain_context_from_message_inputs() {
        let ctx = ArbChainContext::new(
            Some(123),
            Some(U256::from(50_000_000_000_u64)),
            Some(77),
            Some(42),
        );
        assert_eq!(ctx.l1_block_number, Some(123));
        assert_eq!(ctx.l1_base_fee_wei, Some(U256::from(50_000_000_000_u64)));
        assert_eq!(ctx.delayed_messages_read, Some(77));
        assert_eq!(ctx.sequence_number, Some(42));
    }
}
