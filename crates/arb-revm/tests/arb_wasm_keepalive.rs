use alloy_core::sol_types::SolCall;
use arb_revm::{
    api::default_ctx::{ArbContext, DefaultArb},
    arb_journal::ArbCall,
    arbos_init::{ArbosInitConfig, initialize_arbos_state},
    precompiles::ArbPrecompilesEnum,
    storage::{
        ArbosState,
        programs::{ARBITRUM_START_TIME, ProgramInfo},
    },
};
use arbitrum_alloy_precompiles::ArbWasm;
use revm::{
    context_interface::{ContextTr, JournalTr},
    database_interface::EmptyDB,
    interpreter::InstructionResult,
    primitives::{Address, B256, U256, address},
};

const ARB_WASM: Address = address!("0000000000000000000000000000000000000071");
const CALLER: Address = address!("c8c724faf86a649aea0f062ba713deae61a2d8fd");

/// Arbitrum One tx 0x4f71e64d…1c36 (block 518,873,673): `codehashKeepalive` on a 1,362,944-byte
/// program, 89,543 gas on Nitro. Less 21,000 intrinsic, 576 calldata (36 non-zero bytes) and 83
/// L1 gas, the precompile itself spent 67,884.
#[test]
fn codehash_keepalive_matches_nitro_gas() {
    let mut ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();
    initialize_arbos_state(
        &ArbosInitConfig {
            initial_arbos_version: 61,
            initial_chain_owner: Address::ZERO,
            chain_id: U256::from(42_161_u64),
            genesis_block_number: 0,
            initial_l1_base_fee: U256::from(50_000_000_000_u64),
            serialized_chain_config: b"{\"chainId\":42161}".to_vec(),
            debug_precompiles: false,
        },
        ctx.journal_mut(),
    )
    .expect("initialize ArbOS state");
    let now = ARBITRUM_START_TIME + 400 * 24 * 3600;
    ctx.block.timestamp = U256::from(now);

    // Activated 40 days ago: past the 31-day keepalive window, well within the 365-day expiry.
    let codehash = B256::with_last_byte(0x42);
    let params_word = ArbosState::open()
        .programs
        .read_params_word(ctx.journal_mut())
        .expect("read Stylus params");
    ArbosState::open()
        .programs
        .write_program(
            codehash,
            &ProgramInfo {
                version: u16::from_be_bytes([params_word[0], params_word[1]]),
                activated_at: ((now - ARBITRUM_START_TIME) / 3600 - 40 * 24) as u32,
                asm_estimate_kb: 1_331,
                ..Default::default()
            },
            ctx.journal_mut(),
        )
        .expect("write program record");

    // The call's value has already moved into ArbWasm by the time the precompile runs.
    let value = U256::from(10u128.pow(18));
    ctx.journal_mut()
        .load_account(ARB_WASM)
        .expect("load ArbWasm");
    ctx.journal_mut()
        .balance_incr(ARB_WASM, value)
        .expect("fund ArbWasm");

    let input = ArbWasm::codehashKeepaliveCall { codehash }.abi_encode();
    let call = ArbCall {
        input: &input,
        gas_limit: 200_000,
        caller: CALLER,
        value,
        bytecode_address: ARB_WASM,
        acting_address: ARB_WASM,
        is_static: false,
    };
    let result = ArbPrecompilesEnum::ArbWasm.run_dispatch(&mut ctx, &call);

    assert_eq!(result.result, InstructionResult::Return);
    // Includes the network fee account read in payActivationDataFee (800).
    assert_eq!(result.gas.total_gas_spent(), 67_884);
}
