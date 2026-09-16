#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
compose_file="$repository_root/compose.yaml"
project_name=agentic-memory-fixture

compose() {
  COMPOSE_REMOVE_ORPHANS=false docker-compose --file "$compose_file" --project-name "$project_name" --project-directory "$repository_root" "$@"
}

sh "$repository_root/scripts/fixture-preflight.sh" delete
(cd "$repository_root" && just setup)
(cd "$repository_root" && sh scripts/verify-fixture.sh)
(cd "$repository_root" && just verify)

log_file=$(mktemp "${TMPDIR:-/tmp}/agentic-memory-postgres-logs.XXXXXX")
trap 'rm -f "$log_file"' EXIT HUP INT TERM
if ! (ulimit -f 1024 && compose logs --no-color postgres) >"$log_file" 2>/dev/null; then
  echo 'PostgreSQL logs could not be captured within the 1 MiB limit' >&2
  exit 1
fi
log_size=$(wc -c <"$log_file") || {
  echo 'PostgreSQL log size could not be checked' >&2
  exit 1
}
if [ "$log_size" -ge 1048576 ]; then
  echo 'PostgreSQL logs reached the 1 MiB safety ceiling' >&2
  exit 1
fi
if grep -Eiq 'SENTINEL|DUPLICATE_[AB]|TRAILING_DATA|TOO_MANY_SUBJECTS|TOO_DEEP|TOO_MANY_MEMBERS|IMMUTABILITY_VIOLATION|WRONG_TYPE|CAP_RESULT_TOKEN|ALTERNATIVE PRINCIPAL MATCH|PRIVATE CREATE|NO SUCH MEMORY QUERY|STALE HISTORY|CROSS APP SUBJECT|LIST_(WITHDRAWN|DELETED|EXPIRED|FUTURE|PAGE|STORAGE)|FORGED_|FORBIDDEN_|ALLOWED_ALPHA_HANDBOOK|RAW_|OVERSIZE_|synthetic body failure|body[ _-]?error|authorization[[:space:]_:=-]|bearer[[:space:]]+[A-Za-z0-9._~-]+|(^|[^[:xdigit:]])[[:xdigit:]]{64}([^[:xdigit:]]|$)' "$log_file"; then
  echo 'PostgreSQL log non-disclosure check failed' >&2
  exit 1
else
  scan_status=$?
  if [ "$scan_status" -ne 1 ]; then
    echo 'PostgreSQL log scan could not run' >&2
    exit "$scan_status"
  fi
fi
echo 'Clean synthetic verification and PostgreSQL log non-disclosure check passed'
