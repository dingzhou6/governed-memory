#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
compose() {
  COMPOSE_REMOVE_ORPHANS=false docker-compose --file "$repository_root/compose.yaml" \
    --project-name agentic-memory-fixture --project-directory "$repository_root" "$@"
}
psql_file() {
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory -f "$1"
}
psql_value() {
  compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory -c "$1"
}

compose up -d --wait
if [ "$(psql_value "SELECT to_regclass('public.embedding_generations') IS NOT NULL")" = t ]; then
  psql_file /migrations/0004_scoped_semantic_readiness.down.sql
  if [ "$(psql_value "SELECT atttypmod=-1 FROM pg_attribute WHERE attrelid='public.embedding_representations'::regclass AND attname='embedding'")" = t ]; then
    psql_file /migrations/0003_external_vectors.down.sql
  fi
  psql_file /migrations/0002_governed_semantic.down.sql
fi
psql_file /migrations/0001_read_slice.sql
test "$(psql_value "SELECT to_regclass('public.revisions') IS NOT NULL AND to_regclass('public.embedding_jobs') IS NULL")" = t
psql_file /migrations/0002_governed_semantic.up.sql
test "$(psql_value "SELECT extversion FROM pg_extension WHERE extname='vector'")" = 0.8.6
test "$(psql_value "SELECT prorettype::regtype::text FROM pg_proc WHERE oid='claim_embedding_jobs(integer,integer)'::regprocedure")" = record
psql_file /migrations/0003_external_vectors.up.sql
psql_file /migrations/0004_scoped_semantic_readiness.up.sql
test "$(psql_value "SELECT has_function_privilege('agentic_memory_runtime','semantic_readiness(text,text,text[])','EXECUTE') AND NOT has_function_privilege('agentic_memory_purge_worker','semantic_readiness(text,text,text[])','EXECUTE') AND to_regprocedure('semantic_readiness(text,text)') IS NOT NULL")" = t
psql_file /migrations/0004_scoped_semantic_readiness.down.sql
test "$(psql_value "SELECT to_regprocedure('semantic_readiness(text,text,text[])') IS NULL AND to_regprocedure('semantic_readiness(text,text)') IS NOT NULL")" = t
test "$(psql_value "SELECT atttypmod=-1 FROM pg_attribute WHERE attrelid='public.embedding_representations'::regclass AND attname='embedding'")" = t
psql_file /migrations/0003_external_vectors.down.sql
test "$(psql_value "SELECT atttypmod<>-1 FROM pg_attribute WHERE attrelid='public.embedding_representations'::regclass AND attname='embedding'")" = t
psql_file /migrations/0003_external_vectors.up.sql
psql_value "INSERT INTO embedding_generations (tenant_id,id,model_version,input_recipe_version,input_recipe,dimensions) VALUES ('00000000000000000000000000000001','migration_non3','migration-model','migration-recipe','synthetic',4)" >/dev/null
if psql_file /migrations/0003_external_vectors.down.sql; then
  echo 'N3 external-vector downgrade unexpectedly accepted non-3-dimensional state' >&2
  exit 1
fi
test "$(psql_value "SELECT dimensions FROM embedding_generations WHERE id='migration_non3'")" = 4
psql_value "DELETE FROM embedding_generations WHERE id='migration_non3'" >/dev/null
psql_file /migrations/0003_external_vectors.down.sql
psql_file /migrations/0002_governed_semantic.down.sql
test "$(psql_value "SELECT to_regclass('public.embedding_jobs') IS NULL AND to_regclass('public.revisions') IS NOT NULL")" = t
psql_file /migrations/0002_governed_semantic.up.sql
psql_file /migrations/0003_external_vectors.up.sql
test "$(psql_value "SELECT vector_dims('[1,0,0]'::vector(3))")" = 3
psql_file /migrations/0004_scoped_semantic_readiness.up.sql
echo 'N3 clean upgrade, row shape, compatible rollback, refusal with non-3 state and forward migration checks passed'
