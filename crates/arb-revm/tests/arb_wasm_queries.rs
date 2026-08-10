use alloy_core::sol_types::{SolCall, SolValue};
use arb_revm::{
    api::default_ctx::{ArbContext, DefaultArb},
    arb_journal::{ArbCall, ArbPrecompileCtx},
    arbos_init::{ArbosInitConfig, initialize_arbos_state},
    precompiles::ArbPrecompilesEnum,
    storage::{
        ArbosState,
        programs::{ARBITRUM_START_TIME, ProgramInfo},
    },
};
use arbitrum_alloy_precompiles::ArbWasm;
use revm::{
    database_interface::EmptyDB,
    interpreter::InstructionResult,
    primitives::{Address, B256, U256, address},
};

const ARB_WASM: Address = address!("0000000000000000000000000000000000000071");

fn ctx() -> ArbContext<EmptyDB> {
    let mut ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();
    initialize_arbos_state(
        &ArbosInitConfig {
            initial_arbos_version: 31,
            initial_chain_owner: Address::ZERO,
            chain_id: U256::from(412_346_u64),
            genesis_block_number: 0,
            initial_l1_base_fee: U256::from(50_000_000_000_u64),
            serialized_chain_config: b"{\"chainId\":412346}".to_vec(),
            debug_precompiles: false,
        },
        ctx.journal_mut(),
    )
    .expect("initialize ArbOS state");
    ctx
}

#[test]
fn codehash_asm_size_reads_active_program_and_charges_storage() {
    let mut ctx = ctx();
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
                activated_at: ctx
                    .block_timestamp()
                    .saturating_sub(ARBITRUM_START_TIME)
                    .saturating_div(3600) as u32,
                asm_estimate_kb: 729,
                ..Default::default()
            },
            ctx.journal_mut(),
        )
        .expect("write program record");

    let input = ArbWasm::codehashAsmSizeCall { codehash }.abi_encode();
    let call = ArbCall {
        input: &input,
        gas_limit: 100_000,
        caller: Address::ZERO,
        value: U256::ZERO,
        bytecode_address: ARB_WASM,
        acting_address: ARB_WASM,
        is_static: true,
    };
    let result = ArbPrecompilesEnum::ArbWasm.run_dispatch(&mut ctx, &call);

    assert_eq!(result.result, InstructionResult::Return);
    assert_eq!(<(u32,)>::abi_decode(&result.output).unwrap().0, 746_496);
    // Dispatcher: OpenArbosState + input/output copies = 806. ArbWasm itself reads the packed
    // params word (100) and active program record (800), matching Nitro's 1,706 gas total.
    assert_eq!(result.gas.total_gas_spent(), 1_706);
}
