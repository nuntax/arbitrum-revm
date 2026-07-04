use crate::{
    api::exec::ArbContextTr,
    storage::{ArbosState, StorageSlot},
};
use arb_alloy_precompiles::addresses::ARB_NATIVE_TOKEN_MANAGER;
use revm::{
    Database as _,
    context_interface::{Block, ContextTr, JournalTr, Transaction, journaled_state::account::JournaledAccountTr},
    primitives::{Address, B256, Bytes, I256, U256, keccak256},
    state::Bytecode,
};

/// EIP-2935 history storage contract (`0x0000F90827F1C53a10cb7A02335B175320002935`).
const HISTORY_STORAGE_ADDRESS: Address = Address::new([
    0x00, 0x00, 0xF9, 0x08, 0x27, 0xF1, 0xC5, 0x3a, 0x10, 0xcb, 0x7A, 0x02, 0x33, 0x5B, 0x17, 0x53,
    0x20, 0x00, 0x29, 0x35,
]);
/// Arbitrum's EIP-2935 ring-buffer size (`0x05ffd0`); Nitro widened the Ethereum default
/// of 8191 to this. See `params.HistoryStorageCodeArbitrum`.
const HISTORY_SERVE_WINDOW: u64 = 393_168;
/// `params.HistoryStorageCodeArbitrum` — the EIP-2935 history contract runtime code
/// (rings at 393168 and reads the L2 block number via ArbSys `arbBlockNumber`, since the
/// `NUMBER` opcode returns the L1 block number on Arbitrum). Deployed at the v40 upgrade.
const HISTORY_STORAGE_CODE_HEX: &str = "3373fffffffffffffffffffffffffffffffffffffffe1460605760203603605c575f3563a3b1b31d5f5260205f6004601c60645afa15605c575f51600181038211605c57816205ffd0910311605c576205ffd09006545f5260205ff35b5f5ffd5b5f356205ffd0600163a3b1b31d5f5260205f6004601c60645afa15605c575f5103065500";

/// Nitro: l1pricing.InitialPerBatchGasCostV12 (used during v11 upgrade)
const INITIAL_PER_BATCH_GAS_COST_V12: i64 = 210_000;
/// Nitro: l2pricing.InitialPerTxGasLimitV50
const INITIAL_PER_TX_GAS_LIMIT_V50: u64 = 32 * 1_000_000;

const START_BLOCK_SELECTOR_TEXT: &[u8] = b"startBlock(uint256,uint64,uint64,uint64)";
const BATCH_POSTING_REPORT_SELECTOR_TEXT: &[u8] =
    b"batchPostingReport(uint256,address,uint64,uint64,uint256)";
const BATCH_POSTING_REPORT_V2_SELECTOR_TEXT: &[u8] =
    b"batchPostingReportV2(uint256,address,uint64,uint64,uint64,uint64,uint256)";

const START_BLOCK_CALLDATA_WORDS: usize = 4;
const BATCH_POSTING_REPORT_CALLDATA_WORDS: usize = 5;
const BATCH_POSTING_REPORT_V2_CALLDATA_WORDS: usize = 7;

const ABI_WORD_SIZE: usize = 32;
const SELECTOR_SIZE: usize = 4;

pub(crate) fn start_block_selector() -> [u8; 4] {
    let hash = keccak256(START_BLOCK_SELECTOR_TEXT);
    [hash[0], hash[1], hash[2], hash[3]]
}

pub(crate) fn batch_posting_report_selector() -> [u8; 4] {
    let hash = keccak256(BATCH_POSTING_REPORT_SELECTOR_TEXT);
    [hash[0], hash[1], hash[2], hash[3]]
}

pub(crate) fn batch_posting_report_v2_selector() -> [u8; 4] {
    let hash = keccak256(BATCH_POSTING_REPORT_V2_SELECTOR_TEXT);
    [hash[0], hash[1], hash[2], hash[3]]
}

