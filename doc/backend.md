# Shared analysis backend contract

This boundary is editor-independent. Neovim and a future Zed integration should
invoke the same Flowistry Rust backend. No Rust semantics, ownership rules, borrow
checking, or information-flow computation are implemented in this repository.

```text
                  cargo flowistry (Rust)
             compiler / ownership / information flow
                             |
                 compressed JSON over stdout
                     /                 \
       Neovim transport + UI        future Zed adapter
```

The integration targets Flowistry fork revision
`ad859be49003040aac7f6767db6798d971d0d4a2` (0.5.44), with its locked
`rustc_utils = 0.15.0-nightly-2026-05-01` dependency. This is a description of that
protocol, not a claim of upstream version stability. The fork includes the
combined command below, refined human-facing focus ranges, and optional
[cached callee summaries](summaries.md). The package consumes this commit
directly without build patches.

The range refinement keeps simple independent arguments dimmed in forward uses of a value.
For example, focusing `section` leaves `camera[0]` dim in a call whose other
arguments depend on `section`. Selecting the call result retains all its inputs.
It uses the existing dependency analysis before the call; it does not infer
dependencies from identifier spelling. Macros, methods, adjusted/overloaded
expressions and side-effecting arguments retain conservative ranges.

Repeated target dependencies, location-to-source mappings and character-range
conversions are reused within each function analysis. FocusOutput remains
unchanged, and both per-function and combined requests use the same refinement.

## Packaged combined analysis

```text
flowistry-backend file-focus FILE [LINE COLUMN]
```

Without a position, analyzes all function bodies in the file in one compiler
session. With a position, discovers all bodies but analyzes only the smallest
enclosing body. This avoids a separate compiler run just to discover functions.
The response uses the same encoding as upstream:

```text
Ok: {
  bodies: [{ range: Range, focus: { Ok: FocusOutput } | { Err: string } | null,
             cached: boolean | null }],
  cache: { hits: number, misses: number, validation?: "snapshot" }
}
```

An unanalyzed body has `focus: null`; clients can later use upstream's `focus`
command for that body. All file IDs in this response share one compiler session.
The Neovim adapter instead sends another position-specific `file-focus` request
and merges that function's result into its existing cache.
Individual analysis errors do not remove other bodies' successful results.
Cache metadata is additive and may be absent from older backends. Persistent
results are validated by the compiler and reconstructed using the current file
IDs and source coordinates; see [caching](cache.md).
The Neovim launcher batches only files of at most 600 lines by default, avoiding
an eager analysis of every function in large generated files. This command and
its results remain editor-independent and can be reused by a Zed adapter.

In the default intraprocedural mode, the compiler callback collects additional
Polonius facts only for the requested source file, or the bodies enclosing the
requested position. Collecting enclosing bodies preserves nested-closure facts.
Ordinary rustc checking still runs across the crate; errors elsewhere are still
reported. Other analysis modes retain upstream's full collection behavior.

## Commands

Run in the Cargo workspace, using the compiler pinned by the backend:

```text
cargo +nightly-2026-05-01 flowistry spans /absolute/path/to/file.rs
cargo +nightly-2026-05-01 flowistry focus /absolute/path/to/file.rs LINE COLUMN
```

`spans` discovers function and closure bodies in the requested file. `focus`
analyzes the smallest body enclosing the position and returns precomputed
dependency slices for its source places. Both commands read files from disk.
`LINE` and `COLUMN` are zero-based; columns count **Unicode scalar values**,
including combining characters separately. They are neither byte offsets nor
UTF-16 code units. Ranges have exclusive ends.

Compiler setup must provide the matching toolchain and its compiler libraries.
The default Neovim transport uses `rustc --print target-libdir --print sysroot`,
sets `SYSROOT` and the platform's library path, and uses
`cargo locate-project --workspace --message-format plain` to resolve the workspace.

## Encoding and errors

On success, stdout contains `base64(gzip(UTF-8 JSON))`. Decode in that order. Keep
stderr separate; it contains compiler diagnostics. A nonzero exit status is a
build/process failure, even if stdout contains partial output. An exit status of
zero may still contain a serialized analysis error:

```json
{ "Err": { "type": "AnalysisError", "error": "explanation" } }
```

Other error tags include `FileNotFound` and `BuildError`; compiler build failures
normally use a nonzero process exit instead. Spawn failures, timeouts, malformed
base64/gzip/JSON, and invalid response shapes must all clear stale decorations.

## Decoded response shapes

`spans`:

```json
{
  "Ok": {
    "spans": [{
      "filename": 1,
      "start": { "line": 0, "column": 0 },
      "end": { "line": 5, "column": 1 }
    }]
  }
}
```

`focus` (schematic: each `Range` below has the shape shown above):

```text
Ok: {
  containers: Range[],
  place_info: [{
    range: Range,
    ranges: Range[],
    slice: Range[],
    direct_influence: Range[]
  }]
}
```

- `containers`: body, return type, and optional argument spans that can be dimmed.
  The first entry identifies the selected function body's source file.
- `range`: source place used for cursor lookup.
- `ranges`: selected place's display spans.
- `slice`: code related to that place through information flow.
- `direct_influence`: additional spans to emphasize.

The pinned dependency serializes `filename` as an **opaque numeric file ID**, not
a path. IDs are local to one compiler invocation and must never be compared
between `spans` and `focus` responses. `spans` filters its output to the requested
source file. For `focus`, use the first container's file ID to filter source ranges
before mapping coordinates into the editor. Other files can occur through macro
expansions. The Neovim adapter also accepts the legacy string-path representation.
Newer upstream utility revisions may change this representation again; pin and
verify the dependency before upgrading.

## Frontend responsibilities

1. Capture the saved document revision and invoke `spans` asynchronously.
2. Convert source columns into the editor's coordinate system.
3. Find the smallest enclosing function/closure and lazily invoke `focus`.
4. Find the smallest source place containing the cursor, or the pinned cursor.
5. Dim the complement of the slice within the function containers; emphasize
   selected spans. Do not dim anything when no source place matches.
6. Cache function results until project inputs change. Reject late results after
   edits, renames, disabling, or buffer disposal.

The current Lua modules are an editor adapter, not a backend library. A Zed
extension may need a host-side bridge or language-server transport to expose
this analysis in Zed's editor APIs. That bridge should preserve these Rust
results and keep display coordinates out of the analysis layer. If a persistent
server is added later, put it beside the Rust backend and share it between editors.

## Sources and fixtures

Protocol details were checked against the pinned fork's
`crates/flowistry_ide/src/{plugin.rs,spans.rs,focus/mod.rs}` and the locked
`rustc_utils` crate's `source_map/{range.rs,filename.rs}`. The upstream TypeScript
`Range` declaration is older than the numeric filename serialization and should
not be used as the authoritative wire schema.

`tests/fake_backend.lua` generates synthetic wire responses independently of the
frontend, with distinct file IDs for each command. `tests/live.lua` tests the real
backend when installed. Synthetic fixture success does not establish compatibility
with an untested backend revision.
