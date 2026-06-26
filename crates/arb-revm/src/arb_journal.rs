//! The narrow state-access seam that lets ArbOS storage + precompiles run over EITHER revm's
//! in-EVM `Context`/`Journal` (the existing execution path) OR alloy-evm's [`EvmInternals`]
//! (the reth node path, reached through a `DynPrecompile` in `arb-reth-evm`).
//!
//! ## Why this exists
//! reth v2.0.0's `ConfigureEvm` hard-requires `EvmFactory<…, Precompiles = PrecompilesMap, …>`.
//! `PrecompilesMap` holds `DynPrecompile`s — boxed closures invoked with a `PrecompileInput`
//! carrying an [`EvmInternals`] state handle, NOT a revm `Context`. ArbOS precompiles, however,
//! are written against `ContextTr<Journal: JournalTr>`. This module bridges the two with two
//! traits, so the *same* precompile bodies (hence the same parity-validated logic) serve both
//! paths:
//!
//! * [`ArbJournal`] — the handful of journal ops ArbOS storage actually performs (slot read/write,
//!   balance read, balance debit, code read, log emit).
//! * [`ArbPrecompileCtx`] — what a precompile body reads beyond the journal: block
//!   basefee/number/timestamp, tx origin, and call depth.
//!
//! Both are blanket-impl'd for the in-EVM types (`JournalTr` / `ContextTr`) and concretely for the
//! node path ([`EvmInternals`] / [`ArbNodeCtx`]). Because `JournalTr ⟹ ArbJournal` and
//! `ContextTr ⟹ ArbPrecompileCtx` are strict supersets, migrating a storage accessor or a
//! precompile from the revm bound to the `Arb*` bound is fully backward-compatible with the
//! existing in-EVM dispatcher — the crate compiles and every existing test passes after each
//! individual conversion.

use alloy_evm::{EvmInternals, EvmInternalsError};
use revm::{
    context_interface::{
        Block, ContextTr, JournalTr, Transaction,
        context::SStoreResult,
        journaled_state::{StateLoad, TransferError, account::JournaledAccountTr},
    },
    database_interface::Database,
    primitives::{Address, Bytes, Log, StorageKey, StorageValue, U256},
};

/// The narrow journal surface ArbOS storage + precompiles need. See the module docs.
///
/// Slot reads/writes return the full [`StateLoad`] (carrying the cold/warm flag) to match revm's
/// own accessors exactly; the typed `StorageBacked` helpers discard the flag as before.
pub trait ArbJournal {
    /// Error surfaced by the backing state store.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Warm `account`, then read storage `slot` (mirrors `load_account` + `sload`).
    fn read_slot(
        &mut self,
        account: Address,
        slot: StorageKey,
    ) -> Result<StateLoad<StorageValue>, Self::Error>;

    /// Warm `account`, write `value` to storage `slot`, and touch the account so the write
    /// survives commit (mirrors `load_account` + `sstore` + `touch_account`).
    fn write_slot(
        &mut self,
        account: Address,
        slot: StorageKey,
        value: StorageValue,
    ) -> Result<StateLoad<SStoreResult>, Self::Error>;

    /// Current balance of `account`.
    fn account_balance(&mut self, account: Address) -> Result<U256, Self::Error>;

    /// Deployed bytecode of `account` (empty if none).
    fn account_code(&mut self, account: Address) -> Result<Bytes, Self::Error>;

    /// Debit `amount` from `account`'s balance, returning `false` if the balance is insufficient
    /// (mirrors revm `Account::decr_balance`). Used by `ArbSys` for L2->L1 value burn.
    fn debit_balance(&mut self, account: Address, amount: U256) -> Result<bool, Self::Error>;

    /// Move `amount` from `from` to `to`, returning a [`TransferError`] (e.g. out-of-funds) rather
    /// than erroring. Used by L1-pricing settlement.
    fn transfer(
        &mut self,
        from: Address,
        to: Address,
        amount: U256,
    ) -> Result<Option<TransferError>, Self::Error>;

    /// Emit a log.
    fn emit_log(&mut self, log: Log);
}

