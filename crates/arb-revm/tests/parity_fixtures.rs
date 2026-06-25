use std::cell::RefCell;

use arb_revm::{
    ArbExecCfg, ArbExecutionHooks, ArbExecutionInput, ArbExecutionMode, ArbMessageEnvelope,
    ArbParentHeader, ArbStartBlockDerived, ArbSystemCall, constants::ARBOS_ACTS_ADDRESS,
    execute_message_with_hooks,
};
use revm::{
    database::InMemoryDB,
    primitives::{Address, B256, U256, keccak256},
};

#[derive(Clone, Debug)]
struct Expected {
    writes: usize,
    derived: ArbStartBlockDerived,
}

#[derive(Clone, Debug)]
struct Fixture {
    name: &'static str,
    input: ArbExecutionInput,
    expected: Expected,
}

#[derive(Default)]
struct ProbeHooks {
    prelude_call: Option<ArbSystemCall>,
    seen_derived: RefCell<Vec<ArbStartBlockDerived>>,
}

impl ProbeHooks {
    fn with_prelude_call(prelude_call: ArbSystemCall) -> Self {
        Self {
            prelude_call: Some(prelude_call),
            seen_derived: RefCell::new(Vec::new()),
        }
    }
}

impl ArbExecutionHooks for ProbeHooks {
    fn start_block_prelude(
        &self,
        _input: &ArbExecutionInput,
        derived: ArbStartBlockDerived,
    ) -> Option<ArbSystemCall> {
        self.seen_derived.borrow_mut().push(derived);
        self.prelude_call.clone()
    }
}

fn make_fixture(
    mode: ArbExecutionMode,
    l1_timestamp: u64,
    expected_time_last_block: u64,
) -> Fixture {
    let parent = ArbParentHeader {
        number: 12,
        timestamp: 1_000,
        beneficiary: Address::ZERO,
        basefee: 0,
        gas_limit: 30_000_000,
        difficulty: U256::ZERO,
        prevrandao: Some(B256::ZERO),
    };
    let message = ArbMessageEnvelope {
        sequence_number: Some(99),
        l1_block_number: 5_000_000,
        l1_timestamp,
        poster: Address::ZERO,
        l1_base_fee_wei: U256::ZERO,
        delayed_messages_read: 77,
        txs: Vec::new(),
    };
    let cfg = ArbExecCfg::default();
    let input = ArbExecutionInput::new(parent, message, cfg).with_mode(mode);

    let writes = if mode.commits_state() { 2 } else { 0 };
    Fixture {
        name: match mode {
            ArbExecutionMode::Commit => "commit_mode_emits_start_block_write",
            ArbExecutionMode::Prefetch => "prefetch_mode_emits_no_writes",
            ArbExecutionMode::Sequencing => "sequencing_mode_commit_semantics",
        },
        input,
        expected: Expected {
            writes,
            derived: ArbStartBlockDerived {
                l2_block_number: 13,
                time_last_block: expected_time_last_block,
            },
        },
    }
}

#[test]
fn parity_fixture_commit_mode() {
    let fixture = make_fixture(ArbExecutionMode::Commit, 900, 0);
    let hooks = ProbeHooks::with_prelude_call(ArbSystemCall {
        caller: ARBOS_ACTS_ADDRESS,
        target: ARBOS_ACTS_ADDRESS,
        data: encode_start_block_calldata(
            fixture.input.message.l1_base_fee_wei,
            fixture.input.message.l1_block_number,
            fixture.expected.derived.l2_block_number,
            fixture.expected.derived.time_last_block,
        ),
    });
    run_fixture(&fixture, hooks);
}

#[test]
fn parity_fixture_prefetch_mode() {
    let fixture = make_fixture(ArbExecutionMode::Prefetch, 1_250, 250);
    let hooks = ProbeHooks::with_prelude_call(ArbSystemCall {
        caller: ARBOS_ACTS_ADDRESS,
        target: ARBOS_ACTS_ADDRESS,
        data: encode_start_block_calldata(
            fixture.input.message.l1_base_fee_wei,
            fixture.input.message.l1_block_number,
            fixture.expected.derived.l2_block_number,
            fixture.expected.derived.time_last_block,
        ),
    });
    run_fixture(&fixture, hooks);
}

fn encode_start_block_calldata(
    l1_base_fee: U256,
    l1_block_number: u64,
    l2_block_number: u64,
    time_last_block: u64,
) -> revm::primitives::Bytes {
    let selector_hash = keccak256(b"startBlock(uint256,uint64,uint64,uint64)");
    let mut out = Vec::with_capacity(4 + (32 * 4));
    out.extend_from_slice(&selector_hash[..4]);
    out.extend_from_slice(&l1_base_fee.to_be_bytes::<32>());

    let mut word = [0_u8; 32];
    word[24..].copy_from_slice(&l1_block_number.to_be_bytes());
    out.extend_from_slice(&word);

    word = [0_u8; 32];
    word[24..].copy_from_slice(&l2_block_number.to_be_bytes());
    out.extend_from_slice(&word);

    word = [0_u8; 32];
    word[24..].copy_from_slice(&time_last_block.to_be_bytes());
    out.extend_from_slice(&word);

    out.into()
}

fn run_fixture(fixture: &Fixture, hooks: ProbeHooks) {
    let mut db = InMemoryDB::default();
    let out = execute_message_with_hooks(&mut db, &fixture.input, &hooks)
        .unwrap_or_else(|err| panic!("fixture `{}` failed: {err:?}", fixture.name));

    assert_eq!(
        out.writes.len(),
        fixture.expected.writes,
        "fixture `{}` write count mismatch",
        fixture.name
    );

    let seen = hooks.seen_derived.borrow();
    assert_eq!(
        seen.len(),
        1,
        "fixture `{}` expected one prelude invocation",
        fixture.name
    );
    assert_eq!(
        seen[0], fixture.expected.derived,
        "fixture `{}` derived prelude values mismatch",
        fixture.name
    );
}
