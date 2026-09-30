# Compiler incremental-state experiment

The experiment follows #57 on the unified backend/editor stack. No compiler
incremental-state change is retained in production. The reproducible
[patch](experiments/rustc-incremental.patch) and
[probe](../scripts/probe-rustc-incremental.py) preserve the investigated variants;
[the measurement report](measurements/rustc-incremental.json) records the evidence.
This closes the tested compiler experiment within step 11, not the full save,
layout-reuse, performance or final-release milestones.

## Variants and compiler lifecycle

All variants use the same release executable and explicit Cargo target, with
separate cache and rustc incremental directories. `off` keeps the production
behavior, stripping Cargo's incremental option and returning output before
compiler teardown. `retain` passes an isolated incremental directory but keeps
the early process exit. `stop` returns `Compilation::Stop` after analysis.
`finish` explicitly calls `tcx.finish()` and finalizes the session directory
before output and exit.

The pinned compiler is `f53b654a8882fd5fc036c4ca7a4ff41ce32497a6`
(nightly-2026-05-01). Its
[driver callbacks](https://github.com/rust-lang/rust/blob/f53b654a8882fd5fc036c4ca7a4ff41ce32497a6/compiler/rustc_driver_impl/src/lib.rs)
allow stopping after expansion, while
[global-context teardown](https://github.com/rust-lang/rust/blob/f53b654a8882fd5fc036c4ca7a4ff41ce32497a6/compiler/rustc_interface/src/passes.rs)
calls `tcx.finish()`. Saving queries and
[finalizing an incremental session](https://github.com/rust-lang/rust/blob/f53b654a8882fd5fc036c4ca7a4ff41ce32497a6/compiler/rustc_incremental/src/persist/fs.rs)
are separate operations. The measurements agree: `retain` and `stop` leave no
finalized query-cache sessions; `finish` produces reusable finalized sessions.
Some early-exit working directories grow across requests. No production cache
budget or cleanup policy was added for these experimental directories.

The borrow-checker fact provider has thread-local side effects, so bypassing it
through persisted queries needs care. These probes did not reproduce a missing
fact failure; that concern is not presented as a demonstrated correctness bug.
All included outputs match an independent fresh #57 oracle.

## Measurement controls and decision

Each group has eight interleaved forward/reverse samples for each variant.
Cold/warm diagnostics enable incremental statistics; a further warmup and all
measured samples disable those statistics. `perf stat` counts user instructions
across the process tree. Cargo replay is disabled, and every audited request
invokes the compiler once. The reference is an independently frozen #57 binary.
The host is loaded by the corpus job, so wall times are retained only as context.
There is no quiet latency or peak-RSS claim.

The controlled library/binary fixture exercises both modes, a closure, callee
mutation, undo and a feature exposing an unrelated ill-typed function. Edited
and feature-enabled requests each use a matching fresh reference. Real-project
probes copy locked source trees with file hashes and internal symlink targets;
they do not mutate the corpus roots.

The semantic-cache probes append a newline at EOF before every request. This
invalidates snapshot replay without shifting the selected body. Every measured
candidate then runs the compiler, hits one semantic result and invokes no
solver. This separates compiler validation cost from cached solver work.

Instruction ratios below are candidate / `off`; above 1 is worse. All 510
requests passed equivalence, including all 320 measured samples.

| Workload | Mode | retain / off | stop / off | finish / off |
|---|---|---:|---:|---:|
| Fixture lib | SigOnly | 1.0521 | 1.0555 | 1.0510 |
| Fixture lib | Recurse | 1.0084 | 1.0108 | 1.0083 |
| Fixture bin | SigOnly | 1.0544 | 1.0578 | 1.0535 |
| Fixture bin | Recurse | 1.0120 | 1.0144 | 1.0119 |
| serde_json, cache off | SigOnly | 1.0830 | 1.0914 | 1.0578 |
| serde_json, cache off | Recurse | 1.0898 | 1.0991 | 1.0340 |
| serde_json, validated cache | SigOnly | 1.0406 | 1.0450 | 1.0259 |
| serde_json, validated cache | Recurse | 1.0499 | 1.0554 | 1.0138 |
| just, validated cache | SigOnly | 1.0640 | 1.0701 | 1.0570 |
| just, validated cache | Recurse | 1.0639 | 1.0701 | 1.0570 |

These results reject the tested compiler-state variants as optimizations for
this path. They do not rule out a different persistent compiler architecture or
a benefit on another workload. Production Rust files remain byte-identical to
#57. Keep the existing correctness checks and compiler fallback while continuing
safe layout reuse and final measurements.

## Reproduction and retained attempts

In an isolated checkout of #57, apply `docs/experiments/rustc-incremental.patch`
from this PR, build release executables with the pinned toolchain, and archive
with `scripts/freeze-validation-build.py`. The retained experimental v2 build
is `7de1ad147`; the fresh reference is `fda9c7e8e`. Both are under
`target/continuation-validation`, with `build.json` and binary checksums. Run the
probe in that toolchain environment, for example:

```sh
python3 scripts/probe-rustc-incremental.py \
  --backend-dir "$ARTIFACTS/rustc-incremental-experiment-v2" \
  --reference-dir "$ARTIFACTS/editor-inputs-v2" \
  --work-dir "$ARTIFACTS/new-isolated-probe" \
  --json "$ARTIFACTS/new-isolated-probe.json" \
  --perf /path/to/perf --repeat 8
```

For real projects, add `--project-source PATH --source src/de.rs --line 329
--column 8`. Add `--compiler-validated-cache` to measure semantic reuse with
compiler validation. Use a fresh work directory and a 6 GiB process-group limit,
two Cargo build jobs, and at most two heavy harnesses including other live jobs.
Raw reports contain exact commands, hashes, source inputs, counters, stderr
paths and incremental-file inventories. Do not compare old and new source
manifests as though they were one run.

The first fixture run used an incorrect no-feature oracle for a feature-expanded
body inventory; even its `off` variant differed. That harness comparison was
fixed and the raw attempt remains excluded. Earlier exploratory builds enabled
incremental statistics during timing and are also excluded from the table.
Three just setup attempts ended before analysis: one used a nonexistent source
path and two rejected internal documentation symlinks. The final harness
preserves internal relative symlinks and rejects links outside the copied tree.
These setup failures are retained as logs, not counted as compiler failures.
