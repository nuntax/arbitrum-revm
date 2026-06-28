//! ArbOS genesis initialization — the Rust equivalent of Nitro's
//! `arbos/arbosState/arbosstate.go::InitializeArbosState`.
//!
//! A brand-new Arbitrum chain has no downloadable genesis state: block 0's state root is the
//! *output* of running ArbOS init against the chain config. This module writes that initial
//! ArbOS state (the special storage at [`ARBOS_STATE_ADDRESS`], the fake precompile code, and
//! the genesis chain owner) so a fresh node can construct block 0 itself rather than importing a
//! snapshot.
//!
//! # Faithfulness to Nitro
//!
//! `InitializeArbosState` writes the version-1 baseline (using the V0 pricing constants) and then
//! calls `UpgradeArbosVersion(desiredVersion, firstTime=true)`. We mirror that exactly: this
//! function performs the baseline writes and then delegates to the already-validated
//! [`crate::internal_tx::upgrade_arbos_version`] to run the per-version migration cascade.
//! Verified against Nitro `arbosstate.go:217-361`, `l1pricing.go:86-117`, `l2pricing.go:58-66`,
//! `batchPoster.go`, `addressSet.go`, `storage/queue.go::InitializeQueue`. In this Nitro revision
//! the `firstTime` flag only gates v11's `ClearList` (skipped at genesis), so the runtime upgrader
//! reproduces the genesis cascade without a `first_time` parameter.
//!
//! Writes of zero to a fresh (zero) slot are SSTORE no-ops that create no trie entry, so the base
//! writes here only set the non-zero values; the zero-valued offsets Nitro "writes" are omitted.
//!
//! # Stylus/programs genesis state (ArbOS ≥ 30)
//!
//! The `programs` subspace (Subspace 8) is now fully initialized for ArbOS versions ≥ 30.
//! The v30 cascade step calls [`crate::storage::programs::ArbosPrograms::initialize`], which
//! writes the packed Stylus params word (Version=1, InkPrice=10000, MaxStackDepth=262144,
//! FreePages=2, PageGas=1000, PageLimit=128, MinInitGas=72, MinCachedInitGas=11,
//! InitCostScalar=50, CachedCostScalar=50, ExpiryDays=365, KeepaliveDays=31,
//! BlockCacheSize=32), the data-pricer fields (bytes_per_second=34865,
//! last_update_time=ArbitrumStartTime=1421388000, min_price=82928201, inertia=21360419),
//! and the cacheManagers set (no-op on a fresh trie). Subsequent Stylus-param upgrades are
//! applied in the cascade:
//! - v31: Version → 2, MinInitGas → 69 (v2MinInitGas).
//! - v40: MaxWasmSize → 131072 (becomes a stored field).
//! - v50: MaxStackDepth capped at 22000 if larger.
//! - v60: MaxFragmentCount → 2 (StylusContractLimit field added).

use revm::{
    context_interface::{
        journaled_state::account::JournaledAccountTr, ContextTr, JournalTr,
    },
    primitives::{address, Address, B256, Bytes, KECCAK_EMPTY, U256},
    state::Bytecode,
};

use crate::{
    constants::{ARBOS_ACTS_ADDRESS, ARBOS_STATE_ADDRESS, BATCH_POSTER_ADDRESS},
    internal_tx::upgrade_arbos_version,
    storage::ArbosState,
};

/// A single account's genesis state, ready to be turned into a reth genesis allocation.
///
/// All fields come directly from the ArbOS-initialized journal state and represent
/// block 0's state root contribution for this account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArbGenesisAccount {
    /// Account address.
    pub address: Address,
    /// ETH balance (wei). Zero for all ArbOS-installed accounts.
    pub balance: U256,
    /// Account nonce.
    pub nonce: u64,
    /// Deployed bytecode (original bytes). Empty for accounts without code.
    pub code: Bytes,
    /// Non-zero storage entries, sorted by slot key. Each entry is (slot, value) as `B256`.
    pub storage: Vec<(B256, B256)>,
}

/// Runs [`initialize_arbos_state`] against a fresh empty state and extracts every account
/// that was touched/written as a plain, serializable [`ArbGenesisAccount`] list.
///
/// This is the canonical way to build the genesis allocation (`alloc`) for an Arbitrum chain:
/// call it once during chain setup and feed the result into the reth genesis config.
///
/// # Correctness note
/// The extraction uses `JournalTr::finalize` which consumes the journal's dirty-set and
/// returns an `EvmState` (the revm `HashMap<Address, Account>` type). Accounts that were
/// loaded but never modified (zero balance, nonce 0, no code, no storage written) are
/// filtered out so only genuine trie entries appear in the output.
///
/// The returned `Vec` is sorted by address for deterministic output.
pub fn arb_genesis_accounts(
    config: &ArbosInitConfig,
) -> Result<Vec<ArbGenesisAccount>, String> {
    use crate::api::default_ctx::{ArbContext, DefaultArb};
    use revm::database_interface::EmptyDB;

    let mut ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();
    initialize_arbos_state(config, ctx.journal_mut())?;

    // `JournalTr::finalize(&mut self) -> Self::State` — returns the dirty EvmState.
    // For `ArbContext<EmptyDB>`, `State = revm::state::EvmState = AddressMap<Account>`.
    let evm_state = ctx.journal_mut().finalize();

    Ok(evm_state_to_genesis_accounts(evm_state))
}

