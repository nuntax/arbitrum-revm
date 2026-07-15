//! Nitro-only fixture capture orchestration.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use arb_stf_fixture::{
    FixtureCase, FixtureInput, FixtureManifest, FixturePrestate, FixtureProvenance, FixtureResult,
    FixtureSuite, ObjectStore,
};
use eyre::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    normalize::{canonical_json, normalize_execution_witness},
    rpc::RpcClient,
};

pub struct CaptureArgs {
    pub rpc: String,
    pub block: String,
    pub case_id: String,
    pub out: PathBuf,
    pub nitro_revision: String,
    pub nitro_binary: PathBuf,
    pub chain_config: PathBuf,
    pub feed_payload: Option<PathBuf>,
    pub derived_transactions: Option<PathBuf>,
    pub labels: Vec<String>,
    pub required_features: Vec<String>,
}

pub async fn capture_block(args: CaptureArgs) -> Result<()> {
    let oracle = read_oracle_provenance(&args)?;
    let client = RpcClient::new(&args.rpc)?;
    let client_version = client.request("web3_clientVersion", json!([])).await?;
    ensure!(
        client_version
            .as_str()
            .is_some_and(|version| version.to_ascii_lowercase().contains("nitro")),
        "RPC endpoint does not identify itself as Nitro: {client_version}"
    );

    let (block_hash, decoded_block) = resolve_block(&client, &args.block).await?;
    let effective_arbos_version = decode_arbos_version(&decoded_block)?;
    let raw_block = client
        .request("debug_getRawBlock", json!([block_hash]))
        .await?;
    let raw_receipts = client
        .request("debug_getRawReceipts", json!([block_hash]))
        .await?;
    let witness = normalize_execution_witness(
        client
            .request("debug_executionWitnessByHash", json!([block_hash]))
            .await?,
    )?;
    let witness_prestate = capture_witness_prestate(&client, &decoded_block, &witness).await?;
    let call_trace = client
        .request(
            "debug_traceBlockByHash",
            json!([block_hash, {"tracer": "callTracer"}]),
        )
        .await?;
    let state_diff = client
        .request(
            "debug_traceBlockByHash",
            json!([block_hash, {"tracer": "prestateTracer", "tracerConfig": {"diffMode": true}}]),
        )
        .await?;

    validate_raw_block(&raw_block)?;
    validate_raw_receipts(&raw_receipts)?;

    let store = ObjectStore::new(&args.out);
    let prestate = store.put(
        &canonical_bytes(&witness_prestate)?,
        "application/x-ethereum-execution-witness+json",
    )?;
    let input = if let Some(path) = &args.feed_payload {
        let bytes = fs::read(path)
            .wrap_err_with(|| format!("failed to read feed payload {}", path.display()))?;
        FixtureInput::RawFeed {
            object: store.put(&bytes, "application/x-arbitrum-feed-payload")?,
        }
    } else if let Some(path) = &args.derived_transactions {
        let bytes = fs::read(path).wrap_err_with(|| {
            format!(
                "failed to read derived transaction input {}",
                path.display()
            )
        })?;
        FixtureInput::DerivedTransactions {
            object: store.put(&bytes, "application/x-arbitrum-derived-transactions")?,
        }
    } else {
        unreachable!("argument parsing requires one input")
    };
    let expected = json!({
        "schema": "arb-stf-capture-output-v1",
        "nitro_client_version": client_version,
        "block_hash": block_hash,
        "decoded_block": canonical_json(decoded_block),
        "raw_block_rlp": raw_block,
        "raw_receipts_rlp": raw_receipts,
        "call_trace": canonical_json(call_trace),
        "state_diff": canonical_json(state_diff),
    });
    let expected = FixtureResult {
        object: store.put(
            &canonical_bytes(&expected)?,
            "application/x-arbitrum-stf-output+json",
        )?,
    };

    let mut fixture = FixtureCase::new(
        args.case_id.clone(),
        args.labels,
        oracle,
        effective_arbos_version,
        FixturePrestate::ExecutionWitness { object: prestate },
        input,
        expected,
    );
    fixture.required_features = args.required_features;
    fixture.validate()?;
    write_case_and_manifest(&args.out, &fixture)?;
    println!(
        "captured Nitro fixture {} for block {block_hash}",
        fixture.id
    );
    Ok(())
}

