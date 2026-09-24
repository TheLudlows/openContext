//! Local filesystem blob store (M2).
//!
//! Raw file bytes live outside the relational database. The real on-disk path
//! is derived only from the [`BlobKey`] (scope + root + path); a key can never
//! address a system path outside the base directory (A2.5).

use std::path::{Component, Path, PathBuf};

use crate::storage::capabilities::BlobKey;
use crate::storage::error::{StorageError, StorageResult};
use crate::storage::traits::{BlobStore, Lifecycle};

/// A blob store rooted at a base directory. Paths are partitioned by scope
/// (`tenant/workspace`) then `root` then `path`.
pub struct LocalBlobStore {
    base: PathBuf,
}

impl LocalBlobStore {
    /// Open the store rooted at `base`, creating the directory if absent.
    pub async fn open(base: impl AsRef<Path>) -> StorageResult<Self> {
        let store = Self {
            base: base.as_ref().to_path_buf(),
        };
        store.initialize().await?;
        Ok(store)
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Derive the on-disk path for a key, rejecting any key that would escape
    /// the base directory (absolute paths, `..`, prefixes).
    fn resolve(&self, key: &BlobKey) -> StorageResult<PathBuf> {
        let scope_dir =
            PathBuf::from(key.scope.tenant_id.to_string()).join(key.scope.workspace_id.to_string());
        let rel = Path::new(&key.root).join(&key.path);
        if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            return Err(StorageError::Conflict(
                "blob key escapes the base directory".to_string(),
            ));
        }
        Ok(self.base.join(scope_dir).join(rel))
    }
}

impl BlobStore for LocalBlobStore {
    async fn put(&self, key: &BlobKey, data: Vec<u8>) -> StorageResult<()> {
        let path = self.resolve(key)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, data).await?;
        Ok(())
    }

    async fn get(&self, key: &BlobKey) -> StorageResult<Vec<u8>> {
        let path = self.resolve(key)?;
        tokio::fs::read(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound
            } else {
                StorageError::Io(e)
            }
        })
    }

    async fn delete(&self, key: &BlobKey) -> StorageResult<()> {
        let path = self.resolve(key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StorageError::Io(e)),
        }
    }
}

impl Lifecycle for LocalBlobStore {
    async fn initialize(&self) -> StorageResult<()> {
        tokio::fs::create_dir_all(&self.base).await?;
        Ok(())
    }

    async fn check(&self) -> StorageResult<()> {
        let meta = tokio::fs::metadata(&self.base).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StorageError::Unavailable("blob base directory missing".to_string())
            } else {
                StorageError::Io(e)
            }
        })?;
        if !meta.is_dir() {
            return Err(StorageError::Unavailable(
                "blob base is not a directory".to_string(),
            ));
        }
        Ok(())
    }

    async fn shutdown(&self) -> StorageResult<()> {
        Ok(())
    }
}