pub(crate) fn apply_internal_tx<CTX: ArbContextTr>(ctx: &mut CTX) -> Result<(), String> {
    let input = ctx.tx().input().clone();
    if input.len() < SELECTOR_SIZE {
        return Err("[ARBITRUM] internal tx calldata shorter than selector".into());
    }

    let selector =
        <[u8; 4]>::try_from(&input[..SELECTOR_SIZE]).expect("selector slice length is fixed");

    if selector == start_block_selector() {
        return apply_start_block(ctx, &input);
    }
    if selector == batch_posting_report_selector() {
        return apply_batch_posting_report(ctx, &input);
    }
    if selector == batch_posting_report_v2_selector() {
        return apply_batch_posting_report_v2(ctx, &input);
    }

    Err(format!(
        "[ARBITRUM] unsupported internal tx selector 0x{}",
        hex_encode(&selector)
    ))
}

fn apply_start_block<CTX: ArbContextTr>(ctx: &mut CTX, input: &Bytes) -> Result<(), String> {
    let (_l1_base_fee, mut l1_block_number, l2_block_number, mut time_last_block) =
        decode_start_block_calldata(input)?;

    let current_l2_block_number: u64 = ctx
        .block()
        .number()
        .try_into()
        .map_err(|_| "[ARBITRUM] block.number does not fit in u64".to_string())?;
    if l2_block_number != current_l2_block_number {
        return Err(format!(
            "[ARBITRUM] startBlock l2BlockNumber mismatch: got {l2_block_number}, expected {current_l2_block_number}"
        ));
    }
    let current_time: u64 = ctx
        .block()
        .timestamp()
        .try_into()
        .map_err(|_| "[ARBITRUM] block.timestamp does not fit in u64".to_string())?;

    let prev_hash = if current_l2_block_number == 0 {
        B256::ZERO
    } else {
        ctx.journal_mut()
            .db_mut()
            .block_hash(current_l2_block_number - 1)
            .map_err(|err| format!("[ARBITRUM] failed to read parent block hash: {err}"))?
    };

    let arbos_state = ArbosState::open();
    let journal = ctx.journal_mut();
    let arbos_version = arbos_state
        .arbos_version
        .get(journal)
        .map_err(|err| format!("[ARBITRUM] failed to read ArbOS version: {err}"))?;

    // EIP-2935 (ArbOS >= 40): mirror `core.ProcessParentBlockHash`. Nitro issues a
    // system call to the history-storage contract with the parent hash as calldata; the
    // contract's only persistent effect is storing that hash at slot
    // `(blockNumber - 1) % HistoryServeWindow`, so we apply that write directly. Gated on
    // the *pre-upgrade* version, matching Nitro (the v40 activation block deploys the
    // contract but does not yet write, since the stored version is still < 40 here).
    if arbos_version >= 40 && current_l2_block_number > 0 {
        let slot_index = (current_l2_block_number - 1) % HISTORY_SERVE_WINDOW;
        let slot_key = B256::from(U256::from(slot_index).to_be_bytes::<32>());
        StorageSlot::new(HISTORY_STORAGE_ADDRESS, slot_key)
            .set_inner(U256::from_be_bytes(prev_hash.0), journal)
            .map_err(|err| format!("[ARBITRUM] EIP-2935 history write failed: {err}"))?;
    }

    // Version compatibility shims mirror Nitro behavior.
    if arbos_version < 3 {
        time_last_block = l2_block_number;
    }
    if arbos_version < 8 {
        l1_block_number = l1_block_number.saturating_add(1);
    }

    let old_l1_block_number = arbos_state
        .block_hashes
        .l1_block_number(journal)
        .map_err(|err| format!("[ARBITRUM] failed to read ArbOS L1 block number: {err}"))?;
    if l1_block_number > old_l1_block_number {
        arbos_state
            .block_hashes
            .record_new_l1_block(l1_block_number - 1, prev_hash, arbos_version, journal)
            .map_err(|err| format!("[ARBITRUM] failed to record ArbOS L1 block hash: {err}"))?;
    }

    // The `NUMBER` opcode returns the ArbOS-state L1 block number (Nitro's patched
    // `opNumber` reads `ProcessingHook.L1BlockNumber`, which is exactly this stored value
    // *after* the start-block update), not the raw message block number. That distinction
    // matters because the stored value is monotonic and, for ArbOS < 8, one higher than the
    // message's number (the `l1_block_number++` above). The block-scoped chain context still
    // carries the raw message value the driver/replay seeded it with, so refresh it from the
    // post-update ArbOS state here; every user tx in this block then sees the correct
    // `NUMBER`. Without this, a tx that reads/stores `NUMBER` diverges from canonical (first
    // observed at Arb One block 22207832).
    let new_l1_block_number = arbos_state
        .block_hashes
        .l1_block_number(journal)
        .map_err(|err| format!("[ARBITRUM] failed to read updated ArbOS L1 block number: {err}"))?;

    // Nitro reaps up to two retryables during StartBlock.
    let _ = arbos_state
        .retryables
        .try_to_reap_one(current_time, journal);
    let _ = arbos_state
        .retryables
        .try_to_reap_one(current_time, journal);

    arbos_state
        .l2_pricing
        .update_pricing_model(time_last_block, arbos_version, journal)
        .map_err(|err| format!("[ARBITRUM] failed to update ArbOS L2 pricing model: {err}"))?;

    // UpgradeArbosVersionIfNecessary: if a scheduled upgrade has reached its
    // flag-day timestamp, advance the stored ArbOS version one step at a time,
    // running per-version state migrations.
    // Nitro reference: arbosState.go UpgradeArbosVersionIfNecessary / UpgradeArbosVersion.
    let upgrade_version = arbos_state
        .upgrade_version
        .get(journal)
        .map_err(|err| format!("[ARBITRUM] failed to read ArbOS upgrade version: {err}"))?;
    let upgrade_timestamp = arbos_state
        .upgrade_timestamp
        .get(journal)
        .map_err(|err| format!("[ARBITRUM] failed to read ArbOS upgrade timestamp: {err}"))?;
    if upgrade_version > arbos_version && current_time >= upgrade_timestamp {
        upgrade_arbos_version(
            arbos_version,
            upgrade_version,
            // A runtime upgrade is never the firstTime (genesis) path.
            false,
            &arbos_state,
            journal,
        )
        .map_err(|err| format!("[ARBITRUM] ArbOS version upgrade failed: {err}"))?;
    }

    // Refresh the block-scoped L1 block number now that the journal borrow is released, so
    // the `NUMBER` opcode (which reads `chain().l1_block_number`) returns the post-update
    // ArbOS-state value for the rest of this block's transactions.
    ctx.chain_mut().l1_block_number = new_l1_block_number;

    // New block: no retryables have been submitted yet, so drop any zombie-escrow tickets
    // carried over from the previous block. This is what scopes the pre-Stylus zombie escrow
    // to same-block submit+redeem pairs (see `ArbChainContext::pending_zombie_escrow_tickets`).
    ctx.chain_mut().pending_zombie_escrow_tickets.clear();

    Ok(())
}

