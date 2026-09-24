-- Complete schema for a NEW PostgreSQL database. No version history or upgrades.
CREATE EXTENSION IF NOT EXISTS vector;
CREATE SCHEMA IF NOT EXISTS oc;

CREATE TABLE oc.workspaces (
    tenant_id uuid NOT NULL,
    id uuid PRIMARY KEY,
    name text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, id)
);

CREATE TABLE oc.api_keys (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL,
    workspace_id uuid NOT NULL,
    token_hash text NOT NULL UNIQUE,
    role text NOT NULL CHECK (role IN ('reader','writer','reviewer','admin')),
    revoked boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (tenant_id, workspace_id) REFERENCES oc.workspaces(tenant_id, id)
);

CREATE TABLE oc.files (
    tenant_id uuid NOT NULL, workspace_id uuid NOT NULL, id uuid NOT NULL,
    name text NOT NULL, media_type text NOT NULL, hash text NOT NULL, size bigint NOT NULL,
    deleted boolean NOT NULL DEFAULT false,
    created_by uuid NOT NULL, created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id) REFERENCES oc.workspaces(tenant_id, id)
);

CREATE TABLE oc.events (
    tenant_id uuid NOT NULL, workspace_id uuid NOT NULL, id uuid NOT NULL,
    content text NOT NULL, kind text NOT NULL, file_id uuid,
    state text NOT NULL DEFAULT 'active' CHECK (state IN ('active','retracted')),
    created_by uuid NOT NULL, created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id) REFERENCES oc.workspaces(tenant_id, id),
    FOREIGN KEY (tenant_id,workspace_id,file_id) REFERENCES oc.files(tenant_id,workspace_id,id)
);

CREATE TABLE oc.assets (
    tenant_id uuid NOT NULL, workspace_id uuid NOT NULL, id uuid NOT NULL,
    kind text NOT NULL CHECK (kind IN ('memory','knowledge')),
    title text NOT NULL, fact_key text, current_version integer,
    deleted boolean NOT NULL DEFAULT false, created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id) REFERENCES oc.workspaces(tenant_id, id)
);
CREATE UNIQUE INDEX memory_fact_slot ON oc.assets(tenant_id,workspace_id,fact_key)
WHERE kind = 'memory';

CREATE TABLE oc.versions (
    tenant_id uuid NOT NULL, workspace_id uuid NOT NULL, asset_id uuid NOT NULL,
    version integer NOT NULL CHECK (version > 0), content text NOT NULL, content_hash text NOT NULL,
    source_event_id uuid NOT NULL, restored_from integer, review_id uuid, title text NOT NULL,
    created_by uuid NOT NULL, created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id,workspace_id,asset_id,version),
    FOREIGN KEY (tenant_id,workspace_id,asset_id) REFERENCES oc.assets(tenant_id,workspace_id,id),
    FOREIGN KEY (tenant_id,workspace_id,source_event_id) REFERENCES oc.events(tenant_id,workspace_id,id)
);
ALTER TABLE oc.assets ADD CONSTRAINT current_version_belongs_to_asset
FOREIGN KEY (tenant_id,workspace_id,id,current_version)
REFERENCES oc.versions(tenant_id,workspace_id,asset_id,version) DEFERRABLE INITIALLY DEFERRED;

CREATE TABLE oc.candidates (
    tenant_id uuid NOT NULL, workspace_id uuid NOT NULL, id uuid NOT NULL,
    asset_id uuid, source_event_id uuid NOT NULL, fact_key text NOT NULL,
    content text NOT NULL, revision integer NOT NULL DEFAULT 1,
    expected_version integer, state text NOT NULL DEFAULT 'candidate'
      CHECK (state IN ('candidate','approved','rejected','withdrawn','published')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id,workspace_id,id),
    FOREIGN KEY (tenant_id,workspace_id,asset_id) REFERENCES oc.assets(tenant_id,workspace_id,id),
    FOREIGN KEY (tenant_id,workspace_id,source_event_id) REFERENCES oc.events(tenant_id,workspace_id,id)
);

