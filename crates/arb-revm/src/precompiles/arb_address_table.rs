use super::*;

pub(super) fn run_arb_address_table<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
) -> InterpreterResult
where
    CTX: ContextTr<Journal: JournalTr>,
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

    match call {
        ArbAddressTable::ArbAddressTableCalls::addressExists(c) => {
            let exists = match state.address_table.lookup(c.account, ctx.journal_mut()) {
                Ok(opt) => opt.is_some(),
                Err(e) => return revert_result(gas_limit, &format!("ArbAddressTable: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(exists,)),
            )
        }
        ArbAddressTable::ArbAddressTableCalls::lookup(c) => {
            match state.address_table.lookup(c.account, ctx.journal_mut()) {
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
            match state.address_table.lookup_index(idx, ctx.journal_mut()) {
                Ok(Some(addr)) => ok_result(
                    gas_limit,
                    alloy_core::sol_types::SolValue::abi_encode(&(addr,)),
                ),
                Ok(None) => revert_result(gas_limit, "ArbAddressTable: index out of bounds"),
                Err(e) => revert_result(gas_limit, &format!("ArbAddressTable: error: {e}")),
            }
        }
        ArbAddressTable::ArbAddressTableCalls::size(_) => {
            let num_items = match state.address_table.len(ctx.journal_mut()) {
                Ok(n) => n,
                Err(e) => return revert_result(gas_limit, &format!("ArbAddressTable: error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(U256::from(num_items),)),
            )
        }
        ArbAddressTable::ArbAddressTableCalls::register(c) => {
            match state.address_table.register(c.account, ctx.journal_mut()) {
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
    }
}
