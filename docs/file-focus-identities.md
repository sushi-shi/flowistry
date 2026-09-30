# Portable filename identities in file-focus

The full cache comparison found an ambiguity that a per-response filename index
cannot resolve. At `just`'s locked `src/unindent.rs:49:23` selection, the closure
contains `matches!`. One container range points to the macro definition at zero-based lines
428–431 of Rust's `library/core/src/macros/mod.rs`. Its filename index shifts from
1 to 2 between fresh and cached execution, while the requested file shifts from
0 to 1. Both compiler commands succeed.

The corrected legacy comparator rejects this unresolved foreign identity.
Consequently the ongoing legacy run records two undecodable responses, one per
mode. Those are protocol-validation failures, not compiler crashes. Its original
records remain unchanged. Copies of the exact `unindent.rs` source reproduce the
problem in an isolated small crate; see `unindent-wire-repro.json` under
`target/continuation-validation`.

File-focus now includes an additive `files` dictionary mapping every emitted
filename index to its compiler SourceMap path. Existing numeric fields remain
unchanged. The dictionary covers body ranges, focus range tables and containers,
including foreign macro sources. Snapshot replay preserves it with the encoded
response. Existing consumers can continue using the numeric fields; consumers
that compare or identify files across responses can resolve the dictionary.

The harness resolves the table before comparing outputs, preserves the actual
foreign filename, and ignores unused table entries and index allocation order.
A missing mapping is an explicit protocol error. Legacy responses still support
their identifiable requested-file index; unknown foreign indices are rejected.
Decoder failures on successful commands now retain their reason and fail the
gate as protocol errors instead of being labeled compiler crashes.

The first filename-table candidate is frozen at `77e347ae8` in
`target/continuation-validation/file-identities-v1/`. The exact copied `just`
reproduction resolves both filenames in all six requests (cache-off, fresh cache
and warm cache in both modes). Every preexisting output field matches the old
response after the existing ordering/cache-metadata normalization. The new
table identifies the foreign source in the pinned compiler's Nix rust-src path;
see `unindent-wire-resolved.json`.

Six standalone macro/filename cases, all 84 edit cases, all 16 publication cases,
all 18 summary cases, 54 Python harness regressions and the IDE all-targets suite
pass. The [compact evidence](measurements/file-identities.json) retains exact
build/harness provenance, outcomes and full-report checksums. The six cases
verify complete table coverage, preservation of numeric fields, real macro-source
identity, fresh/warm equality and compiler-free snapshot replay. Full corpus
validation must be repeated at this candidate after the active legacy runs end.
This fix does not convert old digest-only records into successful comparisons.

The persistent result index also retains the requested file path in body ranges,
instead of storing a process-local filename slot. The index is scoped to that
file; the existing focus/file-focus numeric fields remain unchanged. Validation
of this final metadata adjustment is pending.