/// Runs per-version ArbOS state migrations from `current_version` up to (and including)
/// `target_version`, incrementing one step at a time.
///
/// Mirrors Nitro's `UpgradeArbosVersion` loop in arbosState.go.
/// Stylus-program-param upgrades (v30 programs init, v31 Version+MinInitGas, v40 MaxWasmSize,
/// v50 MaxStackDepth cap, v60 MaxFragmentCount) are now fully implemented.
pub(crate) fn upgrade_arbos_version<J: JournalTr>(
    current_version: u64,
    target_version: u64,
    // Mirrors Nitro's `firstTime` flag: true for the genesis cascade (`UpgradeArbosVersion(desired,
    // firstTime=true)`), false for a runtime upgrade. A few steps (e.g. v11's chain-owner list
    // clear) run only when NOT firstTime.
    first_time: bool,
    state: &ArbosState,
    journal: &mut J,
) -> Result<(), String> {
    let mut version = current_version;
    while version < target_version {
        version += 1;
        match version {
            // v2: reset L1 pricing surplus accumulator to 0.
            // Nitro calls SetLastSurplus(0, version=1); version 1 < 7 → use set_pre_version7.
            2 => {
                let _ = state
                    .l1_pricing
                    .last_surplus
                    .set_pre_version7(I256::ZERO, journal);
            }
            // v3: clear per-batch gas cost and set amortization cap to MaxUint64.
            3 => {
                let _ = state
                    .l1_pricing
                    .per_batch_gas_cost
                    .set(0_i64, journal);
                let _ = state
                    .l1_pricing
                    .amortized_cost_cap_bips
                    .set(u64::MAX, journal);
            }
            // v4-v9: no storage-level changes needed.
            4 | 5 | 6 | 7 | 8 | 9 => {}
            // v10: seed l1_fees_available from L1PricerFundsPool balance.
            // The journal's database holds the canonical balance.
            10 => {
                use crate::constants::L1_PRICER_FUNDS_POOL_ADDRESS;
                if let Ok(account) = journal.load_account(L1_PRICER_FUNDS_POOL_ADDRESS) {
                    let balance = account.data.info.balance;
                    let _ = state.l1_pricing.l1_fees_available.set(balance, journal);
                }
            }
            // v11: update per-batch gas cost, fix amortization cap if it was set to MaxUint64.
            11 => {
                let _ = state
                    .l1_pricing
                    .per_batch_gas_cost
                    .set(INITIAL_PER_BATCH_GAS_COST_V12, journal);

                let old_cap = state
                    .l1_pricing
                    .amortized_cost_cap_bips
                    .get(journal)
                    .unwrap_or(0);
                if old_cap == u64::MAX {
                    let _ = state
                        .l1_pricing
                        .amortized_cost_cap_bips
                        .set(0_u64, journal);
                }
                // Clear the chain-owners list, but only on a runtime upgrade: Nitro guards this
                // with `if !firstTime { chainOwners.ClearList() }`, so the genesis cascade must
                // NOT run it (a chain genesis'd at >= v11 keeps its owner list). It zeroes the list
                // slots + size (members stay in the by-address mapping), a real storage write:
                // skipping it on a runtime upgrade diverges the state root at the v11 block.
                if !first_time {
                    let _ = state.chain_owners.clear_list(journal);
                }
            }
            // v12-v19: reserved for Orbit chains; no mainnet state changes.
            12..=19 => {}
            // v20: enable fast brotli compression (level 0 → 1).
            20 => {
                let _ = state.brotli_compression_level.set(1_u64, journal);
            }
            // v21-v29: reserved for Orbit chains.
            21..=29 => {}
            // v30: Stylus program genesis state initialization.
            // Nitro: `programs.Initialize(nextArbosVersion=30, sto)` in arbosstate.go.
            // Writes the packed params word (Version=1, all initial constants), data-pricer
            // fields (bytes_per_second, last_update_time=ArbitrumStartTime, min_price, inertia),
            // and the cacheManagers set (a no-op on a fresh trie).
            30 => {
                state
                    .programs
                    .initialize(30, journal)
                    .map_err(|e| format!("[ARBITRUM] programs.initialize failed: {e}"))?;
            }
            // v31: Stylus params v2 upgrade.
            // Nitro: `params.UpgradeToVersion(2)` + `params.Save()` in arbosstate.go.
            // Sets Version = 2 and MinInitGas = v2MinInitGas (69).
            31 => {
                use crate::storage::programs::{stylus_param_layout as l, pack_uint, V2_STYLUS_VERSION, V2_MIN_INIT_GAS};
                let mut params_word = state
                    .programs
                    .read_params_word(journal)
                    .map_err(|e| format!("[ARBITRUM] v31: failed to read Stylus params: {e}"))?;
                pack_uint(&mut params_word, l::VERSION.0,       l::VERSION.1,       V2_STYLUS_VERSION);
                pack_uint(&mut params_word, l::MIN_INIT_GAS.0,  l::MIN_INIT_GAS.1,  V2_MIN_INIT_GAS);
                state
                    .programs
                    .write_params_word(params_word, journal)
                    .map_err(|e| format!("[ARBITRUM] v31: failed to write Stylus params: {e}"))?;
            }
            // v32: no storage changes.
            32 => {}
            // v33-v39: reserved for Orbit chains.
            33..=39 => {}
            // v40: EIP-2935 history storage and Stylus params — EVM code install handled externally.
            // v40: deploy the EIP-2935 history-storage contract (nonce=1 + Arbitrum code).
            // Nitro: arbosState.go UpgradeArbosVersion case ArbosVersion_40.
            40 => {
                let code = Bytecode::new_raw(
                    hex::decode(HISTORY_STORAGE_CODE_HEX)
                        .map_err(|e| format!("[ARBITRUM] invalid history contract code: {e}"))?
                        .into(),
                );
                // load_account_mut warms the account (set_code below assumes it's loaded).
                journal
                    .load_account_mut(HISTORY_STORAGE_ADDRESS)
                    .map_err(|e| format!("[ARBITRUM] failed to load history account: {e}"))?
                    .data
                    .set_nonce(1);
                journal.set_code(HISTORY_STORAGE_ADDRESS, code);

                // Stylus params: at v40 `MaxWasmSize` becomes a stored parameter (it was a
                // constant before), initialized to `initialMaxWasmSize` (128 KiB). Mirrors
                // Nitro `StylusParams.UpgradeToArbosVersion(40)` + `Save()`; the only stored
                // change is the MaxWasmSize field (bytes [25..29] of the packed params word).
                let mut params_word = state
                    .programs
                    .read_params_word(journal)
                    .map_err(|e| format!("[ARBITRUM] failed to read Stylus params: {e}"))?;
                params_word[25..29].copy_from_slice(&(128u32 * 1024).to_be_bytes());
                state
                    .programs
                    .write_params_word(params_word, journal)
                    .map_err(|e| format!("[ARBITRUM] failed to write Stylus params: {e}"))?;
            }
            // v41: install the ArbNativeTokenManager (0x73) precompile account. Nitro
            // arbosState.go UpgradeArbosVersion installs `[INVALID]` (0xfe) code for every
            // precompile whose MinArbOSVersion == the version being activated, giving the
            // account a non-empty code hash in the trie. ArbNativeTokenManager activates at
            // v41 (ArbFilteredTransactionsManager at 0x74 activates at v60 — handled there).
            41 => {
                journal
                    .load_account_mut(ARB_NATIVE_TOKEN_MANAGER)
                    .map_err(|e| format!("[ARBITRUM] failed to load ArbNativeTokenManager: {e}"))?;
                journal.set_code(ARB_NATIVE_TOKEN_MANAGER, Bytecode::new_raw(vec![0xfe].into()));
            }
            // v42-v49: reserved for Orbit chains.
            42..=49 => {}
            // v50: set per-tx gas limit; Stylus param upgrade handled by runtime.
            50 => {
                // Stylus params v50 upgrade (Nitro StylusParams.UpgradeToArbosVersion(50)):
                // cap MaxStackDepth at arbOS50MaxWasmSize (22000) if larger. MaxStackDepth
                // is bytes [5..9] of the packed params word.
                let mut params_word = state
                    .programs
                    .read_params_word(journal)
                    .map_err(|e| format!("[ARBITRUM] failed to read Stylus params: {e}"))?;
                let max_stack = u32::from_be_bytes(
                    params_word[5..9].try_into().expect("4-byte MaxStackDepth field"),
                );
                if max_stack > 22_000 {
                    params_word[5..9].copy_from_slice(&22_000u32.to_be_bytes());
                    state
                        .programs
                        .write_params_word(params_word, journal)
                        .map_err(|e| format!("[ARBITRUM] failed to write Stylus params: {e}"))?;
                }
                let _ = state
                    .l2_pricing
                    .per_tx_gas_limit
                    .set(INITIAL_PER_TX_GAS_LIMIT_V50, journal);
            }
            // v51: no storage changes.
            51 => {}
            // v52-v59: reserved for Orbit chains.
            52..=59 => {}
            // v60: Stylus StylusContractLimit param + transaction-filterer init.
            // Nitro: `p.UpgradeToArbosVersion(60)` sets MaxFragmentCount = initialMaxFragmentCount (2)
            // + `addressSet.Initialize(transactionFiltererSubspace)` (no-op on fresh trie).
            60 => {
                use crate::storage::programs::{stylus_param_layout as l, pack_uint, INITIAL_MAX_FRAGMENT_COUNT};
                let mut params_word = state
                    .programs
                    .read_params_word(journal)
                    .map_err(|e| format!("[ARBITRUM] v60: failed to read Stylus params: {e}"))?;
                pack_uint(&mut params_word, l::MAX_FRAGMENT_COUNT.0, l::MAX_FRAGMENT_COUNT.1, INITIAL_MAX_FRAGMENT_COUNT);
                state
                    .programs
                    .write_params_word(params_word, journal)
                    .map_err(|e| format!("[ARBITRUM] v60: failed to write Stylus params: {e}"))?;
                // transaction-filterer AddressSet.Initialize writes 0 to slot 0 — SSTORE no-op
                // on a fresh trie; the AddressSet already initializes lazily on first use.
            }
            unknown => {
                return Err(format!(
                    "[ARBITRUM] chain is upgrading to unsupported ArbOS version {unknown}; please upgrade the node"
                ));
            }
        }

        // Persist the incremented version after each successful step.
        let _ = state.arbos_version.set(version, journal);
    }
    Ok(())
}

