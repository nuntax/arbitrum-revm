use super::*;
use crate::arb_journal::{ArbPrecompileCtx, MeteredJournal};

pub(super) fn run_arb_address_table<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
) -> InterpreterResult
where
    CTX: ArbPrecompileCtx,
{
    let call = match ArbAddressTable::ArbAddressTableCalls::abi_decode(input) {
        Ok(c) => c,
        Err(e) => {
            return revert_result(
                gas_limit,
                &format!("ArbAddressTable: invalid calldata: {e}"),
            );
        }
    };

    let state = ArbosState::open();

    // Nitro meters every ArbOS-storage read/write performed inside these methods through the
    // precompile burner (StorageReadCost=800 / StorageWriteCost each), on top of the per-call
    // args/result/OpenArbosState charge that `arbos_call_extra_gas` already folds in. Route the
    // table ops through `MeteredJournal` and add its total to the call's gas. Example: `lookupIndex`
    // reads `numItems` + the address (2×800=1600), so canonical bills 806+1600 = 2406; without
    // this we'd undercharge 1600 per call.
    let mut journal = MeteredJournal::new(ctx.journal_mut());

    let mut result = match call {
        ArbAddressTable::ArbAddressTableCalls::addressExists(c) => {
            match state.address_table.lookup(c.account, &mut journal) {
                Ok(opt) => ok_result(
                    gas_limit,
                    alloy_core::sol_types::SolValue::abi_encode(&(opt.is_some(),)),
                ),
                Err(e) => revert_result(gas_limit, &format!("ArbAddressTable: error: {e}")),
            }
        }
        ArbAddressTable::ArbAddressTableCalls::lookup(c) => {
            match state.address_table.lookup(c.account, &mut journal) {
                Ok(Some(idx)) => ok_result(
                    gas_limit,
                    alloy_core::sol_types::SolValue::abi_encode(&(U256::from(idx),)),
                ),
                Ok(None) => revert_result(gas_limit, "ArbAddressTable: address not registered"),
                Err(e) => revert_result(gas_limit, &format!("ArbAddressTable: error: {e}")),
            }
        }
        ArbAddressTable::ArbAddressTableCalls::lookupIndex(c) => {
            let idx: u64 = c.index.try_into().unwrap_or(u64::MAX);
            match state.address_table.lookup_index(idx, &mut journal) {
                Ok(Some(addr)) => ok_result(
                    gas_limit,
                    alloy_core::sol_types::SolValue::abi_encode(&(addr,)),
                ),
                Ok(None) => revert_result(gas_limit, "ArbAddressTable: index out of bounds"),
                Err(e) => revert_result(gas_limit, &format!("ArbAddressTable: error: {e}")),
            }
        }
        ArbAddressTable::ArbAddressTableCalls::size(_) => {
            match state.address_table.len(&mut journal) {
                Ok(num_items) => ok_result(
                    gas_limit,
                    alloy_core::sol_types::SolValue::abi_encode(&(U256::from(num_items),)),
                ),
                Err(e) => revert_result(gas_limit, &format!("ArbAddressTable: error: {e}")),
            }
        }
        ArbAddressTable::ArbAddressTableCalls::register(c) => {
            match state.address_table.register(c.account, &mut journal) {
                Ok(idx) => ok_result(
                    gas_limit,
                    alloy_core::sol_types::SolValue::abi_encode(&(U256::from(idx),)),
                ),
                Err(e) => revert_result(gas_limit, &format!("ArbAddressTable: error: {e}")),
            }
        }
        ArbAddressTable::ArbAddressTableCalls::compress(_)
        | ArbAddressTable::ArbAddressTableCalls::decompress(_) => revert_result(
            gas_limit,
            "ArbAddressTable: compress/decompress not yet implemented",
        ),
    };

    // Fold the burner total into the call's gas (Nitro bills these per-op through the burner).
    let burned = journal.burned;
    if !result.gas.record_regular_cost(burned) {
        result.result = revm::interpreter::InstructionResult::OutOfGas;
        result.output = revm::primitives::Bytes::new();
    }
    result
}
