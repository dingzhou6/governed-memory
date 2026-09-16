#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
compose() {
  COMPOSE_REMOVE_ORPHANS=false docker-compose --file "$repository_root/compose.yaml" \
    --project-name agentic-memory-fixture --project-directory "$repository_root" "$@"
}

base_installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory \
  -c "SELECT to_regclass('public.revisions') IS NOT NULL")
if [ "$base_installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory -f /migrations/0001_read_slice.sql
elif [ "$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT to_regclass('public.lexical_representations') IS NOT NULL AND to_regprocedure('public.search_current_memories(text,text,text,text,integer,text[])') IS NOT NULL AND to_regprocedure('public.activate_document_extraction(text,text,text,text,bytea,text,text,text,text,text[],text[],text[],text[],text[])') IS NOT NULL")" != t ]; then
  echo 'base migration is present but its required schema/function shape is incomplete' >&2
  exit 1
fi

installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory \
  -c "SELECT to_regclass('public.embedding_generations') IS NOT NULL")
if [ "$installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory \
    -f /migrations/0002_governed_semantic.up.sql
elif [ "$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT extversion='0.8.6' AND to_regprocedure('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)') IS NOT NULL AND has_function_privilege('agentic_memory_runtime','semantic_readiness(text,text)','EXECUTE') AND has_function_privilege('agentic_memory_purge_worker','claim_embedding_jobs(integer,integer)','EXECUTE') FROM pg_extension WHERE extname='vector'")" != t ]; then
  echo 'N3 migration is present but its pinned extension/function shape is incomplete' >&2
  exit 1
fi

external_vectors_installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT atttypmod=-1 FROM pg_attribute WHERE attrelid='public.embedding_representations'::regclass AND attname='embedding'")
if [ "$external_vectors_installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory \
    -f /migrations/0003_external_vectors.up.sql
fi

scoped_readiness_installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT to_regprocedure('public.semantic_readiness(text,text,text[])') IS NOT NULL")
if [ "$scoped_readiness_installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory \
    -f /migrations/0004_scoped_semantic_readiness.up.sql
fi
test "$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT has_function_privilege('agentic_memory_runtime','semantic_readiness(text,text,text[])','EXECUTE') AND NOT has_function_privilege('agentic_memory_purge_worker','semantic_readiness(text,text,text[])','EXECUTE')")" = t

disjunctive_query_installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT to_regprocedure('public.search_current_memories_disjunctive_v2(text,text,text,text,integer,text[])') IS NOT NULL")
if [ "$disjunctive_query_installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory \
    -f /migrations/0005_disjunctive_query.up.sql
fi
test "$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT has_function_privilege('agentic_memory_runtime','search_current_memories_disjunctive_v2(text,text,text,text,integer,text[])','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','lexical_query_disjunctive_v2(text)','EXECUTE') AND NOT has_function_privilege('agentic_memory_purge_worker','search_current_memories_disjunctive_v2(text,text,text,text,integer,text[])','EXECUTE')")" = t
native_disjunctive_query_installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT to_regprocedure('public.search_current_memories_disjunctive_v3(text,text,text,text,integer,text[])') IS NOT NULL")
if [ "$native_disjunctive_query_installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory \
    -f /migrations/0006_native_disjunctive_query.up.sql
fi
test "$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT has_function_privilege('agentic_memory_runtime','search_current_memories_disjunctive_v3(text,text,text,text,integer,text[])','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','lexical_query_disjunctive_v3(text)','EXECUTE') AND NOT has_function_privilege('agentic_memory_purge_worker','search_current_memories_disjunctive_v3(text,text,text,text,integer,text[])','EXECUTE')")" = t

configurable_context_installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('AND p_max_context_bytes BETWEEN 1 AND 65535' in pg_get_functiondef('public.search_current_memories_query_core(text,text,text,text,integer,text[],tsquery)'::regprocedure)) > 0")
if [ "$configurable_context_installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory \
    -f /migrations/0007_configurable_search_context.up.sql
fi
