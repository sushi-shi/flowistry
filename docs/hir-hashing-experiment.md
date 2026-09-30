# HIR hashing investigation: no compiler change retained

Continuation step 5 concludes with no retained optimization. The current compiler
does observable HIR hashing for library targets, but the examined shortcuts either
do not bypass it or change target/hash semantics. This is a scoped rejection of
those approaches, not a claim that compiler hashing can never be improved.

## Profile and source evidence

Sampled normal release `f1163469c`, with semantic cache off, isolated Cargo replay,
one warmup and 12 repeated SigOnly requests per case:

- Locked serde_json: `src/de.rs:329:8`.
- Locked just: `src/indentation.rs:45:4`.
- A binary-only fixture with `main` and 128 tuple-argument functions, one
  deterministically selected position. This fixture is an additional probe,
  not part of the locked corpus coverage gate.

Both real projects select **library** targets in this corpus; the replay records
confirm their target kind and compiler `--crate-type lib` arguments. Do not treat
just as binary-target evidence merely because it is distributed as a command.
Library profiles contain `TyCtxt::hash_owner_nodes` and HIR `HashStable` symbols.
Each final case has 12 successful, semantically stable samples. No HIR owner/hash
symbol was sampled in the explicit binary fixture, consistent with the guard
below; absence from a sample is not a proof of zero cost.
The captures include preparation, warmup and wrapper processes on a loaded host;
they locate work but do not establish an exact fraction of an editor request or
a latency improvement. Stack unwinding was incomplete, so the call relationship
below comes from the pinned source, not an inferred complete sampled call graph.

In pinned rustc `f53b654a8882fd5fc036c4ca7a4ff41ce32497a6`, AST-to-HIR lowering
constructs owners, invokes their hashing and then optionally computes the crate's
HIR hash. Query scheduling of later MIR analysis does not avoid that owner-hashing
step. See [the pinned lowering implementation](https://github.com/rust-lang/rust/blob/f53b654a8882fd5fc036c4ca7a4ff41ce32497a6/compiler/rustc_ast_lowering/src/lib.rs#L531).

`hash_owner_nodes` is guarded by `needs_crate_hash`; it hashes the owner, its
out-of-line bodies, attributes and opaque-type definitions. This is compiler HIR
hashing, distinct from Flowistry's semantic-cache key construction. See
[the pinned owner-hashing implementation](https://github.com/rust-lang/rust/blob/f53b654a8882fd5fc036c4ca7a4ff41ce32497a6/compiler/rustc_middle/src/hir/mod.rs#L233).

The guard depends on the compiler's own debug build, incremental compilation,
metadata requirements, coverage and metrics. Rlib, dylib and proc-macro targets
require metadata regardless of an output-format flag. Flowistry already removes
the incremental option for demand-driven analysis. See
[the pinned guard and metadata conditions](https://github.com/rust-lang/rust/blob/f53b654a8882fd5fc036c4ca7a4ff41ce32497a6/compiler/rustc_middle/src/ty/context.rs#L1168)
and `crates/flowistry_ide/src/plugin.rs::run_with_callbacks`.

## Approaches considered

| Approach | Outcome |
|---|---|
| Disable semantic caching | Already disabled in the probes; compiler owner hashing remains. No cache validation hash is removed. |
| Disable rustc incremental state | Already done by the current driver; not a new optimization, and does not remove library metadata requirements. |
| Change emit/embed-metadata settings | Source inspection rejects this as a way to bypass the guard: metadata necessity is determined by crate type. No timing gain is claimed. |
| Analyze every target as cdylib or executable | Rejected because it changes crate/target semantics; the inherited handoff also reports a slower cdylib experiment, which was not remeasured here. |
| Replace hash providers or suppress required fingerprints | Rejected: the owner-hashing guard is not a configurable per-body query, and weakening hashes would require a separate correctness design/compiler change. |
| Defer hashing by changing later query order | The pinned lowering path performs owner hashing before those later queries; no safe scheduling change was identified. |

No flags, crate types, macros, dependency metadata or cache fingerprints change.
Full combined-chain validation and baseline measurements remain open independently
of this experiment. Future compiler work may revisit hashing behind a supported
analysis mode with its own correctness evidence.

## Reproduction and artifacts

The compact profile manifest is
[measurements/hir-profile-manifest.json](measurements/hir-profile-manifest.json).
Full artifacts are under `target/continuation-validation/`: `hir-{serde,just,binary}-profile.{data,json}`
and trimmed `hir-*-rustc.txt` symbol reports. JSON reports contain source, harness,
environment and binary identities and all repeated semantic outcomes.

Profiles used `perf record --call-graph dwarf -e cycles:u -F 199` around the
measurement harness. Inspect compiler-thread symbols with `perf report --stdio
--no-children --call-graph none --comms rustc --sort symbol`. Compiler worker
threads are named `rustc`, so filtering only `flowistry-drive` omits these samples.
Use the recorded case, frozen binary, `--repeat 12 --warmup 1 --cargo-replay on
--base-cache off --cache-dir ... --phases --memory-limit 6G -j 1` to repeat it.

The binary fixture's Cargo manifest declares only package `hir-binary-fixture`,
version `0.0.0`, edition `2024`, and an empty workspace. Generate `src/main.rs` as
`fn main() {}` followed by `fn fN(x: (u32, u32)) -> u32 {`, a separate line
`  x.0 + x.1`, then `}` for N in 0 through 127. The sampler needs multiline bodies.
Use `--position src/main.rs:8:2`; the harness inherits the locked corpus's sampling
count even for an ad hoc path, so `--positions 1` alone does not limit this probe.
Use `--fresh` after changing generated fixture sources. The earlier empty and
interrupted fixture captures are retained separately and excluded from evidence.
