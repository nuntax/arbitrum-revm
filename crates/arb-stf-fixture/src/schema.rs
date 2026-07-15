//! Versioned, neutral fixture manifests.
//!
//! The schema uses content references for all potentially large input and output
//! payloads. Values in those objects are protocol data, not Rust execution types.

use std::{collections::BTreeSet, fmt, path::Path};

use serde::{Deserialize, Serialize};

pub const STF_FIXTURE_SCHEMA: &str = "arb-stf-fixture";
pub const STF_FIXTURE_SCHEMA_VERSION: u32 = 3;

/// Compression used for a fixture object. Object identity is always over the
/// decompressed bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObjectCompression {
    Zstd,
}

/// A content-addressed immutable object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureObject {
    pub sha256: String,
    pub compression: ObjectCompression,
    pub uncompressed_length: u64,
    pub media_type: String,
}

impl FixtureObject {
    pub fn validate(&self) -> Result<(), FixtureError> {
        if !is_lower_hex_digest(&self.sha256) {
            return Err(FixtureError::InvalidObjectDigest(self.sha256.clone()));
        }
        if self.media_type.is_empty() || self.media_type.contains(char::is_whitespace) {
            return Err(FixtureError::InvalidMediaType(self.media_type.clone()));
        }
        Ok(())
    }
}

/// Immutable provenance for the sole behavioral oracle used to produce expected
/// output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureProvenance {
    pub name: String,
    pub git_revision: String,
    pub binary_sha256: String,
    pub chain_config_sha256: String,
}

impl FixtureProvenance {
    pub fn validate(&self) -> Result<(), FixtureError> {
        if self.name != "nitro" {
            return Err(FixtureError::UnsupportedOracle(self.name.clone()));
        }
        if !is_git_revision(&self.git_revision) {
            return Err(FixtureError::InvalidGitRevision(self.git_revision.clone()));
        }
        for digest in [&self.binary_sha256, &self.chain_config_sha256] {
            if !is_lower_hex_digest(digest) {
                return Err(FixtureError::InvalidObjectDigest(digest.clone()));
            }
        }
        Ok(())
    }
}

/// The prestate representation supplied to an offline replay runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FixturePrestate {
    ExecutionWitness { object: FixtureObject },
    Complete { object: FixtureObject },
}

impl FixturePrestate {
    fn object(&self) -> &FixtureObject {
        match self {
            Self::ExecutionWitness { object } | Self::Complete { object } => object,
        }
    }
}

/// The protocol boundary at which a fixture begins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FixtureInput {
    RawFeed { object: FixtureObject },
    DerivedTransactions { object: FixtureObject },
}

impl FixtureInput {
    fn object(&self) -> &FixtureObject {
        match self {
            Self::RawFeed { object } | Self::DerivedTransactions { object } => object,
        }
    }
}

/// The complete canonical observation produced by Nitro.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureResult {
    pub object: FixtureObject,
}

/// A single immutable input-to-output parity case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureCase {
    pub schema: String,
    pub schema_version: u32,
    pub case_revision: u32,
    pub id: String,
    pub labels: Vec<String>,
    pub oracle: FixtureProvenance,
    /// ArbOS version under which this input must execute. It is decoded from
    /// Nitro's canonical output header and cross-checked against the fixture's
    /// authenticated parent state before execution.
    pub effective_arbos_version: u64,
    pub prestate: FixturePrestate,
    pub input: FixtureInput,
    pub expected: FixtureResult,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_features: Vec<String>,
}

