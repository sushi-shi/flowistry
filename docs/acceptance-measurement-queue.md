# Quiet optimization acceptance queue

The queue prepares the still-open row-group/seed retention gates and part of the
performance baseline, directly above #59. It does not complete those gates until
its measured results are inspected. It does not replace whole-project, editor
save distributions, remaining reference checks or final resource budgets.

The frozen binaries are valid immediate-predecessor comparisons. `baseline/`
(`2630ee356`) has the same crates, Cargo manifests/lock and toolchain file as
`perf/combined-chain-measurements` (`6b45b1c16`, #42). `row-groups/` (`3199b4095`)
has the same runtime sources as #43 (`715bc5357`). `seed-rows/` (`f1163469c`)
likewise matches #44's runtime. The plan generator verifies these comparisons
again and records actual branch revisions and build/compiler/binary hashes.
No independent per-PR debug build is needed.

For each optimization, the selected locked workloads are:

| Crate | Zero-based positions | Purpose |
|---|---|---|
| either | `src/into_either.rs:58:8` | Small normal request |
| serde_json | `src/de.rs:329:8` | Typical parsing request |
| just | `src/analyzer.rs:177:4`, `src/compiler.rs:13:4`, `src/error.rs:926:10` | Large aggregate/seed stress |
| niri | `src/niri.rs:1605:8`, `src/backend/tty.rs:649:20` | Largest observed Recurse memory cases in the completed seed corpus |

Every position runs in both modes. Normal performance jobs use eight alternating
A/B, B/A samples, two warmups, semantic cache off, independently isolated Cargo
replay stores, user instruction/cycle counters, peak RSS and output-size evidence.
Wall measurements now retain microsecond precision instead of rounding to 10 ms.
The existing summarizer retains all samples and nearest-rank p95; eight-sample
tails remain exploratory, not high-confidence percentile estimates.

Diagnostic jobs are separate, with phase/row logging and one recorded request per
backend after warmups. Row enumeration itself costs work, so those times must not
be presented as production latency. The report retains each analysis's logical
rows, explicitly stored rows, location count and row entries. These are sums over
location states, **not unique heap allocation counts**. Peak RSS is measured
separately. Keep the complete per-analysis list rather than guessing which row
record represents a selected function.

The runner pins its inputs and does not overwrite state, logs or reports. It waits
for the two explicitly identified corpus processes, including their Linux start
times to detect PID reuse. A stopped process still counts as live. Terminal
processes must have complete, successful corpus reports before measurement starts.
It does not terminate those jobs or mutate their prepared roots while they run.

Each job needs four consecutive five-second quiet checks: no unrelated compiler/
Cargo/Nix/linker process, at most 5% host CPU use, at most 0.5 unrelated CPU cores,
and one-minute load at most 2. During the job, two-second process samples record
unrelated CPU/build activity. Descendants of the runner are its own work. The
thresholds and observations are retained. Sampling cannot exclude activity shorter
than its interval; “quiet” means this stated policy passed. Detected contamination
stops the queue for review and leaves all evidence intact.

Jobs run serially with one corpus worker and a 6 GiB request cap. No heavy job is
started until the two current full corpus runs have ended. SIGTERM/SIGINT request
a stop after the bounded active child finishes; no other processes are killed.
Failed status, missing counters, lack of direct replay, source drift, incomplete
coverage or missing diagnostic data stop the queue. Results are never silently
resumed as timing samples or automatically retried until a result passes.

Use `scripts/plan-optimization-measurements.py` to create a new directory/plan,
then `scripts/run-acceptance-queue.py --plan PLAN --state NEW_STATE`. Run them in
the cached dev environment from the handoff. The artifact directory must contain
the three frozen build archives, and `--after PID REPORT` names the currently live
corpus jobs. Preserve the runner's state/identity and check it before relaunching.

Validation: all 61 Python harness tests pass, including PID reuse, stopped/zombie
process handling, unrelated-load detection, exact measurement coverage and
mandatory counters/replay/diagnostics. A real loaded-host functional probe on all
three frozen backends records matching storage diagnostics; its raw report is
`target/continuation-validation/acceptance-storage-probe-v1.json`. It establishes
parser compatibility, not a performance gain.
