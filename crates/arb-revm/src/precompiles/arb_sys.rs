use super::*;
use revm::context_interface::{Block, journaled_state::account::JournaledAccountTr};
use revm::interpreter::CallInputs;
use revm::primitives::{Address, B256, Bytes, Log, keccak256};

const ARBOS_VERSION_WITH_NATIVE_TOKEN_OWNERS_SEND_RESTRICTION: u64 = 41;
const ARBOS_VERSION_RETURNS_SEND_INDEX: u64 = 4;

const L2_TO_L1_TX_EVENT_SIGNATURE: &[u8] =
    b"L2ToL1Tx(address,address,uint256,uint256,uint256,uint256,uint256,uint256,bytes)";
const SEND_MERKLE_UPDATE_EVENT_SIGNATURE: &[u8] = b"SendMerkleUpdate(uint256,bytes32,uint256)";

pub(super) fn run_arb_sys<CTX>(
    ctx: &mut CTX,
    input: &[u8],
    gas_limit: u64,
    call_inputs: &CallInputs,
) -> InterpreterResult
where
    CTX: ContextTr<Journal: JournalTr>,
{
    let call = match ArbSys::ArbSysCalls::abi_decode(input) {
        Ok(c) => c,
        Err(e) => return revert_result(gas_limit, &format!("ArbSys: invalid calldata: {e}")),
    };

    let state = ArbosState::open();

    match call {
        ArbSys::ArbSysCalls::arbBlockNumber(_) => {
            // L2 block number is stored as the EVM block number.
            let num: u64 = ctx.block().number().try_into().unwrap_or(u64::MAX);
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(U256::from(num),)),
            )
        }
        ArbSys::ArbSysCalls::arbBlockHash(call) => {
            let target: u64 = call.arbBlockNum.try_into().unwrap_or(u64::MAX);
            let current: u64 = ctx.block().number().try_into().unwrap_or(u64::MAX);
            if target >= current || target.saturating_add(256) < current {
                return revert_result(gas_limit, "ArbSys: invalid block number");
            }
            let hash = ctx.block_hash(target).unwrap_or(B256::ZERO);
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(hash,)),
            )
        }
        ArbSys::ArbSysCalls::arbChainID(_) => {
            let chain_id = match state.chain_id.get(ctx.journal_mut()) {
                Ok(id) => id,
                Err(e) => return revert_result(gas_limit, &format!("ArbSys: storage error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(chain_id,)),
            )
        }
        ArbSys::ArbSysCalls::arbOSVersion(_) => {
            let version = match state.arbos_version.get(ctx.journal_mut()) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbSys: storage error: {e}")),
            };
            // Nitro starts at ArbOS version 56 and exposes this as 55 + internal version.
            let encoded_version = U256::from(55_u64.saturating_add(version));
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(encoded_version,)),
            )
        }
        ArbSys::ArbSysCalls::getStorageGasAvailable(_) => {
            // Nitro has no storage gas; return 0 for Classic compatibility.
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(U256::ZERO,)),
            )
        }
        ArbSys::ArbSysCalls::isTopLevelCall(_) => {
            let depth = ctx.journal().depth();
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(depth <= 2,)),
            )
        }
        ArbSys::ArbSysCalls::mapL1SenderContractAddressToL2Alias(call) => {
            match remap_l1_address(call.sender) {
                Ok(aliased) => ok_result(
                    gas_limit,
                    alloy_core::sol_types::SolValue::abi_encode(&(aliased,)),
                ),
                Err(e) => revert_result(gas_limit, &format!("ArbSys: alias error: {e}")),
            }
        }
        ArbSys::ArbSysCalls::wasMyCallersAddressAliased(_) => {
            // An address was aliased if its inverse-remap differs from the L2 address.
            let caller = call_inputs.caller;
            let is_aliased = inverse_remap_l1_address(caller)
                .map(|unaliased| unaliased != caller)
                .unwrap_or(false);
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(is_aliased,)),
            )
        }
        ArbSys::ArbSysCalls::myCallersAddressWithoutAliasing(_) => {
            let caller = call_inputs.caller;
            let unaliased = inverse_remap_l1_address(caller).unwrap_or(caller);
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(unaliased,)),
            )
        }
        ArbSys::ArbSysCalls::sendMerkleTreeState(_) => {
            if call_inputs.caller != Address::ZERO {
                return revert_result(
                    gas_limit,
                    "ArbSys: method can only be called by address zero",
                );
            }
            let journal = ctx.journal_mut();
            let (size, root, partials) = match state.send_merkle.state_for_export(journal) {
                Ok(out) => out,
                Err(e) => return revert_result(gas_limit, &format!("ArbSys: merkle error: {e}")),
            };
            ok_result(
                gas_limit,
                alloy_core::sol_types::SolValue::abi_encode(&(U256::from(size), root, partials)),
            )
        }
        ArbSys::ArbSysCalls::sendTxToL1(call) => apply_send_tx_to_l1(
            ctx,
            gas_limit,
            call_inputs.bytecode_address,
            call_inputs.caller,
            call_inputs.call_value(),
            call.destination,
            call.data.as_ref(),
        ),
        ArbSys::ArbSysCalls::withdrawEth(call) => apply_send_tx_to_l1(
            ctx,
            gas_limit,
            call_inputs.bytecode_address,
            call_inputs.caller,
            call_inputs.call_value(),
            call.destination,
            &[],
        ),
    }
}

