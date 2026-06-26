//! Batch multiplexer: walk a decoded batch's segments and emit the sequence of
//! [`DerivedMessage`]s. Mirrors `nitro/arbstate/inbox.go` `inboxMultiplexer`.
//!
//! Milestone 1 = the sequencer path (kinds L2Message / L2MessageBrotli /
//! AdvanceTimestamp / AdvanceL1BlockNumber). A `DelayedMessages` segment needs
//! external delayed-inbox data and returns [`MultiplexerError::DelayedUnsupported`]
//! for now (milestone 2).

use alloy_primitives::U256;
use alloy_rlp::Decodable;

use crate::batch::{segment_kind, BatchHeader, Segment};
use crate::message::{
    DerivedMessage, L1IncomingMessageHeader, BATCH_POSTER_ADDRESS, KIND_L2_MESSAGE,
};

/// Nitro `arbostypes.MaxL2MessageSize`.
const MAX_L2_MESSAGE_SIZE: usize = 262_144;

/// Errors from [`extract_messages`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiplexerError {
    /// An Advance* segment carried a malformed RLP u64 delta.
    AdvanceDelta(&'static str),
    /// An L2MessageBrotli segment failed to decompress.
    Brotli,
    /// An over-large L2MessageBrotli payload (exceeds `MaxL2MessageSize`).
    OversizeL2Message,
    /// A DelayedMessages segment was encountered (needs the delayed inbox — milestone 2).
    DelayedUnsupported,
}

#[inline]
fn clamp(v: u64, lo: u64, hi: u64) -> u64 {
    v.max(lo).min(hi)
}

/// Decode a batch's segments into its sequencer messages.
///
/// `before_delayed_count` is this batch's starting delayed-message cursor (the
/// previous batch's `afterDelayedMessages`, from the L1 event). The running
/// timestamp / L1-block start at the header minimums and advance by the Advance*
/// segment deltas, clamped to `[min, max]` on each emitted message.
pub fn extract_messages(
    header: &BatchHeader,
    segments: &[Segment],
    before_delayed_count: u64,
) -> Result<Vec<DerivedMessage>, MultiplexerError> {
    let mut timestamp = header.min_timestamp;
    let mut block = header.min_l1_block;
    let delayed = before_delayed_count;
    let mut out = Vec::new();

    for seg in segments {
        match seg.kind {
            segment_kind::ADVANCE_TIMESTAMP => {
                let delta = decode_u64(&seg.data).map_err(|_| MultiplexerError::AdvanceDelta("timestamp"))?;
                timestamp = timestamp.saturating_add(delta);
            }
            segment_kind::ADVANCE_L1_BLOCK => {
                let delta = decode_u64(&seg.data).map_err(|_| MultiplexerError::AdvanceDelta("l1block"))?;
                block = block.saturating_add(delta);
            }
            segment_kind::L2_MESSAGE => {
                out.push(make_l2_message(header, seg.data.clone(), timestamp, block, delayed));
            }
            segment_kind::L2_MESSAGE_BROTLI => {
                let l2 = brotli::decompress(&seg.data, brotli::Dictionary::Empty)
                    .map_err(|_| MultiplexerError::Brotli)?;
                if l2.len() > MAX_L2_MESSAGE_SIZE {
                    return Err(MultiplexerError::OversizeL2Message);
                }
                out.push(make_l2_message(header, l2, timestamp, block, delayed));
            }
            segment_kind::DELAYED_MESSAGES => return Err(MultiplexerError::DelayedUnsupported),
            // Unknown segment kinds are skipped, matching Nitro.
            _ => {}
        }
    }
    Ok(out)
}

fn decode_u64(mut data: &[u8]) -> Result<u64, alloy_rlp::Error> {
    u64::decode(&mut data)
}

fn make_l2_message(
    header: &BatchHeader,
    l2_msg: Vec<u8>,
    timestamp: u64,
    block: u64,
    delayed: u64,
) -> DerivedMessage {
    DerivedMessage {
        header: L1IncomingMessageHeader {
            kind: KIND_L2_MESSAGE,
            poster: BATCH_POSTER_ADDRESS,
            block_number: clamp(block, header.min_l1_block, header.max_l1_block),
            timestamp: clamp(timestamp, header.min_timestamp, header.max_timestamp),
            request_id: None,
            l1_base_fee: U256::ZERO,
        },
        l2_msg,
        delayed_messages_read: delayed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> BatchHeader {
        BatchHeader {
            min_timestamp: 1_000,
            max_timestamp: 2_000,
            min_l1_block: 100,
            max_l1_block: 200,
            after_delayed_messages: 7,
        }
    }

    fn advance(kind: u8, delta: u64) -> Segment {
        Segment { kind, data: alloy_rlp::encode(delta) }
    }

    #[test]
    fn emits_l2_messages_with_running_timestamp_and_poster() {
        let h = header();
        let segs = vec![
            advance(segment_kind::ADVANCE_TIMESTAMP, 250),
            Segment { kind: segment_kind::L2_MESSAGE, data: b"tx-a".to_vec() },
            advance(segment_kind::ADVANCE_L1_BLOCK, 5),
            Segment { kind: segment_kind::L2_MESSAGE, data: b"tx-b".to_vec() },
        ];
        let msgs = extract_messages(&h, &segs, 7).unwrap();
        assert_eq!(msgs.len(), 2);

        assert_eq!(msgs[0].header.poster, BATCH_POSTER_ADDRESS);
        assert_eq!(msgs[0].header.kind, KIND_L2_MESSAGE);
        assert_eq!(msgs[0].header.timestamp, 1_250); // 1000 + 250, within [1000,2000]
        assert_eq!(msgs[0].header.block_number, 100); // no advance yet
        assert_eq!(msgs[0].l2_msg, b"tx-a");
        assert_eq!(msgs[0].delayed_messages_read, 7);
        assert!(msgs[0].header.request_id.is_none());

        assert_eq!(msgs[1].header.timestamp, 1_250);
        assert_eq!(msgs[1].header.block_number, 105); // 100 + 5
        assert_eq!(msgs[1].l2_msg, b"tx-b");
    }

    #[test]
    fn timestamp_clamped_to_max() {
        let h = header();
        let segs = vec![
            advance(segment_kind::ADVANCE_TIMESTAMP, 10_000), // overshoots max
            Segment { kind: segment_kind::L2_MESSAGE, data: b"x".to_vec() },
        ];
        let msgs = extract_messages(&h, &segs, 0).unwrap();
        assert_eq!(msgs[0].header.timestamp, 2_000); // clamped to max_timestamp
    }

    #[test]
    fn l2_message_brotli_is_decompressed() {
        let h = header();
        let inner = b"decompressed-l2-message".to_vec();
        let compressed =
            brotli::compress(&inner, 11, brotli::DEFAULT_WINDOW_SIZE, brotli::Dictionary::Empty).unwrap();
        let segs = vec![Segment { kind: segment_kind::L2_MESSAGE_BROTLI, data: compressed }];
        let msgs = extract_messages(&h, &segs, 0).unwrap();
        assert_eq!(msgs[0].l2_msg, inner);
    }

    #[test]
    fn delayed_segment_is_unsupported_for_now() {
        let h = header();
        let segs = vec![Segment { kind: segment_kind::DELAYED_MESSAGES, data: vec![] }];
        assert_eq!(extract_messages(&h, &segs, 0), Err(MultiplexerError::DelayedUnsupported));
    }
}
