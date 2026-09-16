# Native Rust memory — service scope

Status: scope and roadmap, reviewed 2026-09-07. This service is independent of any one host application; host policy is not part of the native domain.

This page owns **scope**, the [engine contract](agentic-memory-engine-design.md) owns native implementation requirements, and the implementation checklist alone owns current progress and evidence. Historical dashboard-first phases and “implementation paused/no implementation exists” statements are removed; they no longer describe the active headless goal.

## Decision and scope

Build an independently deployable service for authorized memory/evidence retrieval, exact revisions, applicability and controlled lifecycle. A host application is not a required dependency. A native tenant does not acquire company owners, employee onboarding, departments or publication reviewers implicitly.

In scope: scoped identity and operation enforcement; collections/items/revisions; direct and lexical retrieval; governed document passages; correction/forget; local audit and idempotent receipts. Semantic representations, production ingestion, processing jobs and portable data are later capabilities with their own gates.

Out of scope for Rust: agent/conversation orchestration, “remember this?” UX, company policy and approval workflows, provider OAuth, business actions, billing, workflow automation and host notification feeds. Versioned skill text may later be stored as content; installing or executing a skill and granting its tools belong to the host. No standalone dashboard, automatic capture, MCP server or CLI hook is implied by the current goal.

## Runtime and storage

- Hosted-first PostgreSQL is the selected storage direction. Reuse its relational constraints and lexical search; pgvector is planned, not evidence of implemented semantic recall.
- Memory has its own tables, migrations and credentials; it must not join ConsultAI's business tables. A shared PostgreSQL server is a deployment option, not a shared data-ownership boundary.
- Standard tenants may share runtime/storage under logical isolation. Dedicated deployment separates runtime, storage, credentials and backups; it does not promise secrecy from the privileged operator.
- The current artifact is a library/router and synthetic PostgreSQL harness. A production listener, credential provisioning, service admission, recovery and operating limits need their own release work.
- No second database, vector cluster or worker fleet initially. Bounded background processing can share the service deployment when implemented, but parser isolation and CPU/memory admission remain separate from HTTP request handling.
- Standalone uploaded originals may use an immutable local volume; managed uploads remain in Go-owned R2. The native service receives admitted bounded input/source identity, never broad platform/provider credentials.

See the [engine file contract](agentic-memory-engine-design.md#files-and-other-resources) for office-format coverage and limits. Existing synthetic fixture credentials and reset migrations are not a production provisioning mechanism.

## Content model

| Native object | Meaning |
| --- | --- |
| Tenant / Principal / App | Distinct identity and scope boundaries |
| Collection | One audience for its items, using fixed native authority |
| Item / Revision | Stable identity and immutable current content state |
| Applicability | Which subjects/circumstances an assertion concerns, not its readers |
| Source Revision / Extraction Set / Source Passage | Exact governed source, complete processing result and citable evidence |
| Derived Representation | Optional summary/embedding tied to current permitted inputs |
| Operation Receipt / Audit / Deletion Marker | Commit evidence, sanitized accountability and non-resurrection controls |

Use the [native glossary](../memory/CONTEXT.md). A small preference needs no hierarchy of summaries. Similarity and repeated model statements do not establish truth, consent, shared publication or same-fact identity.

## ACL: mandatory in every native operation

The trusted credential binds tenant, principal, app, operation and expiry. Body/path/filter fields only narrow that scope. Reader authority does not permit writes; a separately authorized trusted writer binds the exact request. Memory validates both the operation and current resource authority, not just a host-provided “allowed” flag.

Use the [native ordering and schema contract](agentic-memory-engine-design.md#first-core-native-contract--implementation-entry). Restrictive runtime grants, constrained SQL entry points, tenant-qualified references and post-lock authority checks work together. RLS alone is not a proof of isolation, especially for privileged definer functions.

### Audiences and capabilities

Private collections are owned by a native principal; restricted collections use native grants. The implemented fixture model is not a full group/role administration system. ConsultAI's Company-wide audience, Owner read policy and reviewer eligibility are translated by Go according to the host design, never inferred from role names in Rust.

### Standalone versus ConsultAI-managed authority

The first core uses trusted native setup and scoped, expiring/revocable opaque credentials. It has no dynamic signup, grant administration or managed impersonation.

Managed integration later accepts only the trusted host's bound operations and one-way native projection under ADR-0127. The same tenant cannot accept a second native authority editor. Workload authentication, on-behalf-of binding, projection ordering and reconciliation must be proved before enabling that mode. Standalone service credentials are not a shortcut to managed Employee impersonation.

### Prevent information leaks

Return only metadata/content authorized for the native operation; snippets, counts, citations, derived content, cursors and job results are protected too. Enforce current revisions, applicability and source/processing eligibility before release. Reader revocation does not withdraw evidence from everyone; source withdrawal does.

Hosts must separately enforce intended recipients, external-source permission and model egress. Native grant checks cannot establish Google/Notion access. ConsultAI therefore mediates every managed disclosure.

## Retrieval and remembering

The engine owns bounded candidate selection, lexical/semantic eligibility, evidence packing and lifecycle feedback. The host decides whether to search, directly read known evidence, ask a follow-up or save a proposed memory. Its Context Packet design owns whole-prompt tokens and conversational continuity.

“Saved” means the explicit current revision and lexical representation committed durably; semantic readiness is separate. Corrections require an exact item and expected revision. Forget immediately removes selected memory and derivatives from serving, with physical cleanup and restoration behavior governed separately.

### Applicability, contradictions, and correction

Keep subjects, asserted validity and recorded time separate from audience. Current-only retrieval remains the first-core contract; historical reconstruction is unsupported until a separate slice. Similarity can suggest related evidence but cannot retarget an update, merge assertions or approve a business exception.

### Short-term context ownership and host contract

Recent conversation, compaction, required policies, answer generation, destination review and external-data save suggestions belong to ConsultAI orchestration. Another standalone host must supply equivalent disclosure safeguards for guarantees to extend beyond the native response. Memory cannot recall bytes already delivered to an arbitrary client.

## Skills and integrations

Keep versioned HTTP/JSON as the native application protocol. ConsultAI's later agent-facing MCP/CLI/UI paths go through Go; they do not expose managed native credentials. A standalone adapter must bind its own native scope. Do not implement multiple transports, a universal plugin framework or executable skill installation speculatively.

## Delivery sequence and gates

Follow the active implementation checklist, not the historical dashboard-first roadmap. That task owns source/migrations/tests and its evidence. Proposed host work does not expand it.

Later native slices include checked query/schema discipline, governed ingestion, semantic/job lifecycle, supported authentication and operational recovery. Their exact implementation order requires the applicable goal and approvals. ConsultAI integration separately proves identity projection, human intent/review, full dependency revalidation, source/model boundaries and user-visible status. Native tests cannot stand in for those host tests.

Validate recall, citations, current-state exclusion, malformed inputs, isolation, retries and revocation with positive and negative controls. Evaluate model quality/token cost separately from deterministic SQL/HTTP fixtures; the evaluation plan records methodology. Do not run fixture-resetting benchmarks concurrently with the ongoing task.

## ADR impact and deliberate exclusions

The ADR guide identifies accepted ConsultAI and cross-context policy. Keep those policies at their owner; native storage must support the required invariant without importing business role semantics. Research and discussion records explain past alternatives, not present authority.

External-source capture, automatic memory, shared publishing, tenant relocation, production retention and release commitments remain gated as documented. No dependency addition, runtime change, accepted-policy amendment or production-readiness claim follows from this scope cleanup.
