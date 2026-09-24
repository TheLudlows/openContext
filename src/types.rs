use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct AuthContext {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub workspace_id: Uuid,
    pub role: String,
}

impl AuthContext {
    pub fn require(&self, permission: &str) -> crate::error::Result<()> {
        let allowed = match permission {
            "read" => true,
            "write" => matches!(self.role.as_str(), "writer" | "reviewer" | "admin"),
            "review" | "publish" => matches!(self.role.as_str(), "reviewer" | "admin"),
            "delete" => self.role == "admin",
            _ => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(crate::error::AppError::Forbidden)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryInput {
    pub fact_key: String,
    pub content: String,
    #[serde(default)]
    pub publish_if_authorized: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureInput {
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeInput {
    pub title: String,
    pub content: Option<String>,
    pub file_id: Option<Uuid>,
    #[serde(default = "text_format")]
    pub format: String,
    pub asset_id: Option<Uuid>,
    pub expected_version: Option<i32>,
}
fn text_format() -> String {
    "text".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewInput {
    pub decision: String,
    pub expected_revision: i32,
    pub expected_version: Option<i32>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreInput {
    pub target_version: i32,
    pub expected_version: i32,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchInput {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default = "keyword_mode")]
    pub mode: String,
    #[serde(default)]
    pub allow_partial: bool,
}
fn default_limit() -> usize {
    10
}
fn keyword_mode() -> String {
    "keyword".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveInput {
    pub query: String,
    #[serde(default = "default_budget")]
    pub budget_tokens: usize,
    #[serde(default = "keyword_mode")]
    pub mode: String,
    #[serde(default)]
    pub allow_partial: bool,
}
fn default_budget() -> usize {
    2000
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SearchHit {
    pub asset_id: Uuid,
    pub version: i32,
    pub chunk_id: Uuid,
    pub kind: String,
    pub title: String,
    pub content: String,
    pub locator: Value,
    pub source_event_id: Uuid,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub tenant_id: Uuid,
    pub workspace_id: Uuid,
    pub job_id: Uuid,
    pub generation: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub content: String,
    pub locator: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entity {
    pub name: String,
    pub entity_type: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Relation {
    pub source: String,
    pub predicate: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GraphExtraction {
    pub entities: Vec<Entity>,
    pub relations: Vec<Relation>,
}