CREATE TABLE oc.reviews (
    tenant_id uuid NOT NULL, workspace_id uuid NOT NULL, id uuid NOT NULL,
    candidate_id uuid NOT NULL, revision integer NOT NULL, decision text NOT NULL,
    expected_version integer, reviewer uuid NOT NULL, reason text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id,workspace_id,id),
    FOREIGN KEY (tenant_id,workspace_id,candidate_id) REFERENCES oc.candidates(tenant_id,workspace_id,id)
);

CREATE TABLE oc.jobs (
    tenant_id uuid NOT NULL, workspace_id uuid NOT NULL, id uuid NOT NULL,
    created_by uuid NOT NULL, operation text NOT NULL, payload jsonb NOT NULL,
    state text NOT NULL DEFAULT 'pending'
      CHECK (state IN ('pending','processing','retry_wait','completed','failed','cancelled','superseded')),
    run_token bigint NOT NULL DEFAULT 0, generation bigint NOT NULL DEFAULT 1,
    asset_id uuid, source_event_id uuid,
    cancel_requested boolean NOT NULL DEFAULT false,
    outcome text, result jsonb, error_code text,
    created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id,workspace_id,id),
    FOREIGN KEY (tenant_id,workspace_id,asset_id) REFERENCES oc.assets(tenant_id,workspace_id,id),
    FOREIGN KEY (tenant_id,workspace_id,source_event_id) REFERENCES oc.events(tenant_id,workspace_id,id)
);

CREATE TABLE oc.chunks (
    tenant_id uuid NOT NULL, workspace_id uuid NOT NULL, id uuid NOT NULL,
    asset_id uuid NOT NULL, version integer NOT NULL, ordinal integer NOT NULL,
    content text NOT NULL, locator jsonb NOT NULL, search_vector tsvector NOT NULL,
    embedding vector, embedding_profile text,
    PRIMARY KEY (tenant_id,workspace_id,id),
    UNIQUE (tenant_id,workspace_id,asset_id,version,ordinal),
    FOREIGN KEY (tenant_id,workspace_id,asset_id,version) REFERENCES oc.versions(tenant_id,workspace_id,asset_id,version)
);
CREATE INDEX chunks_lexical ON oc.chunks USING gin(search_vector);
CREATE INDEX chunks_scope ON oc.chunks(tenant_id,workspace_id,asset_id,version);
CREATE INDEX evidence_reverse ON oc.versions(tenant_id,workspace_id,source_event_id);

CREATE TABLE oc.commands (
    tenant_id uuid NOT NULL, workspace_id uuid NOT NULL, principal_id uuid NOT NULL,
    operation text NOT NULL, key text NOT NULL, request_hash text NOT NULL, response jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id,workspace_id,principal_id,operation,key)
);

CREATE TABLE oc.audit (
    tenant_id uuid NOT NULL, workspace_id uuid NOT NULL, id uuid NOT NULL,
    actor uuid NOT NULL, action text NOT NULL, target uuid NOT NULL, details jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id,workspace_id,id)
);

