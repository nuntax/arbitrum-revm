//! Stylus hostio bridge.
//!
//! [`StylusHandler`] implements arbutil's [`RequestHandler`]: the WASM runtime serializes
//! each hostio call into `(EvmApiMethod, bytes)`, and [`handle_request`] decodes it and
//! applies it to arb_revm's `Context`/`Journal`, returning `(response, return-data, gas)`.
//!
//! The handler holds a callback that captures the EVM context. `RequestHandler` is
//! `Send + 'static`, but hostios must reach the *borrowed* context mid-call; this is sound
//! because a Stylus program runs **synchronously** inside the call frame that owns the
//! context, so the captured borrow strictly outlives every hostio. The executor performs
//! the lifetime erasure (see [`super::executor`]); here we only define the wire protocol
//! and the per-method state operations.
//!
//! Inspired by arbos-revm's `stylus_api.rs`, rewritten against canonical Nitro + revm 36.

use std::sync::Arc;

use arbutil::evm::{
    api::{EvmApiMethod, EvmApiStatus, Gas as ArbGas, VecReader},
    req::RequestHandler,
};
use revm::{
    context_interface::{ContextTr, JournalTr, context::SStoreResult},
    primitives::{Address, Bytes, Log, LogData, U256},
};

/// Services one hostio request against the captured EVM context.
pub type HostCallFunc = dyn Fn(EvmApiMethod, Vec<u8>) -> (Vec<u8>, VecReader, ArbGas);

/// Bridges the Stylus runtime to arb_revm. See the module docs for the `unsafe Send`
/// rationale (synchronous execution within the owning call frame).
pub struct StylusHandler {
    call: Arc<Box<HostCallFunc>>,
}

// SAFETY: the callback only dereferences the captured context while `call_func` is running
// synchronously on this thread, within the lifetime of the frame that owns it.
unsafe impl Send for StylusHandler {}

impl StylusHandler {
    pub fn new(call: Arc<Box<HostCallFunc>>) -> Self {
        Self { call }
    }
}

impl RequestHandler<VecReader> for StylusHandler {
    fn request(
        &mut self,
        req_type: EvmApiMethod,
        req_data: impl AsRef<[u8]>,
    ) -> (Vec<u8>, VecReader, ArbGas) {
        (self.call)(req_type, req_data.as_ref().to_vec())
    }
}

// EIP-2929 storage/account access costs. TODO(parity): these are the static EIP-2929
// values; SSTORE in particular needs the full EIP-2200/3529 transition cost (against the
// slot's original/current value). Will be tuned exactly against the witness root once
// execution is wired up, revm 36 no longer exposes standalone `sstore_cost`/`sload_cost`.
const COLD_SLOAD_COST: u64 = 2100;
const WARM_STORAGE_READ_COST: u64 = 100;
const COLD_ACCOUNT_ACCESS_COST: u64 = 2600;
const SSTORE_SET_GAS: u64 = 20_000;
const SSTORE_RESET_GAS: u64 = 2900;

/// EIP-2200 + EIP-2929 SSTORE gas from the storage transition (matches go-ethereum's
/// `gasSStoreEIP2929`, which Nitro charges via the hostio). SET vs RESET is chosen by the
/// `original == present` / `original == 0` value transition, NOT by cold/warm. The cold
/// surcharge is added separately when the slot is cold.
fn sstore_cost(res: &SStoreResult, is_cold: bool) -> u64 {
    let mut cost = if is_cold { COLD_SLOAD_COST } else { 0 };
    cost += if res.present_value == res.new_value {
        WARM_STORAGE_READ_COST
    } else if res.original_value == res.present_value {
        if res.original_value.is_zero() {
            SSTORE_SET_GAS
        } else {
            SSTORE_RESET_GAS
        }
    } else {
        WARM_STORAGE_READ_COST
    };
    cost
}

#[inline]
fn word(bytes: &[u8]) -> U256 {
    U256::from_be_slice(&bytes[..32])
}

#[inline]
fn ok_status() -> Vec<u8> {
    vec![EvmApiStatus::Success as u8]
}

#[inline]
fn empty_reader() -> VecReader {
    VecReader::new(Vec::new())
}

