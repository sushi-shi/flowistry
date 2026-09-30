# File-focus cache comparison

The initial full gate at frozen backend `235760027` completed all 24 entries,
both modes and all 2,880 locked selections. Both backends returned successful
file-focus responses. However, 2,040 digest comparisons differed, so that run
does **not** pass the equality gate. Its raw report is retained unchanged at
`target/continuation-validation/file-cache-corpus.json`; the manifest, checksum
and per-entry coverage are in [measurements/file-cache-initial.json](measurements/file-cache-initial.json).

The first itertools selection (`src/adaptors/mod.rs:180:8`, SigOnly) was rerun
with complete outputs. The fresh response uses FilenameIndex 0 and the warm
response uses 1 for every range in the requested file. The pinned rustc_utils
serializer emits its response-local interner slot, with no filename table.
The editor already anchors this slot to the requested buffer using the body
range. Compiler/cache execution can intern that file in a different order.
After resolving that one anchored identity, the complete outputs compare equal.
The reproduction is retained as `file-cache-diff-probe.{json,log}`.

The harness now applies that same requested-file interpretation to file-focus.
It keeps literal string filenames significant, preserves every range and indexed
maybe-slice, and rejects foreign numeric filename IDs whose identities cannot
be resolved from this protocol. It does not guess that two foreign interner
slots identify the same file. Regression tests cover shifted slots, body order,
foreign ranges and real range changes; all 51 Python harness tests pass.

Only one original pair has complete reproduced output. The original hashes
cannot be retroactively corrected, and the other differences are not presumed
equivalent. A full corrected run is required, with its own harness manifest and
checkpoint directory: `file-cache-resolved.{json,log}` and
`file-cache-resolved-checkpoints/`. The frozen backend is unchanged.

The initial warm run recorded 2,044 snapshot-validated responses and 836 compiler
paths. These are correctness observations under load, not quiet latency claims.

The corrected legacy run subsequently encountered a foreign macro-source index at
`just`'s `src/unindent.rs:49:23`, in both modes. Its commands exited successfully,
but the filename identity is absent from the legacy protocol. An isolated source
copy reproduces this failure. The [filename-table fix](file-focus-identities.md)
adds explicit identities and resolves the case while preserving every preexisting
output field. The legacy run remains useful coverage evidence and is left running;
its two raw `crash` classifications are retained with this protocol-error triage.
