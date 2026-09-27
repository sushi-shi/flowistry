# Reusing analysis

The backend implementation is reviewed separately in
[Flowistry PR #2](https://github.com/sushi-shi/flowistry/pull/2).

Caching is enabled by default, at two levels:

- Memory results handle cursor movement, buffer switching, off/on, unchanged
  saves, and edits undone before saving without launching the backend.
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
`$HOME/.cache/flowistry/focus-v1`. `FLOWISTRY_CACHE_DIR` can override the shared
root for all adapters. The backend also accepts `FLOWISTRY_CACHE=off` or
`FLOWISTRY_CACHE=refresh`. Removing the directory is safe.

Entries are atomic and checksummed, capped at 32 MiB each. The shared directory
is limited to 256 MiB and 2048 entries, evicting the oldest written results.
Corrupt entries, unwritable storage and unsupported cases become cache misses.
Compilation failures never become successful cache entries.

`require("flowistry").cache_status()` reports the latest batched request's
`{ hits, misses }`, and `indicator()` identifies a selected disk-cached body.
Older backends without cache metadata remain compatible.

## Latency and scope

A persistent hit skips Flowistry's information-flow solve and slice generation.
Cargo and rustc still validate the current crate to establish that reuse is safe;
this also catches errors outside the selected function. On the measured large
game function, that validation still took roughly three seconds. Persistent
reuse does not yet provide an instant cold start or a compiler-free disk lookup.

Unsaved edits retain the previous display with its saved-analysis label; they
are not compiled. Pins keep their existing edit/save behavior. Off clears the
pin but retains valid analysis. Unloading a buffer releases its memory cache;
disk results remain available. External source/dependency changes may require
`:Flow refresh`, as before. There is no background whole-project index.