impl FixtureCase {
    pub fn new(
        id: String,
        labels: Vec<String>,
        oracle: FixtureProvenance,
        effective_arbos_version: u64,
        prestate: FixturePrestate,
        input: FixtureInput,
        expected: FixtureResult,
    ) -> Self {
        Self {
            schema: STF_FIXTURE_SCHEMA.to_owned(),
            schema_version: STF_FIXTURE_SCHEMA_VERSION,
            case_revision: 1,
            id,
            labels,
            oracle,
            effective_arbos_version,
            prestate,
            input,
            expected,
            required_features: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), FixtureError> {
        if self.schema != STF_FIXTURE_SCHEMA {
            return Err(FixtureError::UnsupportedSchema(self.schema.clone()));
        }
        if self.schema_version != STF_FIXTURE_SCHEMA_VERSION {
            return Err(FixtureError::UnsupportedSchemaVersion(self.schema_version));
        }
        if self.case_revision == 0 {
            return Err(FixtureError::InvalidCaseRevision);
        }
        if self.effective_arbos_version == 0 {
            return Err(FixtureError::InvalidArbosVersion);
        }
        validate_case_id(&self.id)?;
        validate_sorted_unique("labels", &self.labels)?;
        validate_sorted_unique("required_features", &self.required_features)?;
        self.oracle.validate()?;
        self.prestate.object().validate()?;
        self.input.object().validate()?;
        self.expected.object.validate()?;
        Ok(())
    }

    /// The immutable prestate object carried by this case.
    pub fn prestate_object(&self) -> &FixtureObject {
        self.prestate.object()
    }

    /// The immutable protocol-input object carried by this case.
    pub fn input_object(&self) -> &FixtureObject {
        self.input.object()
    }
}

/// Index of all committed cases in a fixture suite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureManifest {
    pub schema: String,
    pub schema_version: u32,
    pub cases: Vec<FixtureSuite>,
}

impl FixtureManifest {
    pub fn empty() -> Self {
        Self {
            schema: STF_FIXTURE_SCHEMA.to_owned(),
            schema_version: STF_FIXTURE_SCHEMA_VERSION,
            cases: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), FixtureError> {
        if self.schema != STF_FIXTURE_SCHEMA {
            return Err(FixtureError::UnsupportedSchema(self.schema.clone()));
        }
        if self.schema_version != STF_FIXTURE_SCHEMA_VERSION {
            return Err(FixtureError::UnsupportedSchemaVersion(self.schema_version));
        }
        let mut ids = BTreeSet::new();
        for case in &self.cases {
            case.validate()?;
            if !ids.insert(&case.id) {
                return Err(FixtureError::DuplicateCaseId(case.id.clone()));
            }
        }
        Ok(())
    }
}

/// A compact suite index entry. The actual case is stored at `path` relative to
/// the fixture-root directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureSuite {
    pub id: String,
    pub path: String,
    pub labels: Vec<String>,
    pub prestate_sha256: String,
    pub input_sha256: String,
    pub expected_sha256: String,
}

impl FixtureSuite {
    pub fn from_case(case: &FixtureCase) -> Self {
        let prestate_sha256 = case.prestate.object().sha256.clone();
        let input_sha256 = case.input.object().sha256.clone();
        let expected_sha256 = case.expected.object.sha256.clone();
        Self {
            id: case.id.clone(),
            path: format!("cases/{}/case.json", case.id),
            labels: case.labels.clone(),
            prestate_sha256,
            input_sha256,
            expected_sha256,
        }
    }

