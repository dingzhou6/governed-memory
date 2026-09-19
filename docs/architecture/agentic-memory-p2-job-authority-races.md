# P2 — Job / authority races

**Started:** 2026-09-19  
**Status:** Named public HTTP race suite green on 55432. Remaining cells below are explicit gaps, not silent omissions. Not production-ready.  
**Owner:** `governed-memory` (this crate). Host projection stays out of tree.

This is the working ledger for the native race slice that P1 deferred. Come back here before adding ingest, LoCoMo, or more ACL grid cases.

Related: [P1 ACL matrix](agentic-memory-p1-acl-matrix.md) (HTTP grid + I02, done), [implementation checklist](agentic-memory-implementation-checklist.md) required-benchmark row “Job/authority races”, [engine contract](agentic-memory-engine-design.md) (jobs, `stopped_stale`, extraction activation).

## What this slice is

A worker may already hold a lease when the current item, extraction set, or credential changes. The engine must not let that late commit:

- activate a **partial** extraction set
- write embeddings for a **superseded** revision or set
- report semantic **ready** on stale input
- **duplicate** a mutation (idempotency / lease replay)
- leak forgotten, withdrawn, or revoked evidence on GET / search / list / hybrid

Existing SQL fencing lives in `tests/n3_semantic.rs` (`crash_retry_and_lifecycle_changes_cannot_activate_stale_evidence`, `claim_rechecks_authority_after_the_tenant_barrier`). P2 makes the same contract **named on public HTTP** (and records any remaining SQL-only cells).

## Order of work

1. **This page (P2)** — named race suite on 55432.
2. **Later:** P4 production ingest; P5 embeddings; LoCoMo on a **new** DB; N5 / ADR-0127.
3. **Not in scope:** Qdrant; overlay `0019`; resetting frozen capture ports.

## Do not touch

- Frozen long-context captures on **55468** (and 55456–55467). Dual-gate originals stay 12/12 complete and 8/8 answerable on 0018.
- `just verify-clean` / `just fixture` against those DBs.
- Overlay `0019` on 55468.
- Treating retrieval rank as authority.

P2 tests use the ordinary synthetic fixture on **55432**, serially (`--test-threads=1`).

## Contract matrix

| Case | Public visibility | P2 status |
| --- | --- | --- |
| HTTP correct while an embedding job is leased | GET/search show the new revision; late `complete_embedding_job` is `stopped_stale`; no old-revision vectors | **Covered** `p2_http_correct_fences_inflight_embedding_job` |
| HTTP forget while a job is leased | GET 404 / search empty; late complete is `stopped_stale` | **Covered** `p2_http_forget_and_withdraw_fence_inflight_jobs` |
| Withdraw or revoke while a job is leased | HTTP search/GET hide the target; late complete is `stopped_stale` | **Covered** withdraw in `p2_http_forget_and_withdraw_fence_inflight_jobs`; owner deactivate in `p2_owner_revoke_fences_inflight_embedding_job` |
| Extract-replace while a passage job is leased | Old spans gone on lexical search; late complete is `stopped_stale`; no old-passage vectors | **Covered** `p2_extract_replace_fences_inflight_passage_job` |
| Concurrent HTTP correct, same `expected_revision_id` | One current revision; loser `stale_context`; GET cites only the winner | **Covered** `p2_concurrent_correct_and_duplicate_create_are_single_mutations` |
| Duplicate create, same idempotency key + body | Replay, same `item_id`, no second item | **Covered** in the same test |
| Lease crash: old attempt completes after reclaim | Old attempt `stopped_stale`; reclaimed attempt may complete once; no duplicate representation | **Covered** `p2_expired_lease_late_complete_is_stopped_stale` |
| Claim waits on tenant authority, then sees revoke | SQL barrier in N3 | **Deferred** to keep P2 on public HTTP; N3 `claim_rechecks_authority_after_the_tenant_barrier` remains the evidence |
| Host revoke between search and disclosure | ADR-0127 | **Host / N5** |

## Implementation slices (check off here)

- [x] **P2.0** Named suite `tests/p2_job_authority_races.rs` on 55432. Must not reset 55468.
- [x] **P2.1** HTTP correct fences an in-flight embedding job.
- [x] **P2.2** HTTP forget, withdraw, and revoke fence in-flight jobs without leaking current evidence.
- [x] **P2.3** Extract-replace fences in-flight passage jobs; incomplete replacement is not searchable.
- [x] **P2.4** Concurrent correct + create idempotency: one current mutation.
- [x] **P2.5** Crash/lease expiry: late complete of the old attempt is `stopped_stale`.
- [x] **P2.6** Remaining cells: N3 claim-barrier stays SQL evidence; ADR-0127 → N5. Hybrid-after-crash not separately named (GET current evidence is).

**P2 exit:** each matrix cell maps to a named public test **or** an explicit gap on this page.

## Verification for this slice

`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, then `cargo test --test p2_job_authority_races -- --test-threads=1`. Also rerun `cargo test --test p1_acl_matrix -- --test-threads=1` if you touch shared fixture helpers. Inspect `justfile` before `just verify-clean`.
