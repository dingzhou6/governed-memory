#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
compose_file="$repository_root/compose.yaml"
project_name=agentic-memory-fixture
capture_dir=$(mktemp -d "${TMPDIR:-/tmp}/agentic-memory-fixture.XXXXXX")
trap 'rm -rf "$capture_dir"' EXIT HUP INT TERM
chmod 700 "$capture_dir"
umask 077
fixture_output="$capture_dir/fixture.out"
database_output="$capture_dir/database.out"
persistent_output="$capture_dir/persistent.out"
runtime_output="$capture_dir/runtime.out"

compose() {
  COMPOSE_REMOVE_ORPHANS=false docker-compose --file "$compose_file" --project-name "$project_name" --project-directory "$repository_root" "$@"
}

command -v shasum >/dev/null 2>&1 || {
  echo 'Synthetic fixture verification requires shasum' >&2
  exit 1
}

(cd "$repository_root" && just fixture) >"$fixture_output"
for label in ALICE_READER_BEARER BOB_READER_BEARER ALICE_WRITER_BEARER; do
  count=$(grep -Ec "^${label}=[[:xdigit:]]{64}$" "$fixture_output") || {
    echo "Synthetic fixture did not emit exactly one valid $label" >&2
    exit 1
  }
  if [ "$count" -ne 1 ]; then
    echo "Synthetic fixture did not emit exactly one valid $label" >&2
    exit 1
  fi
done

if [ "$(grep -Ec '^(ALICE_READER_BEARER|BOB_READER_BEARER|ALICE_WRITER_BEARER)=' "$fixture_output")" -ne 3 ]; then
  echo 'Synthetic fixture emitted an unexpected credential label' >&2
  exit 1
fi

alice_reader=$(sed -n 's/^ALICE_READER_BEARER=//p' "$fixture_output")
bob_reader=$(sed -n 's/^BOB_READER_BEARER=//p' "$fixture_output")
alice_writer=$(sed -n 's/^ALICE_WRITER_BEARER=//p' "$fixture_output")
if [ "$alice_reader" = "$bob_reader" ] || [ "$alice_reader" = "$alice_writer" ] || [ "$bob_reader" = "$alice_writer" ]; then
  echo 'Synthetic fixture bearers are not unique' >&2
  exit 1
fi

alice_digest=$(printf '%s' "$alice_reader" | shasum -a 256 | awk '{print $1}')
{
  printf '%s\n' "BEGIN; SELECT set_config('app.credential_digest',\$1,true)" "\\bind '$alice_digest'" '\g /dev/null'
  printf '%s\n' "SELECT set_config('app.tenant_id','00000000000000000000000000000001',true); SELECT set_config('app.operation','read',true); SELECT 'VERIFY_READ=' || coalesce((SELECT item_id FROM read_current_item('00000000000000000000000000000001','c0000000000000000000000000000001','00000000-0000-0000-0000-000000000001','40000000000000000000000000000001',NULL,NULL)),'NONE'); ROLLBACK;"
  printf '%s\n' "BEGIN; SELECT set_config('app.credential_digest',\$1,true)" "\\bind '$alice_digest'" '\g /dev/null'
  printf '%s\n' "SELECT set_config('app.tenant_id','00000000000000000000000000000001',true); SELECT set_config('app.operation','search',true); SELECT 'VERIFY_OMITTED=' || coalesce((SELECT item_id FROM search_current_memories('00000000000000000000000000000001','c0000000000000000000000000000001','00000000-0000-0000-0000-000000000002','SCOPED OPERATOR HANDBOOK SENTINEL',4096,NULL)),'NONE'); SELECT 'VERIFY_MATCH=' || coalesce((SELECT item_id FROM search_current_memories('00000000000000000000000000000001','c0000000000000000000000000000001','00000000-0000-0000-0000-000000000003','SCOPED OPERATOR HANDBOOK SENTINEL',4096,ARRAY['customer_alpha','project_alpha'])),'NONE'); SELECT 'VERIFY_WRONG=' || coalesce((SELECT item_id FROM search_current_memories('00000000000000000000000000000001','c0000000000000000000000000000001','00000000-0000-0000-0000-000000000004','SCOPED OPERATOR HANDBOOK SENTINEL',4096,ARRAY['customer_other','project_alpha'])),'NONE'); ROLLBACK;"
} |
  compose exec -T -e PGPASSWORD=synthetic-runtime-only postgres psql -X -At -v ON_ERROR_STOP=1 -U agentic_memory_runtime -d agentic_memory >"$runtime_output"
