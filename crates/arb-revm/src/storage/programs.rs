use eyre::Result;
use revm::{
    context_interface::{context::SStoreResult, journaled_state::StateLoad},
    primitives::{B256, U256},
};

use super::{AddressSet, StorageBacked, StorageSpace};
use crate::arb_journal::ArbJournal;

// ---------------------------------------------------------------------------
// Stylus genesis initial-value constants (Nitro: arbos/programs/params.go)
// ---------------------------------------------------------------------------

/// Stylus params version at genesis. Nitro: hard-coded 1 in `initStylusParams`.
pub const INITIAL_STYLUS_VERSION: u32 = 1;
/// ink price: 1 EVM gas buys 10_000 ink. Nitro: `initialInkPrice`.
pub const INITIAL_INK_PRICE: u32 = 10_000;
/// Initial WASM stack depth limit (4 × 64 KiB pages). Nitro: `initialStackDepth = 4 * 65536`.
pub const INITIAL_MAX_STACK_DEPTH: u32 = 4 * 65_536; // 262144
/// Pages given for free per tx. Nitro: `InitialFreePages`.
pub const INITIAL_FREE_PAGES: u32 = 2;
/// Linear gas cost per additional memory page. Nitro: `InitialPageGas`.
pub const INITIAL_PAGE_GAS: u32 = 1_000;
/// Page limit (reject WASMs with memories > 8 MiB). Nitro: `initialPageLimit`.
pub const INITIAL_PAGE_LIMIT: u32 = 128;
/// Minimum initialization gas in 128-gas increments. Nitro: `initialMinInitGas`.
pub const INITIAL_MIN_INIT_GAS: u32 = 72;
/// Minimum cached-init gas in 32-gas increments. Nitro: `initialMinCachedGas`.
pub const INITIAL_MIN_CACHED_INIT_GAS: u32 = 11;
/// Initialization cost scalar in 2% increments. Nitro: `initialInitCostScalar`.
pub const INITIAL_INIT_COST_SCALAR: u32 = 50;
/// Cached cost scalar in 2% increments. Nitro: `initialCachedCostScalar`.
pub const INITIAL_CACHED_COST_SCALAR: u32 = 50;
/// Program expiry in days. Nitro: `initialExpiryDays`.
pub const INITIAL_EXPIRY_DAYS: u32 = 365;
/// Keepalive wait in days before re-activation is allowed. Nitro: `initialKeepaliveDays`.
pub const INITIAL_KEEPALIVE_DAYS: u32 = 31;
/// Block-level recent-program cache size. Nitro: `initialRecentCacheSize`.
pub const INITIAL_BLOCK_CACHE_SIZE: u32 = 32;
/// Max decompressed WASM size in bytes (stored only for ArbOS ≥ 40). Nitro: `initialMaxWasmSize`.
pub const INITIAL_MAX_WASM_SIZE: u32 = 128 * 1_024; // 131072
/// Max fragment count (stored only for ArbOS ≥ 60 / StylusContractLimit). Nitro: `initialMaxFragmentCount`.
pub const INITIAL_MAX_FRAGMENT_COUNT: u32 = 2;
/// Stylus params version after v31 upgrade. Nitro: `v2MinInitGas` bump companion.
pub const V2_STYLUS_VERSION: u32 = 2;
/// Revised minimum initialization gas used from ArbOS v31 onward (128-gas increments).
/// Nitro: `v2MinInitGas = 69` (→ 69 × 128 = 8832 gas; cachedGas is added on top from v2).
pub const V2_MIN_INIT_GAS: u32 = 69;

// ---------------------------------------------------------------------------
// Data-pricer genesis initial-value constants (Nitro: arbos/programs/data_pricer.go)
// ---------------------------------------------------------------------------

