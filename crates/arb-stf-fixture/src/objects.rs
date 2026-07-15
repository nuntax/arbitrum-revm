//! Content-addressed object storage for fixture sidecars.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::schema::{FixtureObject, ObjectCompression};

/// Filesystem-backed object storage rooted at a `testdata/stf/v2`-style directory.
#[derive(Debug, Clone)]
pub struct ObjectStore {
    root: PathBuf,
}

impl ObjectStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn put(
        &self,
        bytes: &[u8],
        media_type: impl Into<String>,
    ) -> Result<FixtureObject, ObjectStoreError> {
        let digest = digest(bytes);
        let object = FixtureObject {
            sha256: digest.clone(),
            compression: ObjectCompression::Zstd,
            uncompressed_length: bytes
                .len()
                .try_into()
                .map_err(|_| ObjectStoreError::ObjectTooLarge)?,
            media_type: media_type.into(),
        };
        object.validate().map_err(ObjectStoreError::InvalidObject)?;

        let path = self.object_path(&object);
        if path.exists() {
            self.verify(&object)?;
            return Ok(object);
        }
        let parent = path.parent().expect("object paths always have a parent");
        fs::create_dir_all(parent).map_err(|source| ObjectStoreError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let compressed = zstd::stream::encode_all(bytes, 19).map_err(ObjectStoreError::Compress)?;
        let temporary = path.with_extension("zst.tmp");
        fs::write(&temporary, compressed).map_err(|source| ObjectStoreError::Io {
            path: temporary.clone(),
            source,
        })?;
        match fs::rename(&temporary, &path) {
            Ok(()) => Ok(object),
            Err(_source) if path.exists() => {
                let _ = fs::remove_file(&temporary);
                self.verify(&object)?;
                Ok(object)
            }
            Err(source) => {
                let _ = fs::remove_file(&temporary);
                Err(ObjectStoreError::Io { path, source })
            }
        }
    }

    pub fn get(&self, object: &FixtureObject) -> Result<Vec<u8>, ObjectStoreError> {
        object.validate().map_err(ObjectStoreError::InvalidObject)?;
        let path = self.object_path(object);
        let encoded = fs::read(&path).map_err(|source| ObjectStoreError::Io {
            path: path.clone(),
            source,
        })?;
        let bytes = match object.compression {
            ObjectCompression::Zstd => zstd::stream::decode_all(encoded.as_slice())
                .map_err(ObjectStoreError::Decompress)?,
        };
        if bytes.len() as u64 != object.uncompressed_length {
            return Err(ObjectStoreError::LengthMismatch {
                expected: object.uncompressed_length,
                actual: bytes.len() as u64,
            });
        }
        let actual = digest(&bytes);
        if actual != object.sha256 {
            return Err(ObjectStoreError::DigestMismatch {
                expected: object.sha256.clone(),
                actual,
            });
        }
        Ok(bytes)
    }

    pub fn verify(&self, object: &FixtureObject) -> Result<(), ObjectStoreError> {
        self.get(object).map(|_| ())
    }

    pub fn object_path(&self, object: &FixtureObject) -> PathBuf {
        self.root
            .join("objects")
            .join("sha256")
            .join(format!("{}.zst", object.sha256))
    }
}

#[derive(Debug)]
pub enum ObjectStoreError {
    InvalidObject(crate::schema::FixtureError),
    ObjectTooLarge,
    Io { path: PathBuf, source: io::Error },
    Compress(io::Error),
    Decompress(io::Error),
    LengthMismatch { expected: u64, actual: u64 },
    DigestMismatch { expected: String, actual: String },
}

impl fmt::Display for ObjectStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidObject(error) => write!(f, "invalid fixture object: {error}"),
            Self::ObjectTooLarge => f.write_str("fixture object is too large to address"),
            Self::Io { path, source } => write!(f, "I/O error at {}: {source}", path.display()),
            Self::Compress(error) => write!(f, "failed to compress fixture object: {error}"),
            Self::Decompress(error) => write!(f, "failed to decompress fixture object: {error}"),
            Self::LengthMismatch { expected, actual } => write!(
                f,
                "fixture object length mismatch: expected {expected}, got {actual}"
            ),
            Self::DigestMismatch { expected, actual } => write!(
                f,
                "fixture object digest mismatch: expected {expected}, got {actual}"
            ),
        }
    }
}

impl std::error::Error for ObjectStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidObject(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::Compress(error) | Self::Decompress(error) => Some(error),
            _ => None,
        }
    }
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_content_addressing() {
        let temporary = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(temporary.path());
        let object = store
            .put(b"fixture bytes", "application/octet-stream")
            .unwrap();
        assert_eq!(store.get(&object).unwrap(), b"fixture bytes");
        assert_eq!(
            store
                .put(b"fixture bytes", "application/octet-stream")
                .unwrap(),
            object
        );
    }

    #[test]
    fn rejects_tampered_object() {
        let temporary = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(temporary.path());
        let object = store
            .put(b"fixture bytes", "application/octet-stream")
            .unwrap();
        fs::write(store.object_path(&object), b"not zstd").unwrap();
        assert!(matches!(
            store.get(&object),
            Err(ObjectStoreError::Decompress(_))
        ));
    }
}
