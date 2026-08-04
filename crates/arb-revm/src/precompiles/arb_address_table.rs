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
        Err(_) => return gated_revert_result(gas_limit),
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
        ArbAddressTable::ArbAddressTableCalls::compress(c) => {
            match state.address_table.compress(c.account, &mut journal) {
                Ok(encoded) => ok_result(
                    gas_limit,
                    alloy_core::sol_types::SolValue::abi_encode(&(revm::primitives::Bytes::from(
                        encoded,
                    ),)),
                ),
                Err(e) => revert_result(gas_limit, &format!("ArbAddressTable: error: {e}")),
            }
        }
        ArbAddressTable::ArbAddressTableCalls::decompress(c) => match usize::try_from(c.offset) {
            Ok(offset) => match c.buf.get(offset..) {
                Some(encoded) => match state.address_table.decompress(encoded, &mut journal) {
                    Ok((address, consumed)) => ok_result(
                        gas_limit,
                        alloy_core::sol_types::SolValue::abi_encode(&(
                            address,
                            U256::from(consumed),
                        )),
                    ),
                    Err(e) => revert_result(gas_limit, &format!("ArbAddressTable: error: {e}")),
                },
                None => revert_result(gas_limit, "ArbAddressTable: invalid offset"),
            },
            Err(_) => revert_result(gas_limit, "ArbAddressTable: invalid offset"),
        },
    };

    // Fold the burner total into the call's gas (Nitro bills these per-op through the burner).
    let burned = journal.burned;
    if !result.gas.record_regular_cost(burned) {
        result.result = revm::interpreter::InstructionResult::OutOfGas;
        result.output = revm::primitives::Bytes::new();
    }
    result
}

#[cfg(test)]
mod tests {
    use alloy_core::sol_types::{SolCall, SolValue};
    use revm::{
        database_interface::EmptyDB,
        interpreter::InstructionResult,
        primitives::{Address, Bytes, U256, address},
    };

    use super::{ArbAddressTable, run_arb_address_table};
    use crate::api::default_ctx::{ArbContext, DefaultArb};

    const ACCOUNT: Address = address!("c5d2460186f7233c927e7db2dcc703c0e500b653");

    fn run(ctx: &mut ArbContext<EmptyDB>, input: Vec<u8>) -> revm::interpreter::InterpreterResult {
        run_arb_address_table(ctx, &input, 100_000)
    }

    #[test]
    fn dispatcher_covers_init_register_lookup_and_bounds() {
        let mut ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();

        let size = run(&mut ctx, ArbAddressTable::sizeCall {}.abi_encode());
        assert_eq!(size.result, InstructionResult::Return);
        assert_eq!(<(U256,)>::abi_decode(&size.output).unwrap().0, U256::ZERO);
        assert_eq!(size.gas.total_gas_spent(), 800);

        let missing = run(
            &mut ctx,
            ArbAddressTable::lookupCall { account: ACCOUNT }.abi_encode(),
        );
        assert_eq!(missing.result, InstructionResult::Revert);
        assert_eq!(missing.gas.total_gas_spent(), 800);

        let registered = run(
            &mut ctx,
            ArbAddressTable::registerCall { account: ACCOUNT }.abi_encode(),
        );
        assert_eq!(registered.result, InstructionResult::Return);
        assert_eq!(
            <(U256,)>::abi_decode(&registered.output).unwrap().0,
            U256::ZERO
        );
        assert_eq!(registered.gas.total_gas_spent(), 61_600);

        let exists = run(
            &mut ctx,
            ArbAddressTable::addressExistsCall { account: ACCOUNT }.abi_encode(),
        );
        assert!(<(bool,)>::abi_decode(&exists.output).unwrap().0);
        let by_index = run(
            &mut ctx,
            ArbAddressTable::lookupIndexCall { index: U256::ZERO }.abi_encode(),
        );
        assert_eq!(
            <(Address,)>::abi_decode(&by_index.output).unwrap().0,
            ACCOUNT
        );
        let out_of_bounds = run(
            &mut ctx,
            ArbAddressTable::lookupIndexCall { index: U256::ONE }.abi_encode(),
        );
        assert_eq!(out_of_bounds.result, InstructionResult::Revert);
    }

    #[test]
    fn dispatcher_compresses_and_decompresses_with_offsets() {
        let mut ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();
        let literal = run(
            &mut ctx,
            ArbAddressTable::compressCall { account: ACCOUNT }.abi_encode(),
        );
        let literal = <(Bytes,)>::abi_decode(&literal.output).unwrap().0;
        assert_eq!(literal.len(), 21);

        let decoded = run(
            &mut ctx,
            ArbAddressTable::decompressCall {
                buf: literal.clone(),
                offset: U256::ZERO,
            }
            .abi_encode(),
        );
        assert_eq!(
            <(Address, U256)>::abi_decode(&decoded.output).unwrap(),
            (ACCOUNT, U256::from(21))
        );

        run(
            &mut ctx,
            ArbAddressTable::registerCall { account: ACCOUNT }.abi_encode(),
        );
        let compressed = run(
            &mut ctx,
            ArbAddressTable::compressCall { account: ACCOUNT }.abi_encode(),
        );
        let compressed = <(Bytes,)>::abi_decode(&compressed.output).unwrap().0;
        assert_eq!(compressed.as_ref(), &[0x80]);

        let mut padded = vec![99];
        padded.extend_from_slice(&compressed);
        padded.push(33);
        let decoded = run(
            &mut ctx,
            ArbAddressTable::decompressCall {
                buf: Bytes::from(padded.clone()),
                offset: U256::ONE,
            }
            .abi_encode(),
        );
        assert_eq!(
            <(Address, U256)>::abi_decode(&decoded.output).unwrap(),
            (ACCOUNT, U256::ONE)
        );
        assert_eq!(decoded.gas.total_gas_spent(), 1_600);

        let invalid = run(
            &mut ctx,
            ArbAddressTable::decompressCall {
                buf: Bytes::from(padded),
                offset: U256::from(4),
            }
            .abi_encode(),
        );
        assert_eq!(invalid.result, InstructionResult::Revert);
        assert_eq!(invalid.gas.total_gas_spent(), 0);
    }
}
