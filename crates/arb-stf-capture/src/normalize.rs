//! Deterministic JSON normalization for RPC observations.

use std::collections::BTreeMap;

use alloy_primitives::keccak256;
use eyre::{Result, bail};
use serde_json::{Map, Value};

/// Canonicalizes JSON object order recursively. Array ordering is retained unless
/// the corresponding RPC field is explicitly defined as unordered.
pub fn canonical_json(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonical_json).collect()),
        Value::Object(values) => {
            let values: BTreeMap<_, _> = values
                .into_iter()
                .map(|(key, value)| (key, canonical_json(value)))
                .collect();
            Value::Object(values.into_iter().collect::<Map<_, _>>())
        }
        value => value,
    }
}

/// Normalizes a Nitro execution witness while retaining all protocol bytes.
///
/// Nitro builds code and trie collections through Go maps, so those arrays have
/// no semantic order. Header objects are ordered by descending number; their
/// parent linkage remains part of the captured data for the offline importer to
/// validate.
pub fn normalize_execution_witness(mut witness: Value) -> Result<Value> {
    let object = witness
        .as_object_mut()
        .ok_or_else(|| eyre::eyre!("debug_executionWitnessByHash returned a non-object result"))?;
    for field in ["headers", "codes", "state", "keys"] {
        if !object.contains_key(field) {
            bail!("execution witness is missing {field:?}");
        }
    }
    // `Witness::ToExtWitness` populates headers, codes, and state, but leaves
    // `Keys` nil. Go serializes that nil slice as JSON `null`; it is the same
    // empty key set as `[]` for this witness format. Canonicalize the wire
    // representation before sorting so either producer encoding is stable.
    if object.get("keys").is_some_and(Value::is_null) {
        object.insert("keys".to_owned(), Value::Array(Vec::new()));
    }
    for field in ["codes", "state"] {
        sort_hex_bytes_by_hash(object.get_mut(field).expect("checked above"), field)?;
    }
    sort_hex_bytes_lexicographically(object.get_mut("keys").expect("checked above"), "keys")?;
    sort_headers(object.get_mut("headers").expect("checked above"))?;
    Ok(canonical_json(witness))
}

fn sort_headers(value: &mut Value) -> Result<()> {
    let array = value
        .as_array_mut()
        .ok_or_else(|| eyre::eyre!("execution witness headers is not an array"))?;
    let mut headers = array
        .drain(..)
        .map(|header| {
            let header = canonical_json(header);
            let number = header
                .get("number")
                .and_then(Value::as_str)
                .ok_or_else(|| eyre::eyre!("execution witness header has no quantity number"))?;
            let number = parse_quantity(number)?;
            let bytes = serde_json::to_vec(&header)?;
            Ok::<_, eyre::Report>((number, bytes, header))
        })
        .collect::<Result<Vec<_>>>()?;
    headers.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    *array = headers.into_iter().map(|(_, _, header)| header).collect();
    Ok(())
}

fn sort_hex_bytes_by_hash(value: &mut Value, field: &str) -> Result<()> {
    let array = value
        .as_array_mut()
        .ok_or_else(|| eyre::eyre!("execution witness {field} is not an array"))?;
    let mut values = array
        .drain(..)
        .map(|value| {
            let bytes = decode_hex_value(&value, field)?;
            let hash = keccak256(&bytes);
            Ok::<_, eyre::Report>((hash.0.to_vec(), bytes, value))
        })
        .collect::<Result<Vec<_>>>()?;
    values.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    *array = values.into_iter().map(|(_, _, value)| value).collect();
    Ok(())
}

fn sort_hex_bytes_lexicographically(value: &mut Value, field: &str) -> Result<()> {
    let array = value
        .as_array_mut()
        .ok_or_else(|| eyre::eyre!("execution witness {field} is not an array"))?;
    let mut values = array
        .drain(..)
        .map(|value| Ok::<_, eyre::Report>((decode_hex_value(&value, field)?, value)))
        .collect::<Result<Vec<_>>>()?;
    values.sort_by(|left, right| left.0.cmp(&right.0));
    *array = values.into_iter().map(|(_, value)| value).collect();
    Ok(())
}

fn decode_hex_value(value: &Value, field: &str) -> Result<Vec<u8>> {
    let value = value
        .as_str()
        .ok_or_else(|| eyre::eyre!("execution witness {field} has a non-string entry"))?;
    let value = value
        .strip_prefix("0x")
        .ok_or_else(|| eyre::eyre!("execution witness {field} has a non-hex entry"))?;
    Ok(hex::decode(value)?)
}

fn parse_quantity(value: &str) -> Result<u64> {
    let value = value
        .strip_prefix("0x")
        .ok_or_else(|| eyre::eyre!("execution witness header has a non-hex number"))?;
    if value.is_empty() {
        bail!("execution witness header has an empty number");
    }
    Ok(u64::from_str_radix(value, 16)?)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn normalizes_unordered_witness_fields() {
        let value = json!({
            "keys": ["0x02", "0x01"],
            "headers": [{"number": "0x1"}, {"number": "0x2"}],
            "codes": ["0x02", "0x01"],
            "state": ["0x04", "0x03"]
        });
        let normalized = normalize_execution_witness(value).unwrap();
        assert_eq!(normalized["keys"], json!(["0x01", "0x02"]));
        assert_eq!(normalized["headers"][0]["number"], "0x2");
    }

    #[test]
    fn normalizes_nil_key_slice_to_empty_array() {
        let value = json!({
            "keys": null,
            "headers": [],
            "codes": [],
            "state": []
        });
        let normalized = normalize_execution_witness(value).unwrap();
        assert_eq!(normalized["keys"], json!([]));
    }
}
