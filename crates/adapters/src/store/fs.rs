//! Directory blob store: `<root>/<ab>/<full sha256>`, written atomically.

use super::{blob_ref, valid_hash};
use agent_proto::BlobRef;
use agent_runtime::{BlobStore, StoreError};
use async_trait::async_trait;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub struct FsBlobStore {
    root: PathBuf,
}

fn io<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Io(e.to_string())
}

impl FsBlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(io)?;
        Ok(FsBlobStore { root })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    fn path(&self, sha: &str) -> Result<PathBuf, StoreError> {
        if !valid_hash(sha) {
            return Err(StoreError::NotFound(format!("invalid blob hash {sha}")));
        }
        Ok(self.root.join(&sha[..2]).join(sha))
    }
}

#[async_trait]
impl BlobStore for FsBlobStore {
    async fn put(&self, bytes: &[u8], media_type: Option<&str>) -> Result<BlobRef, StoreError> {
        let r = blob_ref(bytes, media_type);
        let path = self.path(&r.sha256)?;
        if tokio::fs::try_exists(&path).await.map_err(io)? {
            return Ok(r);
        }
        let dir = path.parent().expect("shard").to_path_buf();
        tokio::fs::create_dir_all(&dir).await.map_err(io)?;
        let tmp = dir.join(format!(".{}.{}.tmp", r.sha256, std::process::id()));
        let data = bytes.to_vec();
        let (tmp2, path2) = (tmp.clone(), path.clone());
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp2)?;
            f.write_all(&data)?;
            f.sync_all()?;
            std::fs::rename(&tmp2, &path2)
        })
        .await
        .map_err(io)?
        .map_err(io)?;
        Ok(r)
    }

    async fn get(&self, blob: &BlobRef) -> Result<Vec<u8>, StoreError> {
        let path = self.path(&blob.sha256)?;
        match tokio::fs::read(&path).await {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StoreError::NotFound(format!("blob {}", blob.sha256))),
            Err(e) => Err(io(e)),
        }
    }

    async fn gc(&self, reachable: &[BlobRef]) -> Result<usize, StoreError> {
        let keep: BTreeSet<String> = reachable.iter().map(|b| b.sha256.clone()).collect();
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || -> Result<usize, StoreError> {
            let mut n = 0;
            for shard in std::fs::read_dir(&root).map_err(io)? {
                let shard = shard.map_err(io)?.path();
                if !shard.is_dir() {
                    continue;
                }
                for f in std::fs::read_dir(&shard).map_err(io)? {
                    let f = f.map_err(io)?.path();
                    let name = f.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
                    if valid_hash(&name) && !keep.contains(&name) {
                        std::fs::remove_file(&f).map_err(io)?;
                        n += 1;
                    }
                }
            }
            Ok(n)
        })
        .await
        .map_err(io)?
    }
}