/// Arbitrum epoch start (Unix). Nitro: `ArbitrumStartTime = 1421388000`.
pub const ARBITRUM_START_TIME: u64 = 1_421_388_000;
/// Initial data-pricer bytes-per-second refill rate.
/// Nitro: `InitialHourlyBytes = 1 * (1<<40) / (365*24)`, then `/ 3600`.
/// Value: ((1u64 << 40) / (365 * 24)) / 3600 = 34865.
pub const INITIAL_BYTES_PER_SECOND: u32 = 34_865;
/// Minimum data price per byte. Nitro: `initialMinPrice = 82928201` (≈ $1 for 5 MiB).
pub const INITIAL_DATA_MIN_PRICE: u32 = 82_928_201;
/// Price inertia (expensive at 1 TiB of demand). Nitro: `initialInertia = 21360419`.
pub const INITIAL_DATA_INERTIA: u32 = 21_360_419;

const PARAMS_KEY: u8 = 0;
const PROGRAM_DATA_KEY: u8 = 1;
const MODULE_HASHES_KEY: u8 = 2;
const DATA_PRICER_KEY: u8 = 3;
const CACHE_MANAGERS_KEY: u8 = 4;
const ACTIVATION_GAS_KEY: u8 = 5;

const DATA_PRICER_DEMAND_OFFSET: u8 = 0;
const DATA_PRICER_BYTES_PER_SECOND_OFFSET: u8 = 1;
const DATA_PRICER_LAST_UPDATE_TIME_OFFSET: u8 = 2;
const DATA_PRICER_MIN_PRICE_OFFSET: u8 = 3;
const DATA_PRICER_INERTIA_OFFSET: u8 = 4;

/// Byte positions of each field within the packed Stylus params storage word (index 0).
///
/// Nitro stores all Stylus configuration parameters tightly packed inside a
/// single 32-byte storage word at key-index 0 of the `params` subspace.  Each
/// constant names the *start byte* and *byte length* of the field.
///
/// Nitro reference: arbos/programs/params.go – Params() / Save().
pub mod stylus_param_layout {
    pub const VERSION:              (usize, usize) = (0,  2);  // uint16
    pub const INK_PRICE:            (usize, usize) = (2,  3);  // uint24
    pub const MAX_STACK_DEPTH:      (usize, usize) = (5,  4);  // uint32
    pub const FREE_PAGES:           (usize, usize) = (9,  2);  // uint16
    pub const PAGE_GAS:             (usize, usize) = (11, 2);  // uint16
    pub const PAGE_LIMIT:           (usize, usize) = (13, 2);  // uint16
    pub const MIN_INIT_GAS:         (usize, usize) = (15, 1);  // uint8
    pub const MIN_CACHED_INIT_GAS:  (usize, usize) = (16, 1);  // uint8
    pub const INIT_COST_SCALAR:     (usize, usize) = (17, 1);  // uint8
    pub const CACHED_COST_SCALAR:   (usize, usize) = (18, 1);  // uint8
    pub const EXPIRY_DAYS:          (usize, usize) = (19, 2);  // uint16
    pub const KEEPALIVE_DAYS:       (usize, usize) = (21, 2);  // uint16
    pub const BLOCK_CACHE_SIZE:     (usize, usize) = (23, 2);  // uint16
    /// MaxWasmSize field present only for ArbOS >= 40.
    pub const MAX_WASM_SIZE:        (usize, usize) = (25, 4);  // uint32
    /// MaxFragmentCount field present only for ArbOS >= 60.
    pub const MAX_FRAGMENT_COUNT:   (usize, usize) = (29, 1);  // uint8
    // Bytes [30..31] are unused padding.

    /// PageRamp is NOT stored in the packed word.  Nitro always uses the
    /// initialPageRamp constant (620674314).
    pub const PAGE_RAMP_CONSTANT: u64 = 620_674_314;
}

/// Stored metadata for an activated Stylus program (Nitro `Program`), read from the
/// program_data mapping. These are the values set at activation time and are the source of
/// truth for init/page gas, Nitro reads them rather than re-deriving from the WASM.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProgramInfo {
    pub version: u16,
    pub init_cost: u16,
    pub cached_cost: u16,
    pub footprint: u16,
    pub activated_at: u32,
    pub asm_estimate_kb: u32,
    pub cached: bool,
}

