use crate::{
    ArbSpecId,
    util::{inverse_remap_l1_address, remap_l1_address},
};
use arb_alloy_precompiles::addresses::{
    ARB_ADDRESS_TABLE, ARB_AGGREGATOR, ARB_BLS, ARB_DEBUG, ARB_FILTERED_TRANSACTIONS_MANAGER,
    ARB_FUNCTION_TABLE, ARB_GAS_INFO, ARB_INFO, ARB_NATIVE_TOKEN_MANAGER, ARB_OWNER,
    ARB_OWNER_PUBLIC, ARB_RETRYABLE_TX, ARB_STATISTICS, ARB_SYS, ARB_WASM, ARB_WASM_CACHE,
};
use alloy_core::sol_types::SolCall;
use revm::{
    context_interface::{ContextTr, JournalTr},
    handler::{EthPrecompiles, PrecompileProvider},
    interpreter::{InstructionResult, InterpreterResult},
    precompile::{Precompiles, secp256r1::P256VERIFY},
    primitives::Address,
};
use std::sync::OnceLock;

mod arb_address_table;
mod arb_aggregator;
mod arb_bls;
mod arb_debug;
mod arb_filtered_transactions_manager;
mod arb_function_table;
mod arb_gas_info;
mod arb_info;
mod arb_native_token_manager;
mod arb_owner;
mod arb_owner_public;
mod arb_retryable_tx;
mod arb_statistics;
mod arb_sys;
mod arb_wasm;
mod arb_wasm_cache;
mod common;

use self::common::{ok_result, revert_result};
pub(super) use crate::{ArbosState, storage::RETRYABLE_LIFETIME_SECONDS};
pub(super) use alloy_core::sol_types::SolInterface;
pub(super) use arb_alloy_precompiles::{
    ArbAddressTable, ArbAggregator, ArbDebug, ArbFunctionTable, ArbGasInfo, ArbInfo, ArbOwner,
    ArbOwnerPublic, ArbRetryableTx, ArbStatistics, ArbSys, ArbWasm, ArbWasmCache,
};
pub(super) use revm::primitives::U256;

use arb_address_table::run_arb_address_table;
use arb_aggregator::run_arb_aggregator;
use arb_bls::run_arb_bls;
use arb_debug::run_arb_debug;
use arb_filtered_transactions_manager::run_arb_filtered_transactions_manager;
use arb_function_table::run_arb_function_table;
use arb_gas_info::run_arb_gas_info;
use arb_info::run_arb_info;
use arb_native_token_manager::run_arb_native_token_manager;
use arb_owner::run_arb_owner;
use arb_owner_public::run_arb_owner_public;
use arb_retryable_tx::run_arb_retryable_tx;
use arb_statistics::run_arb_statistics;
use arb_sys::run_arb_sys;
use arb_wasm::run_arb_wasm;
use arb_wasm_cache::run_arb_wasm_cache;
use common::input_bytes;

// Stylus params are now read/written through the packed word helpers in
// ArbosPrograms::read_params_word / write_params_word (see storage/programs.rs).
// The old per-slot WASM_*_OFFSET constants have been removed.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArbPrecompilesEnum {
    ArbSys,
    ArbInfo,
    ArbAddressTable,
    ArbBls,
    ArbFunctionTable,
    ArbOwnerPublic,
    ArbGasInfo,
    ArbAggregator,
    ArbRetryableTx,
    ArbStatistics,
    ArbOwner,
    ArbWasm,
    ArbWasmCache,
    ArbNativeTokenManager,
    ArbFilteredTransactionsManager,
    ArbDebug,
}

impl ArbPrecompilesEnum {
    #[inline]
    pub fn from_address(address: &Address) -> Option<Self> {
        match *address {
            ARB_SYS => Some(Self::ArbSys),
            ARB_INFO => Some(Self::ArbInfo),
            ARB_ADDRESS_TABLE => Some(Self::ArbAddressTable),
            ARB_BLS => Some(Self::ArbBls),
            ARB_FUNCTION_TABLE => Some(Self::ArbFunctionTable),
            ARB_OWNER_PUBLIC => Some(Self::ArbOwnerPublic),
            ARB_GAS_INFO => Some(Self::ArbGasInfo),
            ARB_AGGREGATOR => Some(Self::ArbAggregator),
            ARB_RETRYABLE_TX => Some(Self::ArbRetryableTx),
            ARB_STATISTICS => Some(Self::ArbStatistics),
            ARB_OWNER => Some(Self::ArbOwner),
            ARB_WASM => Some(Self::ArbWasm),
            ARB_WASM_CACHE => Some(Self::ArbWasmCache),
            ARB_NATIVE_TOKEN_MANAGER => Some(Self::ArbNativeTokenManager),
            ARB_FILTERED_TRANSACTIONS_MANAGER => Some(Self::ArbFilteredTransactionsManager),
            ARB_DEBUG => Some(Self::ArbDebug),
            _ => None,
        }
    }