    fn validate(&self) -> Result<(), FixtureError> {
        validate_case_id(&self.id)?;
        validate_sorted_unique("labels", &self.labels)?;
        if self.path.is_empty() || Path::new(&self.path).is_absolute() || self.path.contains("..") {
            return Err(FixtureError::InvalidCasePath(self.path.clone()));
        }
        for digest in [
            &self.prestate_sha256,
            &self.input_sha256,
            &self.expected_sha256,
        ] {
            if !is_lower_hex_digest(digest) {
                return Err(FixtureError::InvalidObjectDigest(digest.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixtureError {
    UnsupportedSchema(String),
    UnsupportedSchemaVersion(u32),
    InvalidCaseRevision,
    InvalidArbosVersion,
    InvalidCaseId(String),
    InvalidCasePath(String),
    InvalidLabels {
        field: &'static str,
        values: Vec<String>,
    },
    UnsupportedOracle(String),
    InvalidGitRevision(String),
    InvalidObjectDigest(String),
    InvalidMediaType(String),
    DuplicateCaseId(String),
}

impl fmt::Display for FixtureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema(value) => write!(f, "unsupported fixture schema {value:?}"),
            Self::UnsupportedSchemaVersion(value) => {
                write!(f, "unsupported fixture schema version {value}")
            }
            Self::InvalidCaseRevision => f.write_str("case revision must be non-zero"),
            Self::InvalidArbosVersion => f.write_str("effective ArbOS version must be non-zero"),
            Self::InvalidCaseId(value) => write!(f, "invalid case id {value:?}"),
            Self::InvalidCasePath(value) => write!(f, "invalid fixture case path {value:?}"),
            Self::InvalidLabels { field, values } => write!(
                f,
                "{field} must be sorted, unique, and non-empty: {values:?}"
            ),
            Self::UnsupportedOracle(value) => write!(f, "unsupported behavioral oracle {value:?}"),
            Self::InvalidGitRevision(value) => write!(f, "invalid git revision {value:?}"),
            Self::InvalidObjectDigest(value) => write!(f, "invalid SHA-256 digest {value:?}"),
            Self::InvalidMediaType(value) => write!(f, "invalid media type {value:?}"),
            Self::DuplicateCaseId(value) => write!(f, "duplicate fixture case id {value:?}"),
        }
    }
}

impl std::error::Error for FixtureError {}

fn validate_case_id(value: &str) -> Result<(), FixtureError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains("//")
        || value.split('/').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
    {
        return Err(FixtureError::InvalidCaseId(value.to_owned()));
    }
    Ok(())
}

fn validate_sorted_unique(field: &'static str, values: &[String]) -> Result<(), FixtureError> {
    if values.iter().any(String::is_empty) || values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(FixtureError::InvalidLabels {
            field,
            values: values.to_vec(),
        });
    }
    Ok(())
}

fn is_lower_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_git_revision(value: &str) -> bool {
    (7..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn object() -> FixtureObject {
        FixtureObject {
            sha256: DIGEST.to_owned(),
            compression: ObjectCompression::Zstd,
            uncompressed_length: 0,
            media_type: "application/json".to_owned(),
        }
    }

    fn case() -> FixtureCase {
        FixtureCase::new(
            "opcodes/blobbasefee".to_owned(),
            vec!["arbos20".to_owned(), "opcode".to_owned()],
            FixtureProvenance {
                name: "nitro".to_owned(),
                git_revision: "a6181559".to_owned(),
                binary_sha256: DIGEST.to_owned(),
                chain_config_sha256: DIGEST.to_owned(),
            },
            20,
            FixturePrestate::ExecutionWitness { object: object() },
            FixtureInput::DerivedTransactions { object: object() },
            FixtureResult { object: object() },
        )
    }

    #[test]
    fn accepts_well_formed_case() {
        case().validate().unwrap();
    }

    #[test]
    fn rejects_non_nitro_oracle() {
        let mut fixture = case();
        fixture.oracle.name = "other".to_owned();
        assert!(matches!(
            fixture.validate(),
            Err(FixtureError::UnsupportedOracle(_))
        ));
    }

    #[test]
    fn rejects_unsorted_labels() {
        let mut fixture = case();
        fixture.labels.reverse();
        assert!(matches!(
            fixture.validate(),
            Err(FixtureError::InvalidLabels { .. })
        ));
    }

    #[test]
    fn rejects_missing_effective_arbos_version() {
        let mut fixture = case();
        fixture.effective_arbos_version = 0;
        assert!(matches!(
            fixture.validate(),
            Err(FixtureError::InvalidArbosVersion)
        ));
    }
}