fn apply_batch_posting_report<CTX: ArbContextTr>(
    ctx: &mut CTX,
    input: &Bytes,
) -> Result<(), String> {
    let (batch_timestamp, batch_poster_address, _batch_number, batch_data_gas, l1_base_fee_wei) =
        decode_batch_posting_report_calldata(input)?;
    let batch_timestamp: u64 = batch_timestamp.try_into().map_err(|_| {
        "[ARBITRUM] batchPostingReport batchTimestamp does not fit in u64".to_string()
    })?;
    let current_time: u64 = ctx
        .block()
        .timestamp()
        .try_into()
        .map_err(|_| "[ARBITRUM] block.timestamp does not fit in u64".to_string())?;

    let arbos_state = ArbosState::open();
    let journal = ctx.journal_mut();
    let arbos_version = arbos_state
        .arbos_version
        .get(journal)
        .map_err(|err| format!("[ARBITRUM] failed to read ArbOS version: {err}"))?;

    arbos_state
        .l1_pricing
        .apply_batch_posting_report(
            arbos_version,
            batch_timestamp,
            current_time,
            batch_poster_address,
            batch_data_gas,
            l1_base_fee_wei,
            journal,
        )
        .map_err(|err| format!("[ARBITRUM] failed to apply batchPostingReport: {err}"))?;
    Ok(())
}

