#![allow(clippy::field_reassign_with_default)]
//! Regression test for Arbitrum's `BLOBBASEFEE` opcode semantics.
//!
//! Nitro's `opBlobBaseFee` (`go-ethereum/core/vm/eips.go`) errors with "BLOBBASEFEE is not
//! supported on Arbitrum" on every Arbitrum chain, so the frame halts and burns all remaining
//! gas. Executing it as stock revm does (push the blob base fee, succeed) forked Robinhood Chain
//! at L2 block 118,333,128 (tx 0x98475e74e1c0dd46397c5cfd32a85605175f954b1734102a68d546fa54c5f04a).

use arb_revm::transaction::arb_envelope_to_tx_env;
use arb_revm::{ArbBuilder, ArbChainContext, ArbContext, ArbSpecId, ArbTransaction, DefaultArb};
use arbitrum_alloy_consensus::transactions::{ArbTxEnvelope, TxUnsigned};
use revm::{
    ExecuteEvm,
    context::{BlockEnv, CfgEnv, TxEnv},
    database::{CacheDB, EmptyDB},
    primitives::{Address, Bytes, TxKind, U256, keccak256},
    state::{AccountInfo, Bytecode},
};

#[test]
fn blobbasefee_opcode_halts_and_burns_all_gas() {
    // BLOBBASEFEE, PUSH1 0x00, SSTORE, STOP.
    let code = Bytes::from(vec![0x4a, 0x60, 0x00, 0x55, 0x00]);
    let code_hash = keccak256(&code);
    let contract = Address::with_last_byte(0xcc);
    let caller = Address::with_last_byte(0x11);
    let gas_limit = 100_000_u64;

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

    let mut cfg = CfgEnv::new_with_spec(ArbSpecId::ARBOS_61)
        .with_chain_id(412_346)
        .with_disable_priority_fee_check(true);
    cfg.disable_balance_check = true;

    let ctx: ArbContext<&mut _> = ArbContext::arb_with_chain_context(ArbChainContext::new(None))
        .with_db(&mut db)
        .with_cfg(cfg)
        .with_block(BlockEnv::default())
        .with_tx(ArbTransaction::<TxEnv>::default());
    let mut evm = ctx.build_arb();

    let tx = TxUnsigned {
        chain_id: U256::from(412_346_u64),
        from: caller,
        nonce: 0,
        gas_fee_cap: U256::ZERO,
        gas_limit,
        to: TxKind::Call(contract),
        value: U256::ZERO,
        input: Bytes::new(),
    };
    let tx_env = arb_envelope_to_tx_env(&ArbTxEnvelope::from(tx)).expect("convert tx");
    let out = evm.transact(tx_env).expect("execution");

    assert!(
        out.result.is_halt(),
        "BLOBBASEFEE must halt: {:?}",
        out.result
    );
    assert_eq!(
        out.result.tx_gas_used(),
        gas_limit,
        "a halt burns all remaining gas"
    );
    let slot0 = out
        .state
        .get(&contract)
        .and_then(|acct| acct.storage.get(&U256::ZERO))
        .map(|slot| slot.present_value())
        .unwrap_or_default();
    assert_eq!(slot0, U256::ZERO, "nothing after BLOBBASEFEE may execute");
}