/// Converts a finalized revm [`EvmState`](revm::state::EvmState) into the sorted, trie-relevant
/// [`ArbGenesisAccount`] list: drops fully-empty accounts (no code/balance/nonce/storage — they
/// produce no trie entry per EIP-161), filters zero-valued storage slots, and sorts both the
/// per-account storage (by slot) and the account list (by address) for deterministic output.
fn evm_state_to_genesis_accounts(
    evm_state: revm::state::EvmState,
) -> Vec<ArbGenesisAccount> {
    let mut accounts: Vec<ArbGenesisAccount> = evm_state
        .iter()
        .filter_map(|(addr, account)| {
            let info = &account.info;
            let has_code = info.code_hash != KECCAK_EMPTY;
            let has_balance = info.balance != U256::ZERO;
            let has_nonce = info.nonce != 0;

            // Collect non-zero storage slots.
            let mut storage: Vec<(B256, B256)> = account
                .storage
                .iter()
                .filter(|(_slot, evm_slot)| evm_slot.present_value() != U256::ZERO)
                .map(|(slot, evm_slot)| {
                    (
                        B256::from(slot.to_be_bytes()),
                        B256::from(evm_slot.present_value().to_be_bytes()),
                    )
                })
                .collect();
            let has_storage = !storage.is_empty();

            // Skip fully-empty accounts — they produce no trie entry.
            if !has_code && !has_balance && !has_nonce && !has_storage {
                return None;
            }

            // Sort storage by slot for determinism.
            storage.sort_by_key(|(slot, _)| *slot);

            // Extract bytecode original bytes (empty Bytes if no code).
            let code = if has_code {
                info.code
                    .as_ref()
                    .map(|b| b.original_bytes())
                    .unwrap_or_default()
            } else {
                Bytes::new()
            };

            Some(ArbGenesisAccount {
                address: *addr,
                balance: info.balance,
                nonce: info.nonce,
                code,
                storage,
            })
        })
        .collect();

    // Sort by address for deterministic output.
    accounts.sort_by_key(|a| a.address);

    accounts
}

/// A classic-state account to import at genesis — Nitro
/// `statetransfer.AccountInitializationInfo` (the JSON `accounts.json` records).
#[derive(Debug, Clone)]
pub struct GenesisAccountInput {
    /// Account address.
    pub address: Address,
    /// ETH balance (wei).
    pub balance: U256,
    /// Account nonce.
    pub nonce: u64,
    /// Deployed bytecode (original bytes); empty for EOAs.
    pub code: Bytes,
    /// Non-zero storage slots `(slot, value)`.
    pub storage: Vec<(B256, B256)>,
}

/// A retryable ticket to seed at genesis — Nitro
/// `statetransfer.InitializationDataForRetryable` (the JSON `retryables.json` records).
#[derive(Debug, Clone)]
pub struct GenesisRetryableInput {
    /// Ticket id.
    pub id: B256,
    /// Expiry timestamp (seconds).
    pub timeout: u64,
    /// Ticket creator.
    pub from: Address,
    /// Destination (`None` == contract-creation / nil address).
    pub to: Option<Address>,
    /// Escrowed call value (wei).
    pub callvalue: U256,
    /// Beneficiary credited on expiry / cancellation.
    pub beneficiary: Address,
    /// Retry calldata.
    pub calldata: Bytes,
}

/// `keccak256("retryable escrow" ++ ticket_id)[12:]` — the per-ticket escrow address that holds a
/// live retryable's call value (Nitro `retryables.RetryableEscrowAddress`).
fn genesis_retryable_escrow_address(ticket_id: B256) -> Address {
    let mut preimage = Vec::with_capacity(16 + 32);
    preimage.extend_from_slice(b"retryable escrow");
    preimage.extend_from_slice(ticket_id.as_slice());
    Address::from_word(revm::primitives::keccak256(preimage))
}

