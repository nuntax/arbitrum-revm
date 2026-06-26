//! Sequencer batch framing: the 40-byte timeBounds header, header-flag dispatch,
//! brotli decompression, and the RLP segment stream. Mirrors
//! `nitro/arbstate/inbox.go` (`ParseSequencerMessage`) and `daprovider/util.go`.

use alloy_rlp::Header;

/// Header-flag bytes (`nitro/daprovider/util.go`). The flag is the first byte of
/// the post-timeBounds payload.
pub mod flag {
    pub const BROTLI: u8 = 0x00;
    pub const DA_CERTIFICATE: u8 = 0x01;
    pub const ANYTRUST_TREE: u8 = 0x08;
    pub const ZEROHEAVY: u8 = 0x20;
    pub const L1_AUTHENTICATED: u8 = 0x40;
    pub const BLOB_HASHES: u8 = 0x50; // L1_AUTHENTICATED | 0x10
    pub const ANYTRUST: u8 = 0x80;
}

/// Batch segment kinds (`nitro/arbstate/inbox.go`).
pub mod segment_kind {
    pub const L2_MESSAGE: u8 = 0;
    pub const L2_MESSAGE_BROTLI: u8 = 1;
    pub const DELAYED_MESSAGES: u8 = 2;
    pub const ADVANCE_TIMESTAMP: u8 = 3;
    pub const ADVANCE_L1_BLOCK: u8 = 4;
}

/// Length of the big-endian timeBounds header prepended to every sequencer batch.
pub const BATCH_HEADER_LEN: usize = 40;

/// The 40-byte timeBounds header (5 × big-endian u64).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchHeader {
    pub min_timestamp: u64,
    pub max_timestamp: u64,
    pub min_l1_block: u64,
    pub max_l1_block: u64,
    pub after_delayed_messages: u64,
}

impl BatchHeader {
    /// Parse the 40-byte header; returns `(header, remaining_payload)`.
    pub fn parse(data: &[u8]) -> Result<(Self, &[u8]), BatchError> {
        if data.len() < BATCH_HEADER_LEN {
            return Err(BatchError::Truncated);
        }
        let r = |o: usize| u64::from_be_bytes(data[o..o + 8].try_into().unwrap());
        let header = Self {
            min_timestamp: r(0),
            max_timestamp: r(8),
            min_l1_block: r(16),
            max_l1_block: r(24),
            after_delayed_messages: r(32),
        };
        Ok((header, &data[BATCH_HEADER_LEN..]))
    }
}

/// Errors from batch framing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchError {
    /// Data shorter than the 40-byte header (or an empty payload).
    Truncated,
    /// Header flag not handled at this layer (blob/DA/zeroheavy resolved elsewhere).
    UnsupportedFlag(u8),
    /// Brotli decompression failed.
    Brotli,
    /// Malformed RLP in the segment stream.
    Rlp(&'static str),
}

/// Decompress a header-flagged payload into the raw RLP segment stream.
///
/// Handles the brotli (`0x00`) path. The blob (`0x50`), zeroheavy (`0x20`) and DA
/// flags are resolved upstream (blob hashes need external blob retrieval first);
/// after that resolution the recovered bytes re-enter here starting at their own
/// flag byte — which for mainnet sequencer batches is `0x00` brotli.
pub fn decompress_payload(payload: &[u8]) -> Result<Vec<u8>, BatchError> {
    let (&flag, body) = payload.split_first().ok_or(BatchError::Truncated)?;
    match flag {
        flag::BROTLI => {
            brotli::decompress(body, brotli::Dictionary::Empty).map_err(|_| BatchError::Brotli)
        }
        other => Err(BatchError::UnsupportedFlag(other)),
    }
}

/// A decoded batch segment: the `kind` byte plus its payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub kind: u8,
    pub data: Vec<u8>,
}