/// Blanket impl: every revm journal is an [`ArbJournal`]. This is what keeps all existing
/// `<J: JournalTr>`-bounded storage code compiling untouched.
impl<J> ArbJournal for J
where
    J: JournalTr,
{
    type Error = <J::Database as Database>::Error;

    fn read_slot(
        &mut self,
        account: Address,
        slot: StorageKey,
    ) -> Result<StateLoad<StorageValue>, Self::Error> {
        self.load_account(account)?;
        self.sload(account, slot)
    }

    fn write_slot(
        &mut self,
        account: Address,
        slot: StorageKey,
        value: StorageValue,
    ) -> Result<StateLoad<SStoreResult>, Self::Error> {
        self.load_account(account)?;
        let result = self.sstore(account, slot, value)?;
        // Touch so the storage-only change survives `DatabaseCommit`, which skips untouched accounts.
        self.touch_account(account);
        Ok(result)
    }

    fn account_balance(&mut self, account: Address) -> Result<U256, Self::Error> {
        Ok(self.load_account(account)?.data.info.balance)
    }

    fn account_code(&mut self, account: Address) -> Result<Bytes, Self::Error> {
        Ok(self.code(account)?.data)
    }

    fn debit_balance(&mut self, account: Address, amount: U256) -> Result<bool, Self::Error> {
        let mut acct = self.load_account_mut_skip_cold_load(account, false)?;
        Ok(acct.data.decr_balance(amount))
    }

    fn transfer(
        &mut self,
        from: Address,
        to: Address,
        amount: U256,
    ) -> Result<Option<TransferError>, Self::Error> {
        JournalTr::transfer(self, from, to, amount)
    }

    fn emit_log(&mut self, log: Log) {
        JournalTr::log(self, log);
    }
}

/// Node-path journal: a local newtype over alloy-evm's [`EvmInternals`] state handle.
///
/// The newtype (rather than `impl ArbJournal for EvmInternals` directly) is load-bearing: a direct
/// impl collides with the `impl<J: JournalTr> ArbJournal for J` blanket under E0119, because rustc
/// cannot prove the *foreign* `EvmInternals` does not implement `JournalTr`. For this *local* type,
/// rustc has complete knowledge of its `JournalTr` impls (there are none) and the orphan rule bars
/// any other crate from adding one — so the blanket and this impl are provably disjoint.
pub struct ArbInternals<'a, 'b>(pub &'b mut EvmInternals<'a>);

impl ArbJournal for ArbInternals<'_, '_> {
    type Error = EvmInternalsError;

    fn read_slot(
        &mut self,
        account: Address,
        slot: StorageKey,
    ) -> Result<StateLoad<StorageValue>, Self::Error> {
        self.0.load_account(account)?;
        self.0.sload(account, slot)
    }

    fn write_slot(
        &mut self,
        account: Address,
        slot: StorageKey,
        value: StorageValue,
    ) -> Result<StateLoad<SStoreResult>, Self::Error> {
        self.0.load_account(account)?;
        let result = self.0.sstore(account, slot, value)?;
        self.0.touch_account(account)?;
        Ok(result)
    }

    fn account_balance(&mut self, account: Address) -> Result<U256, Self::Error> {
        Ok(self.0.load_account(account)?.data.info.balance)
    }

    fn account_code(&mut self, account: Address) -> Result<Bytes, Self::Error> {
        let acct = self.0.load_account_code(account)?;
        Ok(acct.data.code().map(|c| c.original_bytes()).unwrap_or_default())
    }

    fn debit_balance(&mut self, account: Address, amount: U256) -> Result<bool, Self::Error> {
        let mut acct = self.0.load_account_mut_skip_cold_load(account, false)?;
        Ok(acct.data.decr_balance(amount))
    }

    fn transfer(
        &mut self,
        from: Address,
        to: Address,
        amount: U256,
    ) -> Result<Option<TransferError>, Self::Error> {
        self.0.transfer(from, to, amount)
    }

    fn emit_log(&mut self, log: Log) {
        self.0.log(log);
    }
}

/// What a precompile body reads beyond the journal. See the module docs.
pub trait ArbPrecompileCtx {
    /// The backing journal, itself an [`ArbJournal`].
    type Journal: ArbJournal;

