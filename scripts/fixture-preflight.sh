#!/bin/sh
set -eu
mode=${1:-check}; root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
container=agentic-memory-fixture-postgres; volume=agentic-memory-fixture-postgres-data
containers=$(docker container ls -a --format '{{.Names}}') || { echo 'Fixture container inventory failed' >&2; exit 1; }
volumes=$(docker volume ls --format '{{.Name}}') || { echo 'Fixture volume inventory failed' >&2; exit 1; }
has_container=false; has_volume=false
for name in $containers; do [ "$name" = "$container" ] && has_container=true; done
for name in $volumes; do [ "$name" = "$volume" ] && has_volume=true; done
if [ "$has_container" = true ]; then
  fixture=$(docker container inspect --format '{{ index .Config.Labels "com.consultai.fixture" }}' "$container") || exit 1
  project=$(docker container inspect --format '{{ index .Config.Labels "com.docker.compose.project" }}' "$container") || exit 1
  service=$(docker container inspect --format '{{ index .Config.Labels "com.docker.compose.service" }}' "$container") || exit 1
  config=$(docker container inspect --format '{{ index .Config.Labels "com.docker.compose.project.config_files" }}' "$container") || exit 1
  working=$(docker container inspect --format '{{ index .Config.Labels "com.docker.compose.project.working_dir" }}' "$container") || exit 1
  [ "$fixture" = agentic-memory-synthetic-v1 ] && [ "$project" = agentic-memory-fixture ] && [ "$service" = postgres ] && { [ -z "$config" ] || [ "$config" = "$root/compose.yaml" ]; } && { [ -z "$working" ] || [ "$working" = "$root" ]; } || { echo 'Refusing fixture operation: container identity mismatch' >&2; exit 1; }
fi
if [ "$has_volume" = true ]; then
  fixture=$(docker volume inspect --format '{{ index .Labels "com.consultai.fixture" }}' "$volume") || exit 1
  project=$(docker volume inspect --format '{{ index .Labels "com.docker.compose.project" }}' "$volume") || exit 1
  key=$(docker volume inspect --format '{{ index .Labels "com.docker.compose.volume" }}' "$volume") || exit 1
  [ "$fixture" = agentic-memory-synthetic-v1 ] && [ "$project" = agentic-memory-fixture ] && [ "$key" = agentic-memory-postgres ] || { echo 'Refusing fixture operation: volume identity mismatch' >&2; exit 1; }
fi
if [ "$mode" = delete ]; then
  [ "$has_container" = false ] || docker container rm -f "$container" >/dev/null
  [ "$has_volume" = false ] || docker volume rm "$volume" >/dev/null
elif [ "$mode" != check ]; then exit 2; fi
