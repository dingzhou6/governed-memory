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

clause_coverage_installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT to_regprocedure('public.search_current_memories_clause_union_v1(text,text,text,text,integer,text[])') IS NOT NULL")
if [ "$clause_coverage_installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory \
    -f /migrations/0008_hybrid_clause_coverage.up.sql
fi
test "$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT has_function_privilege('agentic_memory_runtime','search_current_memories_clause_union_v1(text,text,text,text,integer,text[])','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','lexical_clause_queries_v1(text)','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','han_bigram_document_v1(text)','EXECUTE') AND position('search_current_memories_clause_union_v1' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0")" = t

# 0009: comma split is detected from the function body. Unigram E'甲，乙' is 1
# clause before 0009 and 0 after (no Han 2-grams); executable count probe is E'甲乙，丙丁'.
han_comma_split_installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position(E'，、' in pg_get_functiondef('public.lexical_clause_queries_v1(text)'::regprocedure)) > 0")
if [ "$han_comma_split_installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory \
    -f /migrations/0009_han_comma_clause_split.up.sql
fi
test "$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT (SELECT count(*) FROM lexical_clause_queries_v1(E'甲乙，丙丁')) = 2 AND (SELECT preparation_status FROM lexical_clause_queries_v1('lexical beacon')) = 'single_bound' AND (SELECT local_k IS NULL FROM lexical_clause_queries_v1('lexical beacon')) AND has_function_privilege('agentic_memory_runtime','search_current_memories_clause_union_v1(text,text,text,text,integer,text[])','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','lexical_clause_queries_v1(text)','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','han_bigram_document_v1(text)','EXECUTE')")" = t