/// Builds the **full Arbitrum One mainnet genesis state** by reproducing Nitro's
/// `arbos/arbosState/initialize.go::InitializeArbosInDatabase` against a fresh empty state, then
/// extracting every trie account as an [`ArbGenesisAccount`].
///
/// Steps (Nitro order — order matters for balance collisions):
/// 1. [`initialize_arbos_state`] — ArbOS baseline (incl. chain-owner add).
/// 2. Register every `address_table` entry in order (sequential slots).
/// 3. Retryables: expired (`timeout <= current_timestamp`) credit the beneficiary with the call
///    value; live ones are sorted by `(timeout, id)`, get their call value escrowed, and are
///    written as records + enqueued in the timeout queue.
/// 4. Import each classic account: balance / nonce / code / storage. `SetBalance` **overwrites**,
///    so an account also credited as a retryable beneficiary keeps its `accounts.json` balance
///    (escrow addresses never collide with the classic dump, so their credits survive).
///
/// `accounts` is consumed lazily (streamed) — only the finalized [`EvmState`](revm::state::EvmState)
/// is held in memory, which the caller's machine must fit (≈1.29M accounts for Arbitrum One).
pub fn build_mainnet_genesis_accounts<AT, RT, AC>(
    config: &ArbosInitConfig,
    address_table: AT,
    retryables: RT,
    accounts: AC,
    current_timestamp: u64,
) -> Result<Vec<ArbGenesisAccount>, String>
where
    AT: IntoIterator<Item = Address>,
    RT: IntoIterator<Item = GenesisRetryableInput>,
    AC: IntoIterator<Item = GenesisAccountInput>,
{
    use crate::api::default_ctx::{ArbContext, DefaultArb};
    use revm::database_interface::EmptyDB;

    let mut ctx = <ArbContext<EmptyDB> as DefaultArb>::arb();
    let journal = ctx.journal_mut();

    // 1. ArbOS baseline (precompile codes, pricing, chain owner, chain config, ...).
    initialize_arbos_state(config, journal)?;
    let state = ArbosState::open();

    // 2. Address table — register in file order; slot N goes to the N-th address.
    for (i, addr) in address_table.into_iter().enumerate() {
        let slot = state
            .address_table
            .register(addr, journal)
            .map_err(|e| format!("[ARBITRUM] failed to register address-table entry {i}: {e}"))?;
        if slot != i as u64 {
            return Err(format!(
                "[ARBITRUM] address-table slot mismatch at entry {i}: got slot {slot}"
            ));
        }
    }

    // 3. Retryables. Expired tickets just refund the beneficiary; live tickets are recreated.
    let mut live: Vec<GenesisRetryableInput> = Vec::new();
    for r in retryables {
        if r.timeout <= current_timestamp {
            if r.callvalue != U256::ZERO {
                credit_balance(journal, r.beneficiary, r.callvalue)?;
            }
        } else {
            live.push(r);
        }
    }
    // Nitro sorts the survivors by (timeout, then ticket id as a big integer). B256 orders
    // lexicographically big-endian == big-integer order.
    live.sort_by(|a, b| a.timeout.cmp(&b.timeout).then_with(|| a.id.cmp(&b.id)));
    for r in live {
        if r.callvalue != U256::ZERO {
            let escrow = genesis_retryable_escrow_address(r.id);
            credit_balance(journal, escrow, r.callvalue)?;
        }
        let rec = state.retryables.retryable(r.id);
        let e = |what: &'static str| move |err| format!("[ARBITRUM] retryable {what}: {err}");
        rec.num_tries.set(0, journal).map_err(e("numTries"))?;
        rec.from.set(r.from, journal).map_err(e("from"))?;
        rec.set_to(r.to, journal).map_err(e("to"))?;
        rec.callvalue.set(r.callvalue, journal).map_err(e("callvalue"))?;
        rec.beneficiary.set(r.beneficiary, journal).map_err(e("beneficiary"))?;
        rec.timeout.set(r.timeout, journal).map_err(e("timeout"))?;
        rec.timeout_windows_left.set(0, journal).map_err(e("timeoutWindowsLeft"))?;
        rec.calldata.set_fresh(r.calldata.as_ref(), journal).map_err(e("calldata"))?;
        state
            .retryables
            .timeout_queue
            .put(r.id, journal)
            .map_err(|e| format!("[ARBITRUM] failed to enqueue retryable timeout: {e}"))?;
    }

    // 4. Classic accounts — balance / nonce / code / storage (SetBalance overwrites collisions).
    //    All mutators on the journaled-account handle touch the account, so it survives finalize.
    for acct in accounts {
        let loaded = journal
            .load_account_mut(acct.address)
            .map_err(|e| format!("[ARBITRUM] failed to load account {}: {e}", acct.address))?;
        let mut a = loaded.data;
        a.set_balance(acct.balance);
        a.set_nonce(acct.nonce);
        if !acct.code.is_empty() {
            a.set_code_and_hash_slow(Bytecode::new_raw(acct.code.clone()));
        }
        for (slot, value) in &acct.storage {
            a.sstore(
                U256::from_be_bytes(slot.0),
                U256::from_be_bytes(value.0),
                false,
            )
            .map_err(|e| {
                format!("[ARBITRUM] failed to write storage for {}: {e:?}", acct.address)
            })?;
        }
    }

    let evm_state = ctx.journal_mut().finalize();
    Ok(evm_state_to_genesis_accounts(evm_state))
}

/// Adds `amount` to `addr`'s balance (genesis credit), creating + touching the account. Mirrors
/// geth `statedb.AddBalance` as used by Nitro's retryable init.
fn credit_balance<J: JournalTr>(journal: &mut J, addr: Address, amount: U256) -> Result<(), String> {
    let mut loaded = journal
        .load_account_mut(addr)
        .map_err(|e| format!("[ARBITRUM] failed to load account {addr} for credit: {e}"))?;
    loaded.data.incr_balance(amount);
    Ok(())
}

/// ArbOS version at which `networkFeeAccount` / L1 reward recipient become the chain owner
/// (Nitro `params.ArbosVersion_2`).
const ARBOS_VERSION_2: u64 = 2;