fn apply_batch_posting_report_v2<CTX: ArbContextTr>(
    ctx: &mut CTX,
    input: &Bytes,
) -> Result<(), String> {
    let (
        batch_timestamp,
        batch_poster_address,
        _batch_number,
        batch_calldata_length,
        batch_calldata_non_zeros,
        batch_extra_gas,
        l1_base_fee_wei,
    ) = decode_batch_posting_report_v2_calldata(input)?;

    let batch_timestamp: u64 = batch_timestamp.try_into().map_err(|_| {
        "[ARBITRUM] batchPostingReportV2 batchTimestamp does not fit in u64".to_string()
    })?;
    let current_time: u64 = ctx
        .block()
        .timestamp()
        .try_into()
        .map_err(|_| "[ARBITRUM] block.timestamp does not fit in u64".to_string())?;

    let arbos_state = ArbosState::open();
    let journal = ctx.journal_mut();
    let arbos_version = arbos_state
        .arbos_version
        .get(journal)
        .map_err(|err| format!("[ARBITRUM] failed to read ArbOS version: {err}"))?;

    arbos_state
        .l1_pricing
        .apply_batch_posting_report_v2(
            arbos_version,
            batch_timestamp,
            current_time,
            batch_poster_address,
            batch_calldata_length,
            batch_calldata_non_zeros,
            batch_extra_gas,
            l1_base_fee_wei,
            journal,
        )
        .map_err(|err| format!("[ARBITRUM] failed to apply batchPostingReportV2: {err}"))?;
    Ok(())
}

