use std::collections::VecDeque;

use revm::primitives::{Address, B256, HashMap, U256};

/// Arbitrum chain-scoped execution context carried alongside block/tx/cfg.
///
/// This must stay minimal and should not duplicate values already present in
/// block env or transaction/message env.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArbChainContext {
    /// Sequencer feed sequence number for this message.
    pub sequence_number: Option<u64>,
    /// L1 block number for this L2 block. On Arbitrum the EVM `NUMBER` opcode
    /// (`block.number`) returns this, NOT the L2 block number, Nitro patches
    /// `opNumber` to read `ProcessingHook.L1BlockNumber` while keeping the L2
    /// number for chain rules. Block-scoped: set once when the block is built.
    pub l1_block_number: u64,
    /// The block's real base fee, for when the block env no longer carries it (Nitro
    /// `BlockContext.BaseFeeInBlock`).
    ///
    /// geth lowers the block base fee to zero for a simulated call that names no fee, and reth
    /// does the same for `eth_call`/`debug_traceCall`. ArbOS must still price L1 calldata at the
    /// real fee, so Nitro keeps it here and `GasChargingHook` reads it in preference to `BaseFee`.
    ///
    /// Read only when the block env's base fee is zero, so a caller that sets this and then
    /// changes the block env's own base fee cannot silently price transactions off a stale value.
    /// Leave it `None` on the consensus path.
    pub base_fee_in_block: Option<u64>,
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
    /// ArbOS version read during normal-transaction validation. The current version cannot change
    /// during a regular transaction, so later gas, filter, and settlement hooks reuse this value.
    /// Protocol transactions leave it unset and use their existing state-read paths.
    pub arbos_version: Option<u64>,
    /// Stylus memory-page high-water tracking for the current tx (Nitro `statedb` StylusPages).
    /// `open` = pages currently active; `ever` = max ever active. Used by the Stylus memory
    /// model to price page growth across the tx's (possibly nested) Stylus calls.
    pub stylus_pages_open: u16,
    pub stylus_pages_ever: u16,
    /// Code hashes invoked by Stylus in this block, most-recent first. Nitro's `RecentWasms`
    /// cache is discarded at block end and makes repeat program calls use cached-init pricing
    /// from ArbOS 60 onward.
    recent_wasms: VecDeque<B256>,
    /// Gas refund accrued within a Stylus frame. This includes EVM sub-calls made through
    /// call/create hostios and direct `SetTrieSlots` writes. Nitro journals both in StateDB;
    /// revm's normal frame return only propagates the former when it owns the sub-frame, so the
    /// Stylus dispatcher carries the total onto the frame result. Save/restore around each frame
    /// keeps nested calls from double-counting.
    pub stylus_refund: i64,
    /// Open non-delegate call-frame count per acting address for the current tx (Nitro
    /// `TxProcessor.Programs`, maintained by `PushContract`/`PopContract`). A Stylus program
    /// entered while its acting address already has an open span (count > 1 including its own
    /// frame) runs with `EvmData.reentrant` set; programs branch on this (e.g. flash-loan
    /// callbacks that require being re-entered). DELEGATECALL/CALLCODE frames act as the
    /// parent's address, whose span is already open, and are not counted; create frames are
    /// not counted either (EIP-3541 makes a created address never a Stylus program mid-tx and
    /// address collisions with live code fail before a frame opens).
    pub stylus_program_spans: HashMap<Address, u32>,
    /// A normal transaction registered in ArbOS's transaction filter. It skips EVM execution
    /// after gas charging and consumes its full gas limit.
    pub filtered_tx: bool,
    /// Ticket ids of pre-Stylus (`ArbOS < 30`) zero-callvalue retryables *submitted in this
    /// block*. Nitro's `util.TransferBalance` resurrects the escrow (destructed by the submit)
    /// as a present-but-empty "zombie" only when a same-block redeem takes the
    /// `CreateZombieIfDeleted` branch AND succeeds; a failed (e.g. OOG) redeem, or a redeem in a
    /// later block, leaves the escrow ABSENT. The submit hook records the ticket here and the
    /// redeem hook materializes the escrow only on success; cleared each `StartBlock` so that
    /// later-block (manual) redeems, which see no same-block destruct, never resurrect it.
    pub pending_zombie_escrow_tickets: Vec<B256>,
}

