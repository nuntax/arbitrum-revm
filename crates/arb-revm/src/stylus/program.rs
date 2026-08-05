//! Stylus program compile / activate / extraction + the compiled-program LRU cache.
//!
//! Ported from arbos-revm's `stylus_executor.rs`, only the runtime-facing, Context-free
//! helpers, against the canonical Nitro `stylus` crate. These wrap `native::compile` /
//! `native::activate` and brotli decompression; no revm Context is involved here.

use std::{num::NonZeroUsize, sync::Mutex};

use arbutil::{Bytes32, evm::api::Ink};
use lru::LruCache;
use revm::{
    interpreter::Gas,
    primitives::{Address, B256, Bytes, FixedBytes},
};
use stylus::{
    brotli::{self, Dictionary},
    native,
    prover::{
        machine::Module,
        programs::{
            StylusData,
            config::{CompileConfig, PricingParams},
        },
    },
};
use wasmer_types::target::Target;

use super::constants::{
    ARBOS_VERSION_STYLUS_CONTRACT_LIMIT, STYLUS_DISCRIMINANT, STYLUS_FRAGMENT_DISCRIMINANT,
    STYLUS_ROOT_DISCRIMINANT,
};

type ProgramCacheEntry = (Vec<u8>, Module, StylusData);
const ADDRESS_LEN: usize = 20;

lazy_static::lazy_static! {
    /// Compiled-program cache keyed by code hash: (serialized native module, prover module,
    /// Stylus metadata). Mirrors Nitro's in-memory program cache.
    pub static ref PROGRAM_CACHE: Mutex<LruCache<FixedBytes<32>, ProgramCacheEntry>> =
        Mutex::new(LruCache::new(NonZeroUsize::new(1024).unwrap()));

    /// Cranelift-compiled modules keyed by code hash, populated only by native stack overflow
    /// recovery. Small because overflowing programs are rare.
    static ref CRANELIFT_CACHE: Mutex<LruCache<FixedBytes<32>, Vec<u8>>> =
        Mutex::new(LruCache::new(NonZeroUsize::new(16).unwrap()));
}

/// Whether `bytecode` is a directly executable Stylus program at `arbos_version`.
pub fn is_stylus_program(bytecode: &[u8], arbos_version: u64) -> bool {
    has_payload(bytecode, STYLUS_DISCRIMINANT)
        || (arbos_version >= ARBOS_VERSION_STYLUS_CONTRACT_LIMIT
            && has_payload(bytecode, STYLUS_ROOT_DISCRIMINANT))
}

/// Whether `bytecode` is a Stylus code component accepted at contract creation.
pub fn is_stylus_component(bytecode: &[u8], arbos_version: u64) -> bool {
    is_stylus_program(bytecode, arbos_version)
        || (arbos_version >= ARBOS_VERSION_STYLUS_CONTRACT_LIMIT
            && has_payload(bytecode, STYLUS_FRAGMENT_DISCRIMINANT))
}

#[inline]
fn has_payload(bytecode: &[u8], prefix: &[u8]) -> bool {
    bytecode.len() > prefix.len() && bytecode.starts_with(prefix)
}

/// Extract and decompress the WASM for a Stylus program. Root programs are reconstructed by
/// loading their fragments through `load_fragment`. Limits specific to activation are enforced
/// only when `activation` is true, matching Nitro's distinction between activation and runtime
/// cache preparation.
pub fn stylus_code<F>(
    bytecode: &[u8],
    arbos_version: u64,
    max_wasm_size: u32,
    max_fragment_count: u8,
    activation: bool,
    mut load_fragment: F,
) -> Result<Option<Bytes>, Vec<u8>>
where
    F: FnMut(Address) -> Result<Bytes, Vec<u8>>,
{
    if has_payload(bytecode, STYLUS_DISCRIMINANT) {
        return classic_stylus_code(bytecode, max_wasm_size).map(Some);
    }

    if arbos_version < ARBOS_VERSION_STYLUS_CONTRACT_LIMIT {
        return Ok(None);
    }
    if has_payload(bytecode, STYLUS_FRAGMENT_DISCRIMINANT) {
        return Err(
            b"fragmented stylus programs cannot be activated directly; activate the root program instead"
                .to_vec(),
        );
    }
    if !has_payload(bytecode, STYLUS_ROOT_DISCRIMINANT) {
        return Ok(None);
    }

    let root = StylusRoot::parse(bytecode)?;
    if activation {
        if root.decompressed_length > max_wasm_size {
            return Err(format!(
                "invalid wasm: decompressedLength {} is greater then MaxWasmSize {}",
                root.decompressed_length, max_wasm_size
            )
            .into_bytes());
        }
        if root.addresses.len() > usize::from(max_fragment_count) {
            return Err(format!(
                "invalid wasm: fragment count exceeds limit of {max_fragment_count}"
            )
            .into_bytes());
        }
    }
    if root.addresses.is_empty() {
        return Err(b"invalid wasm: fragment count cannot be zero".to_vec());
    }

    let mut compressed = Vec::new();
    for address in root.addresses {
        let fragment = load_fragment(address)?;
        let Some(payload) = fragment.strip_prefix(STYLUS_FRAGMENT_DISCRIMINANT) else {
            return Err(b"specified bytecode is not a Stylus program fragment".to_vec());
        };
        if payload.is_empty() {
            return Err(b"specified bytecode is not a Stylus program fragment".to_vec());
        }
        compressed.extend_from_slice(payload);
    }

    let wasm = decompress(&compressed, root.dictionary)?;
    if wasm.len() != root.decompressed_length as usize {
        return Err(format!(
            "invalid wasm: decompressed length {} does not match expected length {}",
            wasm.len(),
            root.decompressed_length
        )
        .into_bytes());
    }
    Ok(Some(Bytes::from(wasm)))
}

