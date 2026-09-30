# Reproduced stack review fixes

The review of `7007b2abd` reproduced three bugs. Each fix is committed in its
owning PR and carried through dependent branches with ordinary parent merges.
No PR was merged into master and no history was rewritten.

| Owning PR | Reproduction and fix | Regression evidence |
|---|---|---|
| #2, `918917d92` | At a closure's end position, a warm parent response selected the enclosing function while fresh rustc selected the closure. Cached containment now includes the end position, matching rustc. | All 36 real fast-cache cases pass, including cold/warm/fresh closure-boundary equality; 18 warm cases invoke no compiler. |
| #53, `ff195ac55` | Cancelling a foreground request killed its launcher but left the compiler wrapper alive. Unix foreground jobs now own a process group; cancellation and timeout kill that group. | Six process-tree assertions pass. The original real Flowistry/Cargo/compiler reproduction now leaves no live compiler wrapper and delivers no cancelled callback. |
| #57, `e73272889` | Wiping 16 non-Rust buffers retained 4 MiB of source snapshots. Snapshots now belong to buffer IDs and are released on unload/wipe; file rename refreshes the snapshot. | Eight distinct buffers, alternating unload/wipe, release their saved strings. All 54 input-invalidation assertions pass. |

The combined tip passes all **389 Neovim assertions**, **67 Python harness tests**
and the cache process tests above. Runtime build `review-fixes-v1` is frozen from
`9ba7f274a83fd5162974c078835911ec94ae06cb`; #60 uses identical runtime sources.
Foreground process-group cancellation covers descendants that stay in the
group; the background coordinator retains its separate cgroup/lifetime bounds.

The unfinished [project measurement harness](project-measurements.md) is included
in #60. Six focused Python regressions check coverage, failures, terminal counts,
selection identity/ranges, canonical output and missing memory samples. Real
SigOnly and Recurse probes each pass for three bodies, one populate pass, one
warm pass and three cache-off oracle requests. Warm requests invoke no compiler.
These are functional checks on an uncontrolled host, not performance acceptance.

Compact machine-readable evidence is in
[measurements/review-fixes.json](measurements/review-fixes.json). Raw local evidence:

- Before-fix review: `/tmp/flowistry-review-60/review.json` and `repros/` beside it.
- Fixed compiler cancellation: `/tmp/flowistry-review-fixes-repros/cancellation/report.json`.
- Frozen binaries: `target/continuation-validation/review-fixes-v1/`.
- Project probes: `target/continuation-validation/project-measure-probe-v3/{SigOnly,Recurse}/`.
- User-requested stop record: `target/continuation-validation/user-stop-acceptance-20260930.json`.

The broad implementation/acceptance plan remains paused. The stopped corpus
runs have partial checkpoints; they are not completed passes. Existing quiet-host
performance and large-project resource gates remain unestablished. Future tool
usage and reproducible reports can drive further fixes.