fn decode_start_block_calldata(input: &[u8]) -> Result<(U256, u64, u64, u64), String> {
    if input.len() != SELECTOR_SIZE + (START_BLOCK_CALLDATA_WORDS * ABI_WORD_SIZE) {
        return Err(format!(
            "[ARBITRUM] invalid startBlock calldata length {}, expected {}",
            input.len(),
            SELECTOR_SIZE + (START_BLOCK_CALLDATA_WORDS * ABI_WORD_SIZE)
        ));
    }

    let words = &input[SELECTOR_SIZE..];
    let l1_base_fee = word_to_u256(&words[0..ABI_WORD_SIZE]);
    let l1_block_number = word_to_u64(&words[ABI_WORD_SIZE..ABI_WORD_SIZE * 2]);
    let l2_block_number = word_to_u64(&words[ABI_WORD_SIZE * 2..ABI_WORD_SIZE * 3]);
    let time_last_block = word_to_u64(&words[ABI_WORD_SIZE * 3..ABI_WORD_SIZE * 4]);

    Ok((
        l1_base_fee,
        l1_block_number,
        l2_block_number,
        time_last_block,
    ))
}

fn decode_batch_posting_report_calldata(
    input: &[u8],
) -> Result<(U256, Address, u64, u64, U256), String> {
    if input.len() != SELECTOR_SIZE + (BATCH_POSTING_REPORT_CALLDATA_WORDS * ABI_WORD_SIZE) {
        return Err(format!(
            "[ARBITRUM] invalid batchPostingReport calldata length {}, expected {}",
            input.len(),
            SELECTOR_SIZE + (BATCH_POSTING_REPORT_CALLDATA_WORDS * ABI_WORD_SIZE)
        ));
    }

    let words = &input[SELECTOR_SIZE..];
    let batch_timestamp = word_to_u256(&words[0..ABI_WORD_SIZE]);
    let batch_poster_address = word_to_address(&words[ABI_WORD_SIZE..ABI_WORD_SIZE * 2]);
    let batch_number = word_to_u64(&words[ABI_WORD_SIZE * 2..ABI_WORD_SIZE * 3]);
    let batch_data_gas = word_to_u64(&words[ABI_WORD_SIZE * 3..ABI_WORD_SIZE * 4]);
    let l1_base_fee_wei = word_to_u256(&words[ABI_WORD_SIZE * 4..ABI_WORD_SIZE * 5]);

    Ok((
        batch_timestamp,
        batch_poster_address,
        batch_number,
        batch_data_gas,
        l1_base_fee_wei,
    ))
}