-- The application role must neither own these tables nor have BYPASSRLS.
-- Identity is transaction-local: missing settings produce default-deny behavior.
DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['api_keys','events','files','assets','versions','candidates','reviews','jobs','chunks','commands','audit'] LOOP
    EXECUTE format('ALTER TABLE oc.%I ENABLE ROW LEVEL SECURITY', t);
    EXECUTE format('ALTER TABLE oc.%I FORCE ROW LEVEL SECURITY', t);
    EXECUTE format('CREATE POLICY workspace_scope ON oc.%I USING
      (tenant_id = nullif(current_setting(''oc.tenant_id'',true),'''')::uuid AND
       workspace_id = nullif(current_setting(''oc.workspace_id'',true),'''')::uuid)
      WITH CHECK
      (tenant_id = nullif(current_setting(''oc.tenant_id'',true),'''')::uuid AND
       workspace_id = nullif(current_setting(''oc.workspace_id'',true),'''')::uuid)', t);
  END LOOP;
END $$;

-- Only a hash is accepted. The SECURITY DEFINER owner is the bootstrap role.
-- It is deliberately limited to authentication and never returns a secret.
CREATE FUNCTION oc.authenticate(p_hash text)
RETURNS TABLE(id uuid,tenant_id uuid,workspace_id uuid,role text)
LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
  SELECT k.id,k.tenant_id,k.workspace_id,k.role FROM oc.api_keys k
  WHERE k.token_hash=p_hash AND NOT k.revoked
$$;
REVOKE ALL ON FUNCTION oc.authenticate(text) FROM PUBLIC;

CREATE INDEX jobs_asset ON oc.jobs(tenant_id,workspace_id,asset_id);
CREATE INDEX jobs_source ON oc.jobs(tenant_id,workspace_id,source_event_id);
CREATE INDEX events_file ON oc.events(tenant_id,workspace_id,file_id);
REVOKE CREATE ON SCHEMA public FROM PUBLIC;

-- Knowledge graph core: entities, relations, provenance owners and summaries.
-- Stable identity: entity id = sha256(canonical_name)[:16]; relation id = sha256(source||predicate||target)[:16].
-- Same-name entities merge only within one (tenant, workspace) scope.
CREATE TABLE oc.entities (
    tenant_id uuid NOT NULL, workspace_id uuid NOT NULL, id uuid NOT NULL,
    canonical_name text NOT NULL, type text NOT NULL, description text NOT NULL DEFAULT '',
    search_vector tsvector NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id) REFERENCES oc.workspaces(tenant_id, id)
);
CREATE INDEX entities_search ON oc.entities USING gin(search_vector);

CREATE TABLE oc.relations (
    tenant_id uuid NOT NULL, workspace_id uuid NOT NULL, id uuid NOT NULL,
    source_entity uuid NOT NULL, predicate text NOT NULL, target_entity uuid NOT NULL,
    fact_text text NOT NULL DEFAULT '',
    PRIMARY KEY (tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id) REFERENCES oc.workspaces(tenant_id, id),
    FOREIGN KEY (tenant_id, workspace_id, source_entity) REFERENCES oc.entities(tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id, target_entity) REFERENCES oc.entities(tenant_id, workspace_id, id)
);

-- Provenance: which source event introduced each graph object. Deleting a source
-- retracts owners first; relations/entities with no remaining owner are orphans.
CREATE TABLE oc.entity_owners (
    tenant_id uuid NOT NULL, workspace_id uuid NOT NULL,
    source_event_id uuid NOT NULL, entity_id uuid NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, source_event_id, entity_id),
    FOREIGN KEY (tenant_id, workspace_id, source_event_id) REFERENCES oc.events(tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id, entity_id) REFERENCES oc.entities(tenant_id, workspace_id, id)
);

CREATE TABLE oc.relation_owners (
    tenant_id uuid NOT NULL, workspace_id uuid NOT NULL,
    source_event_id uuid NOT NULL, relation_id uuid NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, source_event_id, relation_id),
    FOREIGN KEY (tenant_id, workspace_id, source_event_id) REFERENCES oc.events(tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id, relation_id) REFERENCES oc.relations(tenant_id, workspace_id, id)
);

-- Chunk-level summaries feed a third RRF branch in hybrid retrieval.
CREATE TABLE oc.summaries (
    tenant_id uuid NOT NULL, workspace_id uuid NOT NULL, id uuid NOT NULL,
    chunk_id uuid NOT NULL, text text NOT NULL, model_revision text NOT NULL DEFAULT '',
    search_vector tsvector NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, id),
    FOREIGN KEY (tenant_id, workspace_id, chunk_id) REFERENCES oc.chunks(tenant_id, workspace_id, id)
);
CREATE INDEX summaries_search ON oc.summaries USING gin(search_vector);
CREATE INDEX summaries_chunk_idx ON oc.summaries(tenant_id, workspace_id, chunk_id);

DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['entities','relations','entity_owners','relation_owners','summaries'] LOOP
    EXECUTE format('ALTER TABLE oc.%I ENABLE ROW LEVEL SECURITY', t);
    EXECUTE format('ALTER TABLE oc.%I FORCE ROW LEVEL SECURITY', t);
    EXECUTE format('CREATE POLICY workspace_scope ON oc.%I USING
      (tenant_id = nullif(current_setting(''oc.tenant_id'',true),'''')::uuid AND
       workspace_id = nullif(current_setting(''oc.workspace_id'',true),'''')::uuid)
      WITH CHECK
      (tenant_id = nullif(current_setting(''oc.tenant_id'',true),'''')::uuid AND
       workspace_id = nullif(current_setting(''oc.workspace_id'',true),'''')::uuid)', t);
  END LOOP;
END $$;
