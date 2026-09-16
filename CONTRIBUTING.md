# Contributing

This repository is the `governed-memory` engine. The planned CLI name is `gmem`; no CLI is shipped yet.

Use synthetic fixture data only. Do not commit `.env`, secrets, home-directory paths, or host-application paths.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

`just verify-clean` recreates the dedicated synthetic PostgreSQL fixture. Do not run it against an in-use memory-task database.

Public operations live in `src/lib.rs`. Schema and privilege changes live in `migrations/`.
