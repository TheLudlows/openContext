//! Pluggable storage interfaces (M1: interfaces & transaction contracts).
//!
//! # Ownership and blocking-call boundary (A2.4)
//! The local API and Worker share one in-process [`StorageEngine`]; the engine
//! (and the Kuzu database file it owns) has a single host process. Business
//! modules hold a shared adapter reference and never open their own Kuzu
//! database file — a second process that opens the same Kuzu file is rejected
//! by its file lock. Synchronous Kuzu calls are confined to the adapter's
//! bounded blocking executor so they cannot starve the async API/Worker
//! scheduler.
//!
//! # Shutdown order
//! [`StorageEngine::shutdown`] releases backends in dependency order: the queue
//! stops claiming before vector/graph are closed, and the relational store
//! (authority for visibility) is released last.
//!
//! # Transaction boundary
//! [`RelationalStore::begin`] returns a [`DomainTx`] that carries its scope and
//! exposes only domain operations — no `PgPool`/`SqlitePool`, no raw SQL, no
//! vendor types. `enqueue` is a domain operation on that same transaction;
//! [`JobQueue`] is the consumer side (claim/ack/retry) and never enqueues on a
//! second connection.

pub mod capabilities;
pub mod error;
pub mod ledger;
pub mod local_blob;
pub mod scope;
pub mod sqlite;
pub mod traits;

pub use capabilities::{BlobKey, Capabilities, Embedding, VectorEntry, VectorHit, VectorQuery};
pub use error::{StorageError, StorageResult};
pub use ledger::{LedgerEntry, LedgerKey, LedgerState, Surface, ledger_idempotency_key};
pub use scope::{AuthorizedScope, Permission, Scope, SourceVersion};
pub use traits::{
    BlobStore, ClaimedJob, DomainTx, GraphStore, IssuedKey, JobFinish, JobQueue, Lifecycle,
    RelationalStore, VectorStore, WorkItem,
};

/// Assembles the four stores behind one interface. Owned by the local host;
/// API and Worker share it in-process (A2.4).
pub struct StorageEngine<R, Q, V, G, B> {
    relational: R,
    queue: Q,
    vector: V,
    graph: G,
    blobs: B,
}

impl<R, Q, V, G, B> StorageEngine<R, Q, V, G, B> {
    pub fn new(relational: R, queue: Q, vector: V, graph: G, blobs: B) -> Self {
        Self {
            relational,
            queue,
            vector,
            graph,
            blobs,
        }
    }

    pub fn relational(&self) -> &R {
        &self.relational
    }
    pub fn queue(&self) -> &Q {
        &self.queue
    }
    pub fn vector(&self) -> &V {
        &self.vector
    }
    pub fn graph(&self) -> &G {
        &self.graph
    }
    pub fn blobs(&self) -> &B {
        &self.blobs
    }
}

impl<R, Q, V, G, B> StorageEngine<R, Q, V, G, B>
where
    R: Lifecycle,
    Q: Lifecycle,
    V: Lifecycle,
    G: Lifecycle,
    B: Lifecycle,
{
    /// Initialize all backends; a failure part-way leaves already-initialized
    /// backends reusable on the next attempt (A2.3).
    pub async fn initialize(&self) -> StorageResult<()> {
        self.relational.initialize().await?;
        self.queue.initialize().await?;
        self.vector.initialize().await?;
        self.graph.initialize().await?;
        self.blobs.initialize().await?;
        Ok(())
    }

    /// Fail fast unless every backend reports a compatible structure.
    pub async fn check(&self) -> StorageResult<()> {
        self.relational.check().await?;
        self.queue.check().await?;
        self.vector.check().await?;
        self.graph.check().await?;
        self.blobs.check().await?;
        Ok(())
    }

    /// Release backends in reverse dependency order: stop claiming, close
    /// vector/graph, release the relational authority last.
    pub async fn shutdown(&self) -> StorageResult<()> {
        self.queue.shutdown().await?;
        self.vector.shutdown().await?;
        self.graph.shutdown().await?;
        self.blobs.shutdown().await?;
        self.relational.shutdown().await?;
        Ok(())
    }
}