fn classic_stylus_code(bytecode: &[u8], max_wasm_size: u32) -> Result<Bytes, Vec<u8>> {
    let Some(rest) = bytecode.strip_prefix(STYLUS_DISCRIMINANT) else {
        return Err(b"specified bytecode is not a Stylus program".to_vec());
    };
    let Some((dictionary, compressed)) = rest.split_at_checked(1) else {
        return Err(b"specified bytecode is not a Stylus program".to_vec());
    };
    let wasm = decompress(compressed, dictionary[0])?;
    if wasm.len() > max_wasm_size as usize {
        return Err(format!(
            "invalid wasm: decompressed length {} exceeds maximum {max_wasm_size}",
            wasm.len()
        )
        .into_bytes());
    }
    Ok(Bytes::from(wasm))
}

fn decompress(compressed: &[u8], dictionary: u8) -> Result<Vec<u8>, Vec<u8>> {
    let dictionary = match dictionary {
        0x00 => Dictionary::Empty,
        0x01 => Dictionary::StylusProgram,
        t => return Err(format!("unsupported dictionary {t}").into_bytes()),
    };
    let wasm = brotli::decompress(compressed, dictionary).or_else(|err| {
        // Uncompressed deployments are allowed when the dictionary is empty.
        if dictionary == Dictionary::Empty {
            Ok(compressed.to_vec())
        } else {
            Err(format!("failed decompression: {}", err as u8).into_bytes())
        }
    })?;
    Ok(wasm)
}

#[derive(Debug, PartialEq, Eq)]
struct StylusRoot {
    dictionary: u8,
    decompressed_length: u32,
    addresses: Vec<Address>,
}

impl StylusRoot {
    fn parse(bytecode: &[u8]) -> Result<Self, Vec<u8>> {
        if !has_payload(bytecode, STYLUS_ROOT_DISCRIMINANT) {
            return Err(b"specified bytecode is not a Stylus program root".to_vec());
        }
        if bytecode.len() < 8 {
            return Err(format!(
                "stylus program root too short: need at least 8 bytes, got {}",
                bytecode.len()
            )
            .into_bytes());
        }
        let address_data = &bytecode[8..];
        let (addresses, remainder) = address_data.as_chunks::<ADDRESS_LEN>();
        if !remainder.is_empty() {
            return Err(format!(
                "stylus program root address data has invalid length: {} (must be multiple of {})",
                address_data.len(),
                ADDRESS_LEN
            )
            .into_bytes());
        }
        Ok(Self {
            dictionary: bytecode[3],
            decompressed_length: u32::from_be_bytes(bytecode[4..8].try_into().unwrap()),
            addresses: addresses.iter().copied().map(Address::new).collect(),
        })
    }
}

/// Compile WASM to a serialized native module via Nitro's stylus runtime, using the
/// single-pass compiler.
pub fn stylus_compile(wasm: &Bytes, compile_config: &CompileConfig) -> Result<Vec<u8>, String> {
    compile_with(wasm, compile_config, false)
}

/// Compile WASM with Cranelift instead of single-pass. Nitro reaches for this only to recover
/// from a native stack overflow, because Cranelift's generated code uses less native stack.
pub fn stylus_compile_cranelift(
    wasm: &Bytes,
    compile_config: &CompileConfig,
) -> Result<Vec<u8>, String> {
    compile_with(wasm, compile_config, true)
}

