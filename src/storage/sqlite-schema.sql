-- Complete schema for a NEW SQLite database. No version history or upgrades (A2.3).
--
-- Encodings are fixed here (A5):
--   UUID        -> BLOB (16 bytes, sqlx `uuid` mapping)
--   JSON        -> TEXT (serialised JSON)
--   boolean     -> INTEGER (0/1, sqlx `bool` mapping)
--   timestamp   -> INTEGER (unix epoch milliseconds, bound from Rust)
--   text        -> TEXT
--
-- Scope isolation is enforced by the adapter binding tenant_id/workspace_id on
-- every statement (A2.4): SQLite has no row-level security. `foreign_keys`,
-- `busy_timeout` and WAL are set per-connection by the adapter, never here.

CREATE TABLE oc_workspaces (
    tenant_id BLOB NOT NULL,
    id BLOB PRIMARY KEY,
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (tenant_id, id)
);

CREATE TABLE oc_api_keys (
    id BLOB PRIMARY KEY,
    tenant_id BLOB NOT NULL,
    workspace_id BLOB NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    role TEXT NOT NULL CHECK (role IN ('reader','writer','reviewer','admin')),
    revoked INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (tenant_id, workspace_id) REFERENCES oc_workspaces(tenant_id, id)
);

CREATE TABLE oc_files (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL, id BLOB NOT NULL,
    name TEXT NOT NULL, media_type TEXT NOT NULL, hash TEXT NOT NULL, size INTEGER NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0,
    created_by BLOB NOT NULL, created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id) REFERENCES oc_workspaces(tenant_id, id)
);

CREATE TABLE oc_events (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL, id BLOB NOT NULL,
    content TEXT NOT NULL, kind TEXT NOT NULL, file_id BLOB,
    state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active','retracted')),
    created_by BLOB NOT NULL, created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id) REFERENCES oc_workspaces(tenant_id, id),
    FOREIGN KEY (tenant_id, workspace_id, file_id) REFERENCES oc_files(tenant_id, workspace_id, id)
);

CREATE TABLE oc_assets (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL, id BLOB NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('memory','knowledge')),
    title TEXT NOT NULL, fact_key TEXT, current_version INTEGER,
    deleted INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id) REFERENCES oc_workspaces(tenant_id, id)
);
CREATE UNIQUE INDEX oc_memory_fact_slot ON oc_assets(tenant_id, workspace_id, fact_key)
WHERE kind = 'memory';

CREATE TABLE oc_versions (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL, asset_id BLOB NOT NULL,
    version INTEGER NOT NULL CHECK (version > 0), content TEXT NOT NULL, content_hash TEXT NOT NULL,
    source_event_id BLOB NOT NULL, restored_from INTEGER, review_id BLOB, title TEXT NOT NULL,
    created_by BLOB NOT NULL, created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, asset_id, version),
    FOREIGN KEY (tenant_id, workspace_id, asset_id) REFERENCES oc_assets(tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id, source_event_id) REFERENCES oc_events(tenant_id, workspace_id, id)
);

CREATE TABLE oc_candidates (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL, id BLOB NOT NULL,
    asset_id BLOB, source_event_id BLOB NOT NULL, fact_key TEXT NOT NULL,
    content TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 1,
    expected_version INTEGER, state TEXT NOT NULL DEFAULT 'candidate'
      CHECK (state IN ('candidate','approved','rejected','withdrawn','published')),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id, asset_id) REFERENCES oc_assets(tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id, source_event_id) REFERENCES oc_events(tenant_id, workspace_id, id)
);

CREATE TABLE oc_reviews (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL, id BLOB NOT NULL,
    candidate_id BLOB NOT NULL, revision INTEGER NOT NULL, decision TEXT NOT NULL,
    expected_version INTEGER, reviewer BLOB NOT NULL, reason TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id, candidate_id) REFERENCES oc_candidates(tenant_id, workspace_id, id)
);