/// Parse a decompressed batch body — a stream of consecutive RLP byte strings,
/// each one segment whose first byte is the [`segment_kind`]. Matches Nitro's
/// `rlp.NewStream(...).Decode(&segment)` loop. Empty (zero-length) items are
/// skipped, as in Nitro.
pub fn parse_segments(mut buf: &[u8]) -> Result<Vec<Segment>, BatchError> {
    let mut segs = Vec::new();
    while !buf.is_empty() {
        let header = Header::decode(&mut buf).map_err(|_| BatchError::Rlp("segment header"))?;
        if header.list {
            return Err(BatchError::Rlp("segment is a list, expected string"));
        }
        if header.payload_length > buf.len() {
            return Err(BatchError::Rlp("segment length exceeds data"));
        }
        let (seg, rest) = buf.split_at(header.payload_length);
        buf = rest;
        if let Some((&kind, data)) = seg.split_first() {
            segs.push(Segment { kind, data: data.to_vec() });
        }
    }
    Ok(segs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rlp_string(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        Header { list: false, payload_length: data.len() }.encode(&mut out);
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn header_parses_big_endian_fields() {
        let mut data = Vec::new();
        for v in [1u64, 2, 3, 4, 5] {
            data.extend_from_slice(&v.to_be_bytes());
        }
        data.extend_from_slice(b"payload");
        let (h, rest) = BatchHeader::parse(&data).unwrap();
        assert_eq!(h.min_timestamp, 1);
        assert_eq!(h.max_timestamp, 2);
        assert_eq!(h.min_l1_block, 3);
        assert_eq!(h.max_l1_block, 4);
        assert_eq!(h.after_delayed_messages, 5);
        assert_eq!(rest, b"payload");
    }

    #[test]
    fn header_rejects_truncated() {
        assert_eq!(BatchHeader::parse(&[0u8; 39]), Err(BatchError::Truncated));
    }

    #[test]
    fn brotli_payload_roundtrips() {
        let segments_body = b"the quick brown fox jumps over the lazy dog, repeatedly.".repeat(10);
        let compressed =
            brotli::compress(&segments_body, 11, brotli::DEFAULT_WINDOW_SIZE, brotli::Dictionary::Empty)
                .unwrap();
        let mut payload = vec![flag::BROTLI];
        payload.extend_from_slice(&compressed);
        assert_eq!(decompress_payload(&payload).unwrap(), segments_body);
    }

    #[test]
    fn unsupported_flag_reported() {
        assert_eq!(
            decompress_payload(&[flag::ANYTRUST, 0xaa]),
            Err(BatchError::UnsupportedFlag(flag::ANYTRUST))
        );
    }

    #[test]
    fn segments_stream_parses_kinds_and_data() {
        // An AdvanceTimestamp segment (kind 3 + rlp(u64) delta) then an L2Message (kind 0 + body).
        let advance = {
            let mut s = vec![segment_kind::ADVANCE_TIMESTAMP];
            s.extend_from_slice(&rlp_string(&[]) ); // placeholder inner; multiplexer decodes data itself
            s
        };
        let l2 = {
            let mut s = vec![segment_kind::L2_MESSAGE];
            s.extend_from_slice(b"\x03some-l2-message-bytes");
            s
        };
        let mut stream = Vec::new();
        stream.extend_from_slice(&rlp_string(&advance));
        stream.extend_from_slice(&rlp_string(&l2));

        let segs = parse_segments(&stream).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].kind, segment_kind::ADVANCE_TIMESTAMP);
        assert_eq!(segs[1].kind, segment_kind::L2_MESSAGE);
        assert_eq!(segs[1].data, b"\x03some-l2-message-bytes");
    }

    #[test]
    fn segments_skip_empty_items() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&rlp_string(&[])); // empty -> skipped
        stream.extend_from_slice(&rlp_string(&[segment_kind::L2_MESSAGE, 0xff]));
        let segs = parse_segments(&stream).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].kind, segment_kind::L2_MESSAGE);
        assert_eq!(segs[0].data, vec![0xff]);
    }
}
