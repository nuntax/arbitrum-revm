#![allow(clippy::field_reassign_with_default)]
//! Regression test for Arbitrum's `NUMBER` opcode semantics.
//!
//! On Arbitrum the EVM `NUMBER` opcode (`block.number`) returns the **L1** block
//! number, not the L2 block number, Nitro patches `opNumber` to read
//! `ProcessingHook.L1BlockNumber` (`go-ethereum/core/vm/instructions.go`). We carry
//! that value in `ArbChainContext::l1_block_number` and override the opcode.

use arb_alloy_consensus::transactions::{ArbTxEnvelope, TxUnsigned};
use arb_revm::transaction::arb_envelope_to_tx_env;
use arb_revm::{ArbBuilder, ArbChainContext, ArbContext, ArbSpecId, ArbTransaction, DefaultArb};
use revm::{
    ExecuteEvm,
    context::{BlockEnv, CfgEnv, TxEnv},
    database::{CacheDB, EmptyDB},
    primitives::{Address, Bytes, TxKind, U256, keccak256},
    state::{AccountInfo, Bytecode},
};

#[test]
fn number_opcode_returns_l1_block_number() {
    // NUMBER, PUSH1 0x00, SSTORE, STOP, stores block.number into slot 0.
    let code = Bytes::from(vec![0x43, 0x60, 0x00, 0x55, 0x00]);
    let code_hash = keccak256(&code);
    let contract = Address::with_last_byte(0xcc);
    let caller = Address::with_last_byte(0x11);

    let l2_block_number = 1000_u64;
    let l1_block_number = 777_u64;

    let mut db = CacheDB::new(EmptyDB::default());
    db.insert_account_info(
        contract,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code_hash,
            code: Some(Bytecode::new_raw(code)),
            ..AccountInfo::default()
        },
    );

    let mut cfg = CfgEnv::new_with_spec(ArbSpecId::NITRO)
        .with_chain_id(412_346)
        .with_disable_priority_fee_check(true);
    cfg.disable_balance_check = true;
    let mut block = BlockEnv::default();
    block.number = U256::from(l2_block_number);

    let chain = ArbChainContext::new(None).with_l1_block_number(l1_block_number);
    let ctx: ArbContext<&mut _> = ArbContext::arb_with_chain_context(chain)
        .with_db(&mut db)
        .with_cfg(cfg)
        .with_block(block)
        .with_tx(ArbTransaction::<TxEnv>::default());
    let mut evm = ctx.build_arb();

    let tx = TxUnsigned {
        chain_id: U256::from(412_346_u64),
        from: caller,
        nonce: 0,
        gas_fee_cap: U256::ZERO,
        gas_limit: 100_000,
        to: TxKind::Call(contract),
        value: U256::ZERO,
        input: Bytes::new(),
    };
    let tx_env = arb_envelope_to_tx_env(&ArbTxEnvelope::from(tx)).expect("convert tx");
    let out = evm.transact(tx_env).expect("execution");
    assert!(out.result.is_success(), "call failed: {:?}", out.result);

    let slot0 = out
        .state
        .get(&contract)
        .and_then(|acct| acct.storage.get(&U256::ZERO))
        .map(|slot| slot.present_value())
        .unwrap_or_default();

    assert_eq!(
        slot0,
        U256::from(l1_block_number),
        "NUMBER must return the L1 block number"
    );
    assert_ne!(
        slot0,
        U256::from(l2_block_number),
        "NUMBER must NOT return the L2 block number"
    );
}