/// Reads the ArbOS format version from the canonical block header returned by
/// Nitro. The version is encoded as the big-endian `mixHash[16..24]`; keeping
/// it in the fixture makes the selected EVM rules an asserted input rather
/// than an implicit runner default.
fn decode_arbos_version(block: &Value) -> Result<u64> {
    let extra_data = decode_header_field(block, "extraData")?;
    ensure!(
        extra_data.len() == 32,
        "canonical block extraData has length {}, expected 32",
        extra_data.len()
    );
    let mix_hash = decode_header_field(block, "mixHash")?;
    ensure!(
        mix_hash.len() == 32,
        "canonical block mixHash has length {}, expected 32",
        mix_hash.len()
    );
    let mut version = [0u8; 8];
    version.copy_from_slice(&mix_hash[16..24]);
    let version = u64::from_be_bytes(version);
    ensure!(
        version != 0,
        "canonical block is missing an ArbOS format version"
    );
    Ok(version)
}

fn decode_header_field(block: &Value, field: &str) -> Result<Vec<u8>> {
    let value = block
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| eyre::eyre!("canonical block has no hexadecimal {field}"))?;
    let value = value
        .strip_prefix("0x")
        .ok_or_else(|| eyre::eyre!("canonical block {field} has no 0x prefix"))?;
    hex::decode(value).wrap_err_with(|| format!("canonical block {field} is not valid hex"))
}

async fn capture_witness_prestate(
    client: &RpcClient,
    block: &Value,
    witness: &Value,
) -> Result<Value> {
    let parent_hash = block
        .get("parentHash")
        .and_then(Value::as_str)
        .filter(|hash| is_block_hash(hash))
        .ok_or_else(|| eyre::eyre!("decoded block has no valid parentHash"))?;
    let parent_header_rlp = client
        .request("debug_getRawHeader", json!([parent_hash]))
        .await?;
    validate_raw_header(&parent_header_rlp)?;

    let parent_number = block
        .get("number")
        .and_then(Value::as_str)
        .map(parse_quantity)
        .transpose()?
        .ok_or_else(|| eyre::eyre!("decoded block has no numeric number"))?
        .checked_sub(1)
        .ok_or_else(|| eyre::eyre!("genesis block has no parent execution witness"))?;
    let headers = witness
        .get("headers")
        .and_then(Value::as_array)
        .ok_or_else(|| eyre::eyre!("normalized execution witness has no headers array"))?;
    let mut numbers = BTreeSet::from([parent_number]);
    for header in headers {
        let number = header
            .get("number")
            .and_then(Value::as_str)
            .ok_or_else(|| eyre::eyre!("execution witness header has no number"))?;
        numbers.insert(parse_quantity(number)?);
    }
    let mut raw_headers = Vec::with_capacity(numbers.len());
    for number in numbers.into_iter().rev() {
        let rlp = if number == parent_number {
            parent_header_rlp.clone()
        } else {
            client
                .request("debug_getRawHeader", json!([format!("0x{number:x}")]))
                .await?
        };
        validate_raw_header(&rlp)?;
        raw_headers.push(json!({"number": number, "rlp": rlp}));
    }
    Ok(json!({
        "schema": "arb-stf-execution-witness-v1",
        "parent_header_rlp": parent_header_rlp,
        "raw_headers": raw_headers,
        "witness": witness,
    }))
}

fn read_oracle_provenance(args: &CaptureArgs) -> Result<FixtureProvenance> {
    let binary = fs::read(&args.nitro_binary).wrap_err_with(|| {
        format!(
            "failed to read Nitro binary {}",
            args.nitro_binary.display()
        )
    })?;
    let config = fs::read(&args.chain_config).wrap_err_with(|| {
        format!(
            "failed to read chain config {}",
            args.chain_config.display()
        )
    })?;
    Ok(FixtureProvenance {
        name: "nitro".to_owned(),
        git_revision: args.nitro_revision.clone(),
        binary_sha256: digest(&binary),
        chain_config_sha256: digest(&config),
    })
}

async fn resolve_block(client: &RpcClient, requested: &str) -> Result<(String, Value)> {
    let block = if is_block_hash(requested) {
        client
            .request("eth_getBlockByHash", json!([requested, false]))
            .await?
    } else {
        let quantity = normalized_quantity(requested)?;
        client
            .request("eth_getBlockByNumber", json!([quantity, false]))
            .await?
    };
    if block.is_null() {
        bail!("requested block {requested:?} is unavailable");
    }
    let hash = block
        .get("hash")
        .and_then(Value::as_str)
        .filter(|hash| is_block_hash(hash))
        .ok_or_else(|| eyre::eyre!("requested block {requested:?} has no valid hash"))?;
    Ok((hash.to_owned(), block))
}

fn validate_raw_block(value: &Value) -> Result<()> {
    let value = value
        .as_str()
        .ok_or_else(|| eyre::eyre!("debug_getRawBlock did not return hex bytes"))?;
    ensure!(
        value.starts_with("0x") && value.len() > 2,
        "debug_getRawBlock returned empty bytes"
    );
    hex::decode(&value[2..]).context("debug_getRawBlock returned invalid hex")?;
    Ok(())
}

