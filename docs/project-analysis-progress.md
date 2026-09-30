# Project analysis implementation

Feature 9 is in progress. The first increment provides compiler-discovered body
inventories and explicitly targeted, independently restartable body requests.
The public `cargo flowistry project` scheduler, stream, priorities, cancellation
and resource limits are not implemented yet. These worker commands alone do not
establish the project's resource or performance acceptance gates.

## Worker boundary

Explicit selection uses `--package NAME --target-kind KIND --target-name NAME`
before the command. Supported kinds are `lib`, `bin`, `example`, `test` and `bench`;
the named target must belong to the named workspace package. Custom library and
binary names are honored. A target declaring multiple crate types is currently
rejected with a diagnostic. `--features`, `--all-features` and
`--no-default-features` configure Cargo and participate in the response-cache key.

The hidden `project-bodies FILE` worker enumerates bodies after compiler
expansion, before type-checking unrelated bodies. Its schema-1 inventory contains
portable DefPathHash identities, names and source ranges with filenames. Bodies
without usable on-disk ranges get explicit inventory errors. Nothing from this
inventory alone is advertised as a validated analysis result.

The hidden `body-focus FILE IDENTITY` worker validates that exactly one current
compiler body matches the identity and the requested file, then uses the existing
file-focus analysis/cache path. Selection does not depend on finding a character
inside a body, which is ambiguous for closures and coincident macro spans.
Existing focus and file-focus response formats remain unchanged.

Body requests use the existing semantic cache, persisted summaries, input
snapshots, revision/generation publication checks and shared result index. The
response cache records which identity was selected and never treats that response
as an ordinary positional/full-file request. An unchanged body request can replay
its validated snapshot without invoking Cargo or rustc. Result-index identities
and ranges remain portable. An explicit package controls input discovery even
when a module's source is outside that package's directory.

Explicit targets and feature overrides bypass the old Cargo-command replay
selector, which infers a target from a path. Compiler-free validated response
snapshots remain enabled. Compiler work still goes through Cargo on a miss.

Every Flowistry Cargo launcher holds one advisory lock in its plugin target
directory from artifact preparation until Cargo exits. This includes legacy
foreground requests. It protects the existing library-artifact invalidation from
another Flowistry launcher using the same target directory; cache reads and
publication locks remain independent. The future scheduler must cancel obsolete
workers and bound its queue so this serialization cannot starve foreground work.

## Validation and remaining work

`scripts/test-project-workers.py` compares each discovered body with independent
cache-off file-focus output in both modes, including custom library/binary
targets, shared modules, external module paths and closures. It also checks
compiler-free warm replay, readable result-index entries, feature-dependent
inventory, type-error isolation, invalid identities/files/targets and launch-lock
ordering. The comparison uses the established range-table canonicalizer and
preserves resolved foreign filenames.

Final frozen-build evidence and the existing edit/publication/summary regression
matrix will be recorded before publishing this increment. The next increment
adds the project coordinator with explicit stream selection, cursor/file/body
priorities, bounded worker lifetime, cancellation of descendants and resumable
cache filling. Full project/corpus/resource/latency acceptance remains open.