/// Typed view over ArbOS Programs substorage roots.
#[derive(Debug)]
pub struct ArbosPrograms {
    pub root: StorageSpace,
    pub params: StorageSpace,
    pub program_data: StorageSpace,
    pub module_hashes: StorageSpace,
    pub data_pricer: ProgramDataPricer,
    pub cache_managers: AddressSet,
    pub activation_gas: StorageBacked<u64>,
}

impl ArbosPrograms {
    pub fn open(storage: &StorageSpace) -> Self {
        let root = storage.clone();
        let params = root.open_subspace_with_key(PARAMS_KEY);
        let program_data = root.open_subspace_with_key(PROGRAM_DATA_KEY);
        let module_hashes = root.open_subspace_with_key(MODULE_HASHES_KEY);
        let data_pricer_space = root.open_subspace_with_key(DATA_PRICER_KEY);
        let cache_managers_space = root.open_subspace_with_key(CACHE_MANAGERS_KEY);
        let activation_gas_space = root.open_subspace_with_key(ACTIVATION_GAS_KEY);

        Self {
            root,
            params,
            program_data,
            module_hashes,
            data_pricer: ProgramDataPricer::open(&data_pricer_space),
            cache_managers: AddressSet::open(&cache_managers_space),
            activation_gas: activation_gas_space.storage_backed(0),
        }
    }

    /// Initializes Stylus genesis state, mirroring Nitro `programs.Initialize`.
    ///
    /// Called from `upgrade_arbos_version` at v30 (passing `arbos_version = 30`).
    /// For ArbOS versions ≥ 40 or ≥ 60 the respective extra fields are also written
    /// if the passed `arbos_version` meets those thresholds (relevant when a chain
    /// bootstraps directly at ≥ 40 rather than upgrading from < 40).
    ///
    /// # Sub-steps (mirrors Nitro `programs.Initialize`)
    ///
    /// 1. `initStylusParams`: builds and writes the packed 32-byte params word.
    /// 2. `initDataPricer`: sets the non-zero data-pricer fields.
    /// 3. `cacheManagers`: Nitro calls `addressSet.Initialize` which sets slot 0 to 0
    ///    a SSTORE no-op on a fresh trie, so we do nothing here.
    pub fn initialize<J: ArbJournal>(
        &self,
        arbos_version: u64,
        journal: &mut J,
    ) -> eyre::Result<()> {
        use stylus_param_layout as l;

        // ── 1. initStylusParams ────────────────────────────────────────────────
        let mut word = [0u8; 32];
        pack_uint(&mut word, l::VERSION.0,             l::VERSION.1,             INITIAL_STYLUS_VERSION);
        pack_uint(&mut word, l::INK_PRICE.0,           l::INK_PRICE.1,           INITIAL_INK_PRICE);
        pack_uint(&mut word, l::MAX_STACK_DEPTH.0,     l::MAX_STACK_DEPTH.1,     INITIAL_MAX_STACK_DEPTH);
        pack_uint(&mut word, l::FREE_PAGES.0,          l::FREE_PAGES.1,          INITIAL_FREE_PAGES);
        pack_uint(&mut word, l::PAGE_GAS.0,            l::PAGE_GAS.1,            INITIAL_PAGE_GAS);
        pack_uint(&mut word, l::PAGE_LIMIT.0,          l::PAGE_LIMIT.1,          INITIAL_PAGE_LIMIT);
        pack_uint(&mut word, l::MIN_INIT_GAS.0,        l::MIN_INIT_GAS.1,        INITIAL_MIN_INIT_GAS);
        pack_uint(&mut word, l::MIN_CACHED_INIT_GAS.0, l::MIN_CACHED_INIT_GAS.1, INITIAL_MIN_CACHED_INIT_GAS);
        pack_uint(&mut word, l::INIT_COST_SCALAR.0,    l::INIT_COST_SCALAR.1,    INITIAL_INIT_COST_SCALAR);
        pack_uint(&mut word, l::CACHED_COST_SCALAR.0,  l::CACHED_COST_SCALAR.1,  INITIAL_CACHED_COST_SCALAR);
        pack_uint(&mut word, l::EXPIRY_DAYS.0,         l::EXPIRY_DAYS.1,         INITIAL_EXPIRY_DAYS);
        pack_uint(&mut word, l::KEEPALIVE_DAYS.0,      l::KEEPALIVE_DAYS.1,      INITIAL_KEEPALIVE_DAYS);
        pack_uint(&mut word, l::BLOCK_CACHE_SIZE.0,    l::BLOCK_CACHE_SIZE.1,    INITIAL_BLOCK_CACHE_SIZE);
        // MaxWasmSize: stored only from ArbOS >= 40 (Nitro: `if arbosVersion >= ArbosVersion_40`)
        if arbos_version >= 40 {
            pack_uint(&mut word, l::MAX_WASM_SIZE.0, l::MAX_WASM_SIZE.1, INITIAL_MAX_WASM_SIZE);
        }
        // MaxFragmentCount: stored only from ArbOS >= 60 / StylusContractLimit
        if arbos_version >= 60 {
            pack_uint(&mut word, l::MAX_FRAGMENT_COUNT.0, l::MAX_FRAGMENT_COUNT.1, INITIAL_MAX_FRAGMENT_COUNT);
        }
        self.write_params_word(word, journal)
            .map_err(|e| eyre::eyre!("programs.initialize: write params word: {e}"))?;

        // ── 2. initDataPricer ──────────────────────────────────────────────────
        // demand = 0 → SSTORE no-op on a fresh trie; skip.
        self.data_pricer
            .bytes_per_second
            .set(INITIAL_BYTES_PER_SECOND, journal)
            .map_err(|e| eyre::eyre!("programs.initialize: set bytes_per_second: {e}"))?;
        // last_update_time = ArbitrumStartTime (1_421_388_000): non-zero, must be stored.
        self.data_pricer
            .last_update_time
            .set(ARBITRUM_START_TIME, journal)
            .map_err(|e| eyre::eyre!("programs.initialize: set last_update_time: {e}"))?;
        self.data_pricer
            .min_price
            .set(INITIAL_DATA_MIN_PRICE, journal)
            .map_err(|e| eyre::eyre!("programs.initialize: set min_price: {e}"))?;
        self.data_pricer
            .inertia
            .set(INITIAL_DATA_INERTIA, journal)
            .map_err(|e| eyre::eyre!("programs.initialize: set inertia: {e}"))?;

        // ── 3. cacheManagers ──────────────────────────────────────────────────
        // Nitro calls `addressSet.Initialize` which writes 0 to the set's size slot.
        // On a fresh trie that slot already holds 0, so this is a no-op.
        // Nothing to do here.

        Ok(())
    }

