# Reusing analysis

The backend implementation is reviewed separately in
[Flowistry PR #2](https://github.com/sushi-shi/flowistry/pull/2).

Caching is enabled by default, at three levels:

- Memory results handle cursor movement, buffer switching, off/on, unchanged
  saves, and edits undone before saving without launching the backend.
- Saved-input snapshots replay completed responses before Cargo or rustc starts
  when their recorded source, dependency, configuration and environment inputs
  are unchanged. Cursor positions in the same analyzed function share a response.
- The shared Rust backend persists completed function results across requests
  and editor restarts. It reuses them when compiler-validated fingerprints of
  the function and its dependencies match.

For `Recurse`, dependencies include the resolved transitive callees and cycles.
For `SigOnly`, ordinary callee bodies are irrelevant. Both modes track broader
compiler inputs: declarations, types, constants, visibility, macro expansions,
external crate metadata, compiler settings and the backend binary itself.
Editing an unrelated ordinary function can reuse the selected function's result.
Some declaration edits conservatively invalidate more broadly.

Ranges are anchored to Rust lexer tokens and rebuilt against the current file,
so blank lines, indentation and line splitting can move unchanged code safely.
Comments within the selected function are included conservatively in the key.
Unsupported source ranges are reanalyzed. Cached data contains no reusable
rustc pointers or identities.

## Configuration

```lua
require("flowistry").setup({
  cache = true,
  cache_dir = nil, -- or an explicit shared cache path
})
```

For the Nix launcher, put those options in `vim.g.flowistry_config` instead.
`cache = false` disables disk caching; normal memory reuse remains enabled.
`:Flow refresh` clears memory results and forces computation for the next
analysis request. The `:Flow` action menu includes refresh.

The default disk location is `$XDG_CACHE_HOME/flowistry/focus-v1`, falling back to
`$HOME/.cache/flowistry/focus-v1`. Prepared responses use `responses-v1`
beside it. `FLOWISTRY_CACHE_DIR` can override the shared
root for all adapters. The backend also accepts `FLOWISTRY_CACHE=off` or
`FLOWISTRY_CACHE=refresh`. Removing the directory is safe.

Entries are atomic and checksummed, capped at 32 MiB each. The shared directory
for function results is limited to 256 MiB and 2048 entries; prepared responses
have a separate 256 MiB / 256-entry cap. Both evict the oldest written entries.
Corrupt entries, unwritable storage and unsupported cases become cache misses.
Compilation failures never become successful cache entries.

`require("flowistry").cache_status()` reports the latest batched request's
`{ hits, misses }`, with `validation = "snapshot"` on compiler-free hits, and `indicator()` identifies a selected disk-cached body.
Older backends without cache metadata remain compatible.

## Latency and scope

An unchanged disk snapshot bypasses Cargo metadata, Cargo check and rustc.
Warm requests for the large `gameplay.rs` function measured about 200 ms locally,
down from roughly 3.2 seconds with compiler validation. Memory hits avoid even
that backend request. These are sample timings, not a universal guarantee.

After an input changes, compiler validation still runs; the function-level cache
can then reuse semantically unchanged analysis, including after unrelated edits
or whitespace changes. First-time analysis still takes seconds. This fast path
uses Unix filesystem change stamps and applies to batched `file-focus` requests.
Transient Nix scratch directories do not invalidate replay across launches;
explicit semantic use of those variables requires compiler validation.
Other platforms and the legacy `focus` command use compiler validation. Build
scripts/proc macros must declare external inputs, as they do for Cargo caching.

Unsaved edits retain the previous display with its saved-analysis label; they
are not compiled. Pins keep their existing edit/save behavior. Off clears the
pin but retains valid analysis. Unloading a buffer releases its memory cache;
disk results remain available. External source/dependency changes may require
`:Flow refresh`, as before. There is no background whole-project index.
