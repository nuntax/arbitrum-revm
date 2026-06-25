//! Hermetic block-replay parity tests.
//!
//! Two layers:
//!  * `deposit_fixture_round_trips_and_replays` builds a fixture in code, sends it
//!    through `serde` and the public replay API, and asserts the engine reproduces
//!    the expected outcome — proving the harness end-to-end with no files or node.
//!  * `recorded_fixtures_replay_with_parity` scans `tests/fixtures/*.json` and
//!    replays every captured fixture. Drop a fixture produced by
//!    `replay_block --record <path>` into that directory and it becomes a
//!    deterministic regression test automatically.

use std::fs;
use std::path::{Path, PathBuf};

use arb_alloy_consensus::transactions::ArbTxEnvelope;
use arb_alloy_consensus::transactions::deposit::TxDeposit;
use arb_alloy_rpc_types::ArbTransaction as RpcArbTransaction;
use arb_revm::replay::{
    BlockFixture, ExpectedAccountState, ExpectedTx, PrestateFixture, REPLAY_FIXTURE_SCHEMA,
    ReplayFixture, replay_fixture,
};
use revm::primitives::{Address, B256, U256};
use serde_json::{Value, json};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// Wraps a consensus envelope into the RPC transaction shape the fixtures store,
/// the same shape a node returns from `eth_getBlockByNumber`.
fn rpc_tx_from_envelope(envelope: &ArbTxEnvelope, from: Address) -> RpcArbTransaction {
    let mut obj = serde_json::to_value(envelope)
        .expect("serialize envelope")
        .as_object()
        .expect("envelope serializes to an object")
        .clone();
    obj.insert("from".to_string(), json!(from));
    serde_json::from_value(Value::Object(obj)).expect("deserialize rpc tx")
}

/// A self-contained deposit fixture: a protocol deposit that mints value to an L2
/// recipient. Deposits carry their sender explicitly (no signature), so the fixture
/// is fully hand-constructible and deterministic.
fn build_deposit_fixture() -> ReplayFixture {
    let from = Address::with_last_byte(0x11);
    let to = Address::with_last_byte(0x22);
    let value = U256::from(1_000_000_000_000_000_u64);

    let deposit = TxDeposit {
        chain_id: U256::from(412_346_u64),
        request_id: B256::with_last_byte(0x01),
        from,
        to,
        value,
    };
    let envelope = ArbTxEnvelope::from(deposit);
    let tx_hash = envelope.hash();
    let rpc_tx = rpc_tx_from_envelope(&envelope, from);

    ReplayFixture {
        schema: REPLAY_FIXTURE_SCHEMA.to_string(),
        chain_id: 412_346,
        block: BlockFixture {
            number: 2,
            l1_block_number: 1,
            timestamp: 1_700_000_000,
            basefee: 0,
            gas_limit: 1_125_899_906_842_624,
            difficulty: U256::from(1_u64),
            beneficiary: Address::with_last_byte(0xa0),
            prevrandao: None,
        },
        prestate: PrestateFixture::default(),
        transactions: vec![rpc_tx],
        expected: vec![ExpectedTx {
            tx_hash,
            success: true,
            gas_used: 0,
            created_address: None,
            logs: Vec::new(),
        }],
        // The deposit mints `value` to the recipient — assert that post-state so the
        // fixture also exercises state-write parity.
        expected_state: vec![ExpectedAccountState {
            address: to,
            balance: Some(value),
            nonce: None,
            code_hash: None,
            storage: Vec::new(),
        }],
    }
}

#[test]
fn deposit_fixture_round_trips_and_replays() {
    let fixture = build_deposit_fixture();

    // Round-trip through serde so we exercise exactly what an on-disk fixture does.
    let json = serde_json::to_string_pretty(&fixture).expect("serialize fixture");
    let parsed: ReplayFixture = serde_json::from_str(&json).expect("deserialize fixture");

    let report = replay_fixture(&parsed);
    assert_eq!(report.executed, 1, "expected one tx to execute");
    assert!(
        report.is_parity(),
        "deposit fixture mismatches: {:#?}",
        report.mismatches
    );
}

/// Regenerates the committed example fixture. Run with
/// `cargo test -p arb-revm --test replay_fixtures emit_example_fixture -- --ignored`.
#[test]
#[ignore = "writes the committed example fixture; run on demand"]
fn emit_example_fixture() {
    let dir = fixtures_dir();
    fs::create_dir_all(&dir).expect("create fixtures dir");
    let fixture = build_deposit_fixture();
    let json = serde_json::to_string_pretty(&fixture).expect("serialize fixture");
    let path = dir.join("deposit_mint.json");
    fs::write(&path, json).expect("write fixture");
    eprintln!("wrote {path:?}");
}

#[test]
fn recorded_fixtures_replay_with_parity() {
    let dir = fixtures_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        eprintln!("no fixtures dir at {dir:?}; skipping captured-fixture replay");
        return;
    };

    let mut replayed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let body = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        let fixture: ReplayFixture =
            serde_json::from_str(&body).unwrap_or_else(|e| panic!("parse {path:?}: {e}"));
        assert_eq!(
            fixture.schema, REPLAY_FIXTURE_SCHEMA,
            "fixture {path:?} has unexpected schema {}",
            fixture.schema
        );

        let report = replay_fixture(&fixture);
        assert!(
            report.is_parity(),
            "fixture {path:?} mismatches: {:#?}",
            report.mismatches
        );
        replayed += 1;
    }

    eprintln!("replayed {replayed} captured fixture(s) with parity");
}
