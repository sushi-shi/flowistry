# Combined-chain corpus comparison

The #17 reference (`82d430807`) and integrated backend (`2630ee356`) have now been
compared at all **2,880 locked position/mode pairs across 24 entries**. There are
**2,827 equal successful pairs**, 48 matching benign selections, and five
reference-OOM/integrated-success cases. There are no successful-output differences,
missing positions, duplicates or unexplained status changes.

This is evidence about the integrated starting backend, before the row-group,
seed and versioned-index continuations. It is not a full final-tip release gate.
The [compact report](measurements/combined-equivalence.json) records exact builds,
compiler, corpus, segment provenance, coverage and every resource-limited outcome.

| just position (zero-based) | Mode | Reference outcome | Integrated peak RSS |
|---|---|---|---|
| `src/analyzer.rs:177:4` | Recurse | OOM at 6 GiB cap | 1,698 MiB |
| `src/analyzer.rs:324:10` | Recurse | OOM at 6 GiB cap | 1,699 MiB |
| `src/compiler.rs:13:4` | Recurse | OOM at 6 GiB cap | 2,827 MiB |
| `src/error.rs:926:10` | SigOnly | OOM at 6 GiB cap | 651 MiB |
| `src/error.rs:926:10` | Recurse | OOM at 6 GiB cap | 838 MiB |

Those five pairs cannot establish successful reference equality. The independent
engine/eager sweep also has complete coverage but remains resource-limited at
these positions: four kernel-confirmed OOMs and one timeout. See
[reference-failure-triage.md](reference-failure-triage.md). Raw failures remain in
both reports. Loaded-machine wall times are not quiet performance measurements.

The original harness rejected an internal directory symlink, canceled later
queued entries, and failed to assemble its report. Its 1,560 recovered records
retain the original manifest; recovery checked each checksum, key, locked
position, source hash and saved harness hash. Corrected-harness reruns add
Alacritty/Bevy (240 pairs) and the nine canceled entries (1,080 pairs).

The assembly verifies matching binary/build, compiler, corpus and effective
settings. The original recorded harness hardcoded `focus` and inherited its
launch cache policy; later manifests explicitly record those same defaults.
All three manifests are retained verbatim, with per-crate segment attribution.
No old checkpoint is represented as the product of a newer harness.

Detailed local evidence under `target/continuation-validation`:
`equivalence-{recovered,symlinks,remaining,complete}.json`,
`equivalence-complete-summary.json`, `recover-equivalence.py`, and
`assemble-equivalence.py`. The next live gates compare row groups with their
predecessor across the Recurse corpus and compare cache-off/warm file-focus at
the frozen versioned-index backend. The remaining requirements are tracked in
[continuation-progress.md](continuation-progress.md).
