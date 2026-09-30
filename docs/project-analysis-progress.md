# Project analysis implementation

Feature 9 is in progress. The first increment provides compiler-discovered body
inventories and explicitly targeted, independently restartable body requests.
The next increment implements the public scheduler, stream, priorities,
cancellation and resource limits on top of #53; see
[the coordinator protocol and bounds](project-coordinator.md). Neither the worker
foundation nor focused coordinator tests establish the project's large-project
resource or performance acceptance gates.

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

The frozen `project-workers-v2/` candidate at `4b4915382` passes all **137 process
cases**: 13 worker scenarios, all 84 edit/concurrency/real-project cases, 16
publication cases, 18 summary cases and six filename cases. The IDE all-targets
suite and all 54 Python regressions pass. The worker tests include a target named
`file-focus` to prove that an option's value cannot masquerade as the subcommand.
[The compact report](measurements/project-workers.json) records build and harness
hashes and references the durable full reports/logs. No project throughput or
resource-limit claim is made from these small fixtures.

The coordinator adds explicit stream selection, cursor/file/body priorities,
bounded worker lifetime, cancellation of descendants and resumable cache filling.
Full project/corpus/resource/latency acceptance remains open.