fn apply_send_tx_to_l1<CTX>(
    ctx: &mut CTX,
    gas_limit: u64,
    precompile_address: Address,
    caller: Address,
    callvalue: U256,
    destination: Address,
    calldata_for_l1: &[u8],
) -> InterpreterResult
where
    CTX: ContextTr<Journal: JournalTr>,
{
    let state = ArbosState::open();
    let arb_block_num = U256::from(ctx.block().number());
    let timestamp = U256::from(ctx.block().timestamp());
    let l1_block_num;
    let arbos_version;

    {
        let journal = ctx.journal_mut();
        arbos_version = match state.arbos_version.get(journal) {
            Ok(v) => v,
            Err(e) => return revert_result(gas_limit, &format!("ArbSys: storage error: {e}")),
        };
        l1_block_num = match state.block_hashes.l1_block_number(journal) {
            Ok(v) => v,
            Err(e) => return revert_result(gas_limit, &format!("ArbSys: storage error: {e}")),
        };

        if arbos_version >= ARBOS_VERSION_WITH_NATIVE_TOKEN_OWNERS_SEND_RESTRICTION
            && callvalue > U256::ZERO
        {
            let owner_count = match state.native_token_owners.size.get(journal) {
                Ok(v) => v,
                Err(e) => return revert_result(gas_limit, &format!("ArbSys: storage error: {e}")),
            };
            if owner_count > 0 {
                return revert_result(
                    gas_limit,
                    "ArbSys: not allowed to send value when native token owners exist",
                );
            }
        }
    }

    let l1_block_num_u256 = U256::from(l1_block_num);
    let send_hash = compute_l2_to_l1_send_hash(
        caller,
        destination,
        arb_block_num,
        l1_block_num_u256,
        timestamp,
        callvalue,
        calldata_for_l1,
    );

    let leaf_num;
    let update_events;
    {
        let journal = ctx.journal_mut();
        update_events = match state.send_merkle.append(send_hash, journal) {
            Ok(v) => v,
            Err(e) => return revert_result(gas_limit, &format!("ArbSys: merkle error: {e}")),
        };
        let size = match state.send_merkle.size(journal) {
            Ok(v) => v,
            Err(e) => return revert_result(gas_limit, &format!("ArbSys: merkle error: {e}")),
        };
        leaf_num = size.saturating_sub(1);

        if callvalue > U256::ZERO {
            let mut account = match journal
                .load_account_mut_skip_cold_load(precompile_address, false)
            {
                Ok(acc) => acc,
                Err(e) => return revert_result(gas_limit, &format!("ArbSys: storage error: {e}")),
            };
            if !account.data.decr_balance(callvalue) {
                return revert_result(gas_limit, "ArbSys: insufficient balance for L2->L1 burn");
            }
        }
    }

    {
        let journal = ctx.journal_mut();
        for update in &update_events {
            let position = (U256::from(update.level) << 192) + U256::from(update.num_leaves);
            journal.log(Log::new_unchecked(
                precompile_address,
                vec![
                    keccak256(SEND_MERKLE_UPDATE_EVENT_SIGNATURE),
                    u256_to_b256(U256::ZERO),
                    update.hash,
                    u256_to_b256(position),
                ],
                Bytes::new(),
            ));
        }

        journal.log(Log::new_unchecked(
            precompile_address,
            vec![
                keccak256(L2_TO_L1_TX_EVENT_SIGNATURE),
                address_to_topic(destination),
                send_hash,
                u256_to_b256(U256::from(leaf_num)),
            ],
            Bytes::from(alloy_core::sol_types::SolValue::abi_encode(&(
                caller,
                arb_block_num,
                l1_block_num_u256,
                timestamp,
                callvalue,
                calldata_for_l1.to_vec(),
            ))),
        ));
    }

    let unique_id = if arbos_version >= ARBOS_VERSION_RETURNS_SEND_INDEX {
        U256::from(leaf_num)
    } else {
        U256::from_be_bytes(send_hash.0)
    };

    ok_result(
        gas_limit,
        alloy_core::sol_types::SolValue::abi_encode(&(unique_id,)),
    )
}

fn compute_l2_to_l1_send_hash(
    caller: Address,
    destination: Address,
    arb_block_num: U256,
    l1_block_num: U256,
    timestamp: U256,
    callvalue: U256,
    calldata_for_l1: &[u8],
) -> B256 {
    let mut preimage = Vec::with_capacity(20 + 20 + (32 * 4) + 32 + calldata_for_l1.len());
    preimage.extend_from_slice(caller.as_slice());
    preimage.extend_from_slice(destination.as_slice());
    preimage.extend_from_slice(&arb_block_num.to_be_bytes::<32>());
    preimage.extend_from_slice(&l1_block_num.to_be_bytes::<32>());
    preimage.extend_from_slice(&timestamp.to_be_bytes::<32>());
    preimage.extend_from_slice(&callvalue.to_be_bytes::<32>());
    preimage.extend_from_slice(calldata_for_l1);
    keccak256(preimage)
}

#[inline]
fn u256_to_b256(value: U256) -> B256 {
    B256::from(value.to_be_bytes::<32>())
}

#[inline]
fn address_to_topic(address: Address) -> B256 {
    let mut padded = [0_u8; 32];
    padded[12..].copy_from_slice(address.as_slice());
    B256::from(padded)
}
