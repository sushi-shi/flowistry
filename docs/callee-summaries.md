# Cached callee summaries

This fork improves the existing opt-in `Recurse` mode. A local method that only
uses `self.b` no longer connects an unrelated `self.a` operation to the call just
because its receiver is `&mut self`. Dependencies through nested methods are
retained. The default remains `SigOnly`.

```sh
cargo flowistry --context-mode Recurse file-focus /absolute/path/to/file.rs LINE COLUMN
```

Positions are zero-based. Omitting the position analyzes the file's bodies in one
compiler session and shares their callee summaries. `focus` also supports the
same context flag. The JSON protocol and editor integration are unchanged.

Build with the repository's pinned `nightly-2026-05-01` toolchain, including
`rustc-dev`, `rust-src`, and `llvm-tools-preview`:

```sh
cargo build --locked --release -p flowistry_ide
```

Put `target/release` on `PATH` when running `cargo flowistry` from another
workspace. As with upstream, rustc's shared libraries must be discoverable and
`SYSROOT` must identify the matching compiler. The existing Neovim package still
pins its own backend; updating that package is a separate rollout.

## What is cached

`AnalysisSession` owns completed summaries, keyed by resolved rustc instance
(callee plus region-erased substitutions). Its evaluation mode is fixed at
creation. `compute_flow_with_session` shares it across roots;
`compute_flow` remains a compatibility wrapper creating a fresh session.

A summary contains:

- The formal argument fields the function reads, including read-only calls.
- Fields reachable through arguments that it may write.
- The incoming-field dependencies of each write and each returned field,
  including branch conditions.

Summary analysis uses a separate bitset domain of incoming field origins. An
argument's leaf fields have distinct origins; placing all of them in their
ancestor row would immediately make siblings dependent again. Pointer/address
provenance is distinct from the contents of the referent. Unknown calls explicitly
consume reachable values, preserving dependencies through operations such as
`Vec::len` despite this distinction.

Both summary analysis and ordinary location analysis use the same transfer,
mutation, alias and control-dependency logic. Instantiated call effects are shared
by transfer, forward slicing, and direct-influence highlighting. Forward slicing
records read dependencies before applying mutations, including calls with no
returned value or writes.

The cache retains compact summaries, not the full per-location flow tables of
every callee. Temporary tables are released after summarization. Unsupported
bodies are cached too. No cache contains identities from another compiler run;
each process starts fresh, so changed methods or dependencies are reanalyzed.
This is not a persistent editor cache or a background whole-project index.

## Conservative boundaries

Direct local functions, inherent methods, and statically resolved local trait
implementations are supported. Generic bodies are analyzed parametrically in
their own typing environment. Unresolved generic dispatch remains opaque even
when an outer instantiation could permit further specialization.

The reachable definition graph is divided into strongly connected components
before summaries are computed. Edges inside a recursive component use signature
effects. Other edges use summaries, without an arbitrary call-depth limit. This
also bounds expanding generic recursion and makes results independent of root
order. Intraprocedural loops still use the existing fixed-point solver.

External implementations, indirect/dynamic calls, compiler shims, and generic
callees instantiated with mutable or consuming closures retain signature-based
effects. A local closure called directly is summarized like any other body; its
upvar fields are translated through the caller's closure type. Bodies holding or
dereferencing raw pointers, union access, inline assembly, coroutine suspension,
or other unmodeled terminators are not refined. Argument layouts exceeding the
existing alias model's incoming-pointer depth also use signature effects. Opaque
generic value origins include borrowed contents that become visible in the
caller. Destructors are opaque effects on their operands and reachable state.
Opaque calls receiving shared references to interior-mutable state (`Cell`,
`RefCell`, `Mutex`, atomics) may write its innermost `UnsafeCell`-containing
places, in both modes; frozen sibling fields stay independent.
Private fields and unrepresentable projections widen to a representable ancestor;
array indices are not transferred as another body's MIR locals. When widening
drops a dereference, everything mutably reachable from the ancestor is possibly
written, since writing the ancestor itself would not reach the pointee. Fallbacks
apply to the actual arguments, so an opaque call receiving `&mut self.b` need not
include `self.a`.

