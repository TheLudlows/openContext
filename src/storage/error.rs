//! Vendor-neutral error type for the storage layer.

use std::io;

/// Errors the storage layer returns to the domain. Backends map their own
/// error types (SQLx, LanceDB, Kuzu, `std::io`) into this so the domain never
/// depends on a vendor error or a database pool type (A2.2).
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("scope is required")]
    ScopeRequired,
    #[error("resource not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("permission denied")]
    Forbidden,
    #[error("{0}")]
    Unavailable(String),
    #[error("storage engine is not initialized")]
    NotInitialized,
    #[error("backend error: {0}")]
    Backend(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Result alias for the storage layer.
pub type StorageResult<T> = Result<T, StorageError>;