fn validate_raw_header(value: &Value) -> Result<()> {
    let value = value
        .as_str()
        .ok_or_else(|| eyre::eyre!("debug_getRawHeader did not return hex bytes"))?;
    ensure!(
        value.starts_with("0x") && value.len() > 2,
        "debug_getRawHeader returned empty bytes"
    );
    hex::decode(&value[2..]).context("debug_getRawHeader returned invalid hex")?;
    Ok(())
}

fn validate_raw_receipts(value: &Value) -> Result<()> {
    let receipts = value
        .as_array()
        .ok_or_else(|| eyre::eyre!("debug_getRawReceipts did not return an array"))?;
    for receipt in receipts {
        let receipt = receipt
            .as_str()
            .ok_or_else(|| eyre::eyre!("debug_getRawReceipts contained a non-hex receipt"))?;
        ensure!(
            receipt.starts_with("0x"),
            "debug_getRawReceipts contained invalid hex"
        );
        hex::decode(&receipt[2..]).context("debug_getRawReceipts contained invalid hex")?;
    }
    Ok(())
}

fn write_case_and_manifest(root: &Path, fixture: &FixtureCase) -> Result<()> {
    let case_path = root.join("cases").join(&fixture.id).join("case.json");
    if case_path.exists() {
        bail!(
            "refusing to overwrite existing fixture {}",
            case_path.display()
        );
    }
    let manifest_path = root.join("manifest.json");
    let mut manifest = if manifest_path.exists() {
        serde_json::from_slice::<FixtureManifest>(&fs::read(&manifest_path)?)?
    } else {
        FixtureManifest::empty()
    };
    manifest.cases.push(FixtureSuite::from_case(fixture));
    manifest.cases.sort_by(|left, right| left.id.cmp(&right.id));
    manifest.validate()?;

    let case_parent = case_path.parent().expect("case paths have a parent");
    fs::create_dir_all(case_parent)?;
    let temporary_case = case_path.with_extension("json.tmp");
    fs::write(&temporary_case, serde_json::to_vec_pretty(fixture)?)?;
    fs::rename(&temporary_case, &case_path)?;
    let temporary_manifest = manifest_path.with_extension("json.tmp");
    fs::write(&temporary_manifest, serde_json::to_vec_pretty(&manifest)?)?;
    fs::rename(&temporary_manifest, manifest_path)?;
    Ok(())
}

fn canonical_bytes(value: &Value) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&canonical_json(value.clone()))?)
}

fn normalized_quantity(value: &str) -> Result<String> {
    if let Some(value) = value.strip_prefix("0x") {
        ensure!(
            !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid block quantity"
        );
        let parsed = u64::from_str_radix(value, 16)?;
        return Ok(format!("0x{parsed:x}"));
    }
    let parsed = value.parse::<u64>()?;
    Ok(format!("0x{parsed:x}"))
}

fn parse_quantity(value: &str) -> Result<u64> {
    let value = value
        .strip_prefix("0x")
        .ok_or_else(|| eyre::eyre!("quantity must have a 0x prefix"))?;
    ensure!(
        !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid quantity"
    );
    Ok(u64::from_str_radix(value, 16)?)
}

fn is_block_hash(value: &str) -> bool {
    value.len() == 66
        && value.starts_with("0x")
        && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_decimal_and_hex_block_quantities() {
        assert_eq!(normalized_quantity("42").unwrap(), "0x2a");
        assert_eq!(normalized_quantity("0x2A").unwrap(), "0x2a");
    }

    #[test]
    fn parses_header_quantities() {
        assert_eq!(parse_quantity("0x2a").unwrap(), 42);
        assert!(parse_quantity("42").is_err());
    }

    #[test]
    fn recognizes_only_full_hashes() {
        assert!(!is_block_hash("0x1234"));
        assert!(is_block_hash(&format!("0x{}", "ab".repeat(32))));
    }

    #[test]
    fn decodes_arbos_version_from_canonical_header_fields() {
        let block = json!({
            "extraData": format!("0x{}", "00".repeat(32)),
            "mixHash": "0x0000000000000000000000000000032c00000000000000280000000000000000",
        });

        assert_eq!(decode_arbos_version(&block).unwrap(), 40);
    }

    #[test]
    fn rejects_a_non_arbitrum_header() {
        let block = json!({
            "extraData": format!("0x{}", "00".repeat(32)),
            "mixHash": format!("0x{}", "00".repeat(32)),
        });

        assert!(decode_arbos_version(&block).is_err());
    }
}