    pub fn run<CTX>(
        &self,
        ctx: &mut CTX,
        inputs: &revm::interpreter::CallInputs,
    ) -> InterpreterResult
    where
        CTX: ContextTr<Journal: JournalTr>,
    {
        let raw = input_bytes(ctx, &inputs.input);
        let gas_limit = inputs.gas_limit;
        match self {
            Self::ArbSys => run_arb_sys(ctx, &raw, gas_limit, inputs),
            Self::ArbInfo => run_arb_info(ctx, &raw, gas_limit),
            Self::ArbAddressTable => run_arb_address_table(ctx, &raw, gas_limit),
            Self::ArbBls => run_arb_bls(ctx, &raw, gas_limit),
            Self::ArbFunctionTable => run_arb_function_table(ctx, &raw, gas_limit),
            Self::ArbOwnerPublic => run_arb_owner_public(ctx, &raw, gas_limit),
            Self::ArbGasInfo => run_arb_gas_info(ctx, &raw, gas_limit),
            Self::ArbAggregator => run_arb_aggregator(ctx, &raw, gas_limit),
            Self::ArbRetryableTx => run_arb_retryable_tx(ctx, &raw, gas_limit, inputs),
            Self::ArbStatistics => run_arb_statistics(ctx, &raw, gas_limit),
            Self::ArbOwner => run_arb_owner(ctx, &raw, gas_limit, inputs),
            Self::ArbWasm => run_arb_wasm(ctx, &raw, gas_limit),
            Self::ArbWasmCache => run_arb_wasm_cache(ctx, &raw, gas_limit, inputs),
            Self::ArbNativeTokenManager => run_arb_native_token_manager(ctx, &raw, gas_limit),
            Self::ArbFilteredTransactionsManager => {
                run_arb_filtered_transactions_manager(ctx, &raw, gas_limit)
            }
            Self::ArbDebug => run_arb_debug(ctx, &raw, gas_limit),
        }
    }

    pub fn all_addresses() -> impl Iterator<Item = Address> {
        [
            ARB_SYS,
            ARB_INFO,
            ARB_ADDRESS_TABLE,
            ARB_BLS,
            ARB_FUNCTION_TABLE,
            ARB_OWNER_PUBLIC,
            ARB_GAS_INFO,
            ARB_AGGREGATOR,
            ARB_RETRYABLE_TX,
            ARB_STATISTICS,
            ARB_OWNER,
            ARB_WASM,
            ARB_WASM_CACHE,
            ARB_NATIVE_TOKEN_MANAGER,
            ARB_FILTERED_TRANSACTIONS_MANAGER,
            ARB_DEBUG,
        ]
        .into_iter()
    }
}

/// Per-word copy gas (Nitro `params.CopyGas`), charged for precompile arg/result data.
const COPY_GAS: u64 = 3;
/// Cost of the single ArbOS storage read that `makeContext`'s `OpenArbosState`
/// performs on every non-pure precompile call (Nitro `StorageReadCost`,
/// `= params.SloadGasEIP2200`). Pure methods skip `OpenArbosState`.
const ARBOS_STATE_OPEN_GAS: u64 = 800;

#[inline]
fn words_for_bytes(n: usize) -> u64 {
    (n as u64).div_ceil(32)
}

/// Gas an ArbOS precompile call charges beyond its EVM-visible work, mirroring
/// `nitro/precompiles/precompile.go Call`:
///   argsCost   = CopyGas * words(len(input) - 4)
///   resultCost = CopyGas * words(len(output))
///   + OpenArbosState read (800) for every non-pure method.
///
/// Note: storage reads/writes performed *inside* a method body (beyond the
/// state-open) are not yet metered here — only the per-call costs are. Read-only
/// getters (the common case) do no extra storage I/O, so this is exact for them.
fn arbos_call_extra_gas(
    arb: ArbPrecompilesEnum,
    input_len: usize,
    output_len: usize,
    selector: Option<[u8; 4]>,
) -> u64 {
    let args_cost = COPY_GAS * words_for_bytes(input_len.saturating_sub(4));
    let result_cost = COPY_GAS * words_for_bytes(output_len);
    let is_pure = arb == ArbPrecompilesEnum::ArbSys
        && selector == Some(ArbSys::mapL1SenderContractAddressToL2AliasCall::SELECTOR);
    let state_open = if is_pure { 0 } else { ARBOS_STATE_OPEN_GAS };
    args_cost + result_cost + state_open
}

