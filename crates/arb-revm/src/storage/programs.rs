use super::{AddressSet, StorageBacked, StorageSpace};

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