impl ArbChainContext {
    /// Creates a lean chain context.
    pub fn new(sequence_number: Option<u64>) -> Self {
        Self {
            sequence_number,
            l1_block_number: 0,
            base_fee_in_block: None,
            intrinsic_gas: 0,
            poster_gas: 0,
            hold_gas: 0,
            poster_fee: U256::ZERO,
            paid_gas_price: 0,
            arbos_version: None,
            stylus_pages_open: 0,
            stylus_pages_ever: 0,
            recent_wasms: VecDeque::new(),
            stylus_refund: 0,
            stylus_program_spans: HashMap::default(),
            filtered_tx: false,
            pending_zombie_escrow_tickets: Vec::new(),
        }
    }

    /// Sets the L1 block number returned by the `NUMBER` opcode.
    pub fn with_l1_block_number(mut self, l1_block_number: u64) -> Self {
        self.l1_block_number = l1_block_number;
        self
    }

    /// Sets the block's real base fee for L1 pricing, for callers whose block env has had it
    /// lowered to zero (see [`ArbChainContext::base_fee_in_block`]).
    pub fn with_base_fee_in_block(mut self, base_fee_in_block: u64) -> Self {
        self.base_fee_in_block = Some(base_fee_in_block);
        self
    }

    /// Inserts a Stylus code hash in Nitro's block-scoped LRU and returns whether it was already
    /// present. Nitro normalizes a zero capacity to one entry.
    pub fn insert_recent_wasm(&mut self, code_hash: B256, capacity: u16) -> bool {
        let capacity = usize::from(capacity.max(1));

        if let Some(index) = self.recent_wasms.iter().position(|hash| *hash == code_hash) {
            self.recent_wasms.remove(index);
            self.recent_wasms.push_front(code_hash);
            return true;
        }

        if self.recent_wasms.len() >= capacity {
            self.recent_wasms.pop_back();
        }
        self.recent_wasms.push_front(code_hash);
        false
    }

    /// Resets per-tx gas accounting state. Called at the start of each transaction.
    pub fn reset_poster_state(&mut self) {
        self.poster_gas = 0;
        self.hold_gas = 0;
        self.poster_fee = U256::ZERO;
        self.paid_gas_price = 0;
        self.stylus_pages_open = 0;
        self.stylus_pages_ever = 0;
        self.stylus_refund = 0;
        self.stylus_program_spans.clear();
        self.filtered_tx = false;
    }

    /// Sets the sequence number.
    pub fn with_sequence_number(mut self, sequence_number: Option<u64>) -> Self {
        self.sequence_number = sequence_number;
        self
    }
}

#[cfg(test)]
mod tests {
    use revm::primitives::B256;

    use super::ArbChainContext;

    #[test]
    fn builds_chain_context_from_non_block_inputs() {
        let ctx = ArbChainContext::new(Some(42));
        assert_eq!(ctx.sequence_number, Some(42));
    }

    #[test]
    fn recent_wasms_uses_block_scoped_lru_semantics() {
        let a = B256::with_last_byte(1);
        let b = B256::with_last_byte(2);
        let c = B256::with_last_byte(3);
        let mut ctx = ArbChainContext::new(None);

        assert!(!ctx.insert_recent_wasm(a, 2));
        assert!(!ctx.insert_recent_wasm(b, 2));
        assert!(ctx.insert_recent_wasm(a, 2));
        assert!(!ctx.insert_recent_wasm(c, 2));
        assert!(ctx.insert_recent_wasm(a, 2));
        assert!(!ctx.insert_recent_wasm(b, 2));

        let mut zero_capacity = ArbChainContext::new(None);
        assert!(!zero_capacity.insert_recent_wasm(a, 0));
        assert!(zero_capacity.insert_recent_wasm(a, 0));
        assert!(!zero_capacity.insert_recent_wasm(b, 0));
        assert!(!zero_capacity.insert_recent_wasm(a, 0));

        let mut next_block = ArbChainContext::new(None);
        assert!(!next_block.insert_recent_wasm(a, 2));
    }
}