fn compile_with(
    wasm: &Bytes,
    compile_config: &CompileConfig,
    cranelift: bool,
) -> Result<Vec<u8>, String> {
    native::compile(
        wasm,
        compile_config.version,
        compile_config.debug.debug_funcs,
        Target::default(),
        cranelift,
    )
    .map_err(|e| e.to_string())
}

/// Cranelift module for `code_hash`, compiling and caching it on first use. Mirrors Nitro
/// `getCraneliftAsm`, which checks the activated-ASM cache and the persistent wasm store before
/// compiling. Kept separate from [`PROGRAM_CACHE`] so the ordinary single-pass path is unchanged.
pub fn cranelift_program(
    code_hash: B256,
    wasm: &Bytes,
    compile_config: &CompileConfig,
) -> Result<Vec<u8>, String> {
    let mut cache = CRANELIFT_CACHE.lock().unwrap();
    cache
        .try_get_or_insert(code_hash, || stylus_compile_cranelift(wasm, compile_config))
        .cloned()
}

/// Activate (validate + instrument) a Stylus program, charging activation gas out of `gas`
/// (the unused remainder is refunded on success; all of it is consumed on failure, matching
/// Nitro). Returns the prover module + Stylus metadata.
pub fn stylus_activate(
    mut gas: Option<&mut Gas>,
    wasm: &Bytes,
    code_hash: B256,
    arbos_version: u16,
    stylus_version: u16,
    page_limit: u16,
    debug: bool,
) -> Result<(Module, StylusData), String> {
    let mut activation_gas = if let Some(gas) = gas.as_deref_mut() {
        let remaining = gas.remaining();
        gas.spend_all();
        remaining
    } else {
        u64::MAX
    };
    let (module, stylus_data) = native::activate(
        wasm,
        &Bytes32::from(code_hash.0),
        stylus_version,
        arbos_version as u64,
        page_limit,
        debug,
        &mut activation_gas,
    )
    .map_err(|e| e.to_string())?;
    if let Some(gas) = gas {
        gas.erase_cost(activation_gas);
    }
    Ok((module, stylus_data))
}

/// Convert Stylus ink to EVM gas (ceiling).
pub fn ink_to_gas_ceil(pricing: PricingParams, ink: Ink) -> u64 {
    ink.0.div_ceil(pricing.ink_price as u64)
}

/// Insert a compiled program into the cache (keyed by code hash).
pub fn cache_program(
    code_hash: B256,
    serialized: Vec<u8>,
    module: Module,
    stylus_data: StylusData,
) {
    let mut cache = PROGRAM_CACHE.lock().unwrap();
    cache.get_or_insert(code_hash, || (serialized, module, stylus_data));
}

#[cfg(test)]
mod tests {
    use super::*;
    use stylus::brotli::{DEFAULT_WINDOW_SIZE, compress};
    use wasm_encoder::{
        CodeSection, ExportKind, ExportSection, Function, FunctionSection, Instruction,
        MemorySection, MemoryType, Module as WasmModule, TypeSection, ValType,
    };

    const FIRST: Address = Address::new([0x11; 20]);
    const SECOND: Address = Address::new([0x22; 20]);

    fn root_code(decompressed_length: usize, addresses: &[Address]) -> Vec<u8> {
        let mut root = STYLUS_ROOT_DISCRIMINANT.to_vec();
        root.push(0);
        root.extend_from_slice(&(decompressed_length as u32).to_be_bytes());
        for address in addresses {
            root.extend_from_slice(address.as_slice());
        }
        root
    }

