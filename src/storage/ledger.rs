//! Cross-store ledger types (M3): the relational authority for vector/graph
//! write progress (A2.6). Domain code sees only these types, never SQL.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::storage::scope::{Scope, SourceVersion};

/// Which external surface a ledger entry tracks (A2.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Surface {
    Vector,
    Graph,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vector => "vector",
            Self::Graph => "graph",
        }
    }
}

/// Lifecycle state of a cross-store write (A2.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LedgerState {
    Pending,
    Committed,
    RetryWait,
    Orphan,
}

impl LedgerState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Committed => "committed",
            Self::RetryWait => "retry_wait",
            Self::Orphan => "orphan",
        }
    }
}

/// Retry budget before a write is abandoned as an orphan (A2.6).
pub const MAX_LEDGER_ATTEMPTS: i32 = 5;

/// A pending write left this long without confirm is abandoned by reconcile.
pub const LEDGER_PENDING_TIMEOUT_MS: i64 = 5 * 60 * 1000;

/// The identity of a cross-store write: everything the external store needs to
/// idempotently place one artifact (A2.6). Matches the ledger primary key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LedgerKey {
    pub scope: Scope,
    pub source: SourceVersion,
    pub artifact_id: Uuid,
    pub surface: Surface,
    pub generation: i64,
}

/// The content recorded for a pending write (A2.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub key: LedgerKey,
    pub artifact_type: String,
    pub idempotency_key: String,
}

/// Deterministic idempotency key for a ledger write: the worker derives its
/// external-store key from this so a replay targets the same object (A2.6).
pub fn ledger_idempotency_key(key: &LedgerKey) -> String {
    format!(
        "{}-{}-{}-{}-{}-{}-{}",
        key.scope.tenant_id,
        key.scope.workspace_id,
        key.source.source_id,
        key.source.version,
        key.artifact_id,
        key.surface.as_str(),
        key.generation
    )
}