# 0010: source-block pack order is detected from the hybrid body. CREATE OR REPLACE
# preserves runtime EXECUTE on search_hybrid_current_memories; do not GRANT lexical helpers.
source_block_pack_installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('source_first' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0")
if [ "$source_block_pack_installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory \
    -f /migrations/0010_hybrid_source_block_pack_order.up.sql
fi
test "$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('source_first' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('search_current_memories_clause_union_v1' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position(E'，、' in pg_get_functiondef('public.lexical_clause_queries_v1(text)'::regprocedure)) > 0 AND has_function_privilege('agentic_memory_runtime','search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)','EXECUTE') AND has_function_privilege('agentic_memory_runtime','search_current_memories_clause_union_v1(text,text,text,text,integer,text[])','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','lexical_clause_queries_v1(text)','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','han_bigram_document_v1(text)','EXECUTE')")" = t

# 0011: lexical-head emit is detected from the hybrid body. CREATE OR REPLACE
# preserves runtime EXECUTE on search_hybrid_current_memories; do not GRANT lexical helpers.
lexical_head_pack_installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('lexical_rank<=4' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0")
if [ "$lexical_head_pack_installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory \
    -f /migrations/0011_hybrid_lexical_head_pack_order.up.sql
fi
test "$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('lexical_rank<=4' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('source_first' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('search_current_memories_clause_union_v1' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position(E'，、' in pg_get_functiondef('public.lexical_clause_queries_v1(text)'::regprocedure)) > 0 AND has_function_privilege('agentic_memory_runtime','search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)','EXECUTE') AND has_function_privilege('agentic_memory_runtime','search_current_memories_clause_union_v1(text,text,text,text,integer,text[])','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','lexical_clause_queries_v1(text)','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','han_bigram_document_v1(text)','EXECUTE')")" = t

# 0012: size-defer emit is detected from the hybrid body. CREATE OR REPLACE
# preserves runtime EXECUTE on search_hybrid_current_memories; do not GRANT lexical helpers.
size_defer_pack_installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('octet_length(c.content)>2048' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0")
if [ "$size_defer_pack_installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory \
    -f /migrations/0012_hybrid_size_defer_pack_order.up.sql
fi
test "$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('octet_length(c.content)>2048' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('lexical_rank<=4' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('source_first' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('search_current_memories_clause_union_v1' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position(E'，、' in pg_get_functiondef('public.lexical_clause_queries_v1(text)'::regprocedure)) > 0 AND has_function_privilege('agentic_memory_runtime','search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)','EXECUTE') AND has_function_privilege('agentic_memory_runtime','search_current_memories_clause_union_v1(text,text,text,text,integer,text[])','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','lexical_clause_queries_v1(text)','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','han_bigram_document_v1(text)','EXECUTE')")" = t

# 0013: rarest Han content-gram extras are detected from clause-union. CREATE OR
# REPLACE preserves runtime EXECUTE; do not GRANT lexical helpers.
rarest_han_content_gram_installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('rarest_han_content_gram' in pg_get_functiondef('public.search_current_memories_clause_union_v1(text,text,text,text,integer,text[])'::regprocedure)) > 0")
if [ "$rarest_han_content_gram_installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory \
    -f /migrations/0013_hybrid_rarest_han_content_gram.up.sql
fi
test "$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('rarest_han_content_gram' in pg_get_functiondef('public.search_current_memories_clause_union_v1(text,text,text,text,integer,text[])'::regprocedure)) > 0 AND position('octet_length(c.content)>2048' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('lexical_rank<=4' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('source_first' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position(E'，、' in pg_get_functiondef('public.lexical_clause_queries_v1(text)'::regprocedure)) > 0 AND (SELECT local_k IS NULL FROM lexical_clause_queries_v1('lexical beacon')) AND (SELECT preparation_status FROM lexical_clause_queries_v1('lexical beacon')) = 'single_bound' AND has_function_privilege('agentic_memory_runtime','search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)','EXECUTE') AND has_function_privilege('agentic_memory_runtime','search_current_memories_clause_union_v1(text,text,text,text,integer,text[])','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','lexical_clause_queries_v1(text)','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','han_bigram_document_v1(text)','EXECUTE')")" = t


# 0014: headed lex-neighbor emit is detected from extra_seq in the hybrid body.
# CREATE OR REPLACE preserves runtime EXECUTE; do not GRANT lexical helpers.
headed_lex_neighbor_pack_installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('extra_seq' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0")
if [ "$headed_lex_neighbor_pack_installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory \
    -f /migrations/0014_hybrid_headed_lex_neighbor_pack_order.up.sql
fi
test "$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('extra_seq' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('lexical_rank<=16' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('octet_length(c.content)>2048' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('lexical_rank<=4' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('source_first' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('rarest_han_content_gram' in pg_get_functiondef('public.search_current_memories_clause_union_v1(text,text,text,text,integer,text[])'::regprocedure)) > 0 AND position(E'，、' in pg_get_functiondef('public.lexical_clause_queries_v1(text)'::regprocedure)) > 0 AND (SELECT local_k IS NULL FROM lexical_clause_queries_v1('lexical beacon')) AND has_function_privilege('agentic_memory_runtime','search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)','EXECUTE') AND has_function_privilege('agentic_memory_runtime','search_current_memories_clause_union_v1(text,text,text,text,integer,text[])','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','lexical_clause_queries_v1(text)','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','han_bigram_document_v1(text)','EXECUTE')")" = t


# 0015: extra_seq>=3 after a real neighbor is detected from source_has_neighbor.
# CREATE OR REPLACE preserves runtime EXECUTE; do not GRANT lexical helpers.
headed_extra_seq_defer_installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('source_has_neighbor' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0")
if [ "$headed_extra_seq_defer_installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory \
    -f /migrations/0015_hybrid_headed_extra_seq_defer_pack_order.up.sql
fi
test "$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('source_has_neighbor' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('extra_seq>=3' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('extra_seq' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('lexical_rank<=16' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('octet_length(c.content)>2048' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('lexical_rank<=4' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('source_first' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('rarest_han_content_gram' in pg_get_functiondef('public.search_current_memories_clause_union_v1(text,text,text,text,integer,text[])'::regprocedure)) > 0 AND position(E'，、' in pg_get_functiondef('public.lexical_clause_queries_v1(text)'::regprocedure)) > 0 AND (SELECT local_k IS NULL FROM lexical_clause_queries_v1('lexical beacon')) AND has_function_privilege('agentic_memory_runtime','search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)','EXECUTE') AND has_function_privilege('agentic_memory_runtime','search_current_memories_clause_union_v1(text,text,text,text,integer,text[])','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','lexical_clause_queries_v1(text)','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','han_bigram_document_v1(text)','EXECUTE')")" = t

# 0016: Han two-head dump defer + unheaded extra_seq round-robin is detected from
# source_head_count. CREATE OR REPLACE preserves runtime EXECUTE.
han_two_head_dump_rr_installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('source_head_count>=2' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0")
if [ "$han_two_head_dump_rr_installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory \
    -f /migrations/0016_hybrid_han_two_head_dump_rr_pack_order.up.sql
fi
test "$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('source_head_count>=2' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('source_has_neighbor' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('extra_seq>=3' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('extra_seq' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('lexical_rank<=16' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('octet_length(c.content)>2048' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('lexical_rank<=4' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('source_first' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('rarest_han_content_gram' in pg_get_functiondef('public.search_current_memories_clause_union_v1(text,text,text,text,integer,text[])'::regprocedure)) > 0 AND position(E'，、' in pg_get_functiondef('public.lexical_clause_queries_v1(text)'::regprocedure)) > 0 AND (SELECT local_k IS NULL FROM lexical_clause_queries_v1('lexical beacon')) AND has_function_privilege('agentic_memory_runtime','search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)','EXECUTE') AND has_function_privilege('agentic_memory_runtime','search_current_memories_clause_union_v1(text,text,text,text,integer,text[])','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','lexical_clause_queries_v1(text)','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','han_bigram_document_v1(text)','EXECUTE')")" = t

# 0017: Han unheaded first_ok + adjacent next-passage is detected from
# han_unheaded_adj. CREATE OR REPLACE preserves runtime EXECUTE.
han_unheaded_adj_installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('han_unheaded_adj' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0")
if [ "$han_unheaded_adj_installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory \
    -f /migrations/0017_hybrid_han_unheaded_adj_pack_order.up.sql
fi
test "$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('han_unheaded_adj' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('han_unheaded_first_ok' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('source_head_count>=2' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('source_has_neighbor' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('extra_seq>=3' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('lexical_rank<=16' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('octet_length(c.content)>2048' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('lexical_rank<=4' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('source_first' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('rarest_han_content_gram' in pg_get_functiondef('public.search_current_memories_clause_union_v1(text,text,text,text,integer,text[])'::regprocedure)) > 0 AND position(E'，、' in pg_get_functiondef('public.lexical_clause_queries_v1(text)'::regprocedure)) > 0 AND (SELECT local_k IS NULL FROM lexical_clause_queries_v1('lexical beacon')) AND has_function_privilege('agentic_memory_runtime','search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)','EXECUTE') AND has_function_privilege('agentic_memory_runtime','search_current_memories_clause_union_v1(text,text,text,text,integer,text[])','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','lexical_clause_queries_v1(text)','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','han_bigram_document_v1(text)','EXECUTE')")" = t


# 0018: Han deferred sem-ok span (+1/+3) is detected from
# han_unheaded_sem_deferred. CREATE OR REPLACE preserves runtime EXECUTE.
han_deferred_span_installed=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('han_unheaded_sem_deferred' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0")
if [ "$han_deferred_span_installed" != t ]; then
  compose exec -T postgres psql -X -v ON_ERROR_STOP=1 \
    -U agentic_memory_migrator -d agentic_memory \
    -f /migrations/0018_hybrid_han_deferred_span_pack_order.up.sql
fi
test "$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT position('han_unheaded_sem_deferred' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('han_unheaded_adj' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('han_unheaded_first_ok' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('han_unheaded_pair' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('is_han_neighbor' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('source_has_head AND k.extra_seq=1' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('source_head_count>=2' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('source_has_neighbor' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('extra_seq>=3' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('lexical_rank<=16' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('octet_length(c.content)>2048' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('lexical_rank<=4' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('source_first' in pg_get_functiondef('public.search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)'::regprocedure)) > 0 AND position('rarest_han_content_gram' in pg_get_functiondef('public.search_current_memories_clause_union_v1(text,text,text,text,integer,text[])'::regprocedure)) > 0 AND position(E'，、' in pg_get_functiondef('public.lexical_clause_queries_v1(text)'::regprocedure)) > 0 AND (SELECT local_k IS NULL FROM lexical_clause_queries_v1('lexical beacon')) AND has_function_privilege('agentic_memory_runtime','search_hybrid_current_memories(text,text,text,text,integer,text[],text,text)','EXECUTE') AND has_function_privilege('agentic_memory_runtime','search_current_memories_clause_union_v1(text,text,text,text,integer,text[])','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','lexical_clause_queries_v1(text)','EXECUTE') AND NOT has_function_privilege('agentic_memory_runtime','han_bigram_document_v1(text)','EXECUTE')")" = t
