# Repository scope

This repository is the independently usable `governed-memory` engine (planned CLI: `gmem`). Keep native storage, ACL enforcement, retrieval, revisions, correction/forget, ingestion contracts, migrations, and their fixtures here.

Host application policy, product UX, and orchestration live out of tree. Local copies of host-coupled notes, if present, are under `docs/private/` and are not published.

If `docs/architecture/agentic-memory-implementation-checklist.md` exists on disk, use it for native progress. Preserve an ongoing memory task's source, fixtures, and evidence. Never reset its synthetic database as part of unrelated work.

## Working in this repository

- Runtime behavior is in `src/lib.rs`; storage and privilege changes are in `migrations/`; public-operation tests are in `tests/`. `Cargo.toml`, `Cargo.lock`, and `rust-toolchain.toml` define the dependencies and toolchain.
- Preserve tenant/private isolation, credential-class separation, ACL enforcement, current revision/source/extraction identity, revocation, non-disclosure, transaction atomicity, and sanitized errors/logs. Retrieval relevance and model output cannot grant authority.
- For native implementation slices, follow the checklist's public-operation failing-test → minimum implementation → passing-test workflow when that checklist is present. A plan or benchmark run alone does not pass a gate.
- Current native completeness slice is **P2** (job/authority races). Ledger: `docs/architecture/agentic-memory-p2-job-authority-races.md`. P1 HTTP grid: `docs/architecture/agentic-memory-p1-acl-matrix.md`. Do not start P4 production ingest, LoCoMo, Qdrant, or `0019` unless the user asks. Never reset frozen capture ports 55456–55468.

## Verification and fixture ownership

- Documentation-only changes need link/instruction checks, not a database reset. For Rust edits, use `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and the relevant tests for the changed contract.
- Inspect the relevant `justfile` recipe before running it. `just verify-clean`, `just fixture`, database integration tests, and benchmark recipes may recreate or reset the shared synthetic database. Do not run them against an active memory task's fixture.
- Run database-backed checks serially. Preserve frozen captures, ledger entries, baselines, and held-out evidence when those files exist locally.
- Stop when the requested scope and its applicable checks are complete.