/// Eth precompile set for an ArbOS version, mirroring Nitro's `activePrecompiledContracts`
/// (`gethhook/geth-hook.go`): ArbOS 30-49 = Cancun (0x01-0x0a, NO BLS) + the standalone
/// secp256r1 P256VERIFY (RIP-7212, 0x100); ArbOS 50+ (`IsDia`) = Osaka (Prague + BLS + P256 +
/// EIP-7823/7883 modexp). arb_revm targets ArbOS 40+. NOTE: this is keyed on the ArbOS
/// version, NOT the eth spec — at ArbOS 40-51 the eth spec is Prague throughout, but the
/// precompile set flips at the ArbOS 50 boundary.
fn arb_eth_precompiles(spec: ArbSpecId) -> &'static Precompiles {
    if spec.arbos_version() >= 50 {
        Precompiles::osaka()
    } else {
        static ARBOS30: OnceLock<Precompiles> = OnceLock::new();
        ARBOS30.get_or_init(|| {
            let mut precompiles = Precompiles::cancun().clone();
            precompiles.extend([P256VERIFY]);
            precompiles
        })
    }
}

#[derive(Debug, Clone)]
pub struct ArbPrecompiles {
    pub inner: EthPrecompiles,
    /// Whether `inner.precompiles` is the ArbOS 50+ (`IsDia`/Osaka) set. Tracked separately
    /// because the precompile-set boundary (ArbOS 50) does not coincide with an eth-spec change.
    is_dia: bool,
}

impl ArbPrecompiles {
    pub fn new_with_spec(spec: ArbSpecId) -> Self {
        let mut inner = EthPrecompiles::new(spec.into());
        inner.precompiles = arb_eth_precompiles(spec);
        Self {
            inner,
            is_dia: spec.arbos_version() >= 50,
        }
    }
}

impl Default for ArbPrecompiles {
    fn default() -> Self {
        Self::new_with_spec(ArbSpecId::default())
    }
}

impl<CTX> PrecompileProvider<CTX> for ArbPrecompiles
where
    CTX: ContextTr<Journal: JournalTr, Cfg: revm::context::Cfg<Spec = ArbSpecId>>,
{
    type Output = InterpreterResult;

    fn set_spec(&mut self, spec: <CTX::Cfg as revm::context::Cfg>::Spec) -> bool {
        let eth_spec = spec.into();
        let is_dia = spec.arbos_version() >= 50;
        // The precompile set flips at the ArbOS 50 (IsDia) boundary even though the eth spec
        // (Prague) is unchanged, so the bucket must be compared in addition to the eth spec.
        if eth_spec == self.inner.spec && is_dia == self.is_dia {
            return false;
        }
        self.inner.precompiles = arb_eth_precompiles(spec);
        self.inner.spec = eth_spec;
        self.is_dia = is_dia;
        true
    }

    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &revm::interpreter::CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        if let Some(arb) = ArbPrecompilesEnum::from_address(&inputs.bytecode_address) {
            let raw = input_bytes(context, &inputs.input);
            let input_len = raw.len();
            let selector: Option<[u8; 4]> = raw.get(..4).map(|s| s.try_into().unwrap());
            drop(raw);
            let mut result = arb.run(context, inputs);
            // Nitro charges per-call precompile gas (arg/result copy + ArbosState open)
            // that has no EVM-opcode representation; fold it into the returned gas so
            // the CALL's net cost matches Nitro.
            let extra = arbos_call_extra_gas(arb, input_len, result.output.len(), selector);
            if !result.gas.record_cost(extra) {
                result.result = InstructionResult::OutOfGas;
                result.output = Default::default();
            }
            return Ok(Some(result));
        }
        self.inner.run(context, inputs)
    }

    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        Box::new(
            self.inner
                .warm_addresses()
                .chain(ArbPrecompilesEnum::all_addresses()),
        )
    }

    fn contains(&self, address: &Address) -> bool {
        ArbPrecompilesEnum::from_address(address).is_some() || self.inner.contains(address)
    }
}