runtime_results=$(grep '^VERIFY_' "$runtime_output")
expected_results='VERIFY_READ=40000000000000000000000000000001
VERIFY_OMITTED=NONE
VERIFY_MATCH=40000000000000000000000000000009
VERIFY_WRONG=NONE'
if [ "$runtime_results" != "$expected_results" ]; then
  echo 'Restricted fixture read/search verification failed' >&2; exit 1
fi

manifest_counts=$(compose exec -T postgres psql -X -At -F '|' -v ON_ERROR_STOP=1 -U agentic_memory_migrator -d agentic_memory -c "SELECT (SELECT count(*) FROM apps),(SELECT count(*) FROM subjects),(SELECT count(*) FROM items),(SELECT count(*) FROM revisions),(SELECT count(*) FROM lexical_representations),(SELECT count(*) FROM revision_subjects)")
[ "$manifest_counts" = '3|7|4|4|4|2' ] || { echo 'Synthetic fixture manifest counts are invalid' >&2; exit 1; }

compose exec -T postgres psql -X -At -F '|' -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT id, tenant_id, principal_id, app_id, encode(token_digest, 'hex'), credential_class,
          array_to_string(allowed_operations, ','),
          issued_at <= clock_timestamp(), expires_at > clock_timestamp(),
          expires_at <= issued_at + interval '24 hours', revoked_at IS NULL
   FROM credentials ORDER BY id" >"$database_output"

expected_rows=3
if [ "$(wc -l <"$database_output")" -ne "$expected_rows" ]; then
  echo 'Synthetic fixture database credential count is invalid' >&2
  exit 1
fi
for specification in \
  "c0000000000000000000000000000001|00000000000000000000000000000001|10000000000000000000000000000001|a0000000000000000000000000000001|$alice_reader|agent_reader|list,search,read" \
  "c0000000000000000000000000000002|00000000000000000000000000000001|10000000000000000000000000000002|a0000000000000000000000000000001|$bob_reader|agent_reader|list,search,read" \
  "c0000000000000000000000000000003|00000000000000000000000000000001|10000000000000000000000000000001|a0000000000000000000000000000001|$alice_writer|trusted_writer|create,correct,forget"
do
  credential_id=${specification%%|*}
  remainder=${specification#*|}
  tenant_id=${remainder%%|*}; remainder=${remainder#*|}
  principal_id=${remainder%%|*}; remainder=${remainder#*|}
  app_id=${remainder%%|*}; remainder=${remainder#*|}
  bearer=${remainder%%|*}; remainder=${remainder#*|}
  credential_class=${remainder%%|*}; operations=${remainder#*|}
  digest=$(printf '%s' "$bearer" | shasum -a 256 | awk '{print $1}')
  expected="$credential_id|$tenant_id|$principal_id|$app_id|$digest|$credential_class|$operations|t|t|t|t"
  actual=$(awk -F '|' -v id="$credential_id" '$1 == id { print; count++ } END { if (count != 1) exit 1 }' "$database_output") || actual=
  if [ "$actual" != "$expected" ]; then
    echo "Synthetic fixture database binding is invalid for $credential_id" >&2
    exit 1
  fi
done

runtime_crypto_privileges=$(compose exec -T postgres psql -X -At -F '|' -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory -c \
  "SELECT has_function_privilege('agentic_memory_runtime', 'digest(text,text)', 'EXECUTE'),
          has_function_privilege('agentic_memory_runtime', 'gen_random_bytes(integer)', 'EXECUTE')")
if [ "$runtime_crypto_privileges" != 'f|f' ]; then
  echo 'Runtime role unexpectedly has fixture cryptography privileges' >&2
  exit 1
fi

if ! (ulimit -f 1024 && compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 \
  -U agentic_memory_migrator -d agentic_memory <<'SQL'
SELECT format('SELECT row_to_json(t)::text FROM public.%I AS t;', tablename)
FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename
\gexec
SQL
) >"$persistent_output" 2>/dev/null; then
  echo 'Synthetic fixture persistent-data scan could not be captured safely' >&2
  exit 1
fi
if [ "$(wc -c <"$persistent_output")" -ge 1048576 ]; then
  echo 'Synthetic fixture persistent-data scan reached the safety ceiling' >&2
  exit 1
fi
for bearer in "$alice_reader" "$bob_reader" "$alice_writer"; do
  persisted=false
  while IFS= read -r row; do
    case "$row" in *"$bearer"*) persisted=true; break ;; esac
  done <"$persistent_output"
  if [ "$persisted" = true ]; then
    echo 'Synthetic fixture bearer was persisted in plaintext' >&2
    exit 1
  fi
done

echo 'Synthetic fixture credential issuance check passed'
