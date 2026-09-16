\set QUIET 1
BEGIN;
\ir reset.sql

-- Operator-only recall-ready supplement; integration tests continue to own reset.sql.
INSERT INTO apps (tenant_id, id, active) VALUES
  ('00000000000000000000000000000001', 'a0000000000000000000000000000009', true);
INSERT INTO subjects (tenant_id, id, app_id, kind) VALUES
  ('00000000000000000000000000000001', 'customer_alpha', 'a0000000000000000000000000000001', 'customer'),
  ('00000000000000000000000000000001', 'project_alpha', 'a0000000000000000000000000000001', 'project'),
  ('00000000000000000000000000000001', 'customer_other', 'a0000000000000000000000000000001', 'customer'),
  ('00000000000000000000000000000001', 'customer_alpha_other_app', 'a0000000000000000000000000000009', 'customer'),
  ('00000000000000000000000000000001', 'app_alpha', 'a0000000000000000000000000000001', 'app');
INSERT INTO items (tenant_id, id, collection_id) VALUES
  ('00000000000000000000000000000001', '40000000000000000000000000000009', '30000000000000000000000000000004');
INSERT INTO revisions (tenant_id, item_id, id, content) VALUES
  ('00000000000000000000000000000001', '40000000000000000000000000000009', '50000000000000000000000000000009', 'SCOPED OPERATOR HANDBOOK SENTINEL');
UPDATE items SET active_revision_id = '50000000000000000000000000000009'
WHERE tenant_id = '00000000000000000000000000000001' AND id = '40000000000000000000000000000009';
INSERT INTO revision_subjects VALUES
  ('00000000000000000000000000000001','40000000000000000000000000000009','50000000000000000000000000000009','customer_alpha'),
  ('00000000000000000000000000000001','40000000000000000000000000000009','50000000000000000000000000000009','project_alpha');
INSERT INTO lexical_representations (tenant_id,item_id,revision_id,document)
SELECT tenant_id,item_id,id,to_tsvector('simple',content) FROM revisions;

CREATE TEMP TABLE issued_fixture_credentials (
  label text PRIMARY KEY,
  bearer text NOT NULL UNIQUE,
  credential_id text NOT NULL,
  principal_id text NOT NULL,
  credential_class text NOT NULL,
  allowed_operations text[] NOT NULL
) ON COMMIT PRESERVE ROWS;

INSERT INTO issued_fixture_credentials
  (label, bearer, credential_id, principal_id, credential_class, allowed_operations)
VALUES
  ('ALICE_READER_BEARER', encode(gen_random_bytes(32), 'hex'),
   'c0000000000000000000000000000001', '10000000000000000000000000000001',
   'agent_reader', ARRAY['list', 'search', 'read']),
  ('BOB_READER_BEARER', encode(gen_random_bytes(32), 'hex'),
   'c0000000000000000000000000000002', '10000000000000000000000000000002',
   'agent_reader', ARRAY['list', 'search', 'read']),
  ('ALICE_WRITER_BEARER', encode(gen_random_bytes(32), 'hex'),
   'c0000000000000000000000000000003', '10000000000000000000000000000001',
   'trusted_writer', ARRAY['create', 'correct', 'forget']);

INSERT INTO credentials
  (tenant_id, id, principal_id, app_id, token_digest, credential_class,
   allowed_operations, issued_at, expires_at)
SELECT '00000000000000000000000000000001', credential_id, principal_id,
       'a0000000000000000000000000000001', digest(bearer, 'sha256'),
       credential_class, allowed_operations, statement_timestamp(),
       statement_timestamp() + interval '24 hours'
FROM issued_fixture_credentials;
COMMIT;

\pset format unaligned
\pset tuples_only on
\set QUIET 0
SELECT label || '=' || bearer
FROM issued_fixture_credentials
ORDER BY CASE label
  WHEN 'ALICE_READER_BEARER' THEN 1
  WHEN 'BOB_READER_BEARER' THEN 2
  ELSE 3
END;
\set QUIET 1