fn decode_batch_posting_report_v2_calldata(
    input: &[u8],
) -> Result<(U256, Address, u64, u64, u64, u64, U256), String> {
    if input.len() != SELECTOR_SIZE + (BATCH_POSTING_REPORT_V2_CALLDATA_WORDS * ABI_WORD_SIZE) {
        return Err(format!(
            "[ARBITRUM] invalid batchPostingReportV2 calldata length {}, expected {}",
            input.len(),
            SELECTOR_SIZE + (BATCH_POSTING_REPORT_V2_CALLDATA_WORDS * ABI_WORD_SIZE)
        ));
    }

    let words = &input[SELECTOR_SIZE..];
    let batch_timestamp = word_to_u256(&words[0..ABI_WORD_SIZE]);
    let batch_poster_address = word_to_address(&words[ABI_WORD_SIZE..ABI_WORD_SIZE * 2]);
    let batch_number = word_to_u64(&words[ABI_WORD_SIZE * 2..ABI_WORD_SIZE * 3]);
    let batch_calldata_length = word_to_u64(&words[ABI_WORD_SIZE * 3..ABI_WORD_SIZE * 4]);
    let batch_calldata_non_zeros = word_to_u64(&words[ABI_WORD_SIZE * 4..ABI_WORD_SIZE * 5]);
    let batch_extra_gas = word_to_u64(&words[ABI_WORD_SIZE * 5..ABI_WORD_SIZE * 6]);
    let l1_base_fee_wei = word_to_u256(&words[ABI_WORD_SIZE * 6..ABI_WORD_SIZE * 7]);

    Ok((
        batch_timestamp,
        batch_poster_address,
        batch_number,
        batch_calldata_length,
        batch_calldata_non_zeros,
        batch_extra_gas,
        l1_base_fee_wei,
    ))
}

fn word_to_u256(word: &[u8]) -> U256 {
    let bytes: [u8; ABI_WORD_SIZE] =
        <[u8; ABI_WORD_SIZE]>::try_from(word).expect("ABI word is always 32 bytes");
    U256::from_be_bytes(bytes)
}

fn word_to_address(word: &[u8]) -> Address {
    Address::from_slice(&word[ABI_WORD_SIZE - 20..ABI_WORD_SIZE])
}

fn word_to_u64(word: &[u8]) -> u64 {
    let tail: [u8; 8] = <[u8; 8]>::try_from(&word[ABI_WORD_SIZE - 8..ABI_WORD_SIZE])
        .expect("ABI word tail is always 8 bytes");
    u64::from_be_bytes(tail)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
