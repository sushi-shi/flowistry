# Project measurement harness

`scripts/measure-project.py` inventories every body in one explicit Cargo target,
runs a cache-populating project pass and repeated warm passes, and compares every
successful result with a separate cache-off body request. All requests use the
same frozen backend. This checks project/cache behavior, not independence from
the analysis engine itself.

Run inside the pinned development environment:

```sh
python3 scripts/measure-project.py \
  --backend-dir /absolute/path/to/frozen-build \
  --project /absolute/path/to/prepared-project \
  --target-dir /absolute/path/to/reusable-target \
  --output-dir /absolute/path/to/new-measurement \
  --package example --target-kind lib --target-name example \
  --mode Recurse --memory-mib 6144 --timeout-seconds 600 --warm-repeats 4
```

The output directory must be new; both output and target directories must be
outside the project. Optional `--features` and `--no-default-features` select the
Cargo configuration. Dependency preparation may generate Cargo.lock during the
initial inventory; source hashes are checked from that point onward. External
dependency trees are not included in this source-tree hash. Use prepared, stable
inputs and retain the exact environment when comparing runs.

The report retains build/binary/harness hashes, raw streams, per-process RSS
samples, per-body cgroup peak observations, compiler/solver diagnostics, stream
timings, wire sizes and disk usage. It validates exact inventory coverage,
selection ranges, terminal counts and complete canonical responses, including
source-selection metadata. A failed or mismatched body is unresolved, even when
both requests fail. Interrupted/failed runs retain raw artifacts and cannot pass.

`correctness: passed` concerns output agreement only. `resource_observations_complete`
means every project body outcome has a peak observation; it does not cover all
memory used by the machine. Fast scopes can disappear before the first sample:
their peak remains `null` and their identity appears in
`missing_memory_observations`. Supervisor RSS samples exclude worker memory.
`resource_acceptance` remains explicitly unestablished. This harness certifies
neither host quietness, cold builds, hardware counters nor editor latency.

The three-body functional fixture passes in SigOnly and Recurse: populate and
warm results match all three fresh body requests; warm passes invoke no compiler.
Some short workers have no memory sample, so neither run establishes resource
acceptance. See [the validation record](review-fixes.md). Broad measurement runs
remain paused.
