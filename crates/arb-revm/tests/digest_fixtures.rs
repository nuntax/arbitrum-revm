//! Hermetic `DigestMessage` round-trip tests (Stage E.3).
//!
//! Feeds real captured sequencer feed messages (`testdata/fixtures/*_message_only.json`) through
//! `executor::digest_message_envelope` and asserts the assembled [`ArbMessageEnvelope`], exercising
//! the relocated l2msg decoder (base64 -> `parse_message` -> typed `ArbTxEnvelope`) plus the
//! `L1Header` normalization, end to end, with no node or network.

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use arb_alloy_consensus::transactions::ArbTxEnvelope;
use arb_revm::executor::digest_message_envelope;
use arb_sequencer_network::sequencer::feed::BroadcastFeedMessage;
use revm::primitives::{Address, U256};

const CHAIN_ID: u64 = 42161;

fn feed_fixtures_dir() -> PathBuf {
    // testdata/ lives at the arb_revm workspace root, two levels up from this crate.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/fixtures")
}

fn load_feed_message(name: &str) -> BroadcastFeedMessage {
    let path = feed_fixtures_dir().join(name);
    let body = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("parse {path:?}: {e}"))
}

#[test]
fn digests_real_deposit_feed_message() {
    let feed_msg = load_feed_message("deposit_message_only.json");
    let env = digest_message_envelope(&feed_msg, CHAIN_ID, 0).expect("digest deposit message");

    // Header fields, taken straight from the fixture's L1 message header.
    assert_eq!(env.sequence_number, Some(707));
    assert_eq!(env.l1_block_number, 530);
    assert_eq!(env.l1_timestamp, 1_775_559_442);
    assert_eq!(env.l1_base_fee_wei, U256::from(7u64));
    assert_eq!(env.delayed_messages_read, 23);
    assert_eq!(
        env.poster,
        Address::from_str("0x502fae7d46d88f08fc2f8ed27fcb2ab183eb3e1f").unwrap()
    );

    // A kind=12 EthDeposit decodes to exactly one ArbitrumDepositTx. The l2Msg body is
    // 20-byte `to` ++ 32-byte value (Nitro `parseEthDepositMessage`).
    assert_eq!(env.txs.len(), 1, "deposit message yields one tx");
    match &env.txs[0] {
        ArbTxEnvelope::Deposit(dep) => {
            assert_eq!(
                dep.to,
                Address::from_str("0x3f1eae7d46d88f08fc2f8ed27fcb2ab183eb2d0e").unwrap()
            );
            assert_eq!(dep.value, U256::from(111_000_000_000_000_000u64));
            assert_eq!(
                dep.from,
                Address::from_str("0x502fae7d46d88f08fc2f8ed27fcb2ab183eb3e1f").unwrap(),
                "deposit From is the L1 message poster"
            );
        }
        other => panic!("expected a Deposit tx, got {other:?}"),
    }
}

#[test]
fn digests_real_submit_retryable_feed_message() {
    let feed_msg = load_feed_message("submit_retryable_message_only.json");
    let env = digest_message_envelope(&feed_msg, CHAIN_ID, 0).expect("digest retryable message");

    assert_eq!(env.sequence_number, Some(32_750));
    assert_eq!(env.l1_block_number, 16_673);
    assert_eq!(env.l1_timestamp, 1_775_580_307);
    assert_eq!(env.l1_base_fee_wei, U256::from(7u64));
    assert_eq!(env.delayed_messages_read, 528);

    // A kind=9 SubmitRetryable decodes to exactly one ArbitrumSubmitRetryableTx. Producing the
    // SubmitRetryable variant (without error) exercises the full base64 -> fixed-width decode path.
    assert_eq!(env.txs.len(), 1, "retryable message yields one tx");
    assert!(
        matches!(&env.txs[0], ArbTxEnvelope::SubmitRetryable(_)),
        "expected a SubmitRetryable tx, got {:?}",
        env.txs[0]
    );
}