    /// Reads the packed 32-byte Stylus params word from storage index 0.
    ///
    /// Nitro stores all Stylus parameters tightly packed within a single
    /// 32-byte storage value at key-index 0 of the params subspace.  The
    /// returned array is laid out exactly as Nitro expects (big-endian bytes
    /// matching the `stylus_param_layout` constants).
    pub fn read_params_word<J: ArbJournal>(&self, journal: &mut J) -> Result<[u8; 32]> {
        let word = self
            .params
            .get_u256(U256::ZERO, journal)
            .map_err(|e| eyre::eyre!("Stylus params read error: {e}"))?
            .data;
        Ok(word.to_be_bytes())
    }

    /// Reads the stored metadata for an activated Stylus program, keyed by `code_hash`
    /// (Nitro `getProgram`). The 32-byte word is laid out exactly as Nitro's `setProgram`
    /// packs it. `version == 0` means the program is not activated.
    pub fn read_program<J: ArbJournal>(
        &self,
        code_hash: B256,
        journal: &mut J,
    ) -> Result<ProgramInfo> {
        let word: [u8; 32] = self
            .program_data
            .get_u256(U256::from_be_bytes(code_hash.0), journal)
            .map_err(|e| eyre::eyre!("Stylus program read error: {e}"))?
            .data
            .to_be_bytes();
        Ok(ProgramInfo {
            version: u16::from_be_bytes([word[0], word[1]]),
            init_cost: u16::from_be_bytes([word[2], word[3]]),
            cached_cost: u16::from_be_bytes([word[4], word[5]]),
            footprint: u16::from_be_bytes([word[6], word[7]]),
            activated_at: unpack_uint(&word, 8, 3),
            asm_estimate_kb: unpack_uint(&word, 11, 3),
            cached: word[14] != 0,
        })
    }

