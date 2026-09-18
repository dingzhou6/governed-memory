# governed-memory

A Rust/PostgreSQL memory engine with tenant ACL, current revisions, and correct/forget. Retrieval is not authority.

Repository: https://github.com/dingzhou6/governed-memory

| Layer | Name |
| --- | --- |
| Crate / repo | `governed-memory` |
| Planned CLI | `gmem` |
| Rust import | `governed_memory` |

This tree is an HTTP library and synthetic fixture, not a production deployment. The `gmem` command is not shipped yet.

## What it does

Authenticated search, list, and read. Trusted-writer create, correct, and forget. Opaque bearer credentials, tenant isolation, exact current revisions, and document passages. Semantic search is opt-in exact cosine over caller-supplied vectors; no embedding provider is required.

## Clean verification

Prerequisites: Docker (tested with `docker-compose` 5.1.0), `just`, `rustup`, `shasum`, and local port `55432`.

```sh
just verify-clean
```

That command recreates only the dedicated `agentic-memory-fixture` PostgreSQL volume, then runs format, lint, and public-operation tests on synthetic data. It is not a signup or real-data workflow.

See the [engine contract](docs/architecture/agentic-memory-engine-design.md), [native glossary](docs/memory/CONTEXT.md), and [TypeSafe adaptation plan](docs/architecture/typesafe-adaptation-plan.md).
