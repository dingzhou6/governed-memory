# P1 — Public-operation ACL / tenant matrix

**Started:** 2026-09-19  
**Status:** HTTP grid + I02 named suite green on 55432. Current native slice is **P2** job/authority races: [agentic-memory-p2-job-authority-races.md](agentic-memory-p2-job-authority-races.md). Not production-ready.  
**Owner:** `governed-memory` (this crate). Host projection, chat reuse, and upload UX stay out of tree.

This is the working ledger for making the current engine’s **authority contract** checkable as one named suite. Come back here before adding ACL tests, ingest, or retrieval work.

Related: [implementation checklist](agentic-memory-implementation-checklist.md) (progress + N5), [engine contract](agentic-memory-engine-design.md) (routes), [service plan](agentic-memory-service-plan.md) (scope). Source outline: local `02-governed-memory-correction-and-enhancement-plan` (2026-09-17, snapshot `38e3524`). That file is not in this repo and is not a second ledger.

## Order of work

1. **This page (P1)** — named public-operation ACL/tenant matrix.
2. **Later, not this goal:** P4 bounded production ingestion (office parsers today are fixture activation, not upload/sandbox).
3. **Later, not this goal:** P5 real embeddings in-engine; LoCoMo retrieval on a **new** DB; N5 host ADR-0127 / capacity / recovery.
4. **Not in scope:** Qdrant/S1 storage rewrite (PostgreSQL remains the selected backend); `0019` source-span fill unless the user asks to ship it; resetting frozen capture ports.

## Do not touch

- Frozen long-context captures on **55468** (and 55456–55467). Dual-gate originals stay 12/12 complete and 8/8 answerable on 0018.
- `just verify-clean`, `just fixture`, or other recipes that recreate the shared synthetic DB while a frozen memory task is active.
- Installing overlay `0019` on 55468.
- Treating retrieval rank, a `confirmed` field, or model confidence as write/read authority.

P1 tests use the ordinary synthetic fixture on **55432** (same pattern as `tests/read_item.rs`).

## Public operations

| Method | Path | Credential class |
| --- | --- | --- |
| GET | `/v1/collections` | `agent_reader` (`list`) |
| GET | `/v1/items` | `agent_reader` (`list`) |
| GET | `/v1/items/{id}` | `agent_reader` (`read`) |
| POST | `/v1/search` | `agent_reader` (`search`); lexical and hybrid |
| POST | `/v1/memories` | `trusted_writer` (`create`) |
| PUT | `/v1/items/{id}` | `trusted_writer` (`correct`) |
| DELETE | `/v1/items/{id}` | `trusted_writer` (`forget`) |

## Contract matrix

Legend: **covered** = named case in `tests/p1_acl_matrix.rs` (or kept in an existing public tracer and not retuned); **deferred** = explicit later slice, not a silent omission; **host** = N5 / out of tree.

