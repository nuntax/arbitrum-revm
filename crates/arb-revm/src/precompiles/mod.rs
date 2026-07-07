use crate::{
    ArbSpecId,
    arb_journal::{ArbCall, ArbPrecompileCtx},
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
    interpreter::{Gas, InstructionResult, InterpreterResult},
    precompile::{Precompiles, secp256r1::P256VERIFY},
    primitives::{Address, AddressSet},
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

use self::common::{empty_active_result, gated_revert_result, ok_result, revert_result};
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

    /// Full ArbOS-precompile entry, shared by the in-EVM dispatcher and the node `DynPrecompile`:
    /// ArbOS version + per-method gating (Nitro `precompile.go` Call), body dispatch, and the
    /// Nitro per-call gas (arg/result copy + ArbosState open) that has no EVM-opcode representation.
    /// Generic over [`ArbPrecompileCtx`] so it runs over either a revm `Context` or `ArbNodeCtx`.
    pub fn run_dispatch<CTX>(&self, ctx: &mut CTX, call: &ArbCall) -> InterpreterResult
    where
        CTX: ArbPrecompileCtx,
    {
        let arb = *self;
        let gas_limit = call.gas_limit;
        // ArbOS version-gating: read the version once (the dispatcher read is free, not part of the
        // method's charged gas). Both gated paths return BEFORE arbos_call_extra_gas, matching Nitro
        // which returns before makeContext.
        let arbos_version = ArbosState::open()
            .arbos_version
            .get(ctx.journal_mut())
            .unwrap_or(0);
        // Precompile not yet active => behaves like an account with no code.
        if arbos_version < precompile_min_arbos_version(arb) {
            return empty_active_result(gas_limit);
        }
        let input_len = call.input.len();
        let selector: Option<[u8; 4]> = call.input.get(..4).map(|s| s.try_into().unwrap());
        // Invalid selector, or method not active at this ArbOS version => revert (all gas).
        let method_gated = match selector {
            None => true,
            Some(sel) => {
                let (min, max) = method_arbos_bounds(arb, sel);
                arbos_version < min || (max > 0 && arbos_version > max)
            }
        };
        if method_gated {
            return gated_revert_result(gas_limit);
        }
        let mut result = self.dispatch(ctx, call);
        // ArbOwner is wrapped by Nitro's `OwnerPrecompile`, which returns `multigas.ZeroGas()`
        // the chain owner is NEVER charged for an ArbOwner call (success or revert), so it pays
        // neither the method gas nor the per-call arg/result-copy + ArbosState-open gas. Reset to
        // zero-spent and skip the extra. (ArbDebug's `DebugPrecompile` does NOT do this, it charges
        // normally, so only ArbOwner is exempt.)
        if arb == ArbPrecompilesEnum::ArbOwner {
            result.gas = Gas::new(gas_limit);
            return result;
        }
        // Fold the per-call precompile gas (arg/result copy + ArbosState open) into the returned
        // gas so the CALL's net cost matches Nitro.
        let extra = arbos_call_extra_gas(arb, input_len, result.output.len(), selector);
        if !result.gas.record_regular_cost(extra) {
            result.result = InstructionResult::OutOfGas;
            result.output = Default::default();
        }
        result
    }

    /// Path-agnostic method dispatch (no gating, no per-call gas, see [`Self::run_dispatch`]).
    /// `call.input` is the already-resolved calldata; the immediate-call fields (caller/value/
    /// bytecode_address) come from [`ArbCall`].
    pub fn dispatch<CTX>(&self, ctx: &mut CTX, call: &ArbCall) -> InterpreterResult
    where
        CTX: ArbPrecompileCtx,
    {
        let raw = call.input;
        let gas_limit = call.gas_limit;
        match self {
            Self::ArbSys => run_arb_sys(ctx, raw, gas_limit, call),
            Self::ArbInfo => run_arb_info(ctx, raw, gas_limit),
            Self::ArbAddressTable => run_arb_address_table(ctx, raw, gas_limit),
            Self::ArbBls => run_arb_bls(ctx, raw, gas_limit),
            Self::ArbFunctionTable => run_arb_function_table(ctx, raw, gas_limit),
            Self::ArbOwnerPublic => run_arb_owner_public(ctx, raw, gas_limit),
            Self::ArbGasInfo => run_arb_gas_info(ctx, raw, gas_limit),
            Self::ArbAggregator => run_arb_aggregator(ctx, raw, gas_limit),
            Self::ArbRetryableTx => run_arb_retryable_tx(ctx, raw, gas_limit, call),
            Self::ArbStatistics => run_arb_statistics(ctx, raw, gas_limit),
            Self::ArbOwner => run_arb_owner(ctx, raw, gas_limit, call),
            Self::ArbWasm => run_arb_wasm(ctx, raw, gas_limit),
            Self::ArbWasmCache => run_arb_wasm_cache(ctx, raw, gas_limit, call),
            Self::ArbNativeTokenManager => run_arb_native_token_manager(ctx, raw, gas_limit),
            Self::ArbFilteredTransactionsManager => {
                run_arb_filtered_transactions_manager(ctx, raw, gas_limit)
            }
            Self::ArbDebug => run_arb_debug(ctx, raw, gas_limit),
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
/// state-open) are not yet metered here, only the per-call costs are. Read-only
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

/// ArbOS version at which a whole precompile becomes active. Below it, calling the address
/// behaves like calling an account with no code (empty success, no gas). Mirrors the per-precompile
/// `arbosVersion` set in Nitro `precompiles/precompile.go` init().
fn precompile_min_arbos_version(arb: ArbPrecompilesEnum) -> u64 {
    match arb {
        ArbPrecompilesEnum::ArbWasm | ArbPrecompilesEnum::ArbWasmCache => 30, // ArbosVersion_Stylus
        ArbPrecompilesEnum::ArbNativeTokenManager => 41,
        ArbPrecompilesEnum::ArbFilteredTransactionsManager => 60, // ArbosVersion_TransactionFiltering
        _ => 0,
    }
}

/// `(minArbosVersion, maxArbosVersion)` for a precompile method (max 0 = no upper bound). A call
/// with `version < min` or `version > max>0` reverts (Nitro `precompile.go` Call, method gate).
/// Only methods with a non-default bound that are also decodable by our sol interfaces need an
/// entry, methods absent from the interface already revert via `abi_decode` failure. Covers the
/// 40→51 gates (native-token v41, gas methods v50, and `cacheCodehash` removed after v30).
fn method_arbos_bounds(arb: ArbPrecompilesEnum, sel: [u8; 4]) -> (u64, u64) {
    match arb {
        ArbPrecompilesEnum::ArbGasInfo => {
            if sel == ArbGasInfo::getMaxTxGasLimitCall::SELECTOR
                || sel == ArbGasInfo::getMaxBlockGasLimitCall::SELECTOR
            {
                return (50, 0);
            }
            (0, 0)
        }
        ArbPrecompilesEnum::ArbOwner => {
            if sel == ArbOwner::addNativeTokenOwnerCall::SELECTOR
                || sel == ArbOwner::removeNativeTokenOwnerCall::SELECTOR
                || sel == ArbOwner::setNativeTokenManagementFromCall::SELECTOR
            {
                return (41, 0);
            }
            if sel == ArbOwner::setGasBacklogCall::SELECTOR
                || sel == ArbOwner::setMaxBlockGasLimitCall::SELECTOR
                || sel == ArbOwner::setParentGasFloorPerTokenCall::SELECTOR
            {
                return (50, 0);
            }
            (0, 0)
        }
        ArbPrecompilesEnum::ArbOwnerPublic => {
            if sel == ArbOwnerPublic::isNativeTokenOwnerCall::SELECTOR
                || sel == ArbOwnerPublic::getAllNativeTokenOwnersCall::SELECTOR
            {
                return (41, 0);
            }
            if sel == ArbOwnerPublic::getParentGasFloorPerTokenCall::SELECTOR
                || sel == ArbOwnerPublic::getNativeTokenManagementFromCall::SELECTOR
            {
                return (50, 0);
            }
            (0, 0)
        }
        ArbPrecompilesEnum::ArbWasmCache => {
            // `cacheCodehash` (maxArbosVersion=30) was dropped from our interface entirely, so it
            // already reverts via abi_decode failure. `cacheProgram` is v31 (StylusFixes).
            if sel == ArbWasmCache::cacheProgramCall::SELECTOR {
                return (31, 0);
            }
            (0, 0)
        }
        _ => (0, 0),
    }
}

/// Eth precompile set for an ArbOS version, mirroring Nitro's `activePrecompiledContracts`
/// (`gethhook/geth-hook.go`): ArbOS 30-49 = Cancun (0x01-0x0a, NO BLS) + the standalone
/// secp256r1 P256VERIFY (RIP-7212, 0x100); ArbOS 50+ (`IsDia`) = Osaka (Prague + BLS + P256 +
/// EIP-7823/7883 modexp). arb_revm targets ArbOS 40+. NOTE: this is keyed on the ArbOS
/// version, NOT the eth spec, at ArbOS 40-51 the eth spec is Prague throughout, but the
/// precompile set flips at the ArbOS 50 boundary.
pub fn arb_eth_precompiles(spec: ArbSpecId) -> &'static Precompiles {
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
    /// Combined warm-address set: eth precompile addresses ∪ arb precompile addresses.
    /// Kept in sync with `inner` via `new_with_spec` and `set_spec`.
    warm: AddressSet,
}

fn build_warm_set(inner: &EthPrecompiles) -> AddressSet {
    let mut warm = AddressSet::default();
    warm.clone_from(inner.warm_addresses());
    for addr in ArbPrecompilesEnum::all_addresses() {
        warm.insert(addr);
    }
    warm
}

impl ArbPrecompiles {
    pub fn new_with_spec(spec: ArbSpecId) -> Self {
        let mut inner = EthPrecompiles::new(spec.into());
        inner.precompiles = arb_eth_precompiles(spec);
        let warm = build_warm_set(&inner);
        Self {
            inner,
            is_dia: spec.arbos_version() >= 50,
            warm,
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
        self.warm = build_warm_set(&self.inner);
        true
    }

    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &revm::interpreter::CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        if let Some(arb) = ArbPrecompilesEnum::from_address(&inputs.bytecode_address) {
            // Resolve the (possibly shared-buffer) calldata here on the in-EVM path; the node path
            // gets it pre-resolved from `PrecompileInput.data`. Gating + per-call gas live in
            // `run_dispatch`, shared with the node path.
            let raw = input_bytes(context, &inputs.input);
            let call = ArbCall {
                input: raw.as_ref(),
                gas_limit: inputs.gas_limit,
                caller: inputs.caller,
                value: inputs.call_value(),
                bytecode_address: inputs.bytecode_address,
                is_static: inputs.is_static,
            };
            return Ok(Some(arb.run_dispatch(context, &call)));
        }
        self.inner.run(context, inputs)
    }

    fn warm_addresses(&self) -> &AddressSet {
        &self.warm
    }

    fn contains(&self, address: &Address) -> bool {
        ArbPrecompilesEnum::from_address(address).is_some() || self.inner.contains(address)
    }
}

#[cfg(test)]
mod gating_tests {
    // Note: do NOT `use ArbPrecompilesEnum::*` here, the variant names (ArbGasInfo, ArbOwner, …)
    // would shadow the sol-interface modules of the same name and break `Module::methodCall::SELECTOR`.
    use super::{ArbPrecompilesEnum as E, method_arbos_bounds, precompile_min_arbos_version};
    use super::{ArbGasInfo, ArbOwner, ArbOwnerPublic};
    use alloy_core::sol_types::SolCall;

    #[test]
    fn precompile_level_gates() {
        assert_eq!(precompile_min_arbos_version(E::ArbNativeTokenManager), 41);
        assert_eq!(precompile_min_arbos_version(E::ArbFilteredTransactionsManager), 60);
        assert_eq!(precompile_min_arbos_version(E::ArbWasm), 30);
        assert_eq!(precompile_min_arbos_version(E::ArbWasmCache), 30);
        assert_eq!(precompile_min_arbos_version(E::ArbGasInfo), 0);
        assert_eq!(precompile_min_arbos_version(E::ArbSys), 0);
    }

    #[test]
    fn method_level_gates() {
        // v50 ArbGasInfo gas-limit getters
        assert_eq!(
            method_arbos_bounds(E::ArbGasInfo, ArbGasInfo::getMaxTxGasLimitCall::SELECTOR),
            (50, 0)
        );
        assert_eq!(
            method_arbos_bounds(E::ArbGasInfo, ArbGasInfo::getMaxBlockGasLimitCall::SELECTOR),
            (50, 0)
        );
        // v41 native-token methods
        assert_eq!(
            method_arbos_bounds(E::ArbOwner, ArbOwner::addNativeTokenOwnerCall::SELECTOR),
            (41, 0)
        );
        assert_eq!(
            method_arbos_bounds(
                E::ArbOwnerPublic,
                ArbOwnerPublic::isNativeTokenOwnerCall::SELECTOR
            ),
            (41, 0)
        );
        // v50 ArbOwner setter
        assert_eq!(
            method_arbos_bounds(E::ArbOwner, ArbOwner::setGasBacklogCall::SELECTOR),
            (50, 0)
        );
        // not gated
        assert_eq!(
            method_arbos_bounds(E::ArbGasInfo, ArbGasInfo::getPricesInWeiCall::SELECTOR),
            (0, 0)
        );
        // a gated selector on the wrong precompile is not gated
        assert_eq!(
            method_arbos_bounds(E::ArbSys, ArbGasInfo::getMaxTxGasLimitCall::SELECTOR),
            (0, 0)
        );
    }
}
