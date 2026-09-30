# Edit and concurrency validation

The current backend passes **84 cases**: 31 serial edits and six concurrency or
recovery scenarios in both modes, plus ten edits in isolated `either` and
`smallvec` corpus copies. The frozen executable is `79930142f` with no reference
features. [The compact report](measurements/edit-matrix.json) records its compiler,
binary and harness hashes, source provenance, per-case work counters and outcomes.
All 47 Python harness regressions also pass.

This prepares continuation feature 6. Full corpus release gates remain pending;
versioned publication and background-worker scenarios must be added as those
features land. It does not establish completion of features 7–12.

## Oracle and work counters

Every serial case starts with a cold request, then proves that an unchanged request
replays without invoking the compiler or solver. After applying the edit, the
harness compares reuse with a separate cache-disabled compiler process on the
same source paths, flags and build inputs. It compares the complete canonical
file-focus response, including body ranges, resolved indexed slices and
maybe-slices. Cache telemetry is excluded from semantic equality. The controlled
fixture is required to produce nonempty maybe-slices in both modes.

`RUST_LOG=flowistry::audit=info,flowistry_ide::cache=info` records analysis-compiler
invocations and each focus, shared or summary solve. Compiler counts refer to
analysis callbacks, not every Cargo dependency compilation. A cache-hit label
alone cannot establish that solver work was skipped. Compile-error fixtures must
produce the expected type error in both paths; two arbitrary process failures do
not pass. Regression tests demonstrate rejection of stale output, hidden compiler
errors, empty responses and changed maybe-slices.

Serial cases cover layout, Unicode, comments, rustfmt, literals, restored mtimes,
control flow, callee and unrelated-body edits, recursive cycles, signatures,
types, trait calls, file moves, module changes, features, cfg and Cargo config,
dependencies, build inputs/scripts, includes, environment, proc macros, closures,
compile errors, corrupt entries and abandoned temporary writes. Recurse callee
edits invalidate their caller; ordinary callee-body changes do not needlessly
rerun the SigOnly caller.

## Concurrency and recovery

A fixture-only `RUSTC_WRAPPER` pauses the selected compiler before reading source.
The wrapper and its configuration remain constant across priming, warm reuse and
the edited request. Its gate files live outside the project input tree. This
allows deterministic tests of one or two saves after preflight, without adding
test hooks to the backend. The analyzed output must equal the latest fresh
oracle, and the obsolete preflight must not publish a snapshot response.

Cancellation kills only the newly created request process group, verifies that
the held descendant stopped, and checks that a subsequent request recovers with
fresh-equivalent output. Two overlapping refresh requests must both run the
compiler and solver, agree with fresh analysis, and leave a readable warm
snapshot. Their recorded request intervals must overlap; this is not a claim
that Cargo always runs their compiler phases in parallel. Error recovery and
undo must reuse unchanged validated bodies without rerunning their solver.

Every request uses a new process. Corrupt public entries and abandoned temporary
files test storage recovery; they do not simulate a kill at every individual
write instruction. These tests cover the existing command/cache behavior. They
do not yet test an editor rejecting a completed older request generation, or a
background worker publishing a stale body after it has already read source.
Those require the versioned-publication and background features.

## Reproduction

Run in the pinned development environment, using a normal frozen backend that
includes the audit instrumentation. The work directory must not already exist.

```sh
python3 scripts/test-incremental-matrix.py \
  --backend-dir /path/to/frozen/backend \
  --work-dir /path/to/new/edit-matrix \
  --json /path/to/edit-matrix.json \
  --real-projects-dir /path/to/prepared/smoke-crates \
  --rustfmt /path/to/pinned/rustfmt
python3 -m unittest discover -s scripts -p 'test_*.py'
```

The real-project suite copies source files, excluding target and git directories;
it verifies package versions and locked Cargo.lock files before editing. The
prepared corpus is never mutated. Use repeatable `--case` flags and `--modes` for
targeted investigation. Reports record the selected scope and required cases;
passing a subset cannot appear as full matrix coverage. Failed observations are
retained under the new work directory, with raw responses and stderr.

Detailed local evidence is under `target/continuation-validation`:
`edit-matrix-complete.json`, `edit-matrix-complete/`, and `edit-matrix-v1/build.json`.
These runs overlapped corpus validation. Their timings are diagnostics, not quiet
performance measurements or save-latency claims.
