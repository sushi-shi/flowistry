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

## Lookup before compiler startup

For `file-focus`, an unchanged saved-input snapshot replays the prepared wire
response before Cargo metadata, Cargo check or rustc starts. Different cursor
positions inside the same analyzed body share that result; other functions and
nested bodies need their own completed analysis. Both analysis modes support
this path. The legacy `focus` command uses compiler-validated results only.

The snapshot watches the selected package's dependency closure, including
registry and path sources; all resolved manifests and the workspace lockfile;
Cargo config and toolchain files; backend/compiler executables and configured
wrappers; compiler-declared include/proc-macro inputs; and Cargo build-script
outputs, generated files and declared rerun inputs. The invocation environment
participates in its key, excluding Nix scratch-directory and shell-launcher
variables. Explicit uses of those variables in compiler/build-script dep-info
disable fast replay, preserving declared semantic dependencies. Changes during analysis prevent saving a response.
Input checks use filesystem size, nanosecond mtime, ctime and inode on Unix.
Changed stamps trigger content hashing, so an unchanged save or undo can still
replay a response;
other platforms fall back to compiler validation. Missing/unreadable inputs,
symlink cycles and excessive trees also fall back. As with Cargo, external
inputs used by build scripts or proc macros must be declared. Changes to
untracked network/time inputs require an explicit refresh.

After any recorded input changes, Cargo/rustc validation runs again. The
function-level cache below it can still skip solving and slicing when the
function and its dependencies remain semantically unchanged. This catches
compilation errors elsewhere rather than accepting a stale snapshot. Unsaved
incomplete Rust is not compiled.

Warm `gameplay.rs` requests for `load_sublevel_runtime` measured about 200 ms
with snapshot replay, versus about 3.2 seconds with compiler validation. These
are individual local warm-filesystem samples, not a universal latency guarantee.
The response is stored ready to send, avoiding reconstruction of several MB of
JSON on each hit. Neovim separately avoids backend processes for retained
memory results.

## Controls and storage

| Environment variable | Meaning |
| --- | --- |
| `FLOWISTRY_CACHE=off` | Skip persistent reads and writes |
| `FLOWISTRY_CACHE=refresh` | Recompute and replace results for this request |
| `FLOWISTRY_CACHE_DIR=/path` | Override the shared cache root |

The default root is `$XDG_CACHE_HOME/flowistry`, or `$HOME/.cache/flowistry`.
Function entries live under `focus-v1`; prepared responses under `responses-v1`. Without a cache location or executable identity,
analysis proceeds without persistent caching. Deleting the cache is safe.

Writes are atomic. Invalid JSON, schema/key/checksum mismatches, invalid ranges,
missing files and cache I/O failures become misses. Only successful analyses are
written. Entries are limited to 32 MiB each; storage is trimmed to 256 MiB and
2048 function entries by removing the oldest written results. The response cache
has its own 256 MiB / 256-entry cap. Concurrent access may cause
additional misses but does not require editor-owned locks.

`file-focus` adds `cache: { hits, misses }` metadata and a nullable `cached` flag
on each body. Compiler-free responses also report `cache.validation: "snapshot"`.
`FocusOutput` itself is unchanged. Set
`RUST_LOG=flowistry_ide::cache=info` for stderr hit/miss logs; stdout remains
compressed JSON. Debug logs also report uncacheable ranges.

## Validation

```sh
cargo test --locked --workspace --all-targets
python3 scripts/test-focus-cache.py --backend /path/to/flowistry-backend
python3 scripts/test-fast-cache.py --backend /path/to/flowistry-backend
```

The integration script starts fresh backend processes and checks invalidation,
formatting/Unicode relocation against fresh results, unrelated edits, transitive
and cross-file calls, cycles, closures, macros, constants, types, visibility,
compiler flags, dependency metadata, undo, corrupt entries, unwritable cache
locations and compilation-error rejection. Its fixture and cache are temporary.

The fast-cache tests instrument `RUSTC_WRAPPER` to prove warm hits invoke no
compiler, including cursor changes within a function. They cover external
includes, build-script inputs, environment/config changes, restored mtimes,
refresh, opt-out, corrupted responses and compile-error propagation.
