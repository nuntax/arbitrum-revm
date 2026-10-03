//! Differential check of every ArbOS precompile method against Nitro.
//!
//! A fixture holds, for one pinned block of a live Nitro chain, a list of single calls to ArbOS
//! precompiles. Each call carries the exact prestate Nitro touched (geth's `prestateTracer`) and
//! Nitro's outcome (`callTracer`): success, return data, and gas used. The test seeds an in-memory
//! database with that prestate, runs the call as an ordinary zero-priced transaction, and
//! requires the same success flag, byte-identical return data, and identical gas.
//!
//! Fixtures are produced from a live node and are not checked in; point
//! `ARB_PRECOMPILE_PARITY_DIR` at a directory of `*.json` fixtures and run
//! `cargo test -p arb-revm precompile_parity -- --ignored --nocapture`.

use std::collections::BTreeMap;

use revm::{
    ExecuteEvm,
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::result::ExecutionResult,
    database::CacheDB,
    primitives::{Address, B256, Bytes, KECCAK_EMPTY, TxKind, U256, keccak256},
    state::{AccountInfo, Bytecode},
};
use serde::Deserialize;

use crate::{
    ArbBuilder, ArbChainContext, ArbContext, ArbSpecId, ArbTransaction, ArbosState, DefaultArb,
};

#[derive(Deserialize)]
struct Fixture {
    chain_id: u64,
    block: Block,
    calls: Vec<Call>,
}

#[derive(Deserialize)]
struct Block {
    number: u64,
    timestamp: u64,
    l1_block_number: u64,
    /// The block's real base fee. Calls are zero-priced, so, as in Nitro, the EVM's base fee is
    /// zero (and no L1 poster gas is charged) while precompiles still see this as
    /// `BaseFeeInBlock`.
    basefee_in_block: u64,
    coinbase: Address,
}

#[derive(Deserialize)]
struct Call {
    name: String,
    /// A plain account, or a live chain owner for the `@owner` cases that go through Nitro's
    /// `OwnerPrecompile`.
    caller: Address,
    to: Address,
    /// Logs Nitro emitted (`callTracer` with `withLog`).
    logs: Vec<ExpectedLog>,
    /// Why the recorded outcome is RPC behaviour that block execution never reaches, if it is.
    #[serde(default)]
    skip: Option<String>,
    input: Bytes,
    gas_limit: u64,
    prestate: BTreeMap<Address, PreAccount>,
    expected: Expected,
}

#[derive(Deserialize, PartialEq, Eq, Debug)]
struct ExpectedLog {
    address: Address,
    topics: Vec<B256>,
    data: Bytes,
}

#[derive(Deserialize)]
struct PreAccount {
    #[serde(default)]
    balance: Option<U256>,
    #[serde(default)]
    nonce: Option<u64>,
    #[serde(default)]
    code: Option<Bytes>,
    #[serde(default)]
    storage: BTreeMap<B256, B256>,
}

#[derive(Deserialize)]
struct Expected {
    success: bool,
    /// `None` where Nitro's answer depends on RPC estimation rather than consensus.
    output: Option<Bytes>,
    /// Gas used beyond the intrinsic cost.
    gas_used: u64,
}

/// Intrinsic gas of a plain call carrying `data` (no access list).
fn intrinsic(data: &[u8]) -> u64 {
    let nonzero = data.iter().filter(|b| **b != 0).count() as u64;
    21_000 + 16 * nonzero + 4 * (data.len() as u64 - nonzero)
}

fn run(fixture: &Fixture, call: &Call) -> Result<(), String> {
    let (success, output, gas, logs) = execute(fixture, call)?;
    let gas_used = gas - intrinsic(&call.input);

    let mut diffs = Vec::new();
    if success != call.expected.success {
        diffs.push(format!(
            "success {success}, Nitro {}",
            call.expected.success
        ));
    }
    if let Some(expected) = &call.expected.output
        && output != *expected
    {
        diffs.push(format!("output {output}, Nitro {expected}"));
    }
    if logs != call.logs {
        diffs.push(format!("logs {logs:?}, Nitro {:?}", call.logs));
    }
    if gas_used != call.expected.gas_used {
        diffs.push(format!(
            "gas {gas_used}, Nitro {} ({:+})",
            call.expected.gas_used,
            gas_used as i64 - call.expected.gas_used as i64
        ));
    }
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(diffs.join("; "))
    }
}