Possible writes are accumulated at every reachable write, including paths that
panic. Updates through reference arguments remain weak. Widened return effects
and returns with cleanup successors are also weak, avoiding loss of dependencies
through overlapping output projections or the engine's conservative unwind-edge
treatment. This retains Flowistry's existing alias-model assumptions; it is not
a new soundness guarantee for arbitrary unsafe code or interior mutability.

`AnalysisSession::stats()` and `RUST_LOG=flowistry::infoflow::session=info` expose
computations, cache hits, fallback reasons and construction time. Statistics go to
stderr, never protocol stdout. Construction time counts nested builds once and
excludes graph preparation, rustc checking and source-range conversion.

## Validation and measurements

The fork baseline is `6097c62`, containing the pre-existing editor-independent
file-focus and precise-range patches on upstream
`693ceda925bd1d39d8de413ce239cfa6a87bb665`. This feature is a separate branch and
diff against that baseline.

Validation on the pinned compiler:

```sh
cargo test --locked --workspace --all-targets
cargo test --locked -p flowistry --doc
```

Both pass, including 36 new regression tests. An additional
`cargo check --locked --workspace --all-targets --all-features` fails in the
optional legacy `decompose` feature. The same 12 compiler errors reproduce on
`fork-base` (removed source-map/index APIs, `HybridBitSet`, and old visitor
signatures). Repairing that pre-existing optional feature is outside this PR.

The new tests cover untouched fields in both directions, transitive reads,
read-only unit calls, control flow, aliases, independent returned fields, generic
receiver types and distinct instance keys, constant writes, opaque borrowed
inputs, privacy widening, panic paths, a 40-callee chain, dynamic/static trait
dispatch, unsupported memory operations, recursive root-order independence,
shared diamond graphs, batch roots, cached unsupported summaries, deeply nested
borrows, opaque generic borrowed contents, directly called closures writing
through upvars, interior mutation through shared references, raw pointers passed
to opaque calls, and new compiler sessions after edits.
The IDE test checks focus/direct-influence ranges and the unchanged serialized
output shape. Existing slicing fixtures remain unchanged.

The real `interactive_visual_bounds.clear()` to `load_sublevel_runtime()` link in
`stalker-mobile` is a positive case: that method eventually reads the field through
`configure_camera_and_aim_bounds()` and `compute_interactive_aim_bounds()`. The
reduced regression preserves that dependency; it is not an independence example.
The real emitted focus output was also decoded and checked: both targets on the
clear statement retained the later method call in their slices.

Measurements on this development machine, 2026-09-27, release builds, three
measured fresh compiler processes after one priming process per variant:

| Workload | Mode | Median wall time | Median peak RSS |
| --- | --- | ---: | ---: |
| Seven-level diamond graph | Baseline `SigOnly` | 0.09 s | 91.1 MiB |
| Seven-level diamond graph | Baseline `Recurse` | 0.12 s | 100.3 MiB |
| Seven-level diamond graph | Cached `Recurse` | 0.09 s | 91.4 MiB |
| `gameplay.rs`, `load_sublevel_runtime` | Baseline `SigOnly` | 2.70 s | 393.3 MiB |
| Same game function | Baseline `Recurse` | 3.79 s | 850.1 MiB |
| Same game function | Cached `Recurse` | 3.03 s | 668.9 MiB |

The diamond graph produces 15 summary computations and 15 hits instead of 383
full flow analyses including the root. The game request produces 66 computations
and 44 hits instead of 176 full flow analyses including the root. In one measured
game run, summary construction was 107 ms and the root Flow timer was 142 ms
(baseline recursive root: 486 ms). Cached summaries reduce retained memory; rustc
checking and other work still dominate end-to-end latency. These small samples
are measurements, not performance guarantees or justification to switch defaults.

Cargo artifacts were reused, but no summary survived between samples. The priming
processes are not clean builds. An earlier game request that also incurred setup
work took 13.4 seconds; that is not comparable to the warmed-Cargo table above.

Reproduce with two release builds and GNU time on `PATH`:

```sh
python3 scripts/bench-summaries.py \
  --baseline /path/to/baseline/target/release/cargo-flowistry \
  --candidate /path/to/fork/target/release/cargo-flowistry \
  --output target/summary-bench
```

For real code add `--project WORKSPACE --file SOURCE --position LINE COLUMN` and
enter that project's build environment first. The script creates a dependency-free
diamond fixture by default, checks decoded analysis responses, and writes timings,
stderr and decoded JSON into the requested new output directory.
