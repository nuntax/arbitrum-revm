use eyre::Result;
use revm::{
    context_interface::{context::SStoreResult, journaled_state::StateLoad},
    primitives::{B256, U256},
};

use super::{AddressSet, StorageBacked, StorageSpace};
use crate::arb_journal::ArbJournal;

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
/// truth for init/page gas — Nitro reads them rather than re-deriving from the WASM.
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
