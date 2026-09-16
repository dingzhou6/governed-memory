CREATE EXTENSION IF NOT EXISTS pgcrypto;

DO $$
BEGIN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'agentic_memory_runtime') THEN
    CREATE ROLE agentic_memory_runtime;
  END IF;
END
$$;
ALTER ROLE agentic_memory_runtime WITH LOGIN PASSWORD 'synthetic-runtime-only' NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOBYPASSRLS;
ALTER ROLE agentic_memory_runtime RESET ALL;

DO $$
BEGIN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'agentic_memory_purge_worker') THEN
    CREATE ROLE agentic_memory_purge_worker;
  END IF;
END
$$;
ALTER ROLE agentic_memory_purge_worker WITH LOGIN PASSWORD 'synthetic-purge-worker-only' NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOBYPASSRLS;
ALTER ROLE agentic_memory_purge_worker RESET ALL;

DO $$
DECLARE
  runtime_oid oid := (SELECT oid FROM pg_roles WHERE rolname = 'agentic_memory_runtime');
BEGIN
  IF EXISTS (SELECT FROM pg_auth_members WHERE member = runtime_oid) THEN
    RAISE EXCEPTION 'agentic_memory_runtime must not inherit or assume another role';
  END IF;
  IF EXISTS (SELECT FROM pg_database WHERE datdba = runtime_oid)
     OR EXISTS (SELECT FROM pg_namespace WHERE nspowner = runtime_oid)
     OR EXISTS (SELECT FROM pg_class WHERE relowner = runtime_oid)
     OR EXISTS (SELECT FROM pg_proc WHERE proowner = runtime_oid) THEN
    RAISE EXCEPTION 'agentic_memory_runtime must not own database objects';
  END IF;
END
$$;

DO $$
DECLARE
  worker_oid oid := (SELECT oid FROM pg_roles WHERE rolname = 'agentic_memory_purge_worker');
BEGIN
  IF EXISTS (SELECT FROM pg_auth_members WHERE member = worker_oid) THEN
    RAISE EXCEPTION 'agentic_memory_purge_worker must not inherit or assume another role';
  END IF;
  IF EXISTS (SELECT FROM pg_database WHERE datdba = worker_oid)
     OR EXISTS (SELECT FROM pg_namespace WHERE nspowner = worker_oid)
     OR EXISTS (SELECT FROM pg_class WHERE relowner = worker_oid)
     OR EXISTS (SELECT FROM pg_proc WHERE proowner = worker_oid) THEN
    RAISE EXCEPTION 'agentic_memory_purge_worker must not own database objects';
  END IF;
END
$$;

CREATE TABLE IF NOT EXISTS tenants (
  id text PRIMARY KEY CHECK (id ~ '^[A-Za-z0-9_-]{1,64}$')
);

