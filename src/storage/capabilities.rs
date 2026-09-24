//! Vendor-neutral value types and capability declarations.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::storage::scope::{Scope, SourceVersion};

/// A single dense embedding. `Vec<f32>` keeps the storage layer independent of
/// `pgvector::Vector`; adapters convert at the boundary.
pub type Embedding = Vec<f32>;

/// Declared capabilities of a backend (A2.7). Defaults are the conservative
/// "not supported" state; an adapter overrides only what it actually
/// implements. Capability loss must never weaken tenant/workspace or
/// source-validity isolation — those cannot be degraded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Filtered approximate-nearest-neighbour search.
    pub filtered_ann: bool,
    /// Exact (brute-force) scoped search.
    pub exact_search: bool,
    /// Maximum graph traversal hops (0 = no traversal).
    pub max_hops: usize,
    /// Native shared-source delete: removing one owner does not delete a
    /// shared object. When false, semantics come from the relational ledger
    /// instead (A2.6/A2.7).
    pub shared_source_delete: bool,
}

/// A vector query. Carries scope plus the exact profile/dimension/generation
/// that produced the vectors — never a bare vector (A2.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorQuery {
    pub scope: Scope,
    pub profile: String,
    pub dimension: usize,
    pub generation: i64,
    pub embedding: Embedding,
    pub limit: usize,
}

/// A vector to write, tagged with the profile/generation it belongs to and
/// the source version it originates from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorEntry {
    pub id: Uuid,
    pub embedding: Embedding,
    pub profile: String,
    pub dimension: usize,
    pub generation: i64,
    pub source: SourceVersion,
}

/// A scored vector hit. A hit is *not yet visible*: the caller must re-verify
/// the owning source/version against the relational ledger before returning it
/// (A2.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorHit {
    pub id: Uuid,
    pub score: f32,
    pub source: SourceVersion,
}

/// A blob key constrained by scope and a logical root; the adapter derives the
/// real path and never accepts a free-form system path (A2.5).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlobKey {
    pub scope: Scope,
    pub root: String,
    pub path: String,
}
