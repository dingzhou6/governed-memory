# Native Memory

Native Memory stores and retrieves governed evidence for independently authenticated applications. It does not define a host's Company roles, conversation experience or business approvals.

## Identity and authority

**Memory Tenant**:
An isolated memory namespace with one authority mode: native administration or management by a trusted host.
_Avoid_: Company, database server, caller-selected namespace

**Memory Principal**:
A stable person or automation identity within one Memory Tenant whose current authority determines which operations it may perform.
_Avoid_: Employee, email address, credential

**Memory App**:
The registered calling application to which native credentials and applicability subjects are bound.
_Avoid_: Principal, Gadget, impersonation permission

**Native Grant**:
A fixed native authorization over a memory resource, distinct from the host's role names and from a user's intent to save information.
_Avoid_: Company role, provider permission, confirmation

**Trusted Writer**:
A separately authorized caller for exact user-controlled memory mutations whose authority is unavailable to an agent reader.
_Avoid_: AI author, automatic capture, human-presence proof

## Evidence and lifecycle

**Memory Collection**:
A governed set of items sharing an audience and native access rules; its items do not acquire role-specific content copies.
_Avoid_: Company department, directory, per-passage sharing

**Memory Item**:
A stable governed record with a current revision and its own withdrawal/deletion lifecycle.
_Avoid_: Mutable prompt, business source of truth

**Memory Revision**:
An immutable state of one Memory Item, with its assertion, applicability and provenance where supported.
_Avoid_: Current item identity, mutable content

**Applicability**:
The subjects and circumstances in which evidence is relevant, independently of which principals can read it.
_Avoid_: Audience, permission, truth confidence

**Source Revision**:
A specific state of a source, distinguished from its earlier and later states for exact attribution.
_Avoid_: Current source, item revision

**Extraction Set**:
The complete set of Source Passages derived together from one Source Revision under a declared extraction scope and coverage.
_Avoid_: Mixed-revision passages, partial active document

**Source Passage**:
A bounded unit of source evidence preserving meaningful content and an exact locator within its Source Revision.
_Avoid_: Arbitrary text fragment, generated summary

**Adjacent Passage Expansion**:
Adding selected immediate preceding or following passages that continue directly retrieved evidence within the same source and Extraction Set.
_Avoid_: Unconditional neighbors, recursive expansion, cross-source search

**Derived Representation**:
A searchable or condensed representation whose eligibility depends on its exact governed inputs and active processing version.
_Avoid_: Independently verified fact, publication permission

**Access Revocation**:
Loss of a principal's effective access without withdrawing the evidence from other eligible readers.
_Avoid_: Source withdrawal, global deletion

**Source Withdrawal**:
Removal of source evidence from active use for all readers, including dependent representations.
_Avoid_: Reader revocation, permanent erasure

**Operation Receipt**:
Evidence that one identified operation committed, distinct from proof that its resulting revision is still current or readable.
_Avoid_: Current-state authorization, duplicate mutation

**Deletion Marker**:
Minimal non-content evidence that prevents previously deleted material from being served again through stale processing or restoration.
_Avoid_: Retained memory body, universal same-fact erasure
