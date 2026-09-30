# Persisted summary implementation

Continuation step 8 is in progress. The portable core format and session storage
interface are implemented; the IDE disk adapter, shared invalidation keys and
persisted forward/reverse graph are still pending. No production caller enables
the new interface yet, so current CLI requests do not reuse summaries from disk.

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

The adapter is responsible for backend/compiler identity, mode/configuration,
declaration context, semantic dependency fingerprints, integrity and disk limits.
Deserialization or a matching body name alone is not validation. This contract
must be implemented using the existing focus-cache fingerprints and shared store.
Ordinary `AnalysisSession::new` does not consult storage or do the additional
lookup preparation.

Counters distinguish persistent hits/misses, in-session hits and computations.
An invalid payload is a miss; fresh computation replaces it. Successful summaries
and fallback reasons both pass through the same boundary.

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

Next: connect the IDE adapter and checksum/budget enforcement, share semantic
fingerprint logic, persist current dependency/reverse edges, and verify real
cross-process reuse after a caller edit. Callee edits must invalidate affected
Recurse callers; SigOnly must retain ordinary callee-body independence. Finish
the edit matrix and Recurse corpus before declaring step 8 complete.