/// `l1pricing.InitialEquilibrationUnitsV0 = 60 * TxDataNonZeroGasEIP2028(16) * 100_000`.
const INITIAL_EQUILIBRATION_UNITS_V0: u64 = 60 * 16 * 100_000; // 96_000_000
/// `l1pricing.InitialInertia`.
const L1_INITIAL_INERTIA: u64 = 10;
/// `l1pricing.InitialPerUnitReward`.
const L1_INITIAL_PER_UNIT_REWARD: u64 = 10;

/// `l2pricing.InitialSpeedLimitPerSecondV0`.
const L2_INITIAL_SPEED_LIMIT_V0: u64 = 1_000_000;
/// `l2pricing.InitialPerBlockGasLimitV0`.
const L2_INITIAL_PER_BLOCK_GAS_LIMIT_V0: u64 = 20 * 1_000_000;
/// `l2pricing.InitialMinimumBaseFeeWei = params.GWei / 10` (`= InitialBaseFeeWei`).
const L2_INITIAL_MIN_BASE_FEE_WEI: u64 = 100_000_000;
/// `l2pricing.InitialPricingInertia`.
const L2_INITIAL_PRICING_INERTIA: u64 = 102;
/// `l2pricing.InitialBacklogTolerance`.
const L2_INITIAL_BACKLOG_TOLERANCE: u64 = 10;

/// `l1pricing.BatchPosterPayToAddress` (= `BatchPosterAddress`).
const BATCH_POSTER_PAY_TO_ADDRESS: Address = BATCH_POSTER_ADDRESS;

/// Fake precompile code installed at genesis (Nitro: `[]byte{byte(vm.INVALID)}`).
const PRECOMPILE_FAKE_CODE: [u8; 1] = [0xfe];

/// The ArbOS precompiles and the ArbOS version at which each activates, mirroring Nitro's
/// `arbosState.PrecompileMinArbOSVersions` (populated from `precompiles/precompile.go::init`).
/// At genesis every precompile whose `min_version <= initial_arbos_version` is given fake
/// `[INVALID]` code so its account has a non-empty code hash in the trie. Addresses from
/// `go-ethereum/core/types/arbitrum_signer.go`.
///
/// `(address, min_arbos_version, debug_only)` — `debug_only` precompiles (ArbDebug) are installed
/// only when `debug_precompiles` is requested.
const ARBOS_PRECOMPILES: &[(Address, u64, bool)] = &[
    (address!("0x0000000000000000000000000000000000000064"), 0, false), // ArbSys
    (address!("0x0000000000000000000000000000000000000065"), 0, false), // ArbInfo
    (address!("0x0000000000000000000000000000000000000066"), 0, false), // ArbAddressTable
    (address!("0x0000000000000000000000000000000000000067"), 0, false), // ArbBLS
    (address!("0x0000000000000000000000000000000000000068"), 0, false), // ArbFunctionTable
    (address!("0x0000000000000000000000000000000000000069"), 0, false), // ArbosTest
    (address!("0x000000000000000000000000000000000000006b"), 0, false), // ArbOwnerPublic
    (address!("0x000000000000000000000000000000000000006c"), 0, false), // ArbGasInfo
    (address!("0x000000000000000000000000000000000000006d"), 0, false), // ArbAggregator
    (address!("0x000000000000000000000000000000000000006e"), 0, false), // ArbRetryableTx
    (address!("0x000000000000000000000000000000000000006f"), 0, false), // ArbStatistics
    (address!("0x0000000000000000000000000000000000000070"), 0, false), // ArbOwner
    (address!("0x0000000000000000000000000000000000000071"), 30, false), // ArbWasm
    (address!("0x0000000000000000000000000000000000000072"), 30, false), // ArbWasmCache
    (address!("0x0000000000000000000000000000000000000073"), 41, false), // ArbNativeTokenManager
    (address!("0x0000000000000000000000000000000000000074"), 60, false), // ArbFilteredTransactionsManager
    (address!("0x00000000000000000000000000000000000000ff"), 0, true),  // ArbDebug (debug only)
    (ARBOS_ACTS_ADDRESS, 0, false),                                      // ArbosActs (0xa4b05)
];

/// Parameters for genesis ArbOS initialization, derived from the chain's `Initialize` message
/// (Nitro `arbostypes.ParsedInitMessage` + `params.ChainConfig.ArbitrumChainParams`).
#[derive(Debug, Clone)]
pub struct ArbosInitConfig {
    /// `chainConfig.ArbitrumChainParams.InitialArbOSVersion` — must be ≥ 1.
    pub initial_arbos_version: u64,
    /// `chainConfig.ArbitrumChainParams.InitialChainOwner` (may be the zero address).
    pub initial_chain_owner: Address,
    /// `chainConfig.ChainID`.
    pub chain_id: U256,
    /// `chainConfig.ArbitrumChainParams.GenesisBlockNum`.
    pub genesis_block_number: u64,
    /// `initMessage.InitialL1BaseFee` (stored as L1 `pricePerUnit`).
    pub initial_l1_base_fee: U256,
    /// `initMessage.SerializedChainConfig` (stored verbatim in the chainConfig subspace).
    pub serialized_chain_config: Vec<u8>,
    /// Whether the chain registers debug precompiles (installs ArbDebug code at genesis).
    pub debug_precompiles: bool,
}