CREATE TABLE oc_jobs (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL, id BLOB NOT NULL,
    created_by BLOB NOT NULL, operation TEXT NOT NULL, payload TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending'
      CHECK (state IN ('pending','processing','retry_wait','completed','failed','cancelled','superseded')),
    run_token INTEGER NOT NULL DEFAULT 0, generation INTEGER NOT NULL DEFAULT 1,
    attempt INTEGER NOT NULL DEFAULT 0, next_retry_at INTEGER,
    asset_id BLOB, source_event_id BLOB,
    cancel_requested INTEGER NOT NULL DEFAULT 0,
    outcome TEXT, result TEXT, error_code TEXT,
    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id, asset_id) REFERENCES oc_assets(tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id, source_event_id) REFERENCES oc_events(tenant_id, workspace_id, id)
);

CREATE TABLE oc_chunks (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL, id BLOB NOT NULL,
    asset_id BLOB NOT NULL, version INTEGER NOT NULL, ordinal INTEGER NOT NULL,
    content TEXT NOT NULL, locator TEXT NOT NULL, search_terms TEXT NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, id),
    UNIQUE (tenant_id, workspace_id, asset_id, version, ordinal),
    FOREIGN KEY (tenant_id, workspace_id, asset_id, version) REFERENCES oc_versions(tenant_id, workspace_id, asset_id, version)
);
CREATE INDEX oc_chunks_scope ON oc_chunks(tenant_id, workspace_id, asset_id, version);
CREATE INDEX oc_versions_source ON oc_versions(tenant_id, workspace_id, source_event_id);

CREATE TABLE oc_summaries (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL, id BLOB NOT NULL,
    chunk_id BLOB NOT NULL, text TEXT NOT NULL, model_revision TEXT NOT NULL DEFAULT '',
    search_terms TEXT NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id, chunk_id) REFERENCES oc_chunks(tenant_id, workspace_id, id)
);

CREATE TABLE oc_commands (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL, principal_id BLOB NOT NULL,
    operation TEXT NOT NULL, key TEXT NOT NULL, request_hash TEXT NOT NULL, response TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, principal_id, operation, key)
);

CREATE TABLE oc_audit (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL, id BLOB NOT NULL,
    actor BLOB NOT NULL, action TEXT NOT NULL, target BLOB NOT NULL, details TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, id)
);

CREATE INDEX oc_jobs_asset ON oc_jobs(tenant_id, workspace_id, asset_id);
CREATE INDEX oc_jobs_source ON oc_jobs(tenant_id, workspace_id, source_event_id);
CREATE INDEX oc_jobs_claim ON oc_jobs(state, next_retry_at, generation);
CREATE INDEX oc_events_file ON oc_events(tenant_id, workspace_id, file_id);

CREATE TABLE oc_artifact_owners (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL,
    source_id BLOB NOT NULL, version INTEGER NOT NULL,
    artifact_type TEXT NOT NULL CHECK (artifact_type IN ('chunk','summary','entity','relation')),
    artifact_id BLOB NOT NULL,
    chunk_id BLOB,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, source_id, version, artifact_type, artifact_id)
);
CREATE INDEX oc_owners_artifact ON oc_artifact_owners(tenant_id, workspace_id, artifact_type, artifact_id);

CREATE TABLE oc_index_entries (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL,
    artifact_id BLOB NOT NULL,
    field TEXT NOT NULL,
    model_id TEXT NOT NULL,
    dimension INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','ready','removed')),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, artifact_id, field, model_id, generation)
);

CREATE TABLE oc_artifact_ledger (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL,
    source_id BLOB NOT NULL, version INTEGER NOT NULL,
    artifact_type TEXT NOT NULL CHECK (artifact_type IN ('chunk','summary','entity','relation')),
    artifact_id BLOB NOT NULL,
    surface TEXT NOT NULL CHECK (surface IN ('vector','graph')),
    generation INTEGER NOT NULL DEFAULT 1,
    idempotency_key TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','committed','retry_wait','orphan')),
    attempt INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    next_retry_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, source_id, version, artifact_id, surface, generation)
);
CREATE INDEX oc_ledger_state ON oc_artifact_ledger(state, next_retry_at);
