//! User-private, content-addressed artifact storage with quotas.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("artifact root is unavailable: {0}")]
    Root(#[from] std::io::Error),
    #[error("artifact is not found: {0}")]
    NotFound(String),
    #[error("artifact quota exceeded")]
    QuotaExceeded,
    #[error("artifact metadata is invalid: {0}")]
    InvalidMetadata(String),
    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtifactManifest {
    pub uri: String,
    pub media_type: String,
    pub bytes: u64,
    pub source_revision: String,
    pub source_entities: Vec<String>,
    pub generator: String,
    pub parameters: Value,
    pub arrays: BTreeMap<String, Value>,
    pub created_at_ms: u128,
    pub expires_at_ms: Option<u128>,
}

pub struct ArtifactInput<'a> {
    pub bytes: &'a [u8],
    pub media_type: String,
    pub source_revision: String,
    pub source_entities: Vec<String>,
    pub generator: String,
    pub parameters: Value,
    pub expires_at_ms: Option<u128>,
}

#[derive(Clone, Debug)]
pub struct ArtifactStore {
    root: PathBuf,
    max_bytes: u64,
}

impl ArtifactStore {
    pub fn new(root: impl AsRef<Path>, max_bytes: u64) -> Result<Self, ArtifactError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { root, max_bytes })
    }

    pub fn put(&self, input: ArtifactInput<'_>) -> Result<ArtifactManifest, ArtifactError> {
        let digest = digest(input.bytes);
        let data_path = self.root.join(&digest);
        let metadata_path = self.root.join(format!("{digest}.json"));
        if !data_path.exists() {
            if input.bytes.len() as u64 > self.max_bytes
                || self.current_bytes()? + input.bytes.len() as u64 > self.max_bytes
            {
                return Err(ArtifactError::QuotaExceeded);
            }
            let temporary = self
                .root
                .join(format!(".{digest}.tmp-{}", std::process::id()));
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
            }
            file.write_all(input.bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &data_path)?;
        }
        let manifest = ArtifactManifest {
            uri: format!("freecad://artifact/{digest}"),
            media_type: input.media_type,
            bytes: input.bytes.len() as u64,
            source_revision: input.source_revision,
            source_entities: input.source_entities,
            generator: input.generator,
            parameters: input.parameters,
            arrays: BTreeMap::new(),
            created_at_ms: now_ms(),
            expires_at_ms: input.expires_at_ms,
        };
        let encoded = serde_json::to_vec_pretty(&manifest)?;
        let temporary = self
            .root
            .join(format!(".{digest}.json.tmp-{}", std::process::id()));
        let mut metadata = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            metadata.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        metadata.write_all(&encoded)?;
        metadata.sync_all()?;
        fs::rename(temporary, metadata_path)?;
        Ok(manifest)
    }

    pub fn manifest(&self, uri_or_hash: &str) -> Result<ArtifactManifest, ArtifactError> {
        let hash = normalize_hash(uri_or_hash)?;
        let path = self.root.join(format!("{hash}.json"));
        let contents =
            fs::read_to_string(path).map_err(|_| ArtifactError::NotFound(uri_or_hash.into()))?;
        Ok(serde_json::from_str(&contents)?)
    }

    pub fn read(&self, uri_or_hash: &str) -> Result<Vec<u8>, ArtifactError> {
        let hash = normalize_hash(uri_or_hash)?;
        let mut file = File::open(self.root.join(hash))
            .map_err(|_| ArtifactError::NotFound(uri_or_hash.into()))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    pub fn current_bytes(&self) -> Result<u64, ArtifactError> {
        let mut total = 0;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_file() && entry.path().extension().is_none() {
                total += entry.metadata()?.len();
            }
        }
        Ok(total)
    }

    pub fn remove_expired(&self, now: u128) -> Result<u64, ArtifactError> {
        let mut removed = 0;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let contents = fs::read_to_string(entry.path())?;
                let manifest: ArtifactManifest = serde_json::from_str(&contents)?;
                if manifest.expires_at_ms.is_some_and(|expires| expires <= now) {
                    let hash = normalize_hash(&manifest.uri)?;
                    let _ = fs::remove_file(self.root.join(hash));
                    fs::remove_file(entry.path())?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

fn digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256-{}", hex::encode(hasher.finalize()))
}

fn normalize_hash(value: &str) -> Result<String, ArtifactError> {
    let hash = value.rsplit('/').next().unwrap_or(value);
    if !hash.starts_with("sha256-")
        || hash.len() != 71
        || !hash[7..]
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(ArtifactError::InvalidMetadata(value.into()));
    }
    Ok(hash.into())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifacts_are_content_addressed_and_private() {
        let root = std::env::temp_dir().join(format!("topomind-artifacts-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = ArtifactStore::new(&root, 1024).unwrap();
        let manifest = store
            .put(ArtifactInput {
                bytes: b"hello",
                media_type: "text/plain".into(),
                source_revision: "g1".into(),
                source_entities: vec![],
                generator: "test/1".into(),
                parameters: Value::Null,
                expires_at_ms: None,
            })
            .unwrap();
        assert_eq!(store.read(&manifest.uri).unwrap(), b"hello");
        assert_eq!(store.manifest(&manifest.uri).unwrap().bytes, 5);
        let _ = fs::remove_dir_all(root);
    }
}
