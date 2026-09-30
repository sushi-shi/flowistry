# Reference-check resource failures and exit status

The combined `engine-diff,shadow-eager` executable (`2630ee356`, frozen in
`target/continuation-validation/reference-checks`) runs considerably more work
than the normal backend. The full reference corpus run is still incomplete.

Four just requests initially appeared as `crash`, with no stderr and exit 1.
Their recorded times match kernel memory-cgroup OOM events within approximately
one second (the report records its start to whole seconds). The kernel log is
preserved at `target/continuation-validation/reference-oom-kernel.log`.

| File / zero-based position | Mode | Estimated completion UTC | Kernel kill UTC | Killed compiler PID |
|---|---|---|---|---|
| `src/analyzer.rs:324:10` | Recurse | 03:19:38.690 | 03:19:38 | 2051551 |
| `src/compiler.rs:13:4` | Recurse | 03:22:13.200 | 03:22:12 | 2161000 |
| `src/error.rs:926:10` | SigOnly | 03:23:39.310 | 03:23:39 | 2195140 |
| `src/error.rs:926:10` | Recurse | 03:23:56.290 | 03:23:56 | 2206840 |

All times are 2026-09-30. Peak RSS was 6,223–6,255 MiB, with a 6 GiB cgroup cap.
The kernel explicitly reports memory-cgroup OOM kills of `flowistry-drive`, rather
than a host-wide OOM. These are classified resource limits; they do not establish
successful reference equality. `src/analyzer.rs:177:4` Recurse separately timed
out after 600 seconds and remains an incomplete reference check. Preserve the raw
checkpoint statuses; this annotation records the evidence without rewriting them.

The masked signal came from `status.code().unwrap_or(1)` in compiler replay.
A signal-killed child has no ordinary exit code. Replay and the snapshot wrapper
now convert it to the conventional `128 + signal` status and emit a diagnostic;
SIGKILL becomes 137. Ordinary compiler exit codes remain unchanged. This lets
the existing memory-capped harness classify replay OOMs even without child stderr.
Failed output still cannot be cached as a successful response.

Validation: all five IDE library tests pass, including actual child processes
exiting normally and a child killed with SIGKILL before writing stderr. Remaining
reference gates must still be resolved with bounded, targeted reruns; this status
fix is not a substitute for those checks.
