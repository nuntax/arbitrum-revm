//! Stage B exit proof: one Arbitrum transaction executes through
//! `ArbEvmFactory::create_evm(...).transact_raw(tx)` and produces a `ResultAndState` consistent
//! with `arb_revm` running the same tx directly (gas_used matches, balances update).

use super::{ArbEvm, ArbEvmFactory, ArbTx};
use alloy_evm::{Evm, EvmEnv, EvmFactory};
use arb_revm::api::default_ctx::ArbContext;
use arb_revm::{ArbBuilder, ArbChainContext, ArbSpecId, ArbTransaction};
use revm::context::result::ExecutionResult;
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::database::{CacheDB, EmptyDB};
use revm::primitives::{Address, TxKind, U256};
use revm::state::AccountInfo;
use revm::{Context, ExecuteEvm, MainContext};

const CHAIN_ID: u64 = 42_161;
const SENDER: Address = Address::with_last_byte(0x11);
const RECIPIENT: Address = Address::with_last_byte(0x22);
const TRANSFER_VALUE: u128 = 1_000_000_000_000_000_000; // 1 ETH
const START_BALANCE: u128 = 10_000_000_000_000_000_000; // 10 ETH

/// A funded-sender cache db with priority-fee/base-fee checks disabled so a zero-gas-price value
/// transfer executes deterministically (Stage B is a transact-level proof, not a fee-accuracy one).
fn funded_db() -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(EmptyDB::default());
    db.insert_account_info(
        SENDER,
        AccountInfo {
            balance: U256::from(START_BALANCE),
            nonce: 0,
            ..AccountInfo::default()
        },
    );
    db
}

fn cfg() -> CfgEnv<ArbSpecId> {
    let mut cfg = CfgEnv::new_with_spec(ArbSpecId::ARBOS_51)
        .with_chain_id(CHAIN_ID)
        .with_disable_priority_fee_check(true);
    cfg.disable_balance_check = false;
    cfg
}

/// The value-transfer tx env both paths execute.
fn transfer_tx() -> ArbTransaction<TxEnv> {
    ArbTransaction::new(TxEnv {
        tx_type: 2,
        caller: SENDER,
        gas_limit: 100_000,
        gas_price: 0,
        kind: TxKind::Call(RECIPIENT),
        value: U256::from(TRANSFER_VALUE),
        nonce: 0,
        chain_id: Some(CHAIN_ID),
        ..Default::default()
    })
}

/// Oracle: run the same tx through `arb_revm` directly (the path `replay_block` uses), via the
/// `ArbBuilder` + `ExecuteEvm` surface this bridge wraps.
fn oracle_result() -> (u64, U256, U256) {
    let mut db = funded_db();
    let ctx: ArbContext<&mut _> = Context::mainnet()
        .with_chain(ArbChainContext::default())
        .with_db(&mut db)
        .with_block(BlockEnv::default())
        .with_cfg(cfg())
        .with_tx(ArbTransaction::<TxEnv>::default());
    let mut evm = ctx.build_arb();
    let out = evm.transact(transfer_tx()).expect("oracle execution");
    let gas_used = out.result.gas_used();
    let sender = out.state.get(&SENDER).expect("sender in state").info.balance;
    let recipient = out
        .state
        .get(&RECIPIENT)
        .map(|a| a.info.balance)
        .unwrap_or_default();
    (gas_used, sender, recipient)
}

#[test]
fn arb_evm_factory_transact_matches_arb_revm() {
    let (oracle_gas, oracle_sender_bal, oracle_recipient_bal) = oracle_result();

    // Bridge path: build the EVM exactly as reth would, then transact_raw an ArbTx.
    let db = funded_db();
    let evm_env = EvmEnv::new(cfg(), BlockEnv::default());
    let mut evm: ArbEvm<_, _> = ArbEvmFactory.create_evm(db, evm_env);

    let out = evm
        .transact_raw(ArbTx(transfer_tx()))
        .expect("bridge execution");

    assert!(out.result.is_success(), "transfer must succeed: {:?}", out.result);

    // gas_used must be identical to arb_revm running the same tx.
    assert_eq!(
        out.result.gas_used(),
        oracle_gas,
        "bridge gas_used must match arb_revm oracle"
    );

    // Balances must update consistently with the oracle.
    let sender_bal = out.state.get(&SENDER).expect("sender in state").info.balance;
    let recipient_bal = out
        .state
        .get(&RECIPIENT)
        .map(|a| a.info.balance)
        .unwrap_or_default();

    assert_eq!(sender_bal, oracle_sender_bal, "sender balance must match oracle");
    assert_eq!(recipient_bal, oracle_recipient_bal, "recipient balance must match oracle");

    // And the value actually moved.
    assert_eq!(
        recipient_bal,
        U256::from(TRANSFER_VALUE),
        "recipient must receive the transferred value"
    );
    assert!(
        sender_bal <= U256::from(START_BALANCE - TRANSFER_VALUE),
        "sender debited value (plus any fee)"
    );

    // chain_id / block accessors are wired.
    assert_eq!(evm.chain_id(), CHAIN_ID);
}

#[test]
fn create_evm_with_inspector_runs_inspecting_path() {
    use revm::inspector::NoOpInspector;

    let db = funded_db();
    let evm_env = EvmEnv::new(cfg(), BlockEnv::default());
    let mut evm = ArbEvmFactory.create_evm_with_inspector(db, evm_env, NoOpInspector {});

    let out = evm
        .transact_raw(ArbTx(transfer_tx()))
        .expect("inspecting execution");
    assert!(matches!(out.result, ExecutionResult::Success { .. }));
    assert_eq!(
        out.state.get(&RECIPIENT).map(|a| a.info.balance).unwrap_or_default(),
        U256::from(TRANSFER_VALUE)
    );
}
