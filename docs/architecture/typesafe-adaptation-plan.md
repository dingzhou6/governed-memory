# TypeSafe adaptation plan

Status: host-side plan, 2026-09-18. **Owner: host orchestration.** This repository keeps eligibility, ACL, packing, and lifecycle. TypeSafe (Jev) may judge **already authorized** evidence; it does not become a native search, ranking, or authority component.

The [engine contract](agentic-memory-engine-design.md) remains canonical. This page does not amend first-core security, current-only semantics, or public search fields. It records how to attach System One judgments to the packet native search already returns.

Live TypeSafe docs are the API source of truth. Community crates and cookbook numbers are starting points to re-evaluate on our fixtures.

## Docs this plan is based on

Reviewed from [docs.typesafe.ai](https://docs.typesafe.ai/llms.txt) on 2026-09-18 (Jev 1.13 / `jev-latest`). Concept pages, not every SDK class reference.

| Page | Used for |
| --- | --- |
| [Introduction](https://docs.typesafe.ai/introduction.md), [System One](https://docs.typesafe.ai/concepts/system-one.md), [AI primer](https://docs.typesafe.ai/introduction/machine-learning-primer.md) | What Jev is: typed decisions, no generated text, RLCD calibration |
| [How to build](https://docs.typesafe.ai/concepts/how-to-build-with-system-one.md) | Code owns workflow; atomic questions; compose in code |
| [Use-case map](https://docs.typesafe.ai/concepts/use-case-map.md) | Retrieval / ranking / verification vs industries we skip |
| [State](https://docs.typesafe.ai/concepts/state.md), [primitives](https://docs.typesafe.ai/primitives.md), [Choice](https://docs.typesafe.ai/primitives/choice.md) | Named JSON, Noul vs Score vs Choice, 255-option cap |
| [Confidence](https://docs.typesafe.ai/confidence.md), [patterns](https://docs.typesafe.ai/patterns.md) | Act / review / escalate; fan-out; composite scores |
| [Models](https://docs.typesafe.ai/models.md), [Jev 1.13 jaggedness](https://docs.typesafe.ai/model-jaggedness/jev-1.13.md) | Limits, languages, what not to ask |
| [HTTP API](https://docs.typesafe.ai/api.md) | `POST /v1/systemone` |
| RAG cookbooks | [classifying passages](https://docs.typesafe.ai/cookbooks/classifying_rag_passages.md), [rerank](https://docs.typesafe.ai/cookbooks/rerank_typesafe.md), [citation check](https://docs.typesafe.ai/cookbooks/citation_check.md), [line-by-line search](https://docs.typesafe.ai/cookbooks/semantic_find.md), [guardrails](https://docs.typesafe.ai/cookbooks/llm_guardrails.md), [parallel questions](https://docs.typesafe.ai/cookbooks/parallel_questions.md) |

Not treated as engine design: playground JS on some Mintlify pages, per-class SDK docs, Legal, Demos, or cookbooks outside retrieval/verification (games, recruiting, ads).

## What Jev is

Jev is TypeSafe’s flagship **System One** model. Send `state` plus typed questions; get structured answers. It understands natural language. It does **not** write replies, code, summaries, or explanations.

| Primitive | Ask | Returns | Use when |
| --- | --- | --- | --- |
| **Choice** | Which of these options? | `choice`, `probabilities` (sum to 1), `confidence` | One of a closed set (include `none` when nothing may fit). Max **255** options. |
| **Score** | Where on this rubric? | `score` (may fall between levels), `legend`, `probabilities`, `confidence` | Ordered levels you can describe in words. |
| **Noul** | Is this true? | `noul` 0–1 only (no `confidence`) | One yes/no condition. ~0.5 is uncertain, not “medium intensity.” |

Questions in one request share the same state, run **in parallel and in isolation**, and cannot see one another’s answers. Compose them in code. A second request is only when an earlier answer is required to fetch evidence or build new options.

TypeSafe’s own [search-and-retrieval](https://docs.typesafe.ai/concepts/use-case-map.md) map: score query–candidate relevance, rerank a shortlist, select useful context. That **supplements** embeddings; it is not an embedding API.

## What Jev can do for us

These match System One (fast judgment) plus the use-case map’s retrieval / verification / harness rows.

- Judge whether an **already retrieved** passage is relevant, citable, contradictory, or an instruction to the model.
- Rank or Score a **shortlist** (tens of authorized passages), including a separate “nothing useful” Noul so the top Choice is not treated as an answer.
- Check whether a generator claim is supported / contradicted / absent in a **named native locator** (code should string-match quotes first).
- Screen packed evidence for jailbreak / injection before it reaches the answering model.
- Label ingest text with extra semantic features (theme, “contains a deadline”) if we later store those as derived data — still not authority.

Typical latency in TypeSafe’s guide is ~100 ms per request (jaggedness and network still apply). Batching many questions on one state is how they keep cost down ([parallel questions](https://docs.typesafe.ai/cookbooks/parallel_questions.md)).

## What Jev cannot do (do not ask it to)

From [System One](https://docs.typesafe.ai/concepts/system-one.md), [models](https://docs.typesafe.ai/models.md), and [jaggedness](https://docs.typesafe.ai/model-jaggedness/jev-1.13.md).

| Not Jev | What we use instead |
| --- | --- |
| Embeddings / vectors / ANN | Caller-supplied vectors + pgvector when that slice exists; lexical search always |
| Generate answers, rewrites, summaries, or query reformulations | Host generator LLM; native returns source text |
| Parse PDF/Office into passages | Existing extraction sets / parsers |
| Search a whole tenant corpus | Native eligible FTS ∪ semantic, then pack |
| Choice over more than 255 options, or 64k-token dumps of a collection | Pre-filter in SQL; two-pass only if a tiny doc ever needs line-Choice |
| Grant ACL, currentness, forget, or business approval | Native credentials and revisions |
| Count, arithmetic, date ordering, “how many” | Code / SQL. Jev may extract date **parts**; code compares |
| Images, audio, video | Pre-extract text first |
| Reliable CJK as the sole retriever | Native Han lexical path; test Jev on our Chinese fixtures before relying |
| Per-customer fine-tune | Shape answers with `state` + `criteria` only |
| Training on our requests | TypeSafe states it does not train on customer traffic; still apply egress |

Jaggedness also: write the exact condition (literal); filter state before sending (large irrelevant state hurts); do not treat Noul and Choice-yes as interchangeable; do not interpolate Score levels into an exact magnitude; adversarial passage text can steer answers — test injection cases.

Typed output is the **interface**, not truth. Calibration is about groups of predictions, not one row.

## How we use it

Code owns control flow ([how to build](https://docs.typesafe.ai/concepts/how-to-build-with-system-one.md)). Native search does not call TypeSafe. The host (or a standalone egress adapter) does, **after** eligibility.

```
query
  → POST /v1/search  (ACL, hybrid candidates, RRF, pack ≤12)
  → host egress check
  → TypeSafe on authorized rows only   ← Jev lives here
  → code routes (keep / conflict / drop)
  → generator LLM (optional)
  → TypeSafe citation check (optional)
```

**Host** means the app that calls this engine (ConsultAI Go, or another HTTP client). It is not `src/lib.rs`.

| Job | Native engine | Jev (host) |
| --- | --- | --- |
| Who may read the row | Yes | No |
| Get candidates into a window | Lexical ∪ semantic, RRF, budgets | No |
| Is this packed row useful / conflict / injection? | No (rank ≠ usefulness) | Slice A |
| Reorder fused window if packing buries a hit | Optional later disclosure of that window | Slice C, measured |
| Did the answer model cite this locator honestly? | Locator identity only | Slice B |
| Write the user-visible answer | No | No (generator) |

Send **named JSON** (`query`, `item_id`, `revision_id`, `text`, `citation`), not a blob. Point questions at backticked paths. One Noul per independent label. Keep raw probabilities; change weights in code without a new call when question meaning is unchanged.

On TypeSafe timeout / 429 / 529 / missing key: use the packed native packet. Do not hold the search lock while waiting on Jev. Do not fail open into a second undeclared provider.

## Why attach it here

Native search already does the hard governed work: tenant-scoped eligibility, lexical ∪ semantic candidates, RRF-60 fusion, whole-passage packing, exact locators. The contract already says fused rank is **not** usefulness or abstention, that conflicting evidence must survive, and that a reranker is an **optional measured** step after candidate quality, run only on currently authorized text with permitted egress.

Jev fills that named gap: fast typed probabilities the host can branch on, without generating text and without replacing SQL.

## Binding constraints (unchanged)

Copied from the engine contract; a TypeSafe win cannot relax them.

- Eligibility before relevance. No global top-k then application filter as the tenant boundary.
- No search string, summary, match score, Jev probability, or confidence band authorizes a read, write, or business action.
- Preserve relevant conflicting evidence. Do not let popularity, recency, or a model score silently resolve a disagreement.
- Managed content reaches a remote model only after the host’s current source/recipient/egress check. Rust must not send managed candidates to TypeSafe before that check. Standalone deployments use the equivalent configured native boundary.
- Local-only input cannot reach remote embedding, reranking, extraction, answer, or diagnostic services.
- No provider credentials in normalized source text, job input, or model prompts.
- Optional reranker or summary failure falls back to the authorized underlying evidence. Identity/storage failures still deny.
- Measure native recall and citation fidelity separately from host answer quality and token cost. A plan or benchmark run is not a native gate.

## What this repository will not do

- Add `typesafe-rs`, `s1`, or any TypeSafe client to `governed-memory` for the first slices.
- Call `POST https://api.typesafe.ai/v1/systemone` from search SQL, packing, or the request transaction.
- Add native routes or response fields whose meaning is a Jev score.
- Use TypeSafe to rewrite queries, merge corrections, retarget forget, infer historical state, or approve publication.
- Replace RRF, hybrid eligibility, or byte/token packing with a model planner or with Choice-over-corpus.
- Point `jev-axi`, `every`, `tsg`, MCP wrappers, or other agent CLIs at tenant memory.

Host application policy, prompt UX, and provider keys stay out of tree. If a local ConsultAI note is needed, put it under `docs/private/`.

## Current native surface to consume

Use the existing `POST /v1/search` packet. Do not invent a parallel retrieval path.

| Native output | Host uses it as |
| --- | --- |
| `items[]` with `item_id`, `revision_id`, excerpt `text`, and passage `citation` (source revision, extraction set, passage, locator) | Candidate rows for judgments. Copy locators; do not regenerate them. |
| Optional `context` with `format: "native-evidence-json-v1"` | Compact `{evidence:[{item_id, revision_id, text, citation}]}` already ordered for the token budget. Prefer this as TypeSafe `state` when the host requested a token budget. |
| `truncated`, `warnings`, `context_bytes` / `token_count` | Honest coverage. A dropped or oversized passage is a packing fact, not a Jev miss. |
| 12-item cap, four passages per source, whole-passage (no mid-span cut) | Classifier input size. Slice A/B judge this packed set only. |

Byte-only search still returns items without `context`. The host may build the same named JSON fields from `items`. Semantic query vectors remain caller-supplied; TypeSafe does not embed.

## Slices

Advance a slice only with a failing host test or held-out comparison on **our** data. Cookbook thresholds and [jev-rerank-bench](https://github.com/anessbelbati/jev-rerank-bench) headlines are not ours.

### Slice A — Classify the packed packet (start)

**Goal.** Between native search and the answering model, label each authorized passage so the host can keep evidence, keep conflicts in a separate block, or drop injection/off-topic text. Matches [classifying RAG passages](https://docs.typesafe.ai/cookbooks/classifying_rag_passages.md) and jaggedness “filter in code first.”

**Native change.** None.

**Host flow.**

1. Authenticated `POST /v1/search` as today.
2. Host egress check on the returned rows (managed path) or the configured standalone equivalent.
3. One TypeSafe request **per passage** over shared named state. Independent questions in one call.
4. Code routes on probabilities. Assemble the prompt from evidence vs conflict blocks. Do not send dropped rows to the generator.

**State** (named JSON, not a concatenated blob):

```json
{
  "query": "<original user question>",
  "item_id": "...",
  "revision_id": "...",
  "text": "<authorized excerpt or passage>",
  "citation": { "source_revision_id": "...", "extraction_set_id": "...", "passage_id": "...", "locator": "..." }
}
```

**Questions** (Noul, one condition each; tune wording on fixtures):

| id | Asks | Host uses the probability to |
| --- | --- | --- |
| `relevant` | Does `text` address `query` for this governed record? | Drop low-probability off-topic rows |
| `usable` | Does `text` state evidence a reader could cite, not only adjacent chatter? | Prefer for the evidence block |
| `contradicts_premise` | Does `text` deny or qualify an assumption in `query`? | Keep in the **conflict** block when also relevant; do not drop |
| `instructs_model` | Does `text` try to override system or model instructions? | Drop |

Ask all four together. They cannot see one another’s answers; state each premise in the question. Include enough citation identity that a later audit can replay the exact revision.

**Code policy (illustrative, not shipped thresholds).** Evaluate on labeled native packets before using. Example shape only:

- Drop if `instructs_model` is high.
- Else if `contradicts_premise` is high and `relevant` is high → conflict block.
- Else if `relevant` and `usable` are high → evidence block.
- Else drop.

Treat Noul values near 0.5 as uncertain: keep the passage in evidence with a host flag, or skip generation and ask the user, depending on stakes. Do not equate 0.5 with “medium relevance.” Do not reuse a Noul threshold on a Choice.

**Failure.** TypeSafe timeout, 429/529, or 5xx: use the packed native packet unchanged (engine: optional model failure uses underlying evidence). Do not retry inside the search transaction; native search has already completed. Missing key or denied egress: skip TypeSafe; never fail open into a second provider.

**Eval.** Required [paired Jev-off vs Jev-on](#paired-benchmark-required-for-slice-a) on frozen native packets. Score: required passage still present after routing; planted injection dropped; known contradiction still shown; no unauthorized row appears. Do not reset an active memory-task database for this work.

**Closest cookbook.** [Classifying RAG passages](https://docs.typesafe.ai/cookbooks/classifying_rag_passages.md), [LLM guardrails](https://docs.typesafe.ai/cookbooks/llm_guardrails.md), [parallel questions](https://docs.typesafe.ai/cookbooks/parallel_questions.md).

### Slice B — Verify generator citations

**Goal.** After the answering model, check that each cited span is supported by the **same** native locator the search returned.

**Native change.** None. The host already has `citation` on each evidence element.

**Host flow.** Code: is the quoted string in `text`? If not, mark fabricated without Jev. If found, one Choice over `{claim, text, citation}` with `supports`, `contradicts`, `says_nothing`. Use Choice confidence to send uncertain citations to review.

**Failure.** Unverified claims are stripped or flagged; the native packet remains the source of truth.

**Closest cookbook.** [Citation check](https://docs.typesafe.ai/cookbooks/citation_check.md).

### Slice C — Optional fused-window rerank (only if A is not enough)

**Goal.** If packing buries a relevant span that was in the fused candidate union, let the host reorder **authorized** candidates with a comparable Score, then pack.

**Do not start this because a vendor nDCG looks good.** The engine already requires candidate quality first, then a paired comparison of lexical / hybrid / optional rerank on fixed data and equal budgets. Line-by-line Choice over a whole collection is not this slice.

**Native change (prerequisite, still not a TypeSafe client).** Today the public search response is the packed 12. Reordering those 12 cannot recover a row packing already skipped. A later versioned option may disclose the authorized fused window (stable ids, ranks, excerpts) to the **host**, still under ACL, still without calling TypeSafe from Rust. Unknown fields stay rejected until that slice is tested. Alternatively the host may pass an ordered list of already-returned ids into a pack-only helper; that is also a new contract, not a silent search change.

**Host flow (after that disclosure exists).**

1. Egress check on the fused window.
2. One TypeSafe request: per-candidate Score for “how directly this passage answers the query” with standalone level descriptions, plus a Noul `nothing_useful` (a nearest neighbor is not proof of useful evidence).
3. Code reorders by Score, ignores unused-branch uncertainty, applies existing 12-item / four-per-source / whole-passage / byte-or-token rules. Do not collapse independent sources because text matches.

**Eval.** Same corpus and permissions as the RRF packed baseline. Report recall of required evidence after packing, negation/no-answer behavior, latency, and TypeSafe token cost. [jev-rerank-bench](https://github.com/anessbelbati/jev-rerank-bench) is a method reference (batch in one call, “nothing here” AUROC, negation split). Its 0.692 vs Cohere 0.691 headline does not select our rerank.

**Closest cookbook.** [Re-ranking](https://docs.typesafe.ai/cookbooks/rerank_typesafe.md).

### Slice D — Standalone configured egress (later)

If a deployment has no Go Gatekeeper, the same slices still require an explicit, configured model-egress policy equivalent to the managed route: destination allow-list, no local-only leakage, keys only in server env (`TYPESAFE_API_KEY`), fail closed to native evidence. HTTP `POST /v1/systemone` with `jev-latest` is enough. A community Rust client ([typesafe-rs](https://github.com/AbdelStark/typesafe-rs), optional [s1-rs](https://github.com/AbdelStark/s1-rs) policies) may wrap that call **outside** `governed-memory`, with loopback mocks in tests. Official Python/JS SDKs and the [System One Adapter](https://github.com/typesafe-ai/system-one-adapter-python) belong to eval harnesses, not this crate.

## Question and composition rules

From [how to build](https://docs.typesafe.ai/concepts/how-to-build-with-system-one.md) and [primitives](https://docs.typesafe.ai/primitives.md):

- Keep deterministic work (ACL, dates, counts, packing) in code.
- One coherent judgment per question. Several labels that can all apply are several Nouls, not one Choice.
- Put the decision in `instructions`; put allowed answers in `criteria`. Question ids are for code and are not sent to the model.
- Prefer named JSON state. Reference paths in backticks (`query`, `text`, `citation.locator`).
- Include a no-match / nothing-useful outcome when nothing may fit.
- Batch independent questions on the same state, including speculative ones the code may ignore.
- Choice/Score `confidence` is distribution concentration, not permission to act. Thresholds are host policy, evaluated on our labeled packets.
- Keep raw answers. Changing a display filter or weight must not require a new inference when the question meaning is unchanged.

## Evaluation and operations

Slice A is not accepted on a cookbook demo. It needs a **paired Jev-off vs Jev-on** comparison on the **same** native packets. The engine already requires lexical / hybrid / optional-rerank variants on fixed data and equal budgets; this is that gate for the usefulness layer.

### Paired benchmark (required for Slice A)

**Control (Jev off).** Authenticated `POST /v1/search` as today. The host sends the packed `items` / `native-evidence-json-v1` to the answering model unchanged (or scores the packet with no generator). This is the production-shaped baseline.

**Treatment (Jev on).** Identical search request, frozen corpus, ACL, byte/token budget, and generator prompt/schema. Only the post-search route changes: four Nouls per row, then evidence / conflict / drop.

Do not re-retrieve between arms. Capture the native response once per wording and feed **both** arms from that capture so a Jev win cannot be a different candidate set. Pin `jev-latest`’s resolved model id in the report; cookbook `jev-1.12` numbers are not ours.

Lock settings on a development split; decide on held-out. Do not run this against an active memory-task fixture, and do not treat `just verify-clean` / N4 reset recipes as this harness. Prefer a frozen search capture (same style as N1/N4 artifacts) plus a host-side scorer.

| What to score | Jev off | Jev on | Notes |
| --- | --- | --- | --- |
| Required passage still in the prompt set | Packed 12 | After route | Jev must not drop the only supporting span |
| Planted injection reaches the generator | Should be yes if packed | Must be no | Guardrail arm |
| Known contradiction still shown (separate block ok) | Packed together | Conflict block | Off must not “win” by hiding disagreement |
| No-answer / insufficient wordings stay empty or abstain | Native empty or model guess | Abstain if `relevant`/`usable` low | Nearest neighbor ≠ useful |
| Generator correctness / citation grounding (optional second stage) | Same reader as N4-style contract | Same reader | Only after packet-level metrics; equal output budget |
| Latency | Search only | Search + TypeSafe + route | Per wording p50/p95 |
| TypeSafe tokens and $ | $0 | Recorded usage | Host COGS, not a native gate |

Packet-level scores (injection, required-span kept, contradiction kept, abstention) do **not** need a paid answer model. Add the generator only when those packet metrics are stable, under a separate spend cap. A latency/cost regression with no packet or answer gain is a fail; do not ship Jev-on as default.

Report both arms even when Jev-on loses. Failure of TypeSafe in the treatment arm must fall back to the captured packet (same as production); count that as Jev-on degraded, not as a new retrieval.

### Layers (unchanged split)

| Layer | What to measure | What not to treat as a pass |
| --- | --- | --- |
| Native | Isolation, current revision, forget, required evidence before/after fusion and packing | Jev nDCG, answer fluency |
| Host TypeSafe | Paired table above on frozen packets | Cookbook demo accuracy |
| End-to-end | Held-out answer quality at equal context budget, Jev off vs on | A single online anecdote |

Log request id, question ids, model id, latency, and token usage with the same sanitization rules as other model work: no source text, no bearer tokens, no API keys. Retry 429/529 with backoff **after** native search returns. Do not hold a tenant search lock while waiting on TypeSafe.

## Community list: adopted vs parked

Reviewed from [awesome-typesafe](https://github.com/AbdelStark/awesome-typesafe) (2026-09-17 list).

| Adopt into this plan | Leave parked |
| --- | --- |
| Official cookbooks above, HTTP API, confidence/patterns | Games, browser agents, Home Assistant, Pi/Ruby/Rails/.NET/Elixir clients |
| jev-rerank-bench as an eval template | jev-axi / `every` / `tsg` / MCP against tenant data |
| typesafe-rs / s1-rs only in Slice D, out of tree | Choice-over-whole-corpus as native retrieval |
| System One Adapter for LLM-judge A/B in eval | OpenJev, spam eval, social field notes as evidence |

Use-case map rows we **do not** take into this crate: recruiting, ads, gaming, lead-gen, insurance STP, as product features. Legal/compliance *citation and contradiction* checks are Slice A/B, not a native classifier of contracts.

## Done when

Slice A is done when a host (or standalone adapter) can take a current native search packet, apply egress, route passages with recorded thresholds, fall back to the packet on TypeSafe failure—without any `governed-memory` dependency on TypeSafe—and a paired Jev-off vs Jev-on report on a frozen development split exists. Default-on requires the held-out packet metrics (and, if used, the same-generator answer arm) to beat or match off without dropping required evidence.

This plan is **not** done if Jev is invoked from native search, if a score can widen disclosure, or if packing/RRF is removed without a paired held-out comparison on authorized fixtures.
