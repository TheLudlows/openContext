//! Tenant/workspace scope and authorization types.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The `tenant_id` + `workspace_id` pair that scopes every stored object.
///
/// Every business operation carries this explicitly; it can never be omitted
/// or defaulted (A2.4). Object IDs, unique constraints, owners, caches and
/// blob keys are all prefixed with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Scope {
    pub tenant_id: Uuid,
    pub workspace_id: Uuid,
}

/// A principal that has authenticated against a [`Scope`].
///
/// Produced by the privileged `authenticate` path (token lookup), never by an
/// ordinary business query (A2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedScope {
    pub scope: Scope,
    pub principal_id: Uuid,
    pub role: String,
}

/// A permission a domain operation can require (A2.2 `check_current_permission`).
///
/// Roles are `reader | writer | reviewer | admin`; the mapping is fixed here so
/// the storage layer stays independent of the HTTP role matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    Read,
    Write,
    Review,
    Publish,
    Delete,
}

impl Permission {
    /// The permission name used by the role matrix and audit records.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Review => "review",
            Self::Publish => "publish",
            Self::Delete => "delete",
        }
    }

    /// Whether `role` grants `self`. Mirrors the existing role matrix and is
    /// shared by the in-memory contract double and future adapters.
    pub fn granted_by(self, role: &str) -> bool {
        match self {
            Self::Read => true,
            Self::Write => matches!(role, "writer" | "reviewer" | "admin"),
            Self::Review | Self::Publish => matches!(role, "reviewer" | "admin"),
            Self::Delete => role == "admin",
        }
    }
}

/// A specific version of a source (event/asset) that graph and vector records
/// originate from. Graph/vector writes must carry it so visibility can be
/// re-verified against the relational ledger (A2.2, A2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceVersion {
    pub source_id: Uuid,
    pub version: i32,
}