CREATE TABLE IF NOT EXISTS tenant_authority (
  tenant_id text PRIMARY KEY REFERENCES tenants (id),
  authority_epoch bigint NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS principals (
  tenant_id text NOT NULL REFERENCES tenants (id),
  id text NOT NULL CHECK (id ~ '^[A-Za-z0-9_-]{1,64}$'),
  active boolean NOT NULL,
  PRIMARY KEY (tenant_id, id)
);

CREATE TABLE IF NOT EXISTS apps (
  tenant_id text NOT NULL REFERENCES tenants (id),
  id text NOT NULL CHECK (id ~ '^[A-Za-z0-9_-]{1,64}$'),
  active boolean NOT NULL,
  PRIMARY KEY (tenant_id, id)
);

CREATE TABLE IF NOT EXISTS credentials (
  tenant_id text NOT NULL,
  id text NOT NULL CHECK (id ~ '^[A-Za-z0-9_-]{1,64}$'),
  principal_id text NOT NULL,
  app_id text NOT NULL,
  token_digest bytea NOT NULL UNIQUE,
  credential_class text NOT NULL CHECK (credential_class IN ('agent_reader', 'trusted_writer')),
  allowed_operations text[] NOT NULL,
  issued_at timestamptz NOT NULL,
  expires_at timestamptz NOT NULL,
  revoked_at timestamptz,
  PRIMARY KEY (tenant_id, id),
  FOREIGN KEY (tenant_id, principal_id) REFERENCES principals (tenant_id, id),
  FOREIGN KEY (tenant_id, app_id) REFERENCES apps (tenant_id, id),
  CHECK (issued_at < expires_at)
);
ALTER TABLE credentials DROP CONSTRAINT IF EXISTS credentials_operation_class_check;
ALTER TABLE credentials ADD CONSTRAINT credentials_operation_class_check CHECK (
  cardinality(allowed_operations) BETWEEN 1 AND 3
  AND (
    (credential_class = 'agent_reader'
      AND allowed_operations <@ ARRAY['list', 'search', 'read']::text[])
    OR
    (credential_class = 'trusted_writer'
      AND allowed_operations <@ ARRAY['create', 'correct', 'forget']::text[])
  )
);

CREATE TABLE IF NOT EXISTS collections (
  tenant_id text NOT NULL,
  id text NOT NULL CHECK (id ~ '^[A-Za-z0-9_-]{1,64}$'),
  app_id text NOT NULL,
  audience_kind text NOT NULL CHECK (audience_kind IN ('restricted', 'private')),
  owner_principal_id text,
  withdrawn_at timestamptz,
  PRIMARY KEY (tenant_id, id),
  FOREIGN KEY (tenant_id, app_id) REFERENCES apps (tenant_id, id),
  FOREIGN KEY (tenant_id, owner_principal_id) REFERENCES principals (tenant_id, id),
  CHECK ((audience_kind = 'private') = (owner_principal_id IS NOT NULL))
);

CREATE TABLE IF NOT EXISTS items (
  tenant_id text NOT NULL,
  id text NOT NULL CHECK (id ~ '^[A-Za-z0-9_-]{1,64}$'),
  collection_id text NOT NULL,
  active_revision_id text,
  deleted_at timestamptz,
  PRIMARY KEY (tenant_id, id),
  FOREIGN KEY (tenant_id, collection_id) REFERENCES collections (tenant_id, id)
);
ALTER TABLE items ADD COLUMN IF NOT EXISTS deletion_generation bigint NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS revisions (
  tenant_id text NOT NULL,
  item_id text NOT NULL,
  id text NOT NULL CHECK (id ~ '^[A-Za-z0-9_-]{1,64}$'),
  content text NOT NULL CHECK (octet_length(content) BETWEEN 1 AND 32768),
  recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  valid_from timestamptz,
  valid_until timestamptz,
  PRIMARY KEY (tenant_id, item_id, id),
  FOREIGN KEY (tenant_id, item_id) REFERENCES items (tenant_id, id),
  CHECK (valid_from IS NULL OR valid_until IS NULL OR valid_from < valid_until)
);

CREATE TABLE IF NOT EXISTS subjects (
  tenant_id text NOT NULL,
  id text NOT NULL CHECK (id ~ '^[A-Za-z0-9_-]{1,64}$'),
  app_id text NOT NULL,
  kind text NOT NULL CHECK (kind IN ('principal', 'app', 'customer', 'project')),
  principal_id text,
  PRIMARY KEY (tenant_id, id),
  FOREIGN KEY (tenant_id, app_id) REFERENCES apps (tenant_id, id),
  FOREIGN KEY (tenant_id, principal_id) REFERENCES principals (tenant_id, id),
  CHECK ((kind = 'principal') = (principal_id IS NOT NULL))
);

CREATE TABLE IF NOT EXISTS revision_subjects (
  tenant_id text NOT NULL,
  item_id text NOT NULL,
  revision_id text NOT NULL,
  subject_id text NOT NULL,
  PRIMARY KEY (tenant_id, item_id, revision_id, subject_id),
  FOREIGN KEY (tenant_id, item_id, revision_id) REFERENCES revisions (tenant_id, item_id, id),
  FOREIGN KEY (tenant_id, subject_id) REFERENCES subjects (tenant_id, id)
);

CREATE TABLE IF NOT EXISTS lexical_representations (
  tenant_id text NOT NULL,
  item_id text NOT NULL,
  revision_id text NOT NULL,
  document tsvector NOT NULL,
  PRIMARY KEY (tenant_id, item_id, revision_id),
  FOREIGN KEY (tenant_id, item_id, revision_id) REFERENCES revisions (tenant_id, item_id, id)
);

CREATE TABLE IF NOT EXISTS source_revisions (
  tenant_id text NOT NULL,
  item_id text NOT NULL,
  revision_id text NOT NULL,
  id text NOT NULL CHECK (id ~ '^[A-Za-z0-9_-]{1,64}$'),
  source_sha256 bytea NOT NULL CHECK (octet_length(source_sha256) = 32),
  PRIMARY KEY (tenant_id, item_id, revision_id, id),
  UNIQUE (tenant_id, item_id, revision_id),
  FOREIGN KEY (tenant_id, item_id, revision_id)
    REFERENCES revisions (tenant_id, item_id, id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS extraction_sets (
  tenant_id text NOT NULL,
  item_id text NOT NULL,
  revision_id text NOT NULL,
  source_revision_id text NOT NULL,
  id text NOT NULL CHECK (id ~ '^[A-Za-z0-9_-]{1,64}$'),
  parser_id text NOT NULL CHECK (octet_length(parser_id) BETWEEN 1 AND 128 AND parser_id ~ '[^[:space:]]'),
  parser_version text NOT NULL CHECK (octet_length(parser_version) BETWEEN 1 AND 128 AND parser_version ~ '[^[:space:]]'),
  config_version text NOT NULL CHECK (octet_length(config_version) BETWEEN 1 AND 128 AND config_version ~ '[^[:space:]]'),
  search_recipe_version text NOT NULL DEFAULT 'body-v0'
    CHECK (search_recipe_version ~ '^[A-Za-z0-9_-]{1,64}$'),
  passage_count integer NOT NULL CHECK (passage_count BETWEEN 1 AND 1024),
  content_bytes integer NOT NULL CHECK (content_bytes BETWEEN 1 AND 2097152),
  completed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (tenant_id, item_id, revision_id, source_revision_id, id),
  CONSTRAINT extraction_sets_processing_recipe_key UNIQUE (
    tenant_id, item_id, revision_id, source_revision_id,
    parser_id, parser_version, config_version, search_recipe_version
  ),
  FOREIGN KEY (tenant_id, item_id, revision_id, source_revision_id)
    REFERENCES source_revisions (tenant_id, item_id, revision_id, id) ON DELETE CASCADE
);
ALTER TABLE extraction_sets ADD COLUMN IF NOT EXISTS search_recipe_version text
  NOT NULL DEFAULT 'body-v0';
ALTER TABLE extraction_sets DROP CONSTRAINT IF EXISTS extraction_sets_search_recipe_version_check;
ALTER TABLE extraction_sets ADD CONSTRAINT extraction_sets_search_recipe_version_check
  CHECK (search_recipe_version ~ '^[A-Za-z0-9_-]{1,64}$');
ALTER TABLE extraction_sets DROP CONSTRAINT IF EXISTS extraction_sets_tenant_id_item_id_revision_id_source_revisi_key;
ALTER TABLE extraction_sets DROP CONSTRAINT IF EXISTS extraction_sets_processing_recipe_key;
ALTER TABLE extraction_sets ADD CONSTRAINT extraction_sets_processing_recipe_key UNIQUE (
  tenant_id, item_id, revision_id, source_revision_id,
  parser_id, parser_version, config_version, search_recipe_version
);

CREATE TABLE IF NOT EXISTS source_passages (
  tenant_id text NOT NULL,
  item_id text NOT NULL,
  revision_id text NOT NULL,
  source_revision_id text NOT NULL,
  extraction_set_id text NOT NULL,
  id text NOT NULL CHECK (id ~ '^[A-Za-z0-9_-]{1,64}$'),
  structural_parent_id text NOT NULL CHECK (structural_parent_id ~ '^[A-Za-z0-9_-]{1,64}$'),
  passage_order integer NOT NULL CHECK (passage_order BETWEEN 1 AND 1024),
  continuation_direction text NOT NULL CHECK (
    continuation_direction IN ('none', 'from_previous', 'to_next', 'both')
  ),
  locator jsonb NOT NULL CHECK (
    jsonb_typeof(locator) = 'object' AND octet_length(locator::text) BETWEEN 2 AND 2048
  ),
  content text NOT NULL CHECK (octet_length(content) BETWEEN 1 AND 32768),
  search_document tsvector NOT NULL,
  PRIMARY KEY (tenant_id, item_id, revision_id, source_revision_id, extraction_set_id, id),
  UNIQUE (tenant_id, item_id, revision_id, source_revision_id, extraction_set_id, passage_order),
  UNIQUE (tenant_id, item_id, revision_id, source_revision_id, extraction_set_id, locator),
  FOREIGN KEY (tenant_id, item_id, revision_id, source_revision_id, extraction_set_id)
    REFERENCES extraction_sets (tenant_id, item_id, revision_id, source_revision_id, id)
    ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS source_passages_search_document_idx
  ON source_passages USING gin (search_document);

CREATE OR REPLACE FUNCTION source_search_document_v1(p_content text, p_locator jsonb)
RETURNS tsvector
LANGUAGE plpgsql
IMMUTABLE
STRICT
SET search_path = pg_catalog, public
AS $$
DECLARE
  v_fields jsonb := jsonb_build_array(p_content);
  v_header text;
  v_header_bytes integer := 0;
BEGIN
  IF p_locator ? 'title' THEN
    IF jsonb_typeof(p_locator->'title') <> 'string'
       OR octet_length(p_locator->>'title') NOT BETWEEN 1 AND 256
       OR p_locator->>'title' !~ '[^[:space:]]' THEN
      RAISE EXCEPTION 'invalid source title' USING ERRCODE = '22023';
    END IF;
    v_fields := v_fields || jsonb_build_array(p_locator->>'title');
  END IF;
  IF p_locator ? 'heading' THEN
    IF jsonb_typeof(p_locator->'heading') <> 'string'
       OR octet_length(p_locator->>'heading') NOT BETWEEN 1 AND 512
       OR p_locator->>'heading' !~ '[^[:space:]]' THEN
      RAISE EXCEPTION 'invalid source heading' USING ERRCODE = '22023';
    END IF;
    v_fields := v_fields || jsonb_build_array(p_locator->>'heading');
  END IF;
  IF p_locator ? 'table_headers' THEN
    IF jsonb_typeof(p_locator->'table_headers') <> 'array'
       OR jsonb_array_length(p_locator->'table_headers') NOT BETWEEN 1 AND 32
       OR EXISTS (
         SELECT 1 FROM jsonb_array_elements(p_locator->'table_headers') header
         WHERE jsonb_typeof(header) <> 'string'
            OR octet_length(header#>>'{}') NOT BETWEEN 1 AND 256
            OR header#>>'{}' !~ '[^[:space:]]'
       ) THEN
      RAISE EXCEPTION 'invalid source table headers' USING ERRCODE = '22023';
    END IF;
    FOR v_header IN
      SELECT header#>>'{}'
      FROM jsonb_array_elements(p_locator->'table_headers') WITH ORDINALITY values_(header, ordinal)
      ORDER BY ordinal
    LOOP
      v_header_bytes := v_header_bytes + octet_length(v_header);
      IF v_header_bytes > 2048 THEN
        RAISE EXCEPTION 'source table headers budget exceeded' USING ERRCODE = '22023';
      END IF;
      v_fields := v_fields || jsonb_build_array(v_header);
    END LOOP;
  END IF;
  RETURN to_tsvector('simple', v_fields);
END
$$;

CREATE TABLE IF NOT EXISTS active_extraction_sets (
  tenant_id text NOT NULL,
  item_id text NOT NULL,
  revision_id text NOT NULL,
  source_revision_id text NOT NULL,
  extraction_set_id text NOT NULL,
  activated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (tenant_id, item_id, revision_id),
  FOREIGN KEY (tenant_id, item_id, revision_id)
    REFERENCES revisions (tenant_id, item_id, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, item_id, revision_id, source_revision_id, extraction_set_id)
    REFERENCES extraction_sets (tenant_id, item_id, revision_id, source_revision_id, id)
    ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS idempotency_records (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  app_id text NOT NULL,
  operation text NOT NULL CHECK (operation IN ('create', 'correct')),
  key_digest bytea NOT NULL CHECK (octet_length(key_digest) = 32),
  request_digest bytea NOT NULL CHECK (octet_length(request_digest) = 32),
  operation_id text NOT NULL CHECK (operation_id ~ '^[A-Za-z0-9_-]{1,64}$'),
  item_id text NOT NULL CHECK (item_id ~ '^[A-Za-z0-9_-]{1,64}$'),
  revision_id text NOT NULL CHECK (revision_id ~ '^[A-Za-z0-9_-]{1,64}$'),
  completed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at timestamptz NOT NULL,
  PRIMARY KEY (tenant_id, principal_id, app_id, operation, key_digest),
  FOREIGN KEY (tenant_id, principal_id) REFERENCES principals (tenant_id, id),
  FOREIGN KEY (tenant_id, app_id) REFERENCES apps (tenant_id, id)
);
ALTER TABLE idempotency_records DROP CONSTRAINT IF EXISTS idempotency_records_operation_check;
ALTER TABLE idempotency_records ADD CONSTRAINT idempotency_records_operation_check
  CHECK (operation IN ('create', 'correct', 'forget'));

CREATE TABLE IF NOT EXISTS mutation_audit (
  occurred_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  request_id text PRIMARY KEY CHECK (request_id ~ '^[0-9a-f-]{36}$'),
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  app_id text NOT NULL,
  credential_id text NOT NULL,
  operation text NOT NULL CHECK (operation IN ('create', 'correct')),
  operation_id text NOT NULL,
  target_id text NOT NULL,
  revision_id text NOT NULL,
  outcome text NOT NULL CHECK (outcome IN ('created', 'corrected')),
  authority_epoch bigint NOT NULL,
  idempotency_key_digest bytea NOT NULL CHECK (octet_length(idempotency_key_digest) = 32),
  FOREIGN KEY (tenant_id, principal_id) REFERENCES principals (tenant_id, id),
  FOREIGN KEY (tenant_id, app_id) REFERENCES apps (tenant_id, id),
  FOREIGN KEY (tenant_id, credential_id) REFERENCES credentials (tenant_id, id)
);
ALTER TABLE mutation_audit DROP CONSTRAINT IF EXISTS mutation_audit_operation_check;
ALTER TABLE mutation_audit ADD CONSTRAINT mutation_audit_operation_check
  CHECK (operation IN ('create', 'correct', 'forget'));
ALTER TABLE mutation_audit DROP CONSTRAINT IF EXISTS mutation_audit_outcome_check;
ALTER TABLE mutation_audit ADD CONSTRAINT mutation_audit_outcome_check
  CHECK (outcome IN ('created', 'corrected', 'forgotten'));

CREATE TABLE IF NOT EXISTS create_rejection_audit (
  occurred_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  request_id text PRIMARY KEY CHECK (request_id ~ '^[0-9a-f-]{36}$'),
  tenant_id text REFERENCES tenants (id),
  principal_id text,
  app_id text,
  credential_id text,
  operation text NOT NULL CHECK (operation IN ('create', 'correct')),
  outcome text NOT NULL CHECK (
    outcome IN ('malformed', 'unauthenticated', 'unavailable', 'storage_unavailable', 'stale_context', 'idempotency_conflict', 'replayed')
  ),
  authority_epoch bigint,
  idempotency_key_digest bytea,
  FOREIGN KEY (tenant_id, principal_id) REFERENCES principals (tenant_id, id),
  FOREIGN KEY (tenant_id, app_id) REFERENCES apps (tenant_id, id),
  FOREIGN KEY (tenant_id, credential_id) REFERENCES credentials (tenant_id, id)
);
ALTER TABLE create_rejection_audit DROP CONSTRAINT IF EXISTS create_rejection_audit_operation_check;
ALTER TABLE create_rejection_audit ADD CONSTRAINT create_rejection_audit_operation_check
  CHECK (operation IN ('create', 'correct', 'forget'));
ALTER TABLE create_rejection_audit DROP CONSTRAINT IF EXISTS create_rejection_audit_outcome_check;
ALTER TABLE create_rejection_audit ADD CONSTRAINT create_rejection_audit_outcome_check
  CHECK (outcome IN ('malformed', 'unauthenticated', 'unavailable', 'storage_unavailable', 'stale_context', 'idempotency_conflict', 'replayed'));
ALTER TABLE create_rejection_audit ADD COLUMN IF NOT EXISTS authority_epoch bigint;
ALTER TABLE create_rejection_audit ADD COLUMN IF NOT EXISTS idempotency_key_digest bytea;
ALTER TABLE create_rejection_audit DROP CONSTRAINT IF EXISTS create_rejection_audit_key_digest_check;
ALTER TABLE create_rejection_audit ADD CONSTRAINT create_rejection_audit_key_digest_check
  CHECK (idempotency_key_digest IS NULL OR octet_length(idempotency_key_digest) = 32);

CREATE TABLE IF NOT EXISTS deletion_markers (
  tenant_id text NOT NULL,
  item_id text NOT NULL,
  deletion_generation bigint NOT NULL CHECK (deletion_generation > 0),
  deleted_at timestamptz NOT NULL,
  operation_id text NOT NULL CHECK (operation_id ~ '^[A-Za-z0-9_-]{1,64}$'),
  purge_state text NOT NULL CHECK (purge_state IN ('pending', 'complete')),
  PRIMARY KEY (tenant_id, item_id),
  FOREIGN KEY (tenant_id, item_id) REFERENCES items (tenant_id, id)
);

CREATE TABLE IF NOT EXISTS purge_jobs (
  tenant_id text NOT NULL,
  item_id text NOT NULL,
  deletion_generation bigint NOT NULL CHECK (deletion_generation > 0),
  operation_id text NOT NULL CHECK (operation_id ~ '^[A-Za-z0-9_-]{1,64}$'),
  status text NOT NULL CHECK (status IN ('pending', 'complete')),
  completed_at timestamptz,
  PRIMARY KEY (tenant_id, item_id, deletion_generation),
  UNIQUE (tenant_id, operation_id),
  FOREIGN KEY (tenant_id, item_id) REFERENCES items (tenant_id, id)
);

CREATE TABLE IF NOT EXISTS collection_grants (
  tenant_id text NOT NULL,
  collection_id text NOT NULL,
  principal_id text NOT NULL,
  can_read boolean NOT NULL,
  PRIMARY KEY (tenant_id, collection_id, principal_id),
  FOREIGN KEY (tenant_id, collection_id) REFERENCES collections (tenant_id, id),
  FOREIGN KEY (tenant_id, principal_id) REFERENCES principals (tenant_id, id)
);

CREATE TABLE IF NOT EXISTS read_audit (
  occurred_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  request_id text PRIMARY KEY CHECK (request_id ~ '^[0-9a-f-]{36}$'),
  tenant_id text REFERENCES tenants (id),
  principal_id text,
  app_id text,
  credential_id text,
  operation text NOT NULL CHECK (operation = 'read'),
  target_id text,
  outcome text NOT NULL CHECK (outcome IN ('released', 'unavailable', 'malformed', 'unauthenticated', 'storage_unavailable')),
  authority_epoch bigint,
  FOREIGN KEY (tenant_id, principal_id) REFERENCES principals (tenant_id, id),
  FOREIGN KEY (tenant_id, app_id) REFERENCES apps (tenant_id, id),
  FOREIGN KEY (tenant_id, credential_id) REFERENCES credentials (tenant_id, id)
);

CREATE TABLE IF NOT EXISTS list_cursors (
  token text PRIMARY KEY CHECK (token ~ '^[0-9a-f]{32}$'),
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  app_id text NOT NULL,
  resource_kind text NOT NULL CHECK (resource_kind IN ('collections', 'items')),
  after_id text NOT NULL CHECK (after_id ~ '^[A-Za-z0-9_-]{1,64}$'),
  expires_at timestamptz NOT NULL,
  FOREIGN KEY (tenant_id, principal_id) REFERENCES principals (tenant_id, id),
  FOREIGN KEY (tenant_id, app_id) REFERENCES apps (tenant_id, id)
);
ALTER TABLE list_cursors DROP CONSTRAINT IF EXISTS list_cursors_active_slot;
ALTER TABLE list_cursors ADD CONSTRAINT list_cursors_active_slot
  UNIQUE (tenant_id, principal_id, app_id, resource_kind);

ALTER TABLE read_audit ALTER COLUMN tenant_id DROP NOT NULL;
ALTER TABLE read_audit ALTER COLUMN principal_id DROP NOT NULL;
ALTER TABLE read_audit ALTER COLUMN app_id DROP NOT NULL;
ALTER TABLE read_audit ALTER COLUMN credential_id DROP NOT NULL;
ALTER TABLE read_audit ALTER COLUMN authority_epoch DROP NOT NULL;
ALTER TABLE read_audit DROP CONSTRAINT IF EXISTS read_audit_request_id_check;
ALTER TABLE read_audit ADD CONSTRAINT read_audit_request_id_check CHECK (request_id ~ '^[0-9a-f-]{36}$');
ALTER TABLE read_audit DROP CONSTRAINT IF EXISTS read_audit_outcome_check;
ALTER TABLE read_audit ADD CONSTRAINT read_audit_outcome_check
  CHECK (outcome IN ('released', 'unavailable', 'malformed', 'unauthenticated', 'storage_unavailable', 'stale_context', 'unsupported_time_semantics'));
ALTER TABLE read_audit DROP CONSTRAINT IF EXISTS read_audit_operation_check;
ALTER TABLE read_audit ADD CONSTRAINT read_audit_operation_check
  CHECK (operation IN ('read', 'search', 'list'));

CREATE OR REPLACE FUNCTION reject_revision_mutation()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
  RAISE EXCEPTION 'revisions are immutable' USING ERRCODE = '55000';
END
$$;
DROP TRIGGER IF EXISTS revisions_are_immutable ON revisions;
CREATE TRIGGER revisions_are_immutable
BEFORE UPDATE ON revisions
FOR EACH ROW EXECUTE FUNCTION reject_revision_mutation();

CREATE OR REPLACE FUNCTION reject_derived_record_mutation()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
  RAISE EXCEPTION 'derived document records are immutable' USING ERRCODE = '55000';
END
$$;
DROP TRIGGER IF EXISTS source_revisions_are_immutable ON source_revisions;
CREATE TRIGGER source_revisions_are_immutable
BEFORE UPDATE ON source_revisions
FOR EACH ROW EXECUTE FUNCTION reject_derived_record_mutation();
DROP TRIGGER IF EXISTS extraction_sets_are_immutable ON extraction_sets;
CREATE TRIGGER extraction_sets_are_immutable
BEFORE UPDATE ON extraction_sets
FOR EACH ROW EXECUTE FUNCTION reject_derived_record_mutation();
DROP TRIGGER IF EXISTS source_passages_are_immutable ON source_passages;
CREATE TRIGGER source_passages_are_immutable
BEFORE UPDATE ON source_passages
FOR EACH ROW EXECUTE FUNCTION reject_derived_record_mutation();

ALTER TABLE items DROP CONSTRAINT IF EXISTS items_active_revision_fk;
ALTER TABLE items ADD CONSTRAINT items_active_revision_fk
  FOREIGN KEY (tenant_id, id, active_revision_id)
  REFERENCES revisions (tenant_id, item_id, id)
  DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE tenant_authority ENABLE ROW LEVEL SECURITY;
ALTER TABLE principals ENABLE ROW LEVEL SECURITY;
ALTER TABLE apps ENABLE ROW LEVEL SECURITY;
ALTER TABLE credentials ENABLE ROW LEVEL SECURITY;
ALTER TABLE collections ENABLE ROW LEVEL SECURITY;
ALTER TABLE items ENABLE ROW LEVEL SECURITY;
ALTER TABLE revisions ENABLE ROW LEVEL SECURITY;
ALTER TABLE collection_grants ENABLE ROW LEVEL SECURITY;
ALTER TABLE read_audit ENABLE ROW LEVEL SECURITY;
ALTER TABLE subjects ENABLE ROW LEVEL SECURITY;
ALTER TABLE revision_subjects ENABLE ROW LEVEL SECURITY;
ALTER TABLE lexical_representations ENABLE ROW LEVEL SECURITY;
ALTER TABLE idempotency_records ENABLE ROW LEVEL SECURITY;
ALTER TABLE mutation_audit ENABLE ROW LEVEL SECURITY;
ALTER TABLE create_rejection_audit ENABLE ROW LEVEL SECURITY;
ALTER TABLE list_cursors ENABLE ROW LEVEL SECURITY;
ALTER TABLE deletion_markers ENABLE ROW LEVEL SECURITY;
ALTER TABLE purge_jobs ENABLE ROW LEVEL SECURITY;
ALTER TABLE source_revisions ENABLE ROW LEVEL SECURITY;
ALTER TABLE extraction_sets ENABLE ROW LEVEL SECURITY;
ALTER TABLE source_passages ENABLE ROW LEVEL SECURITY;
ALTER TABLE active_extraction_sets ENABLE ROW LEVEL SECURITY;

ALTER TABLE tenant_authority FORCE ROW LEVEL SECURITY;
ALTER TABLE principals FORCE ROW LEVEL SECURITY;
ALTER TABLE apps FORCE ROW LEVEL SECURITY;
ALTER TABLE credentials FORCE ROW LEVEL SECURITY;
ALTER TABLE collections FORCE ROW LEVEL SECURITY;
ALTER TABLE items FORCE ROW LEVEL SECURITY;
ALTER TABLE revisions FORCE ROW LEVEL SECURITY;
ALTER TABLE collection_grants FORCE ROW LEVEL SECURITY;
ALTER TABLE read_audit FORCE ROW LEVEL SECURITY;
ALTER TABLE subjects FORCE ROW LEVEL SECURITY;
ALTER TABLE revision_subjects FORCE ROW LEVEL SECURITY;
ALTER TABLE lexical_representations FORCE ROW LEVEL SECURITY;
ALTER TABLE idempotency_records FORCE ROW LEVEL SECURITY;
ALTER TABLE mutation_audit FORCE ROW LEVEL SECURITY;
ALTER TABLE create_rejection_audit FORCE ROW LEVEL SECURITY;
ALTER TABLE list_cursors FORCE ROW LEVEL SECURITY;
ALTER TABLE deletion_markers FORCE ROW LEVEL SECURITY;
ALTER TABLE purge_jobs FORCE ROW LEVEL SECURITY;
ALTER TABLE source_revisions FORCE ROW LEVEL SECURITY;
ALTER TABLE extraction_sets FORCE ROW LEVEL SECURITY;
ALTER TABLE source_passages FORCE ROW LEVEL SECURITY;
ALTER TABLE active_extraction_sets FORCE ROW LEVEL SECURITY;

CREATE OR REPLACE FUNCTION reader_tenant_scope(p_tenant_id text)
RETURNS boolean
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
  SELECT p_tenant_id = nullif(current_setting('app.tenant_id', true), '')
    AND EXISTS (
      SELECT 1
      FROM public.credentials c
      JOIN public.principals p ON p.tenant_id = c.tenant_id AND p.id = c.principal_id
      JOIN public.apps a ON a.tenant_id = c.tenant_id AND a.id = c.app_id
      WHERE c.tenant_id = p_tenant_id
        AND c.token_digest = decode(nullif(current_setting('app.credential_digest', true), ''), 'hex')
        AND c.credential_class = 'agent_reader'
        AND nullif(current_setting('app.operation', true), '') = 'read'
        AND 'read' = ANY(c.allowed_operations)
        AND c.allowed_operations <@ ARRAY['list', 'search', 'read']::text[]
        AND c.issued_at <= clock_timestamp()
        AND c.expires_at > clock_timestamp()
        AND c.revoked_at IS NULL
        AND p.active
        AND a.active
    )
$$;

CREATE OR REPLACE FUNCTION current_reader_context(p_tenant_id text)
RETURNS boolean
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
  SELECT p_tenant_id = nullif(current_setting('app.tenant_id', true), '')
    AND EXISTS (
      SELECT 1
      FROM public.credentials c
      JOIN public.principals p ON p.tenant_id = c.tenant_id AND p.id = c.principal_id
      JOIN public.apps a ON a.tenant_id = c.tenant_id AND a.id = c.app_id
      WHERE c.tenant_id = p_tenant_id
        AND c.token_digest = decode(nullif(current_setting('app.credential_digest', true), ''), 'hex')
        AND c.credential_class = 'agent_reader'
        AND nullif(current_setting('app.operation', true), '') IN ('read', 'search', 'list')
        AND nullif(current_setting('app.operation', true), '') = ANY(c.allowed_operations)
        AND c.allowed_operations <@ ARRAY['list', 'search', 'read']::text[]
        AND c.issued_at <= clock_timestamp()
        AND c.expires_at > clock_timestamp()
        AND c.revoked_at IS NULL
        AND p.active
        AND a.active
    )
$$;

DROP POLICY IF EXISTS tenant_scope ON tenant_authority;
CREATE POLICY tenant_scope ON tenant_authority USING (current_reader_context(tenant_id));
DROP POLICY IF EXISTS tenant_scope ON principals;
CREATE POLICY tenant_scope ON principals USING (reader_tenant_scope(tenant_id));
DROP POLICY IF EXISTS tenant_scope ON apps;
CREATE POLICY tenant_scope ON apps USING (reader_tenant_scope(tenant_id));
DROP POLICY IF EXISTS credential_scope ON credentials;
CREATE POLICY credential_scope ON credentials USING (
  encode(token_digest, 'hex') = current_setting('app.credential_digest', true)
  AND (
    nullif(current_setting('app.tenant_id', true), '') IS NULL
    OR tenant_id = current_setting('app.tenant_id', true)
  )
);
DROP POLICY IF EXISTS tenant_scope ON collections;
CREATE POLICY tenant_scope ON collections USING (reader_tenant_scope(tenant_id));
DROP POLICY IF EXISTS tenant_scope ON items;
CREATE POLICY tenant_scope ON items USING (reader_tenant_scope(tenant_id));
DROP POLICY IF EXISTS tenant_scope ON revisions;
CREATE POLICY tenant_scope ON revisions USING (reader_tenant_scope(tenant_id));
DROP POLICY IF EXISTS tenant_scope ON collection_grants;
CREATE POLICY tenant_scope ON collection_grants USING (reader_tenant_scope(tenant_id));
DROP POLICY IF EXISTS tenant_scope ON read_audit;
CREATE POLICY tenant_scope ON read_audit USING (current_reader_context(tenant_id));
DROP POLICY IF EXISTS tenant_scope ON subjects;
CREATE POLICY tenant_scope ON subjects USING (current_reader_context(tenant_id));
DROP POLICY IF EXISTS tenant_scope ON revision_subjects;
CREATE POLICY tenant_scope ON revision_subjects USING (current_reader_context(tenant_id));
DROP POLICY IF EXISTS tenant_scope ON lexical_representations;
CREATE POLICY tenant_scope ON lexical_representations USING (current_reader_context(tenant_id));
DROP POLICY IF EXISTS tenant_scope ON list_cursors;
CREATE POLICY tenant_scope ON list_cursors
  USING (current_reader_context(tenant_id))
  WITH CHECK (current_reader_context(tenant_id));
DROP POLICY IF EXISTS tenant_scope ON source_revisions;
CREATE POLICY tenant_scope ON source_revisions USING (current_reader_context(tenant_id));
DROP POLICY IF EXISTS tenant_scope ON extraction_sets;
CREATE POLICY tenant_scope ON extraction_sets USING (current_reader_context(tenant_id));
DROP POLICY IF EXISTS tenant_scope ON source_passages;
CREATE POLICY tenant_scope ON source_passages USING (current_reader_context(tenant_id));
DROP POLICY IF EXISTS tenant_scope ON active_extraction_sets;
CREATE POLICY tenant_scope ON active_extraction_sets USING (current_reader_context(tenant_id));

DROP FUNCTION IF EXISTS resolve_current_reader();
CREATE OR REPLACE FUNCTION resolve_current_reader(p_operation text)
RETURNS TABLE (tenant_id text, credential_id text)
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
  SELECT c.tenant_id, c.id
  FROM public.credentials c
  JOIN public.principals p ON p.tenant_id = c.tenant_id AND p.id = c.principal_id
  JOIN public.apps a ON a.tenant_id = c.tenant_id AND a.id = c.app_id
  WHERE c.token_digest = decode(nullif(current_setting('app.credential_digest', true), ''), 'hex')
    AND c.credential_class = 'agent_reader'
    AND p_operation IN ('read', 'search', 'list')
    AND p_operation = ANY(c.allowed_operations)
    AND c.allowed_operations <@ ARRAY['list', 'search', 'read']::text[]
    AND c.issued_at <= clock_timestamp()
    AND c.expires_at > clock_timestamp()
    AND c.revoked_at IS NULL
    AND p.active
    AND a.active
$$;

DROP FUNCTION IF EXISTS current_writer_context(text, text);
CREATE OR REPLACE FUNCTION current_writer_context(
  p_tenant_id text,
  p_credential_id text,
  p_operation text
)
RETURNS boolean
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
  SELECT p_tenant_id = nullif(current_setting('app.tenant_id', true), '')
    AND EXISTS (
      SELECT 1
      FROM public.credentials c
      JOIN public.principals p ON p.tenant_id = c.tenant_id AND p.id = c.principal_id
      JOIN public.apps a ON a.tenant_id = c.tenant_id AND a.id = c.app_id
      WHERE c.tenant_id = p_tenant_id
        AND c.id = p_credential_id
        AND c.token_digest = decode(nullif(current_setting('app.credential_digest', true), ''), 'hex')
        AND c.credential_class = 'trusted_writer'
        AND p_operation IN ('create', 'correct', 'forget')
        AND p_operation = ANY(c.allowed_operations)
        AND c.allowed_operations <@ ARRAY['create', 'correct', 'forget']::text[]
        AND c.issued_at <= clock_timestamp()
        AND c.expires_at > clock_timestamp()
        AND c.revoked_at IS NULL
        AND p.active
        AND a.active
    )
$$;

DROP FUNCTION IF EXISTS resolve_current_writer();
CREATE OR REPLACE FUNCTION resolve_current_writer(p_operation text)
RETURNS TABLE (tenant_id text, credential_id text)
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
  SELECT c.tenant_id, c.id
  FROM public.credentials c
  JOIN public.principals p ON p.tenant_id = c.tenant_id AND p.id = c.principal_id
  JOIN public.apps a ON a.tenant_id = c.tenant_id AND a.id = c.app_id
  WHERE c.token_digest = decode(nullif(current_setting('app.credential_digest', true), ''), 'hex')
    AND c.credential_class = 'trusted_writer'
    AND p_operation IN ('create', 'correct', 'forget')
    AND p_operation = ANY(c.allowed_operations)
    AND c.allowed_operations <@ ARRAY['create', 'correct', 'forget']::text[]
    AND c.issued_at <= clock_timestamp()
    AND c.expires_at > clock_timestamp()
    AND c.revoked_at IS NULL
    AND p.active
    AND a.active
$$;

DROP FUNCTION IF EXISTS lock_tenant_authority(text);
CREATE FUNCTION lock_tenant_authority(p_tenant_id text)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
BEGIN
  IF public.current_reader_context(p_tenant_id) IS NOT TRUE THEN
    RAISE EXCEPTION 'invalid reader authority context' USING ERRCODE = '42501';
  END IF;
  PERFORM 1 FROM public.tenant_authority WHERE tenant_id = p_tenant_id FOR SHARE;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'unavailable authority context' USING ERRCODE = '42501';
  END IF;
END
$$;

DROP FUNCTION IF EXISTS record_read_rejection(text, text);
CREATE OR REPLACE FUNCTION record_request_rejection(
  p_request_id text,
  p_operation text,
  p_outcome text
)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
BEGIN
  IF (
    p_request_id IS NOT NULL
    AND p_request_id ~ '^[0-9a-f-]{36}$'
    AND p_outcome IN ('malformed', 'unauthenticated', 'unavailable', 'storage_unavailable', 'stale_context', 'idempotency_conflict', 'unsupported_time_semantics')
    AND p_operation IN ('read', 'search', 'list', 'create', 'correct', 'forget')
    AND (p_operation <> 'read' OR p_outcome <> 'unavailable')
  ) IS NOT TRUE THEN
    RAISE EXCEPTION 'invalid rejection audit input' USING ERRCODE = '22023';
  END IF;
  IF p_operation IN ('read', 'search', 'list') THEN
    INSERT INTO public.read_audit
      (request_id, tenant_id, principal_id, app_id, credential_id, operation, target_id, outcome, authority_epoch)
    SELECT p_request_id, c.tenant_id, c.principal_id, c.app_id, c.id,
           p_operation, NULL, p_outcome, NULL
    FROM (VALUES (1)) AS singleton(n)
    LEFT JOIN public.credentials c
      ON c.token_digest = decode(nullif(current_setting('app.credential_digest', true), ''), 'hex');
  ELSE
    INSERT INTO public.create_rejection_audit
      (request_id, tenant_id, principal_id, app_id, credential_id, operation, outcome,
       authority_epoch, idempotency_key_digest)
    SELECT p_request_id, c.tenant_id, c.principal_id, c.app_id, c.id,
           p_operation, p_outcome, t.authority_epoch,
           decode(nullif(current_setting('app.idempotency_key_digest', true), ''), 'hex')
    FROM (VALUES (1)) AS singleton(n)
    LEFT JOIN public.credentials c
      ON c.token_digest = decode(nullif(current_setting('app.credential_digest', true), ''), 'hex')
    LEFT JOIN public.tenant_authority t ON t.tenant_id = c.tenant_id;
  END IF;
END
$$;

CREATE OR REPLACE FUNCTION record_read_audit(p_request_id text, p_outcome text, p_target_id text)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
BEGIN
  IF p_request_id !~ '^[0-9a-f-]{36}$' OR p_outcome NOT IN ('released', 'unavailable') THEN
    RAISE EXCEPTION 'invalid read audit input' USING ERRCODE = '22023';
  END IF;
  IF nullif(current_setting('app.operation', true), '') IS DISTINCT FROM 'read' THEN
    RAISE EXCEPTION 'invalid read operation context' USING ERRCODE = '42501';
  END IF;
  IF public.current_reader_context(nullif(current_setting('app.tenant_id', true), '')) IS NOT TRUE THEN
    RAISE EXCEPTION 'invalid reader authority context' USING ERRCODE = '42501';
  END IF;
  INSERT INTO public.read_audit
    (request_id, tenant_id, principal_id, app_id, credential_id, operation, target_id, outcome, authority_epoch)
  SELECT p_request_id, c.tenant_id, c.principal_id, c.app_id, c.id, 'read',
         CASE WHEN p_outcome = 'released' THEN p_target_id END, p_outcome, t.authority_epoch
  FROM public.credentials c
  JOIN public.tenant_authority t ON t.tenant_id = c.tenant_id
  WHERE c.tenant_id = current_setting('app.tenant_id', true)
    AND c.token_digest = decode(current_setting('app.credential_digest', true), 'hex');
  IF NOT FOUND THEN
    RAISE EXCEPTION 'unavailable read audit context' USING ERRCODE = '42501';
  END IF;
END
$$;

DROP FUNCTION IF EXISTS read_current_item(text, text, text);
DROP FUNCTION IF EXISTS read_current_item(text, text, text, text);
DROP FUNCTION IF EXISTS read_current_item(text, text, text, text, text);
DROP FUNCTION IF EXISTS read_current_item(text, text, text, text, text, text[]);
CREATE FUNCTION read_current_item(
  p_tenant_id text,
  p_credential_id text,
  p_request_id text,
  p_item_id text,
  p_expected_revision_id text,
  p_scope_subject_ids text[]
)
RETURNS TABLE (item_id text, revision_id text, content text, recorded_at text,
               valid_from text, valid_until text, validity_status text)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
DECLARE
  v_principal_id text;
  v_app_id text;
  v_authority_epoch bigint;
BEGIN
  IF (
    p_tenant_id IS NOT NULL
    AND p_credential_id IS NOT NULL
    AND p_request_id IS NOT NULL
    AND p_request_id ~ '^[0-9a-f-]{36}$'
    AND p_item_id IS NOT NULL
    AND p_item_id ~ '^[A-Za-z0-9_-]{1,64}$'
    AND (p_expected_revision_id IS NULL OR p_expected_revision_id ~ '^[A-Za-z0-9_-]{1,64}$')
    AND nullif(current_setting('app.operation', true), '') = 'read'
  ) IS NOT TRUE THEN
    RAISE EXCEPTION 'invalid read authority context' USING ERRCODE = '42501';
  END IF;
  SELECT t.authority_epoch INTO v_authority_epoch
  FROM public.tenant_authority t
  WHERE t.tenant_id = p_tenant_id
  FOR SHARE;
  IF NOT FOUND OR public.current_reader_context(p_tenant_id) IS NOT TRUE THEN
    RAISE EXCEPTION 'unavailable read authority context' USING ERRCODE = '42501';
  END IF;
  SELECT c.principal_id, c.app_id INTO v_principal_id, v_app_id
  FROM public.credentials c
  JOIN public.principals p ON p.tenant_id = c.tenant_id AND p.id = c.principal_id
  JOIN public.apps a ON a.tenant_id = c.tenant_id AND a.id = c.app_id
  WHERE c.tenant_id = p_tenant_id
    AND c.id = p_credential_id
    AND c.token_digest = decode(current_setting('app.credential_digest', true), 'hex')
    AND c.credential_class = 'agent_reader'
    AND 'read' = ANY(c.allowed_operations)
    AND c.allowed_operations <@ ARRAY['list', 'search', 'read']::text[]
    AND c.issued_at <= clock_timestamp()
    AND c.expires_at > clock_timestamp()
    AND c.revoked_at IS NULL
    AND p.active AND a.active;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'invalid read credential' USING ERRCODE = '42501';
  END IF;
  IF p_scope_subject_ids IS NOT NULL AND (
       cardinality(p_scope_subject_ids) NOT BETWEEN 1 AND 8
       OR cardinality(p_scope_subject_ids) <>
          (SELECT count(DISTINCT subject_id) FROM unnest(p_scope_subject_ids) AS subject_id)
       OR EXISTS (
         SELECT 1 FROM unnest(p_scope_subject_ids) AS subject_id
         WHERE subject_id !~ '^[A-Za-z0-9_-]{1,64}$'
       )
     ) THEN
    RAISE EXCEPTION 'invalid read scope' USING ERRCODE = '22023';
  END IF;
  IF p_scope_subject_ids IS NOT NULL AND (
       SELECT count(*) FROM public.subjects s
       WHERE s.tenant_id = p_tenant_id AND s.app_id = v_app_id
         AND s.kind IN ('customer', 'project')
         AND s.id = ANY(p_scope_subject_ids)
     ) <> cardinality(p_scope_subject_ids) THEN
    RAISE EXCEPTION 'invalid read scope' USING ERRCODE = '22023';
  END IF;

  SELECT i.id, r.id, r.content,
         to_char(r.recorded_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
         CASE WHEN r.valid_from IS NULL THEN NULL ELSE
           to_char(r.valid_from AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') END,
         CASE WHEN r.valid_until IS NULL THEN NULL ELSE
           to_char(r.valid_until AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') END,
         CASE WHEN r.valid_from IS NULL AND r.valid_until IS NULL THEN 'unknown' ELSE 'known' END
  INTO item_id, revision_id, content, recorded_at, valid_from, valid_until, validity_status
  FROM public.items i
  JOIN public.collections c ON c.tenant_id = i.tenant_id AND c.id = i.collection_id
  JOIN public.revisions r
    ON r.tenant_id = i.tenant_id AND r.item_id = i.id AND r.id = i.active_revision_id
  WHERE i.tenant_id = p_tenant_id AND i.id = p_item_id AND c.app_id = v_app_id
    AND i.deleted_at IS NULL AND c.withdrawn_at IS NULL
    AND (r.valid_from IS NULL OR r.valid_from <= clock_timestamp())
    AND (r.valid_until IS NULL OR clock_timestamp() < r.valid_until)
    AND (
      (c.audience_kind = 'private' AND c.owner_principal_id = v_principal_id)
      OR (
        c.audience_kind = 'restricted'
        AND EXISTS (
          SELECT 1 FROM public.collection_grants g
          WHERE g.tenant_id = c.tenant_id AND g.collection_id = c.id
            AND g.principal_id = v_principal_id AND g.can_read
        )
      )
    )
    AND NOT EXISTS (
      SELECT s.kind
      FROM public.revision_subjects rs
      JOIN public.subjects s ON s.tenant_id = rs.tenant_id AND s.id = rs.subject_id
      WHERE rs.tenant_id = r.tenant_id AND rs.item_id = r.item_id
        AND rs.revision_id = r.id
      GROUP BY s.kind
      HAVING NOT bool_or(
        (s.app_id = v_app_id AND s.kind = 'principal' AND s.principal_id = v_principal_id)
        OR (s.kind = 'app' AND s.app_id = v_app_id)
        OR (s.app_id = v_app_id AND s.kind IN ('customer', 'project')
            AND p_scope_subject_ids IS NOT NULL AND s.id = ANY(p_scope_subject_ids))
      )
    );
  IF NOT FOUND THEN
    INSERT INTO public.read_audit
      (request_id, tenant_id, principal_id, app_id, credential_id, operation,
       target_id, outcome, authority_epoch)
    VALUES
      (p_request_id, p_tenant_id, v_principal_id, v_app_id, p_credential_id,
       'read', NULL, 'unavailable', v_authority_epoch);
    RETURN;
  END IF;
  IF p_expected_revision_id IS NOT NULL
     AND p_expected_revision_id IS DISTINCT FROM revision_id THEN
    RAISE EXCEPTION 'stale current revision' USING ERRCODE = 'P0003';
  END IF;
  INSERT INTO public.read_audit
    (request_id, tenant_id, principal_id, app_id, credential_id, operation,
     target_id, outcome, authority_epoch)
  VALUES
    (p_request_id, p_tenant_id, v_principal_id, v_app_id, p_credential_id,
     'read', p_item_id, 'released', v_authority_epoch);
  RETURN NEXT;
END
$$;

DROP FUNCTION IF EXISTS activate_document_extraction(
  text, text, text, text, bytea, text, text, text, text,
  text[], text[], text[], text[], text[]
);
CREATE FUNCTION activate_document_extraction(
  p_tenant_id text,
  p_item_id text,
  p_revision_id text,
  p_source_revision_id text,
  p_source_sha256 bytea,
  p_extraction_set_id text,
  p_parser_id text,
  p_parser_version text,
  p_config_version text,
  p_passage_ids text[],
  p_structural_parent_ids text[],
  p_continuation_directions text[],
  p_locators text[],
  p_contents text[]
)
RETURNS text
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
DECLARE
  v_passage_count integer;
  v_content_bytes integer;
  v_existing_source_revision_id text;
  v_existing_source_sha256 bytea;
  v_index integer;
BEGIN
  v_passage_count := cardinality(p_passage_ids);
  IF (
    p_tenant_id ~ '^[A-Za-z0-9_-]{1,64}$'
    AND p_item_id ~ '^[A-Za-z0-9_-]{1,64}$'
    AND p_revision_id ~ '^[A-Za-z0-9_-]{1,64}$'
    AND p_source_revision_id ~ '^[A-Za-z0-9_-]{1,64}$'
    AND octet_length(p_source_sha256) = 32
    AND p_extraction_set_id ~ '^[A-Za-z0-9_-]{1,64}$'
    AND octet_length(p_parser_id) BETWEEN 1 AND 128 AND p_parser_id ~ '[^[:space:]]'
    AND octet_length(p_parser_version) BETWEEN 1 AND 128 AND p_parser_version ~ '[^[:space:]]'
    AND octet_length(p_config_version) BETWEEN 1 AND 128 AND p_config_version ~ '[^[:space:]]'
    AND v_passage_count BETWEEN 1 AND 1024
    AND array_lower(p_passage_ids, 1) = 1
    AND array_lower(p_structural_parent_ids, 1) = 1
    AND array_lower(p_continuation_directions, 1) = 1
    AND array_lower(p_locators, 1) = 1
    AND array_lower(p_contents, 1) = 1
    AND cardinality(p_structural_parent_ids) = v_passage_count
    AND cardinality(p_continuation_directions) = v_passage_count
    AND cardinality(p_locators) = v_passage_count
    AND cardinality(p_contents) = v_passage_count
  ) IS NOT TRUE THEN
    RAISE EXCEPTION 'invalid document extraction envelope' USING ERRCODE = '22023';
  END IF;
  IF EXISTS (
    SELECT 1 FROM unnest(p_passage_ids) AS passage_id
    WHERE passage_id IS NULL OR passage_id !~ '^[A-Za-z0-9_-]{1,64}$'
  ) OR v_passage_count <> (SELECT count(DISTINCT passage_id) FROM unnest(p_passage_ids) AS passage_id)
     OR EXISTS (
       SELECT 1 FROM unnest(p_structural_parent_ids) AS structural_parent_id
       WHERE structural_parent_id IS NULL
         OR structural_parent_id !~ '^[A-Za-z0-9_-]{1,64}$'
     )
     OR EXISTS (
       SELECT 1 FROM unnest(p_continuation_directions) AS direction
       WHERE direction IS NULL OR direction NOT IN ('none', 'from_previous', 'to_next', 'both')
     )
     OR EXISTS (
       SELECT 1 FROM unnest(p_locators) AS locator
       WHERE locator IS NULL OR jsonb_typeof(locator::jsonb) <> 'object'
         OR octet_length(locator) NOT BETWEEN 2 AND 2048
     )
     OR v_passage_count <> (SELECT count(DISTINCT locator) FROM unnest(p_locators) AS locator)
     OR EXISTS (
       SELECT 1 FROM unnest(p_contents) AS content
       WHERE content IS NULL OR octet_length(content) NOT BETWEEN 1 AND 32768
     )
     OR EXISTS (
       SELECT 1 FROM generate_subscripts(p_passage_ids, 1) AS edge(index)
       WHERE (
           p_continuation_directions[edge.index] IN ('from_previous', 'both')
           AND (
             edge.index = 1
             OR p_structural_parent_ids[edge.index - 1]
                IS DISTINCT FROM p_structural_parent_ids[edge.index]
             OR p_continuation_directions[edge.index - 1] NOT IN ('to_next', 'both')
           )
         ) OR (
           p_continuation_directions[edge.index] IN ('to_next', 'both')
           AND (
             edge.index = v_passage_count
             OR p_structural_parent_ids[edge.index + 1]
                IS DISTINCT FROM p_structural_parent_ids[edge.index]
             OR p_continuation_directions[edge.index + 1] NOT IN ('from_previous', 'both')
           )
         )
     ) THEN
    RAISE EXCEPTION 'invalid document source passage' USING ERRCODE = '22023';
  END IF;
  SELECT sum(octet_length(content))::integer INTO v_content_bytes
  FROM unnest(p_contents) AS content;
  IF v_content_bytes NOT BETWEEN 1 AND 2097152 THEN
    RAISE EXCEPTION 'document extraction content budget exceeded' USING ERRCODE = '22023';
  END IF;

  PERFORM 1 FROM public.tenant_authority
  WHERE tenant_id = p_tenant_id
  FOR UPDATE;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'document tenant unavailable' USING ERRCODE = 'P0002';
  END IF;
  PERFORM 1
  FROM public.items i
  JOIN public.collections c
    ON c.tenant_id = i.tenant_id AND c.id = i.collection_id
  WHERE i.tenant_id = p_tenant_id
    AND i.id = p_item_id
    AND i.active_revision_id = p_revision_id
    AND i.deleted_at IS NULL
    AND c.withdrawn_at IS NULL
  FOR UPDATE OF i;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'document revision unavailable' USING ERRCODE = 'P0002';
  END IF;

  SELECT s.id, s.source_sha256
  INTO v_existing_source_revision_id, v_existing_source_sha256
  FROM public.source_revisions s
  WHERE s.tenant_id = p_tenant_id
    AND s.item_id = p_item_id
    AND s.revision_id = p_revision_id;
  IF FOUND AND (
    v_existing_source_revision_id IS DISTINCT FROM p_source_revision_id
    OR v_existing_source_sha256 IS DISTINCT FROM p_source_sha256
  ) THEN
    RAISE EXCEPTION 'source revision identity mismatch' USING ERRCODE = '22023';
  ELSIF NOT FOUND THEN
    INSERT INTO public.source_revisions
      (tenant_id, item_id, revision_id, id, source_sha256)
    VALUES
      (p_tenant_id, p_item_id, p_revision_id, p_source_revision_id, p_source_sha256);
  END IF;

  INSERT INTO public.extraction_sets
    (tenant_id, item_id, revision_id, source_revision_id, id,
     parser_id, parser_version, config_version, search_recipe_version,
     passage_count, content_bytes)
  VALUES
    (p_tenant_id, p_item_id, p_revision_id, p_source_revision_id, p_extraction_set_id,
     p_parser_id, p_parser_version, p_config_version, 'source-structure-v1',
     v_passage_count, v_content_bytes);

  FOR v_index IN 1..v_passage_count LOOP
    INSERT INTO public.source_passages
      (tenant_id, item_id, revision_id, source_revision_id, extraction_set_id, id,
       structural_parent_id, passage_order, continuation_direction, locator, content,
       search_document)
    VALUES
      (p_tenant_id, p_item_id, p_revision_id, p_source_revision_id,
       p_extraction_set_id, p_passage_ids[v_index], p_structural_parent_ids[v_index],
       v_index, p_continuation_directions[v_index], p_locators[v_index]::jsonb,
       p_contents[v_index],
       public.source_search_document_v1(
         p_contents[v_index], p_locators[v_index]::jsonb
       ));
  END LOOP;

  INSERT INTO public.active_extraction_sets
    (tenant_id, item_id, revision_id, source_revision_id, extraction_set_id)
  VALUES
    (p_tenant_id, p_item_id, p_revision_id, p_source_revision_id, p_extraction_set_id)
  ON CONFLICT (tenant_id, item_id, revision_id) DO UPDATE
    SET source_revision_id = EXCLUDED.source_revision_id,
        extraction_set_id = EXCLUDED.extraction_set_id,
        activated_at = clock_timestamp();
  UPDATE public.tenant_authority
  SET authority_epoch = authority_epoch + 1
  WHERE tenant_id = p_tenant_id;
  RETURN p_extraction_set_id;
END
$$;

DROP FUNCTION IF EXISTS eligible_source_passages(
  text, text, text, text, text, text, text, text[], text[]
);
CREATE FUNCTION eligible_source_passages(
  p_tenant_id text,
  p_credential_id text,
  p_request_id text,
  p_item_id text,
  p_revision_id text,
  p_expected_source_revision_id text,
  p_expected_extraction_set_id text,
  p_scope_subject_ids text[],
  p_passage_ids text[]
)
RETURNS TABLE (
  source_revision_id text,
  extraction_set_id text,
  parser_id text,
  parser_version text,
  config_version text,
  passage_id text,
  structural_parent_id text,
  passage_order integer,
  continuation_direction text,
  locator jsonb,
  content text
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
DECLARE
  v_active_source_revision_id text;
  v_active_extraction_set_id text;
BEGIN
  PERFORM 1 FROM public.read_current_item(
    p_tenant_id, p_credential_id, p_request_id, p_item_id,
    p_revision_id, p_scope_subject_ids
  );
  IF NOT FOUND THEN
    RETURN;
  END IF;
  SELECT a.source_revision_id, a.extraction_set_id
  INTO v_active_source_revision_id, v_active_extraction_set_id
  FROM public.active_extraction_sets a
  WHERE a.tenant_id = p_tenant_id
    AND a.item_id = p_item_id
    AND a.revision_id = p_revision_id;
  IF NOT FOUND THEN
    RETURN;
  END IF;
  IF v_active_source_revision_id IS DISTINCT FROM p_expected_source_revision_id
     OR v_active_extraction_set_id IS DISTINCT FROM p_expected_extraction_set_id THEN
    RAISE EXCEPTION 'source extraction revision changed' USING ERRCODE = 'P0003';
  END IF;
  IF (cardinality(p_passage_ids) BETWEEN 1 AND 16) IS NOT TRUE
     OR cardinality(p_passage_ids) <> (
       SELECT count(DISTINCT selected.value)
       FROM unnest(p_passage_ids) AS selected(value)
     )
     OR EXISTS (
       SELECT 1 FROM unnest(p_passage_ids) AS selected(value)
       WHERE selected.value IS NULL OR selected.value !~ '^[A-Za-z0-9_-]{1,64}$'
     ) THEN
    RAISE EXCEPTION 'invalid source passage selector' USING ERRCODE = '22023';
  END IF;
  RETURN QUERY
  SELECT a.source_revision_id, a.extraction_set_id,
         e.parser_id, e.parser_version, e.config_version,
         p.id, p.structural_parent_id, p.passage_order,
         p.continuation_direction, p.locator, p.content
  FROM public.active_extraction_sets a
  JOIN public.extraction_sets e
    ON e.tenant_id = a.tenant_id
   AND e.item_id = a.item_id
   AND e.revision_id = a.revision_id
   AND e.source_revision_id = a.source_revision_id
   AND e.id = a.extraction_set_id
  JOIN public.source_passages p
    ON p.tenant_id = e.tenant_id
   AND p.item_id = e.item_id
   AND p.revision_id = e.revision_id
   AND p.source_revision_id = e.source_revision_id
   AND p.extraction_set_id = e.id
  WHERE a.tenant_id = p_tenant_id
    AND a.item_id = p_item_id
    AND a.revision_id = p_revision_id
    AND a.source_revision_id = p_expected_source_revision_id
    AND a.extraction_set_id = p_expected_extraction_set_id
    AND p.id = ANY(p_passage_ids)
    AND e.passage_count = (
      SELECT count(*) FROM public.source_passages complete
      WHERE complete.tenant_id = e.tenant_id
        AND complete.item_id = e.item_id
        AND complete.revision_id = e.revision_id
        AND complete.source_revision_id = e.source_revision_id
        AND complete.extraction_set_id = e.id
    )
    AND e.content_bytes = (
      SELECT sum(octet_length(complete.content))::integer
      FROM public.source_passages complete
      WHERE complete.tenant_id = e.tenant_id
        AND complete.item_id = e.item_id
        AND complete.revision_id = e.revision_id
        AND complete.source_revision_id = e.source_revision_id
        AND complete.extraction_set_id = e.id
    )
  ORDER BY p.passage_order;
END
$$;

DROP FUNCTION IF EXISTS search_current_memories(text, text, text, text);
DROP FUNCTION IF EXISTS search_current_memories(text, text, text, text, text[]);
DROP FUNCTION IF EXISTS search_current_memories(text, text, text, text, integer, text[]);
CREATE OR REPLACE FUNCTION lexical_query_bounded_question_v1(p_query text)
RETURNS tsquery
LANGUAGE plpgsql
IMMUTABLE
STRICT
SET search_path = pg_catalog, public
AS $$
DECLARE
  v_token text;
  v_word text;
  v_terms text[] := ARRAY[]::text[];
  v_tokens text[];
  v_prepared text;
BEGIN
  IF octet_length(p_query) NOT BETWEEN 1 AND 4096 OR p_query !~ '[^[:space:]]' THEN
    RETURN NULL;
  END IF;
  IF p_query ~ '"'
     OR p_query ~* '(^|[^[:alnum:]_])OR([^[:alnum:]_]|$)'
     OR p_query ~ '(^|[^[:alnum:]_])-[[:space:]]*[^[:space:]]' THEN
    RETURN websearch_to_tsquery('simple', p_query);
  END IF;
  v_tokens := regexp_split_to_array(btrim(p_query), '[[:space:]]+');
  IF cardinality(v_tokens) > 64 THEN
    RETURN websearch_to_tsquery('simple', p_query);
  END IF;
  v_prepared := regexp_replace(
    p_query,
    '^[[:space:]]*(please[[:space:]]+)?(what|which|who|whom|whose|when|where|why|how)[[:space:]]+(is|are|was|were|do|does|did|can|could|would|should)[[:space:]]+(the[[:space:]]+)?',
    '',
    'i'
  );
  IF v_prepared = p_query THEN
    v_prepared := regexp_replace(
      p_query,
      '^[[:space:]]*(please[[:space:]]+)?(can|could|would|should)[[:space:]]+(you|i|we)[[:space:]]+(tell[[:space:]]+me[[:space:]]+)?',
      '',
      'i'
    );
  END IF;
  IF v_prepared = p_query THEN
    v_prepared := regexp_replace(
      p_query,
      '^[[:space:]]*(please[[:space:]]+)?tell[[:space:]]+me([[:space:]]+about)?[[:space:]]+',
      '',
      'i'
    );
  END IF;
  IF v_prepared = p_query THEN
    RETURN websearch_to_tsquery('simple', p_query);
  END IF;
  IF btrim(v_prepared) = '' THEN
    RETURN NULL;
  END IF;
  FOREACH v_token IN ARRAY regexp_split_to_array(btrim(v_prepared), '[[:space:]]+') LOOP
    v_word := lower(regexp_replace(v_token, '^[^[:alnum:]]+|[^[:alnum:]]+$', '', 'g'));
    IF v_token <> lower(v_token)
       OR v_word <> ALL (ARRAY['a','an','the','about','of','on','for','in','to','please']) THEN
      v_terms := array_append(v_terms, v_token);
    END IF;
  END LOOP;
  IF cardinality(v_terms) = 0 THEN
    RETURN NULL;
  END IF;
  IF cardinality(v_terms) > 32 THEN
    RETURN websearch_to_tsquery('simple', p_query);
  END IF;
  RETURN websearch_to_tsquery('simple', array_to_string(v_terms, ' '));
END
$$;
CREATE FUNCTION search_current_memories(
  p_tenant_id text,
  p_credential_id text,
  p_request_id text,
  p_query text,
  p_max_context_bytes integer,
  p_scope_subject_ids text[]
)
RETURNS TABLE (item_id text, revision_id text, content text, recorded_at text,
               valid_from text, valid_until text, validity_status text,
               hit_kind text, source_revision_id text, extraction_set_id text,
               passage_id text, locator text, parent_passage_ids text[],
               candidate_omitted boolean)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
DECLARE
  v_principal_id text;
  v_app_id text;
  v_authority_epoch bigint;
BEGIN
  IF (
    p_tenant_id IS NOT NULL
    AND p_credential_id IS NOT NULL
    AND p_request_id IS NOT NULL
    AND p_request_id ~ '^[0-9a-f-]{36}$'
    AND p_query IS NOT NULL
    AND octet_length(p_query) BETWEEN 1 AND 4096
    AND p_query ~ '[^[:space:]]'
    AND p_max_context_bytes BETWEEN 1 AND 4096
    AND nullif(current_setting('app.operation', true), '') = 'search'
  ) IS NOT TRUE THEN
    RAISE EXCEPTION 'invalid search authority context' USING ERRCODE = '42501';
  END IF;
  SELECT t.authority_epoch INTO v_authority_epoch
  FROM public.tenant_authority t
  WHERE t.tenant_id = p_tenant_id
  FOR SHARE;
  IF NOT FOUND OR public.current_reader_context(p_tenant_id) IS NOT TRUE THEN
    RAISE EXCEPTION 'unavailable search authority context' USING ERRCODE = '42501';
  END IF;
  SELECT c.principal_id, c.app_id INTO v_principal_id, v_app_id
  FROM public.credentials c
  JOIN public.principals p ON p.tenant_id = c.tenant_id AND p.id = c.principal_id
  JOIN public.apps a ON a.tenant_id = c.tenant_id AND a.id = c.app_id
  WHERE c.tenant_id = p_tenant_id
    AND c.id = p_credential_id
    AND c.token_digest = decode(current_setting('app.credential_digest', true), 'hex')
    AND c.credential_class = 'agent_reader'
    AND 'search' = ANY(c.allowed_operations)
    AND c.allowed_operations <@ ARRAY['list', 'search', 'read']::text[]
    AND c.issued_at <= clock_timestamp()
    AND c.expires_at > clock_timestamp()
    AND c.revoked_at IS NULL
    AND p.active AND a.active;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'invalid search credential' USING ERRCODE = '42501';
  END IF;
  IF p_scope_subject_ids IS NOT NULL AND (
       cardinality(p_scope_subject_ids) NOT BETWEEN 1 AND 8
       OR cardinality(p_scope_subject_ids) <>
          (SELECT count(DISTINCT subject_id) FROM unnest(p_scope_subject_ids) AS subject_id)
       OR EXISTS (
         SELECT 1 FROM unnest(p_scope_subject_ids) AS subject_id
         WHERE subject_id !~ '^[A-Za-z0-9_-]{1,64}$'
       )
     ) THEN
    RAISE EXCEPTION 'invalid search scope' USING ERRCODE = '22023';
  END IF;
  IF p_scope_subject_ids IS NOT NULL AND (
       SELECT count(*) FROM public.subjects s
       WHERE s.tenant_id = p_tenant_id AND s.app_id = v_app_id
         AND s.kind IN ('customer', 'project')
         AND s.id = ANY(p_scope_subject_ids)
     ) <> cardinality(p_scope_subject_ids) THEN
    RAISE EXCEPTION 'invalid search scope' USING ERRCODE = '22023';
  END IF;

  RETURN QUERY
  WITH query_plan AS (
    SELECT public.lexical_query_bounded_question_v1(p_query) AS query
  ),
  authorized_revisions AS (
    SELECT i.tenant_id, i.id AS item_id, r.id AS revision_id, r.content,
           to_char(r.recorded_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') AS recorded_at,
           CASE WHEN r.valid_from IS NULL THEN NULL ELSE
             to_char(r.valid_from AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') END AS valid_from,
           CASE WHEN r.valid_until IS NULL THEN NULL ELSE
             to_char(r.valid_until AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') END AS valid_until,
           CASE WHEN r.valid_from IS NULL AND r.valid_until IS NULL THEN 'unknown' ELSE 'known' END AS validity_status,
           l.document
    FROM public.items i
    JOIN public.collections c ON c.tenant_id = i.tenant_id AND c.id = i.collection_id
    JOIN public.revisions r
      ON r.tenant_id = i.tenant_id AND r.item_id = i.id AND r.id = i.active_revision_id
    LEFT JOIN public.lexical_representations l
      ON l.tenant_id = r.tenant_id AND l.item_id = r.item_id AND l.revision_id = r.id
    WHERE i.tenant_id = p_tenant_id AND c.app_id = v_app_id
      AND i.deleted_at IS NULL AND c.withdrawn_at IS NULL
      AND (r.valid_from IS NULL OR r.valid_from <= clock_timestamp())
      AND (r.valid_until IS NULL OR clock_timestamp() < r.valid_until)
      AND (
        (c.audience_kind = 'private' AND c.owner_principal_id = v_principal_id)
        OR (
          c.audience_kind = 'restricted'
          AND EXISTS (
            SELECT 1 FROM public.collection_grants g
            WHERE g.tenant_id = c.tenant_id AND g.collection_id = c.id
              AND g.principal_id = v_principal_id AND g.can_read
          )
        )
      )
      AND NOT EXISTS (
        SELECT s.kind
        FROM public.revision_subjects rs
        JOIN public.subjects s ON s.tenant_id = rs.tenant_id AND s.id = rs.subject_id
        WHERE rs.tenant_id = r.tenant_id AND rs.item_id = r.item_id
          AND rs.revision_id = r.id
        GROUP BY s.kind
        HAVING NOT bool_or(
          (s.app_id = v_app_id AND s.kind = 'principal' AND s.principal_id = v_principal_id)
          OR (s.kind = 'app' AND s.app_id = v_app_id)
          OR (s.app_id = v_app_id AND s.kind IN ('customer', 'project')
              AND p_scope_subject_ids IS NOT NULL AND s.id = ANY(p_scope_subject_ids))
        )
      )
  ),
  memory_candidates AS (
    SELECT a.item_id, a.revision_id, a.content, a.recorded_at, a.valid_from,
           a.valid_until, a.validity_status, 'memory'::text AS hit_kind,
           NULL::text AS source_revision_id, NULL::text AS extraction_set_id,
           NULL::text AS passage_id, NULL::text AS locator,
           ts_rank(a.document, query_plan.query) AS relevance,
           NULL::integer AS passage_order, true AS fits_global_budget
    FROM authorized_revisions a
    CROSS JOIN query_plan
    WHERE query_plan.query IS NOT NULL AND a.document @@ query_plan.query
      AND NOT EXISTS (
        SELECT 1 FROM public.active_extraction_sets active
        WHERE active.tenant_id = a.tenant_id AND active.item_id = a.item_id
          AND active.revision_id = a.revision_id
      )
  ),
  complete_active_sets AS (
    SELECT active.tenant_id, active.item_id, active.revision_id,
           active.source_revision_id, active.extraction_set_id
    FROM authorized_revisions authorized
    JOIN public.active_extraction_sets active
      ON active.tenant_id = authorized.tenant_id
     AND active.item_id = authorized.item_id
     AND active.revision_id = authorized.revision_id
    JOIN public.extraction_sets e
      ON e.tenant_id = active.tenant_id AND e.item_id = active.item_id
     AND e.revision_id = active.revision_id
     AND e.source_revision_id = active.source_revision_id
     AND e.id = active.extraction_set_id
    JOIN public.source_passages complete
      ON complete.tenant_id = e.tenant_id AND complete.item_id = e.item_id
     AND complete.revision_id = e.revision_id
     AND complete.source_revision_id = e.source_revision_id
     AND complete.extraction_set_id = e.id
    GROUP BY active.tenant_id, active.item_id, active.revision_id,
             active.source_revision_id, active.extraction_set_id,
             e.passage_count, e.content_bytes
    HAVING count(*) = e.passage_count
       AND sum(octet_length(complete.content)) = e.content_bytes
  ),
  passage_spans AS (
    SELECT a.item_id, a.revision_id, p.content, a.recorded_at, a.valid_from,
           a.valid_until, a.validity_status, 'passage'::text AS hit_kind,
           p.source_revision_id, p.extraction_set_id, p.id AS passage_id,
           p.locator::text,
           ts_rank(p.search_document, query_plan.query) AS relevance,
           p.passage_order,
           octet_length(p.content) <= p_max_context_bytes AS fits_global_budget
    FROM authorized_revisions a
    JOIN complete_active_sets active
      ON active.tenant_id = a.tenant_id AND active.item_id = a.item_id
     AND active.revision_id = a.revision_id
    JOIN public.source_passages p
      ON p.tenant_id = active.tenant_id AND p.item_id = active.item_id
     AND p.revision_id = active.revision_id
     AND p.source_revision_id = active.source_revision_id
     AND p.extraction_set_id = active.extraction_set_id
    CROSS JOIN query_plan
    WHERE query_plan.query IS NOT NULL AND p.search_document @@ query_plan.query
  ),
  passage_ranked AS (
    SELECT spans.*,
           count(*) OVER (
             PARTITION BY spans.item_id, spans.revision_id,
                          spans.source_revision_id, spans.extraction_set_id
           ) AS source_match_count,
           row_number() OVER (
             PARTITION BY spans.item_id, spans.revision_id,
                          spans.source_revision_id, spans.extraction_set_id
             ORDER BY spans.fits_global_budget DESC, spans.relevance DESC,
                      spans.passage_order, spans.passage_id
           ) AS source_rank
    FROM passage_spans spans
  ),
  direct_candidates AS (
    SELECT memory.*, false AS source_cap_omitted FROM memory_candidates memory
    UNION ALL
    SELECT passage.item_id, passage.revision_id, passage.content, passage.recorded_at,
           passage.valid_from, passage.valid_until, passage.validity_status,
           passage.hit_kind, passage.source_revision_id, passage.extraction_set_id,
           passage.passage_id, passage.locator, passage.relevance, passage.passage_order,
           passage.fits_global_budget, passage.source_match_count > 16 AS source_cap_omitted
    FROM passage_ranked passage
    WHERE passage.source_rank <= 16
  ),
  direct_ranked AS (
    SELECT candidate.*,
           row_number() OVER (
             ORDER BY candidate.fits_global_budget DESC, candidate.relevance DESC,
                      candidate.item_id, candidate.hit_kind,
                      candidate.passage_order NULLS FIRST,
                      candidate.passage_id NULLS FIRST
           ) AS direct_rank,
           count(*) OVER () AS direct_candidate_count,
           bool_or(candidate.source_cap_omitted) OVER () AS any_source_cap_omitted
    FROM direct_candidates candidate
  ),
  direct_window AS (
    SELECT * FROM direct_ranked WHERE direct_rank <= 41
  ),
  expansion_spans AS (
    SELECT neighbor.item_id, neighbor.revision_id, neighbor.content,
           parent.recorded_at, parent.valid_from, parent.valid_until,
           parent.validity_status, 'adjacent_continuation'::text AS hit_kind,
           neighbor.source_revision_id, neighbor.extraction_set_id,
           neighbor.id AS passage_id, neighbor.locator::text,
           parent.relevance, neighbor.passage_order,
           octet_length(neighbor.content) <= p_max_context_bytes AS fits_global_budget,
           parent.passage_id AS parent_passage_id, parent.direct_rank AS parent_rank
    FROM direct_window parent
    JOIN public.source_passages direct
      ON parent.hit_kind = 'passage'
     AND direct.tenant_id = p_tenant_id AND direct.item_id = parent.item_id
     AND direct.revision_id = parent.revision_id
     AND direct.source_revision_id = parent.source_revision_id
     AND direct.extraction_set_id = parent.extraction_set_id
     AND direct.id = parent.passage_id
    JOIN public.source_passages neighbor
      ON neighbor.tenant_id = direct.tenant_id AND neighbor.item_id = direct.item_id
     AND neighbor.revision_id = direct.revision_id
     AND neighbor.source_revision_id = direct.source_revision_id
     AND neighbor.extraction_set_id = direct.extraction_set_id
     AND neighbor.structural_parent_id = direct.structural_parent_id
     AND (
       (
         neighbor.passage_order = direct.passage_order - 1
         AND direct.continuation_direction IN ('from_previous', 'both')
         AND neighbor.continuation_direction IN ('to_next', 'both')
       ) OR (
         neighbor.passage_order = direct.passage_order + 1
         AND direct.continuation_direction IN ('to_next', 'both')
         AND neighbor.continuation_direction IN ('from_previous', 'both')
       )
     )
    WHERE NOT EXISTS (
      SELECT 1 FROM passage_spans lexical
      WHERE lexical.item_id = neighbor.item_id
        AND lexical.revision_id = neighbor.revision_id
        AND lexical.source_revision_id = neighbor.source_revision_id
        AND lexical.extraction_set_id = neighbor.extraction_set_id
        AND lexical.passage_id = neighbor.id
    )
  ),
  deduplicated_expansions AS (
    SELECT expansion.item_id, expansion.revision_id, expansion.content,
           expansion.recorded_at, expansion.valid_from, expansion.valid_until,
           expansion.validity_status, expansion.hit_kind,
           expansion.source_revision_id, expansion.extraction_set_id,
           expansion.passage_id, expansion.locator, expansion.passage_order,
           expansion.fits_global_budget, min(expansion.parent_rank) AS parent_rank,
           array_agg(expansion.parent_passage_id
                     ORDER BY expansion.parent_rank, expansion.parent_passage_id)
             AS parent_passage_ids
    FROM expansion_spans expansion
    GROUP BY expansion.item_id, expansion.revision_id, expansion.content,
             expansion.recorded_at, expansion.valid_from, expansion.valid_until,
             expansion.validity_status, expansion.hit_kind,
             expansion.source_revision_id, expansion.extraction_set_id,
             expansion.passage_id, expansion.locator, expansion.passage_order,
             expansion.fits_global_budget
  ),
  expansion_window AS (
    SELECT expansion.*,
           row_number() OVER (
             ORDER BY expansion.parent_rank, expansion.passage_order,
                      expansion.passage_id
           ) AS expansion_rank
    FROM deduplicated_expansions expansion
  ),
  omissions AS (
    SELECT coalesce(bool_or(direct.any_source_cap_omitted), false)
             OR coalesce(max(direct.direct_candidate_count), 0) > 41
             OR (SELECT count(*) FROM deduplicated_expansions) > 41 AS omitted
    FROM direct_window direct
  ),
  candidates AS (
    SELECT direct.item_id, direct.revision_id, direct.content, direct.recorded_at,
           direct.valid_from, direct.valid_until, direct.validity_status,
           direct.hit_kind, direct.source_revision_id, direct.extraction_set_id,
           direct.passage_id, direct.locator, NULL::text[] AS parent_passage_ids,
           0 AS phase, direct.direct_rank AS candidate_rank
    FROM direct_window direct
    UNION ALL
    SELECT expansion.item_id, expansion.revision_id, expansion.content,
           expansion.recorded_at, expansion.valid_from, expansion.valid_until,
           expansion.validity_status, expansion.hit_kind,
           expansion.source_revision_id, expansion.extraction_set_id,
           expansion.passage_id, expansion.locator, expansion.parent_passage_ids,
           1 AS phase, expansion.expansion_rank AS candidate_rank
    FROM expansion_window expansion
    WHERE expansion.expansion_rank <= 41
  )
  SELECT candidate.item_id, candidate.revision_id, candidate.content,
         candidate.recorded_at, candidate.valid_from, candidate.valid_until,
         candidate.validity_status, candidate.hit_kind, candidate.source_revision_id,
         candidate.extraction_set_id, candidate.passage_id, candidate.locator,
         candidate.parent_passage_ids, omissions.omitted
  FROM candidates candidate CROSS JOIN omissions
  ORDER BY candidate.phase, candidate.candidate_rank;

  INSERT INTO public.read_audit
    (request_id, tenant_id, principal_id, app_id, credential_id, operation,
     target_id, outcome, authority_epoch)
  VALUES (p_request_id, p_tenant_id, v_principal_id, v_app_id, p_credential_id,
          'search', NULL, 'released', v_authority_epoch);
END
$$;

DROP FUNCTION IF EXISTS list_current_resources(text, text, text, text, integer, text);
CREATE FUNCTION list_current_resources(
  p_tenant_id text,
  p_credential_id text,
  p_request_id text,
  p_resource_kind text,
  p_limit integer,
  p_cursor text
)
RETURNS TABLE (
  resource_id text,
  revision_id text,
  collection_id text,
  audience_kind text,
  recorded_at text,
  valid_from text,
  valid_until text,
  validity_status text,
  next_cursor text
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
DECLARE
  v_principal_id text;
  v_app_id text;
  v_authority_epoch bigint;
  v_after_id text := '';
  v_ids text[] := ARRAY[]::text[];
  v_next_cursor text;
BEGIN
  IF (
    p_tenant_id IS NOT NULL
    AND p_credential_id IS NOT NULL
    AND p_request_id IS NOT NULL
    AND p_request_id ~ '^[0-9a-f-]{36}$'
    AND p_resource_kind IN ('collections', 'items')
    AND p_limit BETWEEN 1 AND 100
    AND (p_cursor IS NULL OR p_cursor ~ '^[0-9a-f]{32}$')
  ) IS NOT TRUE THEN
    RAISE EXCEPTION 'invalid list input' USING ERRCODE = '22023';
  END IF;
  IF nullif(current_setting('app.operation', true), '') IS DISTINCT FROM 'list' THEN
    RAISE EXCEPTION 'invalid list operation context' USING ERRCODE = '42501';
  END IF;

  SELECT t.authority_epoch INTO v_authority_epoch
  FROM public.tenant_authority t
  WHERE t.tenant_id = p_tenant_id
  FOR SHARE;
  IF NOT FOUND OR public.current_reader_context(p_tenant_id) IS NOT TRUE THEN
    RAISE EXCEPTION 'unavailable list authority context' USING ERRCODE = '42501';
  END IF;

  SELECT c.principal_id, c.app_id INTO v_principal_id, v_app_id
  FROM public.credentials c
  JOIN public.principals p ON p.tenant_id = c.tenant_id AND p.id = c.principal_id
  JOIN public.apps a ON a.tenant_id = c.tenant_id AND a.id = c.app_id
  WHERE c.tenant_id = p_tenant_id
    AND c.id = p_credential_id
    AND c.token_digest = decode(current_setting('app.credential_digest', true), 'hex')
    AND c.credential_class = 'agent_reader'
    AND 'list' = ANY(c.allowed_operations)
    AND c.allowed_operations <@ ARRAY['list', 'search', 'read']::text[]
    AND c.issued_at <= clock_timestamp()
    AND c.expires_at > clock_timestamp()
    AND c.revoked_at IS NULL
    AND p.active AND a.active;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'invalid list credential' USING ERRCODE = '42501';
  END IF;

  IF p_cursor IS NOT NULL THEN
    SELECT c.after_id INTO v_after_id
    FROM public.list_cursors c
    WHERE c.token = p_cursor
      AND c.tenant_id = p_tenant_id
      AND c.principal_id = v_principal_id
      AND c.app_id = v_app_id
      AND c.resource_kind = p_resource_kind
      AND c.expires_at > clock_timestamp();
    IF NOT FOUND THEN
      RAISE EXCEPTION 'invalid list cursor' USING ERRCODE = '22023';
    END IF;
  END IF;

  IF p_resource_kind = 'collections' THEN
    SELECT coalesce(array_agg(candidate.id ORDER BY candidate.id), ARRAY[]::text[])
    INTO v_ids
    FROM (
      SELECT c.id
      FROM public.collections c
      WHERE c.tenant_id = p_tenant_id
        AND c.app_id = v_app_id
        AND c.withdrawn_at IS NULL
        AND c.id > v_after_id
        AND (
          (c.audience_kind = 'private' AND c.owner_principal_id = v_principal_id)
          OR (
            c.audience_kind = 'restricted'
            AND EXISTS (
              SELECT 1 FROM public.collection_grants g
              WHERE g.tenant_id = c.tenant_id AND g.collection_id = c.id
                AND g.principal_id = v_principal_id AND g.can_read
            )
          )
        )
      ORDER BY c.id
      LIMIT p_limit + 1
    ) AS candidate;
  ELSE
    SELECT coalesce(array_agg(candidate.id ORDER BY candidate.id), ARRAY[]::text[])
    INTO v_ids
    FROM (
      SELECT i.id
      FROM public.items i
      JOIN public.collections c
        ON c.tenant_id = i.tenant_id AND c.id = i.collection_id
      JOIN public.revisions r
        ON r.tenant_id = i.tenant_id AND r.item_id = i.id AND r.id = i.active_revision_id
      WHERE i.tenant_id = p_tenant_id
        AND c.app_id = v_app_id
        AND i.deleted_at IS NULL
        AND c.withdrawn_at IS NULL
        AND i.id > v_after_id
        AND (r.valid_from IS NULL OR r.valid_from <= clock_timestamp())
        AND (r.valid_until IS NULL OR clock_timestamp() < r.valid_until)
        AND (
          (c.audience_kind = 'private' AND c.owner_principal_id = v_principal_id)
          OR (
            c.audience_kind = 'restricted'
            AND EXISTS (
              SELECT 1 FROM public.collection_grants g
              WHERE g.tenant_id = c.tenant_id AND g.collection_id = c.id
                AND g.principal_id = v_principal_id AND g.can_read
            )
          )
        )
        AND NOT EXISTS (
          SELECT s.kind
          FROM public.revision_subjects rs
          JOIN public.subjects s
            ON s.tenant_id = rs.tenant_id AND s.id = rs.subject_id
          WHERE rs.tenant_id = r.tenant_id AND rs.item_id = r.item_id
            AND rs.revision_id = r.id
          GROUP BY s.kind
          HAVING NOT bool_or(
            (s.app_id = v_app_id AND s.kind = 'principal' AND s.principal_id = v_principal_id)
            OR (s.kind = 'app' AND s.app_id = v_app_id)
          )
        )
      ORDER BY i.id
      LIMIT p_limit + 1
    ) AS candidate;
  END IF;

  IF cardinality(v_ids) > p_limit THEN
    v_ids := v_ids[1:p_limit];
    v_next_cursor := replace(gen_random_uuid()::text, '-', '');
    -- ponytail: One cursor slot serializes concurrent pagination per identity/resource.
    -- Replace this with bounded multi-session cursor state only when parallel traversal is required.
    INSERT INTO public.list_cursors
      (token, tenant_id, principal_id, app_id, resource_kind, after_id, expires_at)
    VALUES
      (v_next_cursor, p_tenant_id, v_principal_id, v_app_id, p_resource_kind,
       v_ids[p_limit], clock_timestamp() + interval '15 minutes')
    ON CONFLICT ON CONSTRAINT list_cursors_active_slot DO UPDATE
      SET token = excluded.token,
          after_id = excluded.after_id,
          expires_at = excluded.expires_at;
  END IF;

  IF p_resource_kind = 'collections' THEN
    RETURN QUERY
    SELECT c.id, NULL::text, c.id, c.audience_kind, NULL::text, NULL::text,
           NULL::text, NULL::text, v_next_cursor
    FROM unnest(v_ids) WITH ORDINALITY AS selected(id, position)
    JOIN public.collections c ON c.tenant_id = p_tenant_id AND c.id = selected.id
    ORDER BY selected.position;
  ELSE
    RETURN QUERY
    SELECT i.id, r.id, c.id, c.audience_kind,
           to_char(r.recorded_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
           CASE WHEN r.valid_from IS NULL THEN NULL ELSE
             to_char(r.valid_from AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') END,
           CASE WHEN r.valid_until IS NULL THEN NULL ELSE
             to_char(r.valid_until AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') END,
           CASE WHEN r.valid_from IS NULL AND r.valid_until IS NULL THEN 'unknown' ELSE 'known' END,
           v_next_cursor
    FROM unnest(v_ids) WITH ORDINALITY AS selected(id, position)
    JOIN public.items i ON i.tenant_id = p_tenant_id AND i.id = selected.id
    JOIN public.collections c ON c.tenant_id = i.tenant_id AND c.id = i.collection_id
    JOIN public.revisions r
      ON r.tenant_id = i.tenant_id AND r.item_id = i.id AND r.id = i.active_revision_id
    ORDER BY selected.position;
  END IF;

  INSERT INTO public.read_audit
    (request_id, tenant_id, principal_id, app_id, credential_id, operation,
     target_id, outcome, authority_epoch)
  VALUES (p_request_id, p_tenant_id, v_principal_id, v_app_id, p_credential_id,
          'list', NULL, 'released', v_authority_epoch);
END
$$;

CREATE OR REPLACE FUNCTION parse_memory_validity(p_value text)
RETURNS timestamptz
LANGUAGE plpgsql
IMMUTABLE
STRICT
SET search_path = pg_catalog, public
AS $$
BEGIN
  IF p_value !~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]{1,6})?Z$'
     OR substring(p_value FROM 12 FOR 2)::integer NOT BETWEEN 0 AND 23
     OR substring(p_value FROM 15 FOR 2)::integer NOT BETWEEN 0 AND 59
     OR substring(p_value FROM 18 FOR 2)::integer NOT BETWEEN 0 AND 59 THEN
    RAISE EXCEPTION 'invalid UTC validity instant' USING ERRCODE = '22007';
  END IF;
  RETURN p_value::timestamptz;
EXCEPTION
  WHEN invalid_datetime_format OR datetime_field_overflow THEN
    RAISE EXCEPTION 'invalid UTC validity instant' USING ERRCODE = '22007';
END
$$;
REVOKE ALL ON FUNCTION parse_memory_validity(text) FROM PUBLIC;

DROP FUNCTION IF EXISTS create_private_memory(text, text, text, bytea, bytea, text, text[]);
DROP FUNCTION IF EXISTS create_private_memory(text, text, text, bytea, bytea, text, text[], text, text);
CREATE FUNCTION create_private_memory(
  p_tenant_id text,
  p_credential_id text,
  p_request_id text,
  p_key_digest bytea,
  p_request_digest bytea,
  p_content text,
  p_subject_ids text[],
  p_valid_from text,
  p_valid_until text
)
RETURNS TABLE (item_id text, revision_id text, operation_id text, completed_at text, replayed boolean)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
DECLARE
  v_principal_id text;
  v_app_id text;
  v_collection_id text;
  v_item_id text := replace(gen_random_uuid()::text, '-', '');
  v_revision_id text := replace(gen_random_uuid()::text, '-', '');
  v_operation_id text := replace(gen_random_uuid()::text, '-', '');
  v_authority_epoch bigint;
  v_completed_at timestamptz := clock_timestamp();
  v_inserted integer;
  v_existing_digest bytea;
  v_receipt_deleted boolean;
  v_valid_from timestamptz;
  v_valid_until timestamptz;
BEGIN
  IF p_tenant_id IS NULL
     OR p_credential_id IS NULL
     OR p_request_id IS NULL
     OR p_key_digest IS NULL
     OR p_request_digest IS NULL
     OR p_content IS NULL
     OR p_subject_ids IS NULL
     OR p_request_id !~ '^[0-9a-f-]{36}$'
     OR octet_length(p_key_digest) <> 32
     OR octet_length(p_request_digest) <> 32
     OR octet_length(p_content) NOT BETWEEN 1 AND 32768
     OR cardinality(p_subject_ids) NOT BETWEEN 1 AND 8
     OR cardinality(p_subject_ids) <> (SELECT count(DISTINCT subject_id) FROM unnest(p_subject_ids) AS subject_id) THEN
    RAISE EXCEPTION 'invalid memory input' USING ERRCODE = '22023';
  END IF;
  v_valid_from := public.parse_memory_validity(p_valid_from);
  v_valid_until := public.parse_memory_validity(p_valid_until);
  IF v_valid_from IS NOT NULL AND v_valid_until IS NOT NULL
     AND v_valid_from >= v_valid_until THEN
    RAISE EXCEPTION 'invalid validity interval' USING ERRCODE = '22023';
  END IF;
  SELECT authority_epoch INTO v_authority_epoch
  FROM public.tenant_authority
  WHERE tenant_id = p_tenant_id
  FOR UPDATE;
  IF NOT FOUND OR public.current_writer_context(p_tenant_id, p_credential_id, 'create') IS NOT TRUE THEN
    RAISE EXCEPTION 'unavailable writer authority context' USING ERRCODE = '42501';
  END IF;
  SELECT c.principal_id, c.app_id INTO v_principal_id, v_app_id
  FROM public.credentials c
  WHERE c.tenant_id = p_tenant_id AND c.id = p_credential_id;
  DELETE FROM public.idempotency_records d
  WHERE d.tenant_id = p_tenant_id AND d.principal_id = v_principal_id
    AND d.app_id = v_app_id AND d.operation = 'create'
    AND d.key_digest = p_key_digest AND d.expires_at <= clock_timestamp();
  INSERT INTO public.idempotency_records
    (tenant_id, principal_id, app_id, operation, key_digest, request_digest,
     operation_id, item_id, revision_id, completed_at, expires_at)
  VALUES
    (p_tenant_id, v_principal_id, v_app_id, 'create', p_key_digest, p_request_digest,
     v_operation_id, v_item_id, v_revision_id, v_completed_at, v_completed_at + interval '24 hours')
  ON CONFLICT (tenant_id, principal_id, app_id, operation, key_digest) DO NOTHING;
  GET DIAGNOSTICS v_inserted = ROW_COUNT;
  IF v_inserted = 0 THEN
    SELECT d.request_digest, d.operation_id, d.item_id, d.revision_id, d.completed_at
    INTO v_existing_digest, v_operation_id, v_item_id, v_revision_id, v_completed_at
    FROM public.idempotency_records d
    WHERE d.tenant_id = p_tenant_id AND d.principal_id = v_principal_id
      AND d.app_id = v_app_id AND d.operation = 'create' AND d.key_digest = p_key_digest
    FOR UPDATE;
    SELECT i.deleted_at IS NOT NULL INTO v_receipt_deleted
    FROM public.items i JOIN public.collections c
      ON c.tenant_id = i.tenant_id AND c.id = i.collection_id
    WHERE i.tenant_id = p_tenant_id AND i.id = v_item_id
      AND c.app_id = v_app_id AND c.audience_kind = 'private'
      AND c.owner_principal_id = v_principal_id AND c.withdrawn_at IS NULL
    FOR SHARE OF i;
    IF NOT FOUND THEN
      RAISE EXCEPTION 'private receipt unavailable' USING ERRCODE = 'P0002';
    END IF;
    IF public.current_writer_context(p_tenant_id, p_credential_id, 'create') IS NOT TRUE THEN
      RAISE EXCEPTION 'unavailable writer authority context' USING ERRCODE = '42501';
    END IF;
    IF v_existing_digest IS DISTINCT FROM p_request_digest THEN
      RAISE EXCEPTION 'idempotency key request conflict' USING ERRCODE = 'P0004';
    END IF;
    INSERT INTO public.create_rejection_audit
      (request_id, tenant_id, principal_id, app_id, credential_id, operation, outcome,
       authority_epoch, idempotency_key_digest)
    VALUES (p_request_id, p_tenant_id, v_principal_id, v_app_id, p_credential_id,
            'create', 'replayed', v_authority_epoch, p_key_digest);
    RETURN QUERY SELECT CASE WHEN v_receipt_deleted THEN NULL ELSE v_item_id END,
      CASE WHEN v_receipt_deleted THEN NULL ELSE v_revision_id END, v_operation_id,
      to_char(v_completed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'), true;
    RETURN;
  END IF;
  SELECT c.id INTO v_collection_id
  FROM public.collections c
  WHERE c.tenant_id = p_tenant_id
    AND c.app_id = v_app_id
    AND c.audience_kind = 'private'
    AND c.owner_principal_id = v_principal_id
    AND c.withdrawn_at IS NULL
  ORDER BY c.id
  LIMIT 1;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'private collection unavailable' USING ERRCODE = 'P0002';
  END IF;
  IF (
    SELECT count(*)
    FROM public.subjects s
    WHERE s.tenant_id = p_tenant_id
      AND s.app_id = v_app_id
      AND s.id = ANY(p_subject_ids)
      AND (s.kind <> 'principal' OR s.principal_id = v_principal_id)
  ) <> cardinality(p_subject_ids) THEN
    RAISE EXCEPTION 'invalid private memory subjects' USING ERRCODE = '22023';
  END IF;

  UPDATE public.tenant_authority
  SET authority_epoch = authority_epoch + 1
  WHERE tenant_id = p_tenant_id
  RETURNING authority_epoch INTO v_authority_epoch;
  INSERT INTO public.items (tenant_id, id, collection_id)
  VALUES (p_tenant_id, v_item_id, v_collection_id);
  INSERT INTO public.revisions
    (tenant_id, item_id, id, content, valid_from, valid_until)
  VALUES
    (p_tenant_id, v_item_id, v_revision_id, p_content, v_valid_from, v_valid_until);
  INSERT INTO public.revision_subjects (tenant_id, item_id, revision_id, subject_id)
  SELECT p_tenant_id, v_item_id, v_revision_id, subject_id FROM unnest(p_subject_ids) AS subject_id;
  INSERT INTO public.lexical_representations (tenant_id, item_id, revision_id, document)
  VALUES (p_tenant_id, v_item_id, v_revision_id, to_tsvector('simple', p_content));
  UPDATE public.items SET active_revision_id = v_revision_id
  WHERE tenant_id = p_tenant_id AND id = v_item_id;
  INSERT INTO public.mutation_audit
    (request_id, tenant_id, principal_id, app_id, credential_id, operation,
     operation_id, target_id, revision_id, outcome, authority_epoch, idempotency_key_digest)
  VALUES
    (p_request_id, p_tenant_id, v_principal_id, v_app_id, p_credential_id, 'create',
     v_operation_id, v_item_id, v_revision_id, 'created', v_authority_epoch, p_key_digest);
  v_completed_at := clock_timestamp();
  UPDATE public.idempotency_records d
  SET completed_at = v_completed_at, expires_at = v_completed_at + interval '24 hours'
  WHERE d.tenant_id = p_tenant_id AND d.principal_id = v_principal_id
    AND d.app_id = v_app_id AND d.operation = 'create' AND d.key_digest = p_key_digest;

  RETURN QUERY SELECT v_item_id, v_revision_id, v_operation_id,
    to_char(v_completed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'), false;
END
$$;

DROP FUNCTION IF EXISTS correct_private_memory(text, text, text, bytea, bytea, text, text, text, text[]);
DROP FUNCTION IF EXISTS correct_private_memory(text, text, text, bytea, bytea, text, text, text, text[], text, text);
CREATE FUNCTION correct_private_memory(
  p_tenant_id text, p_credential_id text, p_request_id text,
  p_key_digest bytea, p_request_digest bytea, p_item_id text,
  p_expected_revision_id text, p_content text, p_subject_ids text[],
  p_valid_from text, p_valid_until text
)
RETURNS TABLE (item_id text, revision_id text, operation_id text, completed_at text, replayed boolean)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE
  v_principal_id text; v_app_id text; v_current_revision_id text;
  v_revision_id text := replace(gen_random_uuid()::text, '-', '');
  v_operation_id text := replace(gen_random_uuid()::text, '-', '');
  v_authority_epoch bigint;
  v_completed_at timestamptz := clock_timestamp();
  v_inserted integer;
  v_existing_digest bytea;
  v_receipt_deleted boolean;
  v_valid_from timestamptz;
  v_valid_until timestamptz;
BEGIN
  IF p_tenant_id IS NULL OR p_credential_id IS NULL OR p_request_id IS NULL
     OR p_key_digest IS NULL OR p_request_digest IS NULL OR p_item_id IS NULL
     OR p_expected_revision_id IS NULL OR p_content IS NULL OR p_subject_ids IS NULL
     OR p_request_id !~ '^[0-9a-f-]{36}$'
     OR p_item_id !~ '^[A-Za-z0-9_-]{1,64}$'
     OR p_expected_revision_id !~ '^[A-Za-z0-9_-]{1,64}$'
     OR octet_length(p_key_digest) <> 32 OR octet_length(p_request_digest) <> 32
     OR octet_length(p_content) NOT BETWEEN 1 AND 32768
     OR cardinality(p_subject_ids) NOT BETWEEN 1 AND 8
     OR cardinality(p_subject_ids) <> (SELECT count(DISTINCT subject_id) FROM unnest(p_subject_ids) AS subject_id) THEN
    RAISE EXCEPTION 'invalid correction input' USING ERRCODE = '22023';
  END IF;
  v_valid_from := public.parse_memory_validity(p_valid_from);
  v_valid_until := public.parse_memory_validity(p_valid_until);
  IF v_valid_from IS NOT NULL AND v_valid_until IS NOT NULL
     AND v_valid_from >= v_valid_until THEN
    RAISE EXCEPTION 'invalid validity interval' USING ERRCODE = '22023';
  END IF;
  SELECT authority_epoch INTO v_authority_epoch FROM public.tenant_authority
  WHERE tenant_id = p_tenant_id FOR UPDATE;
  IF NOT FOUND OR public.current_writer_context(p_tenant_id, p_credential_id, 'correct') IS NOT TRUE THEN
    RAISE EXCEPTION 'unavailable correction authority' USING ERRCODE = '42501';
  END IF;
  SELECT c.principal_id, c.app_id INTO v_principal_id, v_app_id
  FROM public.credentials c WHERE c.tenant_id = p_tenant_id AND c.id = p_credential_id;
  DELETE FROM public.idempotency_records d
  WHERE d.tenant_id = p_tenant_id AND d.principal_id = v_principal_id
    AND d.app_id = v_app_id AND d.operation = 'correct'
    AND d.key_digest = p_key_digest AND d.expires_at <= clock_timestamp();
  INSERT INTO public.idempotency_records
    (tenant_id, principal_id, app_id, operation, key_digest, request_digest,
     operation_id, item_id, revision_id, completed_at, expires_at)
  VALUES (p_tenant_id, v_principal_id, v_app_id, 'correct', p_key_digest,
          p_request_digest, v_operation_id, p_item_id, v_revision_id, v_completed_at,
          v_completed_at + interval '24 hours')
  ON CONFLICT (tenant_id, principal_id, app_id, operation, key_digest) DO NOTHING;
  GET DIAGNOSTICS v_inserted = ROW_COUNT;
  IF v_inserted = 0 THEN
    SELECT d.request_digest, d.operation_id, d.item_id, d.revision_id, d.completed_at
    INTO v_existing_digest, v_operation_id, item_id, v_revision_id, v_completed_at
    FROM public.idempotency_records d
    WHERE d.tenant_id = p_tenant_id AND d.principal_id = v_principal_id
      AND d.app_id = v_app_id AND d.operation = 'correct' AND d.key_digest = p_key_digest
    FOR UPDATE;
    SELECT i.deleted_at IS NOT NULL INTO v_receipt_deleted
    FROM public.items i JOIN public.collections c
      ON c.tenant_id = i.tenant_id AND c.id = i.collection_id
    WHERE i.tenant_id = p_tenant_id AND i.id = item_id
      AND c.app_id = v_app_id AND c.audience_kind = 'private'
      AND c.owner_principal_id = v_principal_id AND c.withdrawn_at IS NULL
    FOR SHARE OF i;
    IF NOT FOUND THEN
      RAISE EXCEPTION 'private receipt unavailable' USING ERRCODE = 'P0002';
    END IF;
    IF public.current_writer_context(p_tenant_id, p_credential_id, 'correct') IS NOT TRUE THEN
      RAISE EXCEPTION 'unavailable correction authority' USING ERRCODE = '42501';
    END IF;
    IF v_existing_digest IS DISTINCT FROM p_request_digest THEN
      RAISE EXCEPTION 'idempotency key request conflict' USING ERRCODE = 'P0004';
    END IF;
    INSERT INTO public.create_rejection_audit
      (request_id, tenant_id, principal_id, app_id, credential_id, operation, outcome,
       authority_epoch, idempotency_key_digest)
    VALUES (p_request_id, p_tenant_id, v_principal_id, v_app_id, p_credential_id,
            'correct', 'replayed', v_authority_epoch, p_key_digest);
    RETURN QUERY SELECT CASE WHEN v_receipt_deleted THEN NULL ELSE item_id END,
      CASE WHEN v_receipt_deleted THEN NULL ELSE v_revision_id END, v_operation_id,
      to_char(v_completed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'), true;
    RETURN;
  END IF;
  SELECT i.active_revision_id INTO v_current_revision_id
  FROM public.items i JOIN public.collections c
    ON c.tenant_id = i.tenant_id AND c.id = i.collection_id
  WHERE i.tenant_id = p_tenant_id AND i.id = p_item_id AND i.deleted_at IS NULL
    AND c.app_id = v_app_id AND c.audience_kind = 'private'
    AND c.owner_principal_id = v_principal_id AND c.withdrawn_at IS NULL
  FOR UPDATE OF i;
  IF NOT FOUND THEN RAISE EXCEPTION 'private item unavailable' USING ERRCODE = 'P0002'; END IF;
  IF public.current_writer_context(p_tenant_id, p_credential_id, 'correct') IS NOT TRUE THEN
    RAISE EXCEPTION 'unavailable correction authority' USING ERRCODE = '42501';
  END IF;
  IF v_current_revision_id IS DISTINCT FROM p_expected_revision_id THEN
    RAISE EXCEPTION 'stale current revision' USING ERRCODE = 'P0003';
  END IF;
  IF (SELECT count(*) FROM public.subjects s WHERE s.tenant_id = p_tenant_id
      AND s.app_id = v_app_id AND s.id = ANY(p_subject_ids)
      AND (s.kind <> 'principal' OR s.principal_id = v_principal_id)) <> cardinality(p_subject_ids) THEN
    RAISE EXCEPTION 'invalid correction subjects' USING ERRCODE = '22023';
  END IF;
  UPDATE public.tenant_authority SET authority_epoch = authority_epoch + 1
  WHERE tenant_id = p_tenant_id RETURNING authority_epoch INTO v_authority_epoch;
  INSERT INTO public.revisions
    (tenant_id, item_id, id, content, valid_from, valid_until)
  VALUES
    (p_tenant_id, p_item_id, v_revision_id, p_content, v_valid_from, v_valid_until);
  INSERT INTO public.revision_subjects (tenant_id, item_id, revision_id, subject_id)
  SELECT p_tenant_id, p_item_id, v_revision_id, subject_id FROM unnest(p_subject_ids) AS subject_id;
  INSERT INTO public.lexical_representations (tenant_id, item_id, revision_id, document)
  VALUES (p_tenant_id, p_item_id, v_revision_id, to_tsvector('simple', p_content));
  UPDATE public.items SET active_revision_id = v_revision_id
  WHERE tenant_id = p_tenant_id AND id = p_item_id;
  INSERT INTO public.mutation_audit
    (request_id, tenant_id, principal_id, app_id, credential_id, operation,
     operation_id, target_id, revision_id, outcome, authority_epoch, idempotency_key_digest)
  VALUES (p_request_id, p_tenant_id, v_principal_id, v_app_id, p_credential_id,
          'correct', v_operation_id, p_item_id, v_revision_id, 'corrected',
          v_authority_epoch, p_key_digest);
  v_completed_at := clock_timestamp();
  UPDATE public.idempotency_records d
  SET completed_at = v_completed_at, expires_at = v_completed_at + interval '24 hours'
  WHERE d.tenant_id = p_tenant_id AND d.principal_id = v_principal_id
    AND d.app_id = v_app_id AND d.operation = 'correct' AND d.key_digest = p_key_digest;
  RETURN QUERY SELECT p_item_id, v_revision_id, v_operation_id,
    to_char(v_completed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'), false;
END $$;

DROP FUNCTION IF EXISTS forget_private_memory(text, text, text, bytea, bytea, text, text);
CREATE FUNCTION forget_private_memory(
  p_tenant_id text, p_credential_id text, p_request_id text,
  p_key_digest bytea, p_request_digest bytea, p_item_id text,
  p_expected_revision_id text
)
RETURNS TABLE (operation_id text, completed_at text, replayed boolean, purge_state text,
               deletion_generation bigint)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE
  v_principal_id text; v_app_id text; v_current_revision_id text;
  v_operation_id text := replace(gen_random_uuid()::text, '-', '');
  v_authority_epoch bigint; v_deletion_generation bigint;
  v_completed_at timestamptz := clock_timestamp();
  v_inserted integer; v_existing_digest bytea; v_purge_state text;
  v_receipt_item_id text;
BEGIN
  IF p_tenant_id IS NULL OR p_credential_id IS NULL OR p_request_id IS NULL
     OR p_key_digest IS NULL OR p_request_digest IS NULL OR p_item_id IS NULL
     OR p_expected_revision_id IS NULL OR p_request_id !~ '^[0-9a-f-]{36}$'
     OR p_item_id !~ '^[A-Za-z0-9_-]{1,64}$'
     OR p_expected_revision_id !~ '^[A-Za-z0-9_-]{1,64}$'
     OR octet_length(p_key_digest) <> 32 OR octet_length(p_request_digest) <> 32 THEN
    RAISE EXCEPTION 'invalid forget input' USING ERRCODE = '22023';
  END IF;
  SELECT authority_epoch INTO v_authority_epoch FROM public.tenant_authority
  WHERE tenant_id = p_tenant_id FOR UPDATE;
  IF NOT FOUND OR public.current_writer_context(p_tenant_id, p_credential_id, 'forget') IS NOT TRUE THEN
    RAISE EXCEPTION 'unavailable forget authority' USING ERRCODE = '42501';
  END IF;
  SELECT c.principal_id, c.app_id INTO v_principal_id, v_app_id
  FROM public.credentials c WHERE c.tenant_id = p_tenant_id AND c.id = p_credential_id;
  DELETE FROM public.idempotency_records d
  WHERE d.tenant_id = p_tenant_id AND d.principal_id = v_principal_id
    AND d.app_id = v_app_id AND d.operation = 'forget'
    AND d.key_digest = p_key_digest AND d.expires_at <= clock_timestamp();
  INSERT INTO public.idempotency_records
    (tenant_id, principal_id, app_id, operation, key_digest, request_digest,
     operation_id, item_id, revision_id, completed_at, expires_at)
  VALUES (p_tenant_id, v_principal_id, v_app_id, 'forget', p_key_digest,
          p_request_digest, v_operation_id, p_item_id, p_expected_revision_id,
          v_completed_at, v_completed_at + interval '24 hours')
  ON CONFLICT (tenant_id, principal_id, app_id, operation, key_digest) DO NOTHING;
  GET DIAGNOSTICS v_inserted = ROW_COUNT;
  IF v_inserted = 0 THEN
    SELECT d.request_digest, d.operation_id, d.item_id, d.completed_at
    INTO v_existing_digest, v_operation_id, v_receipt_item_id, v_completed_at
    FROM public.idempotency_records d
    WHERE d.tenant_id = p_tenant_id AND d.principal_id = v_principal_id
      AND d.app_id = v_app_id AND d.operation = 'forget' AND d.key_digest = p_key_digest
    FOR UPDATE;
    SELECT m.deletion_generation, m.purge_state
    INTO v_deletion_generation, v_purge_state
    FROM public.items i JOIN public.collections c
      ON c.tenant_id = i.tenant_id AND c.id = i.collection_id
    JOIN public.deletion_markers m ON m.tenant_id = i.tenant_id AND m.item_id = i.id
    WHERE i.tenant_id = p_tenant_id AND i.id = v_receipt_item_id AND i.deleted_at IS NOT NULL
      AND c.app_id = v_app_id AND c.audience_kind = 'private'
      AND c.owner_principal_id = v_principal_id AND c.withdrawn_at IS NULL
    FOR SHARE OF i;
    IF NOT FOUND THEN RAISE EXCEPTION 'private receipt unavailable' USING ERRCODE = 'P0002'; END IF;
    IF public.current_writer_context(p_tenant_id, p_credential_id, 'forget') IS NOT TRUE THEN
      RAISE EXCEPTION 'unavailable forget authority' USING ERRCODE = '42501';
    END IF;
    IF v_existing_digest IS DISTINCT FROM p_request_digest THEN
      RAISE EXCEPTION 'idempotency key request conflict' USING ERRCODE = 'P0004';
    END IF;
    INSERT INTO public.create_rejection_audit
      (request_id, tenant_id, principal_id, app_id, credential_id, operation, outcome,
       authority_epoch, idempotency_key_digest)
    VALUES (p_request_id, p_tenant_id, v_principal_id, v_app_id, p_credential_id,
            'forget', 'replayed', v_authority_epoch, p_key_digest);
    RETURN QUERY SELECT v_operation_id,
      to_char(v_completed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
      true, v_purge_state, v_deletion_generation;
    RETURN;
  END IF;
  SELECT i.active_revision_id, i.deletion_generation + 1
  INTO v_current_revision_id, v_deletion_generation
  FROM public.items i JOIN public.collections c
    ON c.tenant_id = i.tenant_id AND c.id = i.collection_id
  WHERE i.tenant_id = p_tenant_id AND i.id = p_item_id AND i.deleted_at IS NULL
    AND c.app_id = v_app_id AND c.audience_kind = 'private'
    AND c.owner_principal_id = v_principal_id AND c.withdrawn_at IS NULL
  FOR UPDATE OF i;
  IF NOT FOUND THEN RAISE EXCEPTION 'private item unavailable' USING ERRCODE = 'P0002'; END IF;
  IF public.current_writer_context(p_tenant_id, p_credential_id, 'forget') IS NOT TRUE THEN
    RAISE EXCEPTION 'unavailable forget authority' USING ERRCODE = '42501';
  END IF;
  IF v_current_revision_id IS DISTINCT FROM p_expected_revision_id THEN
    RAISE EXCEPTION 'stale current revision' USING ERRCODE = 'P0003';
  END IF;
  UPDATE public.tenant_authority SET authority_epoch = authority_epoch + 1
  WHERE tenant_id = p_tenant_id RETURNING authority_epoch INTO v_authority_epoch;
  UPDATE public.items SET active_revision_id = NULL, deleted_at = clock_timestamp(),
    deletion_generation = v_deletion_generation
  WHERE tenant_id = p_tenant_id AND id = p_item_id;
  INSERT INTO public.deletion_markers
    (tenant_id, item_id, deletion_generation, deleted_at, operation_id, purge_state)
  VALUES (p_tenant_id, p_item_id, v_deletion_generation, clock_timestamp(),
          v_operation_id, 'pending');
  INSERT INTO public.purge_jobs
    (tenant_id, item_id, deletion_generation, operation_id, status)
  VALUES (p_tenant_id, p_item_id, v_deletion_generation, v_operation_id, 'pending');
  INSERT INTO public.mutation_audit
    (request_id, tenant_id, principal_id, app_id, credential_id, operation,
     operation_id, target_id, revision_id, outcome, authority_epoch, idempotency_key_digest)
  VALUES (p_request_id, p_tenant_id, v_principal_id, v_app_id, p_credential_id,
          'forget', v_operation_id, p_item_id, v_current_revision_id, 'forgotten',
          v_authority_epoch, p_key_digest);
  v_completed_at := clock_timestamp();
  UPDATE public.idempotency_records d SET completed_at = v_completed_at,
    expires_at = v_completed_at + interval '24 hours'
  WHERE d.tenant_id = p_tenant_id AND d.principal_id = v_principal_id
    AND d.app_id = v_app_id AND d.operation = 'forget' AND d.key_digest = p_key_digest;
  RETURN QUERY SELECT v_operation_id,
    to_char(v_completed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
    false, 'pending'::text, v_deletion_generation;
END $$;

DROP FUNCTION IF EXISTS process_forget_purge(text, text, text, bigint);
DROP FUNCTION IF EXISTS process_forget_purge(text, text, bigint);
CREATE FUNCTION process_forget_purge(
  p_tenant_id text, p_item_id text, p_deletion_generation bigint
)
RETURNS text
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE
  v_item_generation bigint; v_deleted_at timestamptz;
  v_marker_generation bigint; v_marker_state text;
  v_job_generation bigint; v_job_status text;
BEGIN
  IF p_tenant_id IS NULL OR p_item_id IS NULL OR p_deletion_generation IS NULL
     OR p_item_id !~ '^[A-Za-z0-9_-]{1,64}$' OR p_deletion_generation <= 0 THEN
    RAISE EXCEPTION 'invalid purge input' USING ERRCODE = '22023';
  END IF;
  PERFORM 1 FROM public.tenant_authority WHERE tenant_id = p_tenant_id FOR UPDATE;
  IF NOT FOUND THEN RAISE EXCEPTION 'stale purge job' USING ERRCODE = 'P0003'; END IF;
  SELECT i.deletion_generation, i.deleted_at INTO v_item_generation, v_deleted_at
  FROM public.items i WHERE i.tenant_id=p_tenant_id AND i.id=p_item_id FOR UPDATE;
  IF NOT FOUND THEN RAISE EXCEPTION 'stale purge item' USING ERRCODE = 'P0003'; END IF;
  SELECT m.deletion_generation, m.purge_state INTO v_marker_generation, v_marker_state
  FROM public.deletion_markers m WHERE m.tenant_id=p_tenant_id AND m.item_id=p_item_id FOR UPDATE;
  IF NOT FOUND THEN RAISE EXCEPTION 'stale purge marker' USING ERRCODE = 'P0003'; END IF;
  SELECT j.deletion_generation, j.status INTO v_job_generation, v_job_status
  FROM public.purge_jobs j WHERE j.tenant_id=p_tenant_id AND j.item_id=p_item_id
    AND j.deletion_generation=p_deletion_generation FOR UPDATE;
  IF NOT FOUND THEN RAISE EXCEPTION 'stale purge job' USING ERRCODE = 'P0003'; END IF;
  IF v_deleted_at IS NULL OR v_item_generation IS DISTINCT FROM p_deletion_generation
     OR v_marker_generation IS DISTINCT FROM p_deletion_generation
     OR v_job_generation IS DISTINCT FROM p_deletion_generation THEN
    RAISE EXCEPTION 'stale purge generation' USING ERRCODE = 'P0003';
  END IF;
  IF v_marker_state = 'complete' AND v_job_status = 'complete' THEN RETURN 'complete'; END IF;
  IF v_marker_state <> 'pending' OR v_job_status <> 'pending' THEN
    RAISE EXCEPTION 'inconsistent purge state' USING ERRCODE = 'P0003';
  END IF;
  DELETE FROM public.lexical_representations
    WHERE tenant_id=p_tenant_id AND item_id=p_item_id;
  DELETE FROM public.revision_subjects
    WHERE tenant_id=p_tenant_id AND item_id=p_item_id;
  DELETE FROM public.revisions WHERE tenant_id=p_tenant_id AND item_id=p_item_id;
  UPDATE public.purge_jobs SET status='complete', completed_at=clock_timestamp()
    WHERE tenant_id=p_tenant_id AND item_id=p_item_id
      AND deletion_generation=p_deletion_generation;
  UPDATE public.deletion_markers SET purge_state='complete'
    WHERE tenant_id=p_tenant_id AND item_id=p_item_id
      AND deletion_generation=p_deletion_generation;
  RETURN 'complete';
END $$;

GRANT CONNECT ON DATABASE agentic_memory TO agentic_memory_runtime;
GRANT CONNECT ON DATABASE agentic_memory TO agentic_memory_purge_worker;
REVOKE CREATE, TEMPORARY ON DATABASE agentic_memory FROM agentic_memory_runtime;
REVOKE CREATE, TEMPORARY ON DATABASE agentic_memory FROM agentic_memory_purge_worker;
REVOKE CREATE, TEMPORARY ON DATABASE agentic_memory FROM PUBLIC;
REVOKE ALL ON SCHEMA public FROM agentic_memory_runtime;
GRANT USAGE ON SCHEMA public TO agentic_memory_runtime;
REVOKE ALL ON SCHEMA public FROM agentic_memory_purge_worker;
GRANT USAGE ON SCHEMA public TO agentic_memory_purge_worker;
REVOKE ALL PRIVILEGES ON ALL TABLES IN SCHEMA public FROM agentic_memory_runtime;
REVOKE ALL PRIVILEGES ON ALL SEQUENCES IN SCHEMA public FROM agentic_memory_runtime;
REVOKE ALL PRIVILEGES ON ALL FUNCTIONS IN SCHEMA public FROM agentic_memory_runtime;
REVOKE ALL PRIVILEGES ON ALL FUNCTIONS IN SCHEMA public FROM PUBLIC;
REVOKE ALL PRIVILEGES ON ALL TABLES IN SCHEMA public FROM agentic_memory_purge_worker;
REVOKE ALL PRIVILEGES ON ALL SEQUENCES IN SCHEMA public FROM agentic_memory_purge_worker;
REVOKE ALL PRIVILEGES ON ALL FUNCTIONS IN SCHEMA public FROM agentic_memory_purge_worker;
REVOKE ALL ON FUNCTION reader_tenant_scope(text), current_reader_context(text), resolve_current_reader(text), current_writer_context(text, text, text), resolve_current_writer(text), lock_tenant_authority(text), record_request_rejection(text, text, text), record_read_audit(text, text, text), read_current_item(text, text, text, text, text, text[]), source_search_document_v1(text, jsonb), activate_document_extraction(text, text, text, text, bytea, text, text, text, text, text[], text[], text[], text[], text[]), eligible_source_passages(text, text, text, text, text, text, text, text[], text[]), lexical_query_bounded_question_v1(text), search_current_memories(text, text, text, text, integer, text[]), list_current_resources(text, text, text, text, integer, text), parse_memory_validity(text), create_private_memory(text, text, text, bytea, bytea, text, text[], text, text), correct_private_memory(text, text, text, bytea, bytea, text, text, text, text[], text, text), forget_private_memory(text, text, text, bytea, bytea, text, text), process_forget_purge(text, text,bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION resolve_current_reader(text), resolve_current_writer(text), record_request_rejection(text, text, text), read_current_item(text, text, text, text, text, text[]), eligible_source_passages(text, text, text, text, text, text, text, text[], text[]), search_current_memories(text, text, text, text, integer, text[]), list_current_resources(text, text, text, text, integer, text), create_private_memory(text, text, text, bytea, bytea, text, text[], text, text), correct_private_memory(text, text, text, bytea, bytea, text, text, text, text[], text, text), forget_private_memory(text, text, text, bytea, bytea, text, text) TO agentic_memory_runtime;
GRANT EXECUTE ON FUNCTION process_forget_purge(text, text, bigint) TO agentic_memory_purge_worker;
