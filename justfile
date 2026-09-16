compose := "env COMPOSE_REMOVE_ORPHANS=false docker-compose --file compose.yaml --project-name agentic-memory-fixture --project-directory ."

setup:
    {{compose}} up -d --wait
    sh scripts/apply-n3-migration.sh

fixture:
    sh scripts/fixture-preflight.sh check
    {{compose}} up -d --wait
    sh scripts/apply-n3-migration.sh
    @{{compose}} exec -T postgres psql -X -v ON_ERROR_STOP=1 -U agentic_memory_migrator -d agentic_memory -f /fixtures/issue-credentials.sql

fmt:
    cargo fmt --check

lint:
    cargo clippy --all-targets -- -D warnings

test:
    cargo test

verify: fmt lint test

verify-clean:
    sh scripts/verify-clean.sh

benchmark:
    sh scripts/fixture-preflight.sh check
    just setup
    cargo test --release --test read_item retrieval_benchmark::before_after -- --ignored --exact --nocapture --test-threads=1

benchmark-office scale="both":
    sh scripts/fixture-preflight.sh check
    just setup
    OFFICE_BENCH_SCALE={{scale}} cargo test --release --test read_item office_retrieval_benchmark::office_corpus -- --ignored --exact --nocapture --test-threads=1

benchmark-office-large:
    sh scripts/fixture-preflight.sh check
    just setup
    cargo test --release --test read_item office_retrieval_benchmark::office_large_files -- --ignored --exact --nocapture --test-threads=1

benchmark-office-parse:
    sh scripts/fixture-preflight.sh check
    cargo test --release --test office_documents actual_large_office_files_parse_benchmark -- --ignored --exact --nocapture --test-threads=1

benchmark-n1:
    sh scripts/fixture-preflight.sh check
    just setup
    cargo test --release --test read_item office_retrieval_benchmark::n1_retrieval_baseline -- --ignored --exact --nocapture --test-threads=1

benchmark-n2 split="development":
    sh scripts/fixture-preflight.sh check
    just setup
    N2_BENCH_SPLIT={{split}} cargo test --release --test read_item office_retrieval_benchmark::n2_lexical_evaluation -- --ignored --exact --nocapture --test-threads=1

test-n2:
    just setup
    cargo test --test read_item office_retrieval_benchmark::n2_public_search_prepares_questions_without_changing_explicit_operators -- --ignored --exact --test-threads=1
    cargo test --test read_item office_retrieval_benchmark::n2_indexes_bounded_source_structure_but_returns_only_original_evidence -- --ignored --exact --test-threads=1

test-n3-migrations:
    sh scripts/test-n3-migrations.sh

test-n3:
    just setup
    cargo test --test n3_semantic -- --test-threads=1