    /// Writes a packed 32-byte Stylus params word back to storage index 0.
    pub fn write_params_word<J: ArbJournal>(
        &self,
        word: [u8; 32],
        journal: &mut J,
    ) -> Result<StateLoad<SStoreResult>> {
        let value = U256::from_be_bytes(word);
        self.params
            .set(
                revm::primitives::FixedBytes::from(U256::ZERO.to_be_bytes()),
                value,
                journal,
            )
            .map_err(|e| eyre::eyre!("Stylus params write error: {e}"))
    }
}

/// Nitro `programs/data_pricer` substorage fields.
#[derive(Debug)]
pub struct ProgramDataPricer {
    pub demand: StorageBacked<u32>,
    pub bytes_per_second: StorageBacked<u32>,
    pub last_update_time: StorageBacked<u64>,
    pub min_price: StorageBacked<u32>,
    pub inertia: StorageBacked<u32>,
}

impl ProgramDataPricer {
    pub fn open(storage: &StorageSpace) -> Self {
        Self {
            demand: storage.storage_backed(DATA_PRICER_DEMAND_OFFSET),
            bytes_per_second: storage.storage_backed(DATA_PRICER_BYTES_PER_SECOND_OFFSET),
            last_update_time: storage.storage_backed(DATA_PRICER_LAST_UPDATE_TIME_OFFSET),
            min_price: storage.storage_backed(DATA_PRICER_MIN_PRICE_OFFSET),
            inertia: storage.storage_backed(DATA_PRICER_INERTIA_OFFSET),
        }
    }
}

// ---------------------------------------------------------------------------
// Packed-word byte-extraction helpers.
// ---------------------------------------------------------------------------

/// Reads a big-endian unsigned integer of `len` bytes from `word[start..]`.
/// `len` must be 1, 2, 3, or 4; panics otherwise (const-range violation).
pub fn unpack_uint(word: &[u8; 32], start: usize, len: usize) -> u32 {
    debug_assert!(len <= 4 && start + len <= 32);
    let mut buf = [0u8; 4];
    buf[4 - len..].copy_from_slice(&word[start..start + len]);
    u32::from_be_bytes(buf)
}

/// Reads a big-endian `u64` of 8 bytes from `word[start..]`.
#[allow(dead_code)]
pub fn unpack_u64(word: &[u8; 32], start: usize) -> u64 {
    debug_assert!(start + 8 <= 32);
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&word[start..start + 8]);
    u64::from_be_bytes(buf)
}