    fn minimal_stylus_wasm() -> Bytes {
        let mut module = WasmModule::new();
        let mut types = TypeSection::new();
        types.ty().function([ValType::I32], [ValType::I32]);
        module.section(&types);

        let mut functions = FunctionSection::new();
        functions.function(0);
        module.section(&functions);

        let mut memories = MemorySection::new();
        memories.memory(MemoryType {
            minimum: 1,
            maximum: Some(1),
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        module.section(&memories);

        let mut exports = ExportSection::new();
        exports.export("memory", ExportKind::Memory, 0);
        exports.export("user_entrypoint", ExportKind::Func, 0);
        module.section(&exports);

        let mut code = CodeSection::new();
        let mut entrypoint = Function::new([]);
        entrypoint.instruction(&Instruction::I32Const(0));
        entrypoint.instruction(&Instruction::End);
        code.function(&entrypoint);
        module.section(&code);
        Bytes::from(module.finish())
    }

    #[test]
    fn stylus_version_three_activates() {
        let wasm = minimal_stylus_wasm();
        let activated = stylus_activate(None, &wasm, B256::ZERO, 61, 3, 128, false);
        assert!(activated.is_ok(), "{activated:?}");
    }

    /// Native stack overflow recovery is only possible if the Cranelift path actually produces a
    /// module, and if the cache returns the same bytes rather than recompiling.
    #[test]
    fn cranelift_fallback_compiles_and_caches() {
        let wasm = minimal_stylus_wasm();
        let config = CompileConfig::version(3, false);
        let code_hash = B256::repeat_byte(0x5a);

        let direct = stylus_compile_cranelift(&wasm, &config).expect("cranelift compile");
        assert!(!direct.is_empty());

        let first = cranelift_program(code_hash, &wasm, &config).expect("cranelift program");
        let second = cranelift_program(code_hash, &wasm, &config).expect("cached program");
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }

    #[test]
    fn root_and_fragment_prefixes_are_version_gated() {
        let root = root_code(1, &[FIRST]);
        let fragment = [STYLUS_FRAGMENT_DISCRIMINANT, &[1]].concat();

        assert!(!is_stylus_program(&root, 59));
        assert!(!is_stylus_component(&fragment, 59));
        assert!(is_stylus_program(&root, 60));
        assert!(is_stylus_component(&root, 60));
        assert!(!is_stylus_program(&fragment, 60));
        assert!(is_stylus_component(&fragment, 60));
    }

    #[test]
    fn root_program_reassembles_and_decompresses_fragments_in_order() {
        let wasm = b"\0asm\x01\0\0\0root-program";
        let compressed = compress(wasm, 1, DEFAULT_WINDOW_SIZE, Dictionary::Empty).unwrap();
        let split = compressed.len() / 2;
        let first = [STYLUS_FRAGMENT_DISCRIMINANT, &compressed[..split]].concat();
        let second = [STYLUS_FRAGMENT_DISCRIMINANT, &compressed[split..]].concat();
        let root = root_code(wasm.len(), &[FIRST, SECOND]);
        let mut loaded = Vec::new();

        let decoded = stylus_code(&root, 60, 256 * 1024, 4, true, |address| {
            loaded.push(address);
            Ok(if address == FIRST {
                Bytes::from(first.clone())
            } else {
                Bytes::from(second.clone())
            })
        })
        .unwrap()
        .unwrap();

        assert_eq!(decoded.as_ref(), wasm);
        assert_eq!(loaded, [FIRST, SECOND]);
    }

    #[test]
    fn root_program_is_not_recognized_before_arbos_60() {
        let root = root_code(1, &[FIRST]);
        let decoded = stylus_code(&root, 59, 128 * 1024, 0, true, |_| {
            panic!("pre-ArbOS 60 must not load fragments")
        })
        .unwrap();
        assert!(decoded.is_none());
    }

    #[test]
    fn fragment_cannot_be_activated_directly() {
        let fragment = [STYLUS_FRAGMENT_DISCRIMINANT, &[1]].concat();
        let error =
            stylus_code(&fragment, 60, 256 * 1024, 4, true, |_| unreachable!()).unwrap_err();
        assert_eq!(
            error,
            b"fragmented stylus programs cannot be activated directly; activate the root program instead"
        );
    }

    #[test]
    fn activation_enforces_root_limits() {
        let too_large = root_code(257 * 1024, &[FIRST]);
        let error =
            stylus_code(&too_large, 60, 256 * 1024, 4, true, |_| unreachable!()).unwrap_err();
        assert!(
            String::from_utf8(error)
                .unwrap()
                .contains("greater then MaxWasmSize")
        );

        let too_many = root_code(1, &[FIRST, SECOND]);
        let error =
            stylus_code(&too_many, 60, 256 * 1024, 1, true, |_| unreachable!()).unwrap_err();
        assert_eq!(error, b"invalid wasm: fragment count exceeds limit of 1");
    }

    #[test]
    fn root_requires_at_least_one_fragment() {
        let error = stylus_code(
            &root_code(0, &[]),
            60,
            256 * 1024,
            4,
            true,
            |_| unreachable!(),
        )
        .unwrap_err();
        assert_eq!(error, b"invalid wasm: fragment count cannot be zero");
    }

    /// A root header is 3 prefix bytes, 1 dictionary byte, and a 4-byte length, followed by whole
    /// 20-byte addresses. Anything else is malformed and must be rejected before any state read.
    #[test]
    fn malformed_root_headers_are_rejected() {
        let truncated = [STYLUS_ROOT_DISCRIMINANT, &[0, 0, 0, 1]].concat();
        assert_eq!(truncated.len(), 7);
        let error =
            stylus_code(&truncated, 60, 256 * 1024, 4, true, |_| unreachable!()).unwrap_err();
        assert!(
            String::from_utf8(error)
                .unwrap()
                .contains("stylus program root too short"),
        );

        // A root with a complete header but no addresses is the zero-fragment case, not a
        // malformed one, so cut a whole address short instead.
        let mut partial_address = root_code(1, &[FIRST]);
        partial_address.pop();
        let error = stylus_code(
            &partial_address,
            60,
            256 * 1024,
            4,
            true,
            |_| unreachable!(),
        )
        .unwrap_err();
        assert!(
            String::from_utf8(error)
                .unwrap()
                .contains("address data has invalid length: 19"),
        );
    }

    /// The reassembled program must decompress to exactly the length the root declares. This is
    /// checked after decompression, so it applies to the runtime path as well as activation.
    #[test]
    fn declared_decompressed_length_must_match() {
        let wasm = b"\0asm\x01\0\0\0length-mismatch";
        let compressed = compress(wasm, 1, DEFAULT_WINDOW_SIZE, Dictionary::Empty).unwrap();
        let fragment = [STYLUS_FRAGMENT_DISCRIMINANT, &compressed].concat();
        let root = root_code(wasm.len() + 1, &[FIRST]);

        for activation in [true, false] {
            let error = stylus_code(&root, 60, 256 * 1024, 4, activation, |_| {
                Ok(Bytes::from(fragment.clone()))
            })
            .unwrap_err();
            assert_eq!(
                String::from_utf8(error).unwrap(),
                format!(
                    "invalid wasm: decompressed length {} does not match expected length {}",
                    wasm.len(),
                    wasm.len() + 1
                ),
            );
        }
    }

    /// Every fragment a root names must actually carry the fragment prefix and a payload.
    #[test]
    fn fragments_must_carry_a_prefix_and_payload() {
        let root = root_code(1, &[FIRST]);

        let error = stylus_code(&root, 60, 256 * 1024, 4, true, |_| {
            Ok(Bytes::from_static(b"not a fragment"))
        })
        .unwrap_err();
        assert_eq!(
            error,
            b"specified bytecode is not a Stylus program fragment"
        );

        let empty = Bytes::from(STYLUS_FRAGMENT_DISCRIMINANT.to_vec());
        let error = stylus_code(&root, 60, 256 * 1024, 4, true, |_| Ok(empty.clone())).unwrap_err();
        assert_eq!(
            error,
            b"specified bytecode is not a Stylus program fragment"
        );
    }

    /// Nitro enforces MaxWasmSize and MaxFragmentCount only when activating. Runtime cache
    /// preparation re-reads programs that were activated under older, looser limits, so it must
    /// keep loading them.
    #[test]
    fn runtime_path_skips_activation_only_limits() {
        let wasm = b"\0asm\x01\0\0\0runtime-load";
        let compressed = compress(wasm, 1, DEFAULT_WINDOW_SIZE, Dictionary::Empty).unwrap();
        let split = compressed.len() / 2;
        let first = [STYLUS_FRAGMENT_DISCRIMINANT, &compressed[..split]].concat();
        let second = [STYLUS_FRAGMENT_DISCRIMINANT, &compressed[split..]].concat();
        let root = root_code(wasm.len(), &[FIRST, SECOND]);

        let load = |address: Address| {
            Ok(Bytes::from(if address == FIRST {
                first.clone()
            } else {
                second.clone()
            }))
        };

        // Both limits are violated: the declared length exceeds MaxWasmSize, and there are more
        // fragments than allowed.
        let activating = stylus_code(&root, 60, 1, 1, true, load).unwrap_err();
        assert!(
            String::from_utf8(activating)
                .unwrap()
                .contains("greater then MaxWasmSize"),
        );

        let decoded = stylus_code(&root, 60, 1, 1, false, load).unwrap().unwrap();
        assert_eq!(decoded.as_ref(), wasm);
    }

    #[test]
    fn unsupported_dictionary_is_rejected() {
        let mut root = STYLUS_ROOT_DISCRIMINANT.to_vec();
        root.push(0x02);
        root.extend_from_slice(&1u32.to_be_bytes());
        root.extend_from_slice(FIRST.as_slice());

        let fragment = [STYLUS_FRAGMENT_DISCRIMINANT, b"payload"].concat();
        let error = stylus_code(&root, 60, 256 * 1024, 4, true, |_| {
            Ok(Bytes::from(fragment.clone()))
        })
        .unwrap_err();
        assert_eq!(error, b"unsupported dictionary 2");
    }
}
