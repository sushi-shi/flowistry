# Streaming project coordinator

Draft [#54](https://github.com/sushi-shi/flowistry/pull/54) adds the public command
on top of the portable inventory/body workers and shared
validated result store from #51, after the combined backend/editor stack in #53.

```sh
cargo flowistry --package my-package --target-kind lib --target-name my_library \
  --context-mode Recurse project --stream ndjson-v1 \
  --cursor-file src/lib.rs --cursor-line 12 --cursor-column 4 \
  --priority-file src/other.rs --memory-mib 6144 --timeout-seconds 600
```

Package and target selection are required. Cursor positions use zero-based
characters, as in focus/file-focus. The innermost cursor body runs first, followed
by repeated `--priority-body ID` arguments, repeated `--priority-file PATH`
arguments, and the remaining inventory in deterministic source order. Missing
priority identities and cursors produce warnings. Existing focus/file-focus
protocols remain unchanged.

The coordinator currently requires Linux and a working systemd user session.
Other platforms retain the existing analysis commands and receive an explicit
unsupported-platform error for `project`. A missing user bus or unavailable
resource controller fails the inventory; there is no unbounded fallback.

## Stream and currentness

Each stdout line is a schema-1 JSON event containing an opaque `run`, monotonic
`sequence`, and elapsed time. Event kinds are `started`, `inventory`,
`body-started`, `body`, `diagnostic`, and `finished`. Diagnostics stay separate from
the encoded results. Each successful body carries a complete
`file-focus-base64-gzip-json` payload, including filename tables and source
selection metadata. The coordinator validates its selected-body result tag
without materializing the large range/dependency tables.

Body outcomes distinguish `current`, `uncached`, `superseded`, `analysis_error`,
`worker_error`, `signal`, `oom`, `timeout`, `output_limit`, `inventory_error`,
`supervisor_error`, and `protocol_error`. Observed peak memory, kernel OOM counters,
exit status, elapsed worker time and bounded diagnostics accompany worker results.
A failed body does not abandon the remaining inventory. Finished status is
`complete`, `partial`, `canceled`, or `failed`; counts include pending bodies.
Exit codes are 0 for complete, 1 for failure/partial, and 130 for cancellation.

Coverage explicitly means **the initial compiler inventory**, not a current
whole-project snapshot. Each publication validates its own source/input revision;
another save may supersede it later. A result observed after a move uses its own
compiler range, rather than the inventory's old offsets. A save during worker
validation can produce `superseded` (worker exit 75). The stream never sets
`project_current` true. Newly added bodies require another inventory/run. Consumers
must retain their buffer/generation guards; dependency-aware restarts are the
later incremental-save feature.

## Bounds and cancellation

There is one worker at a time and one analyzed body per compiler process. Each
worker—including its Cargo/compiler/build-script descendants—gets a fresh
systemd scope with `MemoryMax`, no swap, and a wall-time limit. Kernel OOM events
terminate the scope immediately even if surviving descendants hold pipes open.
The existing plugin-target launch lock serializes Cargo artifact preparation
against foreground requests. Default limits are 6 GiB and 600 seconds per worker;
these are configurable bounds, not measured project performance budgets.

The worker supervisor has its own process group and watches a lifetime pipe held
only by the coordinator. SIGINT/SIGTERM, `--cancel-file PATH`, disconnected output,
or coordinator death close that lifetime. Scope-wide cleanup also reaches children
that called setsid. A stopped coordinator cannot leave a compiler warming forever.
Control commands have their own two-second limits; the coordinator applies a
bounded supervisor grace period. Output backpressure remains cancelable.

Worker stdout is limited to 32 MiB encoded, stderr to 1 MiB retained while the
remaining diagnostics are drained. Supervisor responses, inventories and decoded
result scans are separately bounded. `observed_peak_memory_bytes` can miss a
scope's last moments; it is labeled as an observation rather than a complete RSS
measurement. OOM classification requires a kernel counter, not an empty response.

Restarting re-enumerates the target and reuses each valid body response from the
existing store. There is no second cache or independent invalidation policy.
Inventory expansion still invokes the compiler; unchanged body responses can
replay with zero body compiler invocations.

## Validation and remaining gates

`scripts/test-project-coordinator.py` exercises real systemd scopes and actual
compiler requests in an isolated fixture. It compares streamed body results to
standalone cache-off analysis in both modes, including closures, custom library
and binary targets, and external module files. It checks early output, priorities,
warm restart, type-error isolation, timeout/OOM continuation, signal/cancel-file
handling, setsid descendant cleanup after coordinator SIGTERM/SIGKILL, source
edits, disconnected output, backpressure and missing user-systemd behavior.

These focused tests establish process/protocol behavior. Full project throughput,
time to first useful result and resource-growth measurements on large corpus
projects remain acceptance gates. Neovim background scheduling, foreground
preemption and dependency-aware saves are subsequent features in the same stack.

The immutable `project-coordinator-v1/` build at `2fae9dd12` passes all 16 public
coordinator scenarios and all 13 existing worker scenarios. The IDE all-targets
suite passes 38 tests, and the Python harness suite passes 55 regressions.
[The compact report](measurements/project-coordinator.json) records exact build
and harness hashes, case outcomes and raw-report checksums. The full cache-off/refresh corpus has now passed all 2,880 locked selections
in both modes, with no output differences or status changes. The
[coverage report](measurements/coordinator-cache-refresh-complete.json) retains
build/manifest provenance and the raw report checksum. Its 2,832 analyzed misses
per side exclude 48 successful selections without an analyzed body. The separate
warm-cache corpus remains running. These runs use 6 GiB per request and two
workers; timings collected under host load do not establish performance gates,
and later stack changes still require final-tip validation.