/// Runs `call` on a fresh copy of its prestate: success, output, gas used, logs.
fn execute(fixture: &Fixture, call: &Call) -> Result<(bool, Bytes, u64, Vec<ExpectedLog>), String> {
    let mut db = CacheDB::new(revm::database::EmptyDB::default());
    for (address, account) in &call.prestate {
        // The chain owner is a contract. Nitro's simulated call does not apply EIP-3607, and the
        // sender's code never runs here, so it is seeded without code rather than rejected.
        let code = account
            .code
            .as_ref()
            .filter(|c| !c.is_empty() && *address != call.caller)
            .map(|c| Bytecode::new_raw(c.clone()));
        let code_hash = code
            .as_ref()
            .map_or(KECCAK_EMPTY, |c| keccak256(c.original_bytes()));
        db.insert_account_info(
            *address,
            AccountInfo {
                balance: account.balance.unwrap_or_default(),
                nonce: account.nonce.unwrap_or_default(),
                code_hash,
                code,
                ..AccountInfo::default()
            },
        );
        for (slot, value) in &account.storage {
            db.insert_account_storage(*address, (*slot).into(), (*value).into())
                .map_err(|e| format!("seed storage: {e:?}"))?;
        }
    }

    let spec = ArbSpecId::from_arbos_version(ArbosState::read_effective_version(
        &db,
        fixture.block.timestamp,
    ));
    let mut cfg = CfgEnv::new_with_spec(spec)
        .with_chain_id(fixture.chain_id)
        .with_disable_priority_fee_check(true);
    cfg.disable_balance_check = true;
    cfg.disable_eip7623 = !ArbosState::open()
        .features
        .read_calldata_price_increase_db(&mut db);
    cfg.disable_eip3541 = spec.is_enabled_in(ArbSpecId::ARBOS_30);
    cfg.tx_gas_limit_cap = Some(u64::MAX);
    let block = BlockEnv {
        number: U256::from(fixture.block.number),
        timestamp: U256::from(fixture.block.timestamp),
        basefee: 0,
        beneficiary: fixture.block.coinbase,
        gas_limit: u64::MAX,
        ..BlockEnv::default()
    };
    let chain = ArbChainContext::new(None)
        .with_l1_block_number(fixture.block.l1_block_number)
        .with_base_fee_in_block(fixture.block.basefee_in_block);
    let mut evm = ArbContext::arb_with_chain_context(chain)
        .with_db(&mut db)
        .with_cfg(cfg)
        .with_block(block)
        .with_tx(ArbTransaction::<TxEnv>::default())
        .build_arb();

    let nonce = call
        .prestate
        .get(&call.caller)
        .and_then(|a| a.nonce)
        .unwrap_or_default();
    let tx = TxEnv {
        caller: call.caller,
        kind: TxKind::Call(call.to),
        data: call.input.clone(),
        gas_limit: call.gas_limit + intrinsic(&call.input),
        gas_price: 0,
        nonce,
        chain_id: Some(fixture.chain_id),
        ..TxEnv::default()
    };
    let outcome = evm
        .transact(ArbTransaction::new(tx))
        .map_err(|e| format!("execution error: {e:?}"))?;
    let (success, output) = match &outcome.result {
        ExecutionResult::Success { output, .. } => (true, output.data().clone()),
        ExecutionResult::Revert { output, .. } => (false, output.clone()),
        ExecutionResult::Halt { .. } => (false, Bytes::new()),
    };
    let logs = outcome
        .result
        .logs()
        .iter()
        .map(|log| ExpectedLog {
            address: log.address,
            topics: log.topics().to_vec(),
            data: log.data.data.clone(),
        })
        .collect();
    Ok((success, output, outcome.result.tx_gas_used(), logs))
}

#[test]
#[ignore = "needs fixtures recorded from a live Nitro node; see the module docs"]
fn precompile_parity() {
    let dir = std::env::var("ARB_PRECOMPILE_PARITY_DIR")
        .expect("set ARB_PRECOMPILE_PARITY_DIR to a directory of precompile parity fixtures");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .expect("fixture directory")
        .map(|e| e.expect("directory entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no fixtures in {dir}");

    let mut failures = Vec::new();
    let mut skipped = Vec::new();
    let mut total = 0;
    for path in paths {
        let fixture: Fixture = serde_json::from_slice(&std::fs::read(&path).expect("read fixture"))
            .expect("parse fixture");
        for call in &fixture.calls {
            if let Some(reason) = &call.skip {
                skipped.push(format!("{}: {reason}", call.name));
                continue;
            }
            total += 1;
            if let Err(diff) = run(&fixture, call) {
                failures.push(format!("{}: {diff}", call.name));
            }
        }
    }
    for failure in &failures {
        println!("MISMATCH {failure}");
    }
    for skip in &skipped {
        println!("SKIPPED {skip}");
    }
    println!(
        "{} of {total} calls match Nitro ({} skipped)",
        total - failures.len(),
        skipped.len()
    );
    assert!(failures.is_empty(), "{} mismatches", failures.len());
}