/// Decode and service one hostio request for a Stylus call executing as `contract`.
///
/// Handles the state/log/transient hostios; call/create re-entrancy is left to the executor
/// (it needs to re-enter revm's frame machinery) and the account hostios follow.
pub fn handle_request<CTX>(
    ctx: &mut CTX,
    contract: Address,
    req_type: EvmApiMethod,
    req_data: Vec<u8>,
) -> (Vec<u8>, VecReader, ArbGas)
where
    CTX: ContextTr<Journal: JournalTr>,
{
    let debug = std::env::var("STYLUS_GAS_DEBUG").is_ok();
    let result = match req_type {
        // SLOAD: req = key(32) → value(32) + sload gas.
        EvmApiMethod::GetBytes32 => {
            let key = word(&req_data);
            let loaded = ctx
                .journal_mut()
                .sload(contract, key)
                .map(|l| (l.data, l.is_cold))
                .unwrap_or_default();
            let gas = if loaded.1 {
                COLD_SLOAD_COST
            } else {
                WARM_STORAGE_READ_COST
            };
            (loaded.0.to_be_bytes::<32>().to_vec(), empty_reader(), ArbGas(gas))
        }

        // SSTORE batch: req = gas_left(8) ++ [key(32) ++ value(32)]* → status + total gas.
        EvmApiMethod::SetTrieSlots => {
            let mut total = 0u64;
            let mut off = 8;
            while off + 64 <= req_data.len() {
                let key = word(&req_data[off..]);
                let value = word(&req_data[off + 32..]);
                off += 64;
                if let Ok(load) = ctx.journal_mut().sstore(contract, key, value) {
                    total += sstore_cost(&load.data, load.is_cold);
                }
            }
            (ok_status(), empty_reader(), ArbGas(total))
        }

        // TLOAD: req = key(32) → value(32).
        EvmApiMethod::GetTransientBytes32 => {
            let value = ctx.journal_mut().tload(contract, word(&req_data));
            (value.to_be_bytes::<32>().to_vec(), empty_reader(), ArbGas(0))
        }

        // TSTORE: req = key(32) ++ value(32) → status.
        EvmApiMethod::SetTransientBytes32 => {
            let key = word(&req_data);
            let value = word(&req_data[32..]);
            ctx.journal_mut().tstore(contract, key, value);
            (ok_status(), empty_reader(), ArbGas(0))
        }

        // LOG: req = n_topics(4) ++ [topic(32)]*n ++ data → empty on success.
        EvmApiMethod::EmitLog => {
            let n_topics = u32::from_be_bytes(req_data[..4].try_into().unwrap()) as usize;
            let body = &req_data[4..];
            let topics = (0..n_topics)
                .map(|i| revm::primitives::B256::from_slice(&body[i * 32..i * 32 + 32]))
                .collect::<Vec<_>>();
            let data = Bytes::copy_from_slice(&body[n_topics * 32..]);
            ctx.journal_mut().log(Log {
                address: contract,
                data: LogData::new_unchecked(topics, data),
            });
            (Vec::new(), empty_reader(), ArbGas(0))
        }

        // BALANCE: req = address(20) → balance(32) + access gas.
        EvmApiMethod::AccountBalance => {
            let address = Address::from_slice(&req_data[..20]);
            let (balance, is_cold) = ctx
                .journal_mut()
                .load_account(address)
                .map(|acc| (acc.data.info.balance, acc.is_cold))
                .unwrap_or_default();
            let gas = if is_cold {
                COLD_ACCOUNT_ACCESS_COST
            } else {
                WARM_STORAGE_READ_COST
            };
            (balance.to_be_bytes::<32>().to_vec(), empty_reader(), ArbGas(gas))
        }

        // EXTCODEHASH: req = address(20) → codehash(32) + access gas.
        EvmApiMethod::AccountCodeHash => {
            let address = Address::from_slice(&req_data[..20]);
            let (code_hash, is_cold) = ctx
                .journal_mut()
                .load_account_with_code(address)
                .map(|acc| (acc.data.info.code_hash, acc.is_cold))
                .unwrap_or_default();
            let gas = if is_cold {
                COLD_ACCOUNT_ACCESS_COST
            } else {
                WARM_STORAGE_READ_COST
            };
            (code_hash.0.to_vec(), empty_reader(), ArbGas(gas))
        }

        // Not yet wired: account_code (returns code via the reader), add_pages, capture, and
        // the call/create family (handled by the executor's frame re-entry). TODO(stage 2).
        _ => (Vec::new(), empty_reader(), ArbGas(0)),
    };
    if debug {
        eprintln!(
            "[stylus-hostio] {:?} req_len={} -> resp_len={} gas={}",
            req_type,
            req_data.len(),
            result.0.len(),
            result.2.0,
        );
    }
    result
}
