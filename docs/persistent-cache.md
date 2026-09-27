# Persistent focus results

Successful focus results are cached by default across compiler processes and
editor sessions. If the selected function and its analysis dependencies are
unchanged, Flowistry loads its existing result instead of solving information
flow and constructing slices again. Both `focus` and `file-focus` support this.

The cache belongs to the shared Rust backend. It stores portable source ranges,
never rustc pointers, `DefId`s, borrow tables or session-owned callee summaries.

## Cache validity

The key includes the backend executable and schema, analysis mode, compiler
options/target/cfg, external crate metadata hashes, and expanded local
declarations, visibility, attributes, types and constants. Const-function bodies
and opaque-return implementations are global inputs because they can affect
callers without ordinary call edges.

It also includes the selected body's MIR, region topology, borrow facts and HIR
mapping inputs. In `Recurse`, those inputs include its resolved transitive local
callees and recursive cycles. `SigOnly` does not depend on ordinary callee bodies.

The declaration fingerprint excludes ordinary implementations and their
allocation-dependent body IDs. Editing an unrelated ordinary function can reuse
an existing result. Declaration changes conservatively invalidate more broadly.
Undo can recover an earlier content-addressed entry.

Cached positions use lexer token indices and byte offsets, reconstructed against
current source and file identities. Blank lines, indentation and line splitting
can move code without stale coordinates. Whitespace inside literals remains
significant. Comments inside the selected function are included conservatively
in its token key. Unsupported ranges fall back to analysis.

## Compiler validation still runs

Cargo and rustc check the current crate before accepting a persistent hit. This
supplies resolved types, macros, dependencies and borrow facts for the key, and
prevents cached results from hiding compilation errors elsewhere. The cache
skips Flowistry analysis; it does not skip compiler validation or analyze
incomplete unsaved Rust.

In one `gameplay.rs` check of `load_sublevel_runtime`, forced computation took
3.4 seconds and two confirmed persistent hits took 3.2 and 3.3 seconds. Compiler
validation dominated. These are individual warm-Cargo samples, not a general
speedup claim. Neovim separately avoids subprocesses for unchanged saves,
edit-and-undo, and off/on using retained memory results.

## Controls and storage

| Environment variable | Meaning |
| --- | --- |
| `FLOWISTRY_CACHE=off` | Skip persistent reads and writes |
| `FLOWISTRY_CACHE=refresh` | Recompute and replace results for this request |
| `FLOWISTRY_CACHE_DIR=/path` | Override the shared cache root |

The default root is `$XDG_CACHE_HOME/flowistry`, or `$HOME/.cache/flowistry`.
Entries live under `focus-v1`. Without a cache location or executable identity,
analysis proceeds without persistent caching. Deleting the cache is safe.

Writes are atomic. Invalid JSON, schema/key/checksum mismatches, invalid ranges,
missing files and cache I/O failures become misses. Only successful analyses are
written. Entries are limited to 32 MiB each; storage is trimmed to 256 MiB and
2048 entries by removing the oldest written results. Concurrent access may cause
additional misses but does not require editor-owned locks.

`file-focus` adds `cache: { hits, misses }` metadata and a nullable `cached` flag
on each body. `FocusOutput` itself is unchanged. Set
`RUST_LOG=flowistry_ide::cache=info` for stderr hit/miss logs; stdout remains
compressed JSON. Debug logs also report uncacheable ranges.

## Validation

```sh
cargo test --locked --workspace --all-targets
python3 scripts/test-focus-cache.py --backend /path/to/flowistry-backend
```

The integration script starts fresh backend processes and checks invalidation,
formatting/Unicode relocation against fresh results, unrelated edits, transitive
and cross-file calls, cycles, closures, macros, constants, types, visibility,
compiler flags, dependency metadata, undo, corrupt entries, unwritable cache
locations and compilation-error rejection. Its fixture and cache are temporary.
