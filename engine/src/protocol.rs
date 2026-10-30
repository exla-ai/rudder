//! Stable identities shared by the journal, content store and adapters.

use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    Blob,
    Directory,
    SourceRoot,
    EnvironmentRoot,
    Checkpoint,
    ActionKey,
    ActionResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HashAlgorithm {
    Sha256,
}

/// The digest is of the raw stored bytes, including for metadata. A blob's
/// hash is therefore compatible with REAPI and ordinary sha256sum.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectRef {
    pub kind: ObjectKind,
    pub algorithm: HashAlgorithm,
    pub digest: String,
    pub size_bytes: u64,
}

impl ObjectRef {
    pub fn from_bytes(kind: ObjectKind, bytes: &[u8]) -> Self {
        Self {
            kind,
            algorithm: HashAlgorithm::Sha256,
            digest: hex::encode(Sha256::digest(bytes)),
            size_bytes: bytes.len() as u64,
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.digest.len() == 64, "invalid SHA-256 digest length");
        ensure!(
            self.digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "digest must be lowercase hexadecimal"
        );
        Ok(())
    }

    pub fn verify(&self, bytes: &[u8]) -> Result<()> {
        self.validate()?;
        ensure!(self.size_bytes == bytes.len() as u64, "object size mismatch");
        ensure!(
            Self::from_bytes(self.kind, bytes).digest == self.digest,
            "object digest mismatch"
        );
        Ok(())
    }
}

/// Metadata is compact UTF-8 JSON with lexicographically ordered map keys.
/// Numbers are integers; floating point encodings are excluded from identities.
/// Directory entry ordering and duplicate-name validation belong to its schema.
pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    fn normalize(value: &mut serde_json::Value) -> Result<()> {
        match value {
            serde_json::Value::Number(n) if n.is_f64() => bail!("floating point metadata is unsupported"),
            serde_json::Value::Array(items) => {
                for item in items { normalize(item)?; }
            }
            serde_json::Value::Object(map) => {
                for item in map.values_mut() { normalize(item)?; }
                // serde_json's default Map is a BTreeMap. Reinsert in order so
                // this remains deterministic if a downstream enables preserve_order.
                let sorted: std::collections::BTreeMap<_, _> = std::mem::take(map).into_iter().collect();
                map.extend(sorted);
            }
            _ => {}
        }
        Ok(())
    }
    let mut value = serde_json::to_value(value)?;
    normalize(&mut value)?;
    Ok(serde_json::to_vec(&value)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// All objects and the local retention receipt were synced on this volume.
    Local,
    /// A remote durable store committed the complete closure and root retention.
    Replicated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointConsistency {
    /// The engine has stopped writers, including child processes, at a barrier.
    Quiesced,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerEpoch {
    pub owner: String,
    pub epoch: u64,
}

impl OwnerEpoch {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.owner.trim().is_empty(), "owner is required");
        ensure!(self.epoch > 0, "owner epoch must be positive");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_blob_hashes_remain_standard() {
        let reference = ObjectRef::from_bytes(ObjectKind::Blob, b"abc");
        assert_eq!(reference.digest, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(reference.size_bytes, 3);
        reference.verify(b"abc").unwrap();
        assert!(reference.verify(b"abd").is_err());
        assert!(reference.verify(b"abcd").is_err());
    }

    #[test]
    fn metadata_order_is_canonical_and_floats_are_rejected() {
        let first = serde_json::json!({"z": [{"c": 2, "a": 1}], "a": true});
        let second: serde_json::Value = serde_json::from_str(r#"{"a":true,"z":[{"a":1,"c":2}]}"#).unwrap();
        assert_eq!(canonical_json(&first).unwrap(), canonical_json(&second).unwrap());
        assert!(canonical_json(&serde_json::json!({"x": 1.5})).is_err());
    }

    #[test]
    fn object_paths_cannot_escape_the_content_store() {
        let mut reference = ObjectRef::from_bytes(ObjectKind::Blob, b"");
        reference.digest = "../bad".into();
        assert!(reference.validate().is_err());
        reference.digest = "A".repeat(64);
        assert!(reference.validate().is_err());
    }
}