/// Writes a big-endian unsigned integer of `len` bytes into `word[start..]`.
/// Only the least-significant `len` bytes of `value` are written.
pub fn pack_uint(word: &mut [u8; 32], start: usize, len: usize, value: u32) {
    debug_assert!(len <= 4 && start + len <= 32);
    let buf = value.to_be_bytes();
    word[start..start + len].copy_from_slice(&buf[4 - len..]);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::default_ctx::{ArbContext, DefaultArb},
        arbos_init::{initialize_arbos_state, ArbosInitConfig},
        storage::ArbosState,
    };
    use revm::{
        context_interface::ContextTr,
        database_interface::EmptyDB,
        primitives::{address, U256},
    };

    /// Compile-time check: INITIAL_BYTES_PER_SECOND must equal 34865.
    const _ASSERT_BPS: () = {
        let v: u32 = (((1u64 << 40) / (365 * 24)) / 3600) as u32;
        assert!(v == 34_865, "INITIAL_BYTES_PER_SECOND must be 34865");
    };

    fn fresh() -> ArbContext<EmptyDB> {
        <ArbContext<EmptyDB> as DefaultArb>::arb()
    }

    fn cfg(version: u64) -> ArbosInitConfig {
        ArbosInitConfig {
            initial_arbos_version: version,
            initial_chain_owner: address!("0x00000000000000000000000000000000000a11ce"),
            chain_id: U256::from(412_346_u64),
            genesis_block_number: 0,
            initial_l1_base_fee: U256::from(50_000_000_000_u64),
            serialized_chain_config: b"{\"chainId\":412346}".to_vec(),
            debug_precompiles: false,
        }
    }

    /// After `programs.initialize(30, ..)` the packed params word must have
    /// exactly the Nitro genesis values, and the data-pricer fields must be set.
    #[test]
    fn programs_initialize_v30_exact_values() {
        let mut ctx = fresh();
        let j = ctx.journal_mut();
        let state = ArbosState::open();
        state.programs.initialize(30, j).expect("programs.initialize(30)");

        let word = state.programs.read_params_word(j).expect("read params word");
        use stylus_param_layout as l;
        assert_eq!(unpack_uint(&word, l::VERSION.0,             l::VERSION.1),             1,      "Version");
        assert_eq!(unpack_uint(&word, l::INK_PRICE.0,           l::INK_PRICE.1),           10_000, "InkPrice");
        assert_eq!(unpack_uint(&word, l::MAX_STACK_DEPTH.0,     l::MAX_STACK_DEPTH.1),     262_144,"MaxStackDepth");
        assert_eq!(unpack_uint(&word, l::FREE_PAGES.0,          l::FREE_PAGES.1),          2,      "FreePages");
        assert_eq!(unpack_uint(&word, l::PAGE_GAS.0,            l::PAGE_GAS.1),            1_000,  "PageGas");
        assert_eq!(unpack_uint(&word, l::PAGE_LIMIT.0,          l::PAGE_LIMIT.1),          128,    "PageLimit");
        assert_eq!(unpack_uint(&word, l::MIN_INIT_GAS.0,        l::MIN_INIT_GAS.1),        72,     "MinInitGas");
        assert_eq!(unpack_uint(&word, l::MIN_CACHED_INIT_GAS.0, l::MIN_CACHED_INIT_GAS.1), 11,    "MinCachedInitGas");
        assert_eq!(unpack_uint(&word, l::INIT_COST_SCALAR.0,    l::INIT_COST_SCALAR.1),    50,     "InitCostScalar");
        assert_eq!(unpack_uint(&word, l::CACHED_COST_SCALAR.0,  l::CACHED_COST_SCALAR.1),  50,     "CachedCostScalar");
        assert_eq!(unpack_uint(&word, l::EXPIRY_DAYS.0,         l::EXPIRY_DAYS.1),         365,    "ExpiryDays");
        assert_eq!(unpack_uint(&word, l::KEEPALIVE_DAYS.0,      l::KEEPALIVE_DAYS.1),      31,     "KeepaliveDays");
        assert_eq!(unpack_uint(&word, l::BLOCK_CACHE_SIZE.0,    l::BLOCK_CACHE_SIZE.1),    32,     "BlockCacheSize");
        // At v30 MaxWasmSize and MaxFragmentCount are NOT stored (ArbOS < 40 / < 60).
        assert_eq!(unpack_uint(&word, l::MAX_WASM_SIZE.0,       l::MAX_WASM_SIZE.1),       0,      "MaxWasmSize must be 0 at v30");
        assert_eq!(unpack_uint(&word, l::MAX_FRAGMENT_COUNT.0,  l::MAX_FRAGMENT_COUNT.1),  0,      "MaxFragmentCount must be 0 at v30");

        // Data pricer fields.
        assert_eq!(state.programs.data_pricer.bytes_per_second.get(j).unwrap(), 34_865,     "bytes_per_second");
        assert_eq!(state.programs.data_pricer.last_update_time.get(j).unwrap(), 1_421_388_000, "last_update_time = ArbitrumStartTime");
        assert_eq!(state.programs.data_pricer.min_price.get(j).unwrap(),        82_928_201, "min_price");
        assert_eq!(state.programs.data_pricer.inertia.get(j).unwrap(),          21_360_419, "inertia");
        assert_eq!(state.programs.data_pricer.demand.get(j).unwrap(),           0,          "demand stays 0");
    }

    /// Full `initialize_arbos_state` at v40 must apply v30 (programs init) + v31 (Version→2,
    /// MinInitGas→69) + v40 (MaxWasmSize→131072). MaxStackDepth must still be 262144 (v50 cap
    /// not yet reached).
    #[test]
    fn full_init_v40_stylus_params() {
        let mut ctx = fresh();
        initialize_arbos_state(&cfg(40), ctx.journal_mut()).expect("init v40");
        let j = ctx.journal_mut();
        let state = ArbosState::open();

        let word = state.programs.read_params_word(j).expect("read params word");
        use stylus_param_layout as l;
        // v31 bumped Version to 2.
        assert_eq!(unpack_uint(&word, l::VERSION.0, l::VERSION.1), 2, "Version must be 2 after v31");
        // v31 set MinInitGas to v2MinInitGas = 69.
        assert_eq!(unpack_uint(&word, l::MIN_INIT_GAS.0, l::MIN_INIT_GAS.1), 69, "MinInitGas must be 69 after v31");
        // v40 stored MaxWasmSize = 128 * 1024 = 131072.
        assert_eq!(unpack_uint(&word, l::MAX_WASM_SIZE.0, l::MAX_WASM_SIZE.1), 131_072, "MaxWasmSize must be 131072 at v40");
        // v50 not yet reached, MaxStackDepth still 262144.
        assert_eq!(unpack_uint(&word, l::MAX_STACK_DEPTH.0, l::MAX_STACK_DEPTH.1), 262_144, "MaxStackDepth unchanged before v50");
        // MaxFragmentCount not yet set (v60 not reached).
        assert_eq!(unpack_uint(&word, l::MAX_FRAGMENT_COUNT.0, l::MAX_FRAGMENT_COUNT.1), 0, "MaxFragmentCount 0 before v60");
    }

    /// Full init at v50: MaxStackDepth must be capped at 22000 by the v50 upgrade.
    #[test]
    fn full_init_v50_stack_depth_capped() {
        let mut ctx = fresh();
        initialize_arbos_state(&cfg(50), ctx.journal_mut()).expect("init v50");
        let j = ctx.journal_mut();
        let state = ArbosState::open();

        let word = state.programs.read_params_word(j).expect("read params word");
        use stylus_param_layout as l;
        assert_eq!(unpack_uint(&word, l::MAX_STACK_DEPTH.0, l::MAX_STACK_DEPTH.1), 22_000, "MaxStackDepth capped at 22000 by v50");
        // Version still 2 (no further stylus version bump at v50).
        assert_eq!(unpack_uint(&word, l::VERSION.0, l::VERSION.1), 2, "Version still 2 at v50");
        // MaxWasmSize preserved from v40.
        assert_eq!(unpack_uint(&word, l::MAX_WASM_SIZE.0, l::MAX_WASM_SIZE.1), 131_072, "MaxWasmSize 131072 at v50");
    }

    /// Full init at v60: MaxFragmentCount must be set to 2 by the v60 upgrade.
    #[test]
    fn full_init_v60_max_fragment_count() {
        let mut ctx = fresh();
        initialize_arbos_state(&cfg(60), ctx.journal_mut()).expect("init v60");
        let j = ctx.journal_mut();
        let state = ArbosState::open();

        let word = state.programs.read_params_word(j).expect("read params word");
        use stylus_param_layout as l;
        assert_eq!(unpack_uint(&word, l::MAX_FRAGMENT_COUNT.0, l::MAX_FRAGMENT_COUNT.1), 2, "MaxFragmentCount must be 2 at v60");
        assert_eq!(unpack_uint(&word, l::MAX_STACK_DEPTH.0, l::MAX_STACK_DEPTH.1), 22_000, "MaxStackDepth capped at 22000");
        assert_eq!(unpack_uint(&word, l::MAX_WASM_SIZE.0, l::MAX_WASM_SIZE.1), 131_072, "MaxWasmSize 131072");
        assert_eq!(unpack_uint(&word, l::VERSION.0, l::VERSION.1), 2, "Version 2");
        assert_eq!(unpack_uint(&word, l::MIN_INIT_GAS.0, l::MIN_INIT_GAS.1), 69, "MinInitGas 69");
    }
}