| Contract | Cases | Named evidence | P1 status |
| --- | --- | --- | --- |
| Authentication | Missing, unknown, expired, revoked, forged query/body; auth before untrusted scope | `p1_http_auth_and_tenant_grid`; hybrid missing/expired/revoked in `p1_hybrid_search_does_not_leak_and_replacing_extraction_hides_old_spans` | **Covered** on all seven routes. Forged tenant/principal/scope with a missing, unknown, or expired bearer is 401 and does not echo `FORGED_*`. A valid Alice token plus a forged query is 400 malformed. |
| Wrong operation | Reader used as writer; search-only cannot read/list; list-only cannot search/read | `p1_http_auth_and_tenant_grid` | **Covered.** |
| Wrong / inactive app | Inactive app; credential app ≠ resource app | Same test: wrong-app reader lists empty / GET 404 / lexical search empty; wrong-app writer create/correct/forget 404 unavailable without mutating Alpha; inactive Alpha app 401 on reader routes. Hybrid wrong-app in the hybrid test returns 200 with no Alpha/Bob spans. | **Covered.** |
| Resource policy | Cross-tenant IDs; Bob private vs Alice; restricted grants; guessed missing IDs | GET uniform unavailable in the auth grid; lists/search isolation in `p1_http_list_and_search_keep_alice_bob_and_beta_isolated`; grants on/off in `p1_http_grants_withdraw_and_forget_hide_current_evidence`; hybrid Alice/Bob in the hybrid test. Private ACL stays owner-only even if a grant row exists on Bob’s collection. | **Covered.** |
| Applicability | Empty/forged/unknown subject selectors; no tenant-wide expansion | Auth grid: empty `scope.subjects` and `missing_document_scope` are 400 malformed and do not return Alpha handbook. | **Covered** for lexical search. **Deferred:** hybrid-scoped subjects (same admission function; not a separate named hybrid case); write-path subject applicability stays in the core create/correct tracer. |
| Current evidence | Expired validity, withdrawn collection, forget; list + lexical + hybrid | Grants test: expired current revision hides GET/search/list; withdraw hides GET/search/list/collections; forget hides GET/search/list. Hybrid withdraw/forget in the hybrid test. | **Covered.** Item-level supersede (correct) stays in the core tracer; passage supersede is I02 below. |
| Extraction set (I02) | Replace set without editing the item; incomplete set | Hybrid test: old lexical+hybrid spans disappear after replace; incomplete replacement (jobs not completed) returns 200 with neither old nor new span; completed new set is visible. | **Covered.** |
| Mutations | Expected-revision conflict; exact correct/forget; idempotency; concurrent writers | Core tracer in `tests/read_item.rs` | **Covered** there. Do not retune under P1. |
| Non-disclosure | Uniform unavailable; denials have no snippets, `FORGED_*`, or private beacons | GET private/foreign/missing envelopes match; search/list empty rather than 401 when the bearer is valid; hybrid denials are 401 only for bad credentials, else empty/no-beacon. | **Covered** for the named suite. |
| Limits / failures | Malformed JSON, oversize body, timeouts, semantic degradation | Existing GET/search limit tests and N3 semantic degradation | **Covered** there. Preserve fail-closed degradation; no auth bypass. Not duplicated in the P1 suite. |
| Job / authority races | Correct / extract-replace / withdraw / revoke / forget / crash; no partial active set | Checklist required-benchmark row; N3 some fencing | **Deferred** to a later native slice. Not in this HTTP-grid goal. |
| Managed projection | ADR-0127, oversized ACL, revoke between search and host disclosure | N5 unchecked | **Host / N5.** Out of this P1 goal. |

## Implementation slices (check off here)

- [x] **P1.0** Named suite `tests/p1_acl_matrix.rs` exists and is the place later sessions extend. It may wrap existing assertions; it must not reset 55468.
- [x] **P1.1** HTTP authentication grid: missing, unknown, expired, revoked, forged query/body, wrong-app, inactive app, and wrong-operation on all seven routes (`p1_http_auth_and_tenant_grid`). Writer missing is the unauthenticated loop; writer revoked/expired leave the target item unchanged.
- [x] **P1.2** HTTP tenant/audience grid: Alpha vs Beta, Alice vs Bob private, restricted grant on/off, guessed IDs, list/collection isolation, expired current revision, withdraw, forget (`p1_http_list_and_search_keep_alice_bob_and_beta_isolated`, `p1_http_grants_withdraw_and_forget_hide_current_evidence`).
- [x] **P1.3** Hybrid `POST /v1/search` does not leak across tenant, class, wrong-app, revoke, expire, forget, or withdraw (`p1_hybrid_search_does_not_leak_and_replacing_extraction_hides_old_spans`). Relevance cannot widen the result.
- [x] **P1.4** I02: replacing an extraction set without editing the item hides old passages on lexical **and** hybrid search; an incomplete replacement does not resurrect old spans.
- [x] **P1.5** Remaining cells are explicit: job/authority races → later native slice; ADR-0127 / managed projection → N5; mutations and limit/degradation tests stay in existing tracers; hybrid-scoped subjects not separately named.

Come back here (not the Downloads plan) before starting P4, LoCoMo, Qdrant, or `0019`.

**P1 exit:** each matrix cell maps to a public test **or** an explicit gap on this page. The named suite is runnable on an exclusively owned 55432-style fixture, serially.

## Later (do not start under this goal)

| Later | Why it waits |
| --- | --- |
| P4 production ingest | Fixture parsers + atomic activation exist; upload admission, immutable originals, and parser sandbox are N5. |
| P5 real embeddings | Caller-supplied vectors already work; fixture hash models must not be selectable in production. |
| LoCoMo | Retrieval quality on a **new** DB after P1; cannot prove tenant ACL. |
| N5 / P8 | Capacity, backup/restore, production listener (`src/main.rs` does not exist). |
| S1 Qdrant | Repo already selected hosted-first PostgreSQL. |

## Verification for this slice

Rust: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, then `cargo test --test p1_acl_matrix -- --test-threads=1` (and any reused `read_item` / `n3_semantic` cases you touched). Inspect `justfile` before `just verify-clean`. Documentation-only edits do not reset a database.
