-- The runtime role is deliberately not the schema owner. Run after initialization.
GRANT USAGE ON SCHEMA oc, apalis TO oc_runtime;
GRANT SELECT ON oc.api_keys TO oc_runtime;
GRANT SELECT, INSERT, UPDATE ON oc.events,oc.files,oc.assets,oc.candidates,oc.jobs TO oc_runtime;
GRANT SELECT, INSERT ON oc.versions,oc.reviews,oc.commands,oc.audit TO oc_runtime;
GRANT SELECT, INSERT, DELETE ON oc.chunks TO oc_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON oc.entities,oc.relations TO oc_runtime;
GRANT SELECT, INSERT, DELETE ON oc.entity_owners,oc.relation_owners,oc.summaries TO oc_runtime;
GRANT EXECUTE ON FUNCTION oc.authenticate(text) TO oc_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA apalis TO oc_runtime;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA apalis TO oc_runtime;
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA apalis TO oc_runtime;
