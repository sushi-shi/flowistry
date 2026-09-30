# Persisted summary implementation

Continuation step 8 is in progress. The portable core format and session storage
interface are connected to an IDE disk adapter. Focus and file-focus now use the
adapter; cross-process edit and full-corpus acceptance gates remain pending.

## Portable boundary

`infoflow::PortableSummary` is schema 1. Its fields are private: the core captures
and restores the logical `CalleeSummary`; storage code only serializes the payload.
It contains no physical row groups, matrix layout or process-local compiler IDs.

| Field | Wire representation and validation |
|---|---|
| ABI | Direct argument count or closure tuple-field count, checked against the current compiler body |
| Root | Return, caller operand ordinal, or closure tuple operand/field ordinals; validated against the ABI |
| Path | Dereference, structural field/variant ordinal or any array index; rustc index bounds checked before reconstruction |
| Tail | Complete or truncated, preserving whether a dropped projection dereferenced a pointer |
| Origins | Ordered paths and Address/Reachable tags; return roots rejected |
| Dependencies | Fixed-width origin indices, checked against the origin count |
| Effects | Ordered Return/ArgPointee/SharedState tags, paths and origin indices; root kind checked |
| Opaque operands | Fixed-width indices checked against current ABI operand count |
| Fallback | Explicit enum tags, including unsupported-operation reasons; no compiler identities |

Every integer in the persisted schema is `u32`. Conversion from `usize` is
checked. FieldIdx/VariantIdx values are structural ordinals interpreted against
fingerprinted current types, not compiler entity identities. DefIds, MIR locals,
types, locations, spans and filename interner slots are absent. Unknown schema
versions, enum tags and object fields are rejected. Logical effect order is
preserved, including parent-before-child updates.

## Session interface

`AnalysisSession::with_summary_store` accepts a compiler-aware `SummaryStore`.
Before a lookup, the session resolves the current call graph and recursive
components. Restored summaries still pass through the existing per-call SCC
fallback policy. `direct_dependencies` exposes current local edges so the IDE
can translate them to stable body identities before persistence.

The adapter keys entries by backend/compiler identity, mode/configuration,
declaration context and the current resolved semantic dependency closure. The
body fingerprint code is shared with the focus cache: stable MIR with erased
regions, original region identities and borrow facts, and expanded HIR bodies.
Focus adds its source-relocation token fingerprint. Deserialization or a matching
body name alone is not validation. A checksum covers the key and full payload.
Ordinary `AnalysisSession::new` does not consult storage or do the additional
lookup preparation.

Counters distinguish persistent hits/misses, in-session hits and computations.
An invalid payload is a miss; fresh computation replaces it. Successful summaries
and fallback reasons both pass through the same boundary.

`summaries-v1` and `dependencies-v1` participate in the same publication lock,
atomic writes and combined disk budget as focus/results. Compiler queries and
solving occur outside the lock. Cache-off disables persistence; refresh recomputes
summaries. Content-addressed compiler-validated data can survive cancellation,
but it is never advertised as a current result publication.

Dependency snapshots contain stable body identities, own semantic fingerprints,
resolved direct local calls and reverse edges, under the same context/mode key.
Each immutable snapshot covers one Recurse root's reachable local closure,
including cycles. SigOnly snapshots contain only their root and do not depend
on ordinary callee bodies. Missing, evicted or unvisited project bodies cannot be
assumed current. Future save planning must rebuild current edges and compare
fingerprints; these observations are not a global current-project graph.

`FLOWISTRY_VERIFY_SUMMARIES=1` bypasses completed focus/snapshot response hits and
independently recomputes every loaded summary, asserting complete logical equality.
Audit counters separately report computations, persistent hits and verifications.
This mode is for correctness checks, not performance measurement.

## Validation and remaining work

Four new core regressions cover all path element kinds, input/effect tags,
truncated paths, ABI/schema mismatch, dangling origin indices, invalid structural
ordinals and invalid roots. A real compiler fixture covers closures, recursive
calls, writes through parameters, shared handles and raw-pointer fallback.

The fixture serializes computed summaries to JSON, drops its compiler session,
then restores them in a fresh compiler session and compares every complete
logical summary against independently computed results. All tested bodies reuse
their summaries with zero summary computations. Replacing every stored schema
forces recomputation; the following session reuses repaired entries. This uses
an in-memory byte store and fixed source, and does not prove disk invalidation.

The normal core all-targets suite passes 112 unit/integration tests plus benchmark
smoke tests (`/tmp/flowistry-summary-core-tests.log`). All 112 tests and benchmark
smoke tests also pass with `engine-diff,shadow-eager`, including the final change
that avoids lookup preparation when no store is configured; see
`/tmp/flowistry-summary-core-reference-tests.log`.

The connected adapter passes the workspace all-targets suite, including shared
budget checks across all four namespaces and reverse-edge integrity tests.
The cross-process harness is `scripts/test-summary-cache.py`; its results are
still pending. Callee edits must invalidate affected Recurse callers; SigOnly
must retain ordinary callee-body independence. Finish the edit matrix and
Recurse corpus before declaring step 8 complete.