/// Writes the genesis ArbOS state into `journal`, mirroring Nitro's `InitializeArbosState`.
///
/// The journal must be fresh (ArbOS version slot == 0); this is the genesis path and does not
/// guard against re-initialization (Nitro returns `ErrAlreadyInitialized` — left to the caller).
///
/// After this returns, committing the journal yields block 0's ArbOS state. See the module-level
/// note on the Stylus/programs limitation for target versions ≥ 30.
pub fn initialize_arbos_state<J: JournalTr>(
    config: &ArbosInitConfig,
    journal: &mut J,
) -> Result<(), String> {
    if config.initial_arbos_version == 0 {
        return Err("[ARBITRUM] cannot initialize to ArbOS version 0".to_string());
    }

    let state = ArbosState::open();

    // 0. The ArbOS state account's nonce is set to 1 so geth never treats it as empty
    //    (Nitro `arbos/storage/storage.go::KVStorage` line 73, run whenever the ArbOS storage
    //    backing is opened). Without this the account encodes with nonce 0 and the genesis trie
    //    diverges. Validated against the nitro-testnode genesis (0xa4b05ff… has nonce 1).
    journal
        .load_account_mut(ARBOS_STATE_ADDRESS)
        .map_err(|e| format!("[ARBITRUM] failed to load ArbOS state account: {e}"))?
        .data
        .set_nonce(1);

    // 1. Fake precompile code for every precompile active at the target version. Nitro installs
    //    version-0 ones in InitializeArbosState and later ones during each UpgradeArbosVersion
    //    step; the final state (all precompiles with min_version <= target have code) is identical,
    //    so we install them in one pass here. (The upgrader also re-installs 0x73 at v41 — the
    //    repeated SetCode is idempotent.)
    for (addr, min_version, debug_only) in ARBOS_PRECOMPILES {
        if *min_version > config.initial_arbos_version {
            continue;
        }
        if *debug_only && !config.debug_precompiles {
            continue;
        }
        journal
            .load_account_mut(*addr)
            .map_err(|e| format!("[ARBITRUM] failed to load precompile account {addr}: {e}"))?;
        journal.set_code(*addr, Bytecode::new_raw(PRECOMPILE_FAKE_CODE.to_vec().into()));
    }

    // 2. Top-level metadata. Version starts at 1 and is advanced by the upgrade cascade below.
    //    Zero-valued offsets (upgradeVersion, upgradeTimestamp, brotli, nativeTokenEnabled,
    //    transactionFiltering, infraFeeAccount, ...) are SSTORE no-ops on a fresh trie and omitted.
    let _ = state.arbos_version.set(1_u64, journal);
    let _ = state.chain_id.set(config.chain_id, journal);
    if config.genesis_block_number != 0 {
        let _ = state
            .genesis_block_number
            .set(config.genesis_block_number, journal);
    }
    // networkFeeAccount = initialChainOwner (v>=2), else the zero address (no-op).
    if config.initial_arbos_version >= ARBOS_VERSION_2 {
        let _ = state
            .network_fee_account
            .set(config.initial_chain_owner, journal);
    }

    // 3. Serialized chain config (chainConfigSubspace).
    state
        .chain_config
        .set(&config.serialized_chain_config, journal)
        .map_err(|e| format!("[ARBITRUM] failed to store chain config: {e}"))?;

    // 4. L1 pricing (InitializeL1PricingState). Reward recipient is the batch poster pre-v2,
    //    the chain owner from v2 on.
    let initial_rewards_recipient = if config.initial_arbos_version >= ARBOS_VERSION_2 {
        config.initial_chain_owner
    } else {
        BATCH_POSTER_ADDRESS
    };
    state
        .l1_pricing
        .batch_poster_table
        .add_poster(BATCH_POSTER_ADDRESS, BATCH_POSTER_PAY_TO_ADDRESS, journal)
        .map_err(|e| format!("[ARBITRUM] failed to add genesis batch poster: {e}"))?;
    let _ = state
        .l1_pricing
        .pay_rewards_to
        .set(initial_rewards_recipient, journal);
    let _ = state
        .l1_pricing
        .equilibration_units
        .set(U256::from(INITIAL_EQUILIBRATION_UNITS_V0), journal);
    let _ = state.l1_pricing.inertia.set(L1_INITIAL_INERTIA, journal);
    let _ = state
        .l1_pricing
        .per_unit_reward
        .set(L1_INITIAL_PER_UNIT_REWARD, journal);
    let _ = state
        .l1_pricing
        .price_per_unit
        .set(config.initial_l1_base_fee, journal);

    // 5. L2 pricing (InitializeL2PricingState). gasBacklog = 0 is a no-op and omitted.
    let _ = state
        .l2_pricing
        .speed_limit_per_second
        .set(L2_INITIAL_SPEED_LIMIT_V0, journal);
    let _ = state
        .l2_pricing
        .per_block_gas_limit
        .set(L2_INITIAL_PER_BLOCK_GAS_LIMIT_V0, journal);
    let _ = state
        .l2_pricing
        .base_fee_wei
        .set(U256::from(L2_INITIAL_MIN_BASE_FEE_WEI), journal);
    let _ = state
        .l2_pricing
        .min_base_fee_wei
        .set(U256::from(L2_INITIAL_MIN_BASE_FEE_WEI), journal);
    let _ = state
        .l2_pricing
        .pricing_inertia
        .set(L2_INITIAL_PRICING_INERTIA, journal);
    let _ = state
        .l2_pricing
        .backlog_tolerance
        .set(L2_INITIAL_BACKLOG_TOLERANCE, journal);

    // 6. Retryable timeout queue (InitializeRetryableState -> InitializeQueue: put=get=2).
    state
        .retryables
        .initialize(journal)
        .map_err(|e| format!("[ARBITRUM] failed to initialize retryable state: {e}"))?;

    // 7. Chain owner set. Nitro adds the initial owner unconditionally (even the zero address,
    //    which still writes the set's size + membership slots).
    state
        .chain_owners
        .add(config.initial_chain_owner, journal)
        .map_err(|e| format!("[ARBITRUM] failed to add genesis chain owner: {e}"))?;

    // addressTable / sendMerkle / blockhashes / nativeTokenOwners / transactionFilterers all
    // initialize by writing 0 to offset 0 — SSTORE no-ops on a fresh trie; nothing to do.

    // 8. Run the per-version migration cascade up to the target version (Nitro's
    //    UpgradeArbosVersion(desired, firstTime=true)). See the module note on Stylus state.
    if config.initial_arbos_version > 1 {
        upgrade_arbos_version(1, config.initial_arbos_version, &state, journal)?;
    }

    // 9. firstTime block (Nitro arbosState.go UpgradeArbosVersion lines 519-526): runs ONCE
    //    after the cascade, at genesis only (firstTime=true), when the target version >= 6.
    //    It overrides L1 equilibration units and L2 speed-limit / per-block gas-limit with the
    //    V6 constants — the V0 values written by InitializeL1/L2PricingState are only correct for
    //    a genesis below v6. This is firstTime-ONLY: a *runtime* v6 upgrade does NOT apply it,
    //    which is why `upgrade_arbos_version` omits it (and why mid-chain replay never needs it —
    //    that state is already past v6). Validated slot-for-slot against the nitro-testnode genesis
    //    (ArbOS v40): equilibrationUnits=160e6, speedLimit=7e6, perBlockGasLimit=32e6.
    if config.initial_arbos_version >= 6 {
        // perBatchGasCost = InitialPerBatchGasCostV6 (100_000), ONLY for target < 11; at >= 11 the
        // v11 cascade step already set it to the V12 value (210_000).
        if config.initial_arbos_version < 11 {
            let _ = state.l1_pricing.per_batch_gas_cost.set(100_000_i64, journal);
        }
        // InitialEquilibrationUnitsV6 = TxDataNonZeroGasEIP2028(16) * 10_000_000.
        let _ = state
            .l1_pricing
            .equilibration_units
            .set(U256::from(160_000_000u64), journal);
        // InitialSpeedLimitPerSecondV6 / InitialPerBlockGasLimitV6.
        let _ = state
            .l2_pricing
            .speed_limit_per_second
            .set(7_000_000u64, journal);
        let _ = state
            .l2_pricing
            .per_block_gas_limit
            .set(32_000_000u64, journal);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::default_ctx::{ArbContext, DefaultArb};
    use revm::{context_interface::ContextTr, database_interface::EmptyDB};

    fn cfg(version: u64) -> ArbosInitConfig {
        ArbosInitConfig {
            initial_arbos_version: version,
            initial_chain_owner: address!("0x00000000000000000000000000000000000a11ce"),
            chain_id: U256::from(412_346_u64),
            genesis_block_number: 0,
            initial_l1_base_fee: U256::from(50_000_000_000_u64), // 50 gwei
            serialized_chain_config: b"{\"chainId\":412346}".to_vec(),
            debug_precompiles: false,
        }
    }

    /// Returns a fresh ArbOS context whose journal we can write to and read back from.
    fn fresh() -> ArbContext<EmptyDB> {
        <ArbContext<EmptyDB> as DefaultArb>::arb()
    }

    /// Genesis to a version >= 6 applies the firstTime V6 pricing overrides (Nitro
    /// arbosState.go:519-526). Validated slot-for-slot against the nitro-testnode genesis
    /// (ArbOS v40): equilibrationUnits=160e6, speedLimit=7e6, perBlockGasLimit=32e6.
    #[test]
    fn genesis_v6_plus_applies_v6_pricing_overrides() {
        let mut ctx = fresh();
        initialize_arbos_state(&cfg(40), ctx.journal_mut()).expect("init v40");
        let state = ArbosState::open();
        let j = ctx.journal_mut();
        assert_eq!(
            state.l1_pricing.equilibration_units.get(j).unwrap(),
            U256::from(160_000_000u64),
            "v6 equilibrationUnits override"
        );
        assert_eq!(
            state.l2_pricing.speed_limit_per_second.get(j).unwrap(),
            7_000_000,
            "v6 speedLimit override"
        );
        assert_eq!(
            state.l2_pricing.per_block_gas_limit.get(j).unwrap(),
            32_000_000,
            "v6 perBlockGasLimit override"
        );
        // perBatchGasCost stays at the v11 value (210_000) for target >= 11.
        assert_eq!(state.l1_pricing.per_batch_gas_cost.get(j).unwrap(), 210_000);
    }

    /// Genesis below v6 keeps the V0 pricing constants (no firstTime override).
    #[test]
    fn genesis_below_v6_keeps_v0_pricing() {
        let mut ctx = fresh();
        initialize_arbos_state(&cfg(5), ctx.journal_mut()).expect("init v5");
        let state = ArbosState::open();
        let j = ctx.journal_mut();
        assert_eq!(
            state.l1_pricing.equilibration_units.get(j).unwrap(),
            U256::from(INITIAL_EQUILIBRATION_UNITS_V0),
            "v5 keeps V0 equilibrationUnits"
        );
        assert_eq!(
            state.l2_pricing.speed_limit_per_second.get(j).unwrap(),
            L2_INITIAL_SPEED_LIMIT_V0,
            "v5 keeps V0 speedLimit"
        );
    }

    #[test]
    fn base_init_v1_writes_pricing_and_metadata() {
        let mut ctx = fresh();
        let config = cfg(1);
        initialize_arbos_state(&config, ctx.journal_mut()).expect("init v1");

        let state = ArbosState::open();
        let j = ctx.journal_mut();
        assert_eq!(state.arbos_version.get(j).unwrap(), 1, "version must be 1");
        assert_eq!(state.chain_id.get(j).unwrap(), config.chain_id, "chainId");
        // v1 < 2 => network fee account stays zero.
        assert_eq!(
            state.network_fee_account.get(j).unwrap(),
            Address::ZERO,
            "network fee account unset pre-v2"
        );
        // L2 pricing V0 constants.
        assert_eq!(
            state.l2_pricing.speed_limit_per_second.get(j).unwrap(),
            L2_INITIAL_SPEED_LIMIT_V0
        );
        assert_eq!(
            state.l2_pricing.per_block_gas_limit.get(j).unwrap(),
            L2_INITIAL_PER_BLOCK_GAS_LIMIT_V0
        );
        assert_eq!(
            state.l2_pricing.min_base_fee_wei.get(j).unwrap(),
            U256::from(L2_INITIAL_MIN_BASE_FEE_WEI)
        );
        assert_eq!(
            state.l2_pricing.pricing_inertia.get(j).unwrap(),
            L2_INITIAL_PRICING_INERTIA
        );
        // L1 pricing V0 constants.
        assert_eq!(state.l1_pricing.inertia.get(j).unwrap(), L1_INITIAL_INERTIA);
        assert_eq!(
            state.l1_pricing.equilibration_units.get(j).unwrap(),
            U256::from(INITIAL_EQUILIBRATION_UNITS_V0)
        );
        assert_eq!(
            state.l1_pricing.price_per_unit.get(j).unwrap(),
            config.initial_l1_base_fee,
            "pricePerUnit = initial L1 base fee"
        );
        // pre-v2 reward recipient is the batch poster.
        assert_eq!(
            state.l1_pricing.pay_rewards_to.get(j).unwrap(),
            BATCH_POSTER_ADDRESS
        );
        // Chain owner registered.
        assert!(
            state
                .chain_owners
                .is_member(config.initial_chain_owner, j)
                .unwrap(),
            "chain owner must be a member"
        );
    }

    #[test]
    fn base_init_installs_version0_precompile_code() {
        let mut ctx = fresh();
        initialize_arbos_state(&cfg(1), ctx.journal_mut()).expect("init");
        let j = ctx.journal_mut();

        // A version-0 precompile (ArbSys 0x64) and ArbosActs (0xa4b05) get fake code...
        for addr in [
            address!("0x0000000000000000000000000000000000000064"),
            ARBOS_ACTS_ADDRESS,
        ] {
            let acct = j.load_account(addr).unwrap();
            let code = acct.data.info.code.clone().unwrap_or_default();
            assert_eq!(
                code.original_byte_slice(),
                &PRECOMPILE_FAKE_CODE,
                "precompile {addr} must have [INVALID] code"
            );
        }

        // ...but a version-gated precompile (ArbWasm 0x71 @ v30) is NOT installed at v1.
        let arb_wasm = address!("0x0000000000000000000000000000000000000071");
        let acct = j.load_account(arb_wasm).unwrap();
        assert!(
            acct.data.info.code.clone().unwrap_or_default().is_empty(),
            "ArbWasm must not be installed below v30"
        );

        // ArbDebug (debug-only) is NOT installed when debug_precompiles = false.
        let arb_debug = address!("0x00000000000000000000000000000000000000ff");
        let acct = j.load_account(arb_debug).unwrap();
        assert!(
            acct.data.info.code.clone().unwrap_or_default().is_empty(),
            "ArbDebug must not be installed without debug_precompiles"
        );
    }

    #[test]
    fn init_v11_runs_upgrade_cascade() {
        let mut ctx = fresh();
        let config = cfg(11);
        initialize_arbos_state(&config, ctx.journal_mut()).expect("init v11");
        let state = ArbosState::open();
        let j = ctx.journal_mut();

        assert_eq!(state.arbos_version.get(j).unwrap(), 11, "version reaches 11");
        // v>=2 => network fee account = chain owner.
        assert_eq!(
            state.network_fee_account.get(j).unwrap(),
            config.initial_chain_owner
        );
        // v3 sets per-batch gas cost, v11 overrides it to the V12 value (210_000).
        assert_eq!(
            state.l1_pricing.per_batch_gas_cost.get(j).unwrap(),
            210_000_i64
        );
        // v11 fixes the amortization cap back to 0 (was MaxUint64 after v3).
        assert_eq!(
            state.l1_pricing.amortized_cost_cap_bips.get(j).unwrap(),
            0_u64
        );
        // brotli upgrade is v20 — still 0 at v11.
        assert_eq!(state.brotli_compression_level.get(j).unwrap(), 0_u64);
    }

    #[test]
    fn rejects_version_zero() {
        let mut ctx = fresh();
        assert!(initialize_arbos_state(&cfg(0), ctx.journal_mut()).is_err());
    }

    // ---------------------------------------------------------------------------
    // Genesis allocation builder tests
    // ---------------------------------------------------------------------------

    /// Verifies that `arb_genesis_accounts` with ArbOS v11 produces the expected
    /// account set: ArbOS state account (with storage including the chain_id slot),
    /// ArbSys (with `[0xfe]` code and no storage), and at least 14 accounts total.
    /// Also verifies determinism: calling twice yields identical output.
    #[test]
    fn genesis_accounts_contain_arbos_state_and_precompiles() {
        use crate::constants::ARBOS_STATE_ADDRESS;
        use crate::storage::{ArbosMetadataOffset, StorageSpace};

        let config = cfg(11);
        let accounts = arb_genesis_accounts(&config).expect("genesis accounts v11");

        // --- (1) Account count ≥ 14 ---
        // ArbOS state + 12 v0 precompiles + ArbosActs = 14 minimum at v11.
        assert!(
            accounts.len() >= 14,
            "expected at least 14 genesis accounts, got {}",
            accounts.len()
        );

        // --- (2) ArbOS state account has non-empty storage ---
        let arbos_acct = accounts
            .iter()
            .find(|a| a.address == ARBOS_STATE_ADDRESS)
            .expect("ARBOS_STATE_ADDRESS must be present");
        assert!(
            arbos_acct.storage.len() > 8,
            "ArbOS state account should have >8 storage entries, got {}",
            arbos_acct.storage.len()
        );

        // --- (3) chain_id value appears in ARBOS_STATE_ADDRESS storage ---
        // The chain_id slot is at ArbosMetadataOffset::ChainId (0x04) of the root namespace.
        // Compute it the same way the production code does.
        let root = StorageSpace::arbos();
        let chain_id_slot_bytes = root.slot_for_offset(ArbosMetadataOffset::ChainId as u8);
        let chain_id_slot_key = B256::from(chain_id_slot_bytes.0);
        let chain_id_value = B256::from(config.chain_id.to_be_bytes());

        let chain_id_entry = arbos_acct
            .storage
            .iter()
            .find(|(slot, _)| *slot == chain_id_slot_key);
        assert!(
            chain_id_entry.is_some(),
            "chain_id slot {chain_id_slot_key:#x} must be present in ArbOS state storage"
        );
        let (_, stored_value) = chain_id_entry.unwrap();
        assert_eq!(
            *stored_value, chain_id_value,
            "chain_id storage value mismatch: expected {chain_id_value:#x}, got {stored_value:#x}"
        );

        // --- (4) ArbSys 0x64 has code [0xfe] and empty storage ---
        let arb_sys_addr = address!("0x0000000000000000000000000000000000000064");
        let arb_sys = accounts
            .iter()
            .find(|a| a.address == arb_sys_addr)
            .expect("ArbSys 0x64 must be present");
        assert_eq!(
            arb_sys.code.as_ref(),
            &[0xfe_u8],
            "ArbSys must have code [0xfe]"
        );
        assert!(
            arb_sys.storage.is_empty(),
            "ArbSys must have empty storage"
        );

        // --- (5) Determinism: two independent calls yield identical output ---
        let accounts2 = arb_genesis_accounts(&config).expect("genesis accounts v11 second call");
        assert_eq!(
            accounts, accounts2,
            "arb_genesis_accounts must be deterministic"
        );
    }

    /// Verifies that `arb_genesis_accounts` at ArbOS v40 includes the EIP-2935
    /// history-storage contract at address `0x0000F90827F1C53a10cb7A02335B175320002935`
    /// with non-empty code and nonce == 1.
    #[test]
    fn genesis_accounts_v40_has_eip2935_history_storage() {
        use crate::constants::HISTORY_STORAGE_ADDRESS;

        let config = cfg(40);
        let accounts = arb_genesis_accounts(&config).expect("genesis accounts v40");

        let history_acct = accounts
            .iter()
            .find(|a| a.address == HISTORY_STORAGE_ADDRESS)
            .expect("EIP-2935 HISTORY_STORAGE_ADDRESS must be present at ArbOS v40");

        assert!(
            !history_acct.code.is_empty(),
            "EIP-2935 history storage contract must have non-empty code"
        );
        assert_eq!(
            history_acct.nonce, 1,
            "EIP-2935 history storage contract must have nonce == 1"
        );
    }
}
