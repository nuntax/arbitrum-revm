//! `DigestMessage`: turn a sequencer feed message into executor input.
//!
//! Maps one Arbitrum sequencer [`BroadcastFeedMessage`] into the [`ArbMessageEnvelope`] /
//! [`ArbExecutionInput`] the executor consumes. One feed message produces one block; the parent
//! header is supplied by the caller from the node's chain tip (it is *not* carried in the message).
//! This is the message half of Nitro's block production — the L2 transaction decode is delegated to
//! [`arb_sequencer_network::reader::parse_message`] (verified against Nitro `arbos/parse_l2.go`).

use crate::executor::contract::{
    ArbExecCfg, ArbExecutionInput, ArbMessageEnvelope, ArbParentHeader,
};
use arb_sequencer_network::reader::parse_message;
use arb_sequencer_network::sequencer::feed::{BroadcastFeedMessage, L1Header};
use eyre::{Result, eyre};
use revm::primitives::U256;

/// Decode and assemble the message half of an execution input from a feed message.
///
/// `version` is the feed `Root.version`, which selects legacy vs. v2 batch-posting-report decoding.
/// `chain_id` is used for transaction decoding and should match `cfg.chain_id` at execution time.
pub fn digest_message_envelope(
    feed_msg: &BroadcastFeedMessage,
    chain_id: u64,
    version: u8,
) -> Result<ArbMessageEnvelope> {
    let meta = &feed_msg.message_with_meta_data;
    let l1_message = meta.l1_incoming_message.clone();
    let l1_header = L1Header::from_header(&l1_message.header, meta.delayed_messages_read)
        .map_err(|e| eyre!("invalid L1 header in sequencer message: {e}"))?;

    let txs = parse_message(l1_message, chain_id, version)?;
    let l1_base_fee_wei = l1_header.base_fee_l1.unwrap_or(U256::ZERO);

    Ok(ArbMessageEnvelope {
        sequence_number: Some(feed_msg.sequence_number),
        l1_block_number: l1_header.block_number,
        l1_timestamp: l1_header.timestamp,
        poster: l1_header.poster,
        l1_base_fee_wei,
        delayed_messages_read: l1_header.delayed_messages_read,
        txs,
    })
}

/// Digest a feed message into a full [`ArbExecutionInput`] against a known parent header.
///
/// The `parent` header comes from the node's chain tip; `cfg.chain_id` is used to decode the
/// message transactions.
pub fn digest_message(
    feed_msg: &BroadcastFeedMessage,
    parent: ArbParentHeader,
    cfg: ArbExecCfg,
    version: u8,
) -> Result<ArbExecutionInput> {
    let message = digest_message_envelope(feed_msg, cfg.chain_id, version)?;
    Ok(ArbExecutionInput::new(parent, message, cfg))
}