    /// Mutable access to the journal (ArbOS storage reads/writes go through this).
    fn journal_mut(&mut self) -> &mut Self::Journal;

    /// `block.basefee` — the current L2 base fee (wei).
    fn block_basefee(&self) -> u64;

    /// `block.number`.
    fn block_number(&self) -> u64;

    /// `block.timestamp`.
    fn block_timestamp(&self) -> u64;

    /// Transaction origin (the signer), i.e. revm `tx.caller()`. NOT the immediate CALL caller —
    /// that is supplied per-call (see the dispatcher's `ArbCall`).
    fn tx_caller(&self) -> Address;

    /// Current EVM call depth (for `ArbSys.isTopLevelCall`).
    fn call_depth(&self) -> usize;
}

/// Blanket impl: every revm context is an [`ArbPrecompileCtx`]. Keeps the in-EVM dispatcher and
/// every `<CTX: ContextTr>`-bounded precompile working after migration to the `Arb*` bound.
impl<CTX> ArbPrecompileCtx for CTX
where
    CTX: ContextTr<Journal: JournalTr>,
{
    type Journal = CTX::Journal;

    fn journal_mut(&mut self) -> &mut Self::Journal {
        ContextTr::journal_mut(self)
    }

    fn block_basefee(&self) -> u64 {
        self.block().basefee()
    }

    fn block_number(&self) -> u64 {
        self.block().number().saturating_to()
    }

    fn block_timestamp(&self) -> u64 {
        self.block().timestamp().saturating_to()
    }

    fn tx_caller(&self) -> Address {
        self.tx().caller()
    }

    fn call_depth(&self) -> usize {
        ContextTr::journal_ref(self).depth()
    }
}

/// Per-call inputs an ArbOS precompile reads about the immediate CALL (as opposed to the tx or the
/// block). On the in-EVM path these come from revm's `CallInputs`; on the node path from
/// alloy-evm's `PrecompileInput`. Frozen here so both dispatchers build the same shape.
#[derive(Debug, Clone)]
pub struct ArbCall<'a> {
    /// Resolved calldata (selector + args).
    pub input: &'a [u8],
    /// Gas available to the call.
    pub gas_limit: u64,
    /// Immediate caller (the address that issued this CALL).
    pub caller: Address,
    /// Value attached to the call.
    pub value: U256,
    /// The precompile's own address (its `bytecode_address`).
    pub bytecode_address: Address,
    /// Whether the call is static (no state mutation permitted).
    pub is_static: bool,
}

/// Node-path [`ArbPrecompileCtx`]: wraps an [`ArbInternals`] journal plus the bits `EvmInternals`
/// does not itself expose (tx origin and call depth, threaded in by the `DynPrecompile` wiring).
pub struct ArbNodeCtx<'a, 'b> {
    journal: ArbInternals<'a, 'b>,
    tx_caller: Address,
    call_depth: usize,
}

impl<'a, 'b> ArbNodeCtx<'a, 'b> {
    /// Builds a node-path precompile context over an `EvmInternals` handle.
    ///
    /// `EvmInternals` exposes neither the tx origin nor the call depth, so both are passed in by
    /// the caller (the `DynPrecompile` closure). Call depth is best-effort on this path; see
    /// `ArbSys.isTopLevelCall`.
    pub fn new(internals: &'b mut EvmInternals<'a>, tx_caller: Address, call_depth: usize) -> Self {
        Self { journal: ArbInternals(internals), tx_caller, call_depth }
    }
}

impl<'a, 'b> ArbPrecompileCtx for ArbNodeCtx<'a, 'b> {
    type Journal = ArbInternals<'a, 'b>;

    fn journal_mut(&mut self) -> &mut Self::Journal {
        &mut self.journal
    }

    fn block_basefee(&self) -> u64 {
        self.journal.0.block_env().basefee()
    }

    fn block_number(&self) -> u64 {
        self.journal.0.block_number().saturating_to()
    }

    fn block_timestamp(&self) -> u64 {
        self.journal.0.block_timestamp().saturating_to()
    }

    fn tx_caller(&self) -> Address {
        self.tx_caller
    }

    fn call_depth(&self) -> usize {
        self.call_depth
    }
}
