# Optional cached callee analysis

The packaged backend can look inside supported local methods instead of deriving
every effect from their signatures. A method that writes `self.b` can leave
`self.a` unrelated, even when its receiver is `&mut self`. Calls through nested
methods preserve real transitive dependencies.

Enable this in the Nix launcher with:

```lua
vim.g.flowistry_config = { context_mode = "Recurse" }
```

For direct plugin setup, pass `context_mode = "Recurse"` to
`require("flowistry").setup()`. Omit the option or use `"SigOnly"` to retain the
default. The option is passed before the backend subcommand for both `focus`
and `file-focus`; JSON responses are unchanged. Reconfiguring resets the editor's
cached results so different analysis modes cannot share stale results.

## Packaging and backend ownership

The flake pins [Flowistry fork commit
`9528045feec9f48abf4383f9b43bdcf7e0c6c341`](https://github.com/sushi-shi/flowistry/commit/9528045feec9f48abf4383f9b43bdcf7e0c6c341)
directly. It contains the combined command, precise source ranges, and cached
callee summaries, so the Neovim package needs no build patches.

The backend implementation is reviewed in [Flowistry PR #1](https://github.com/sushi-shi/flowistry/pull/1),
against its `fork-base` branch. The Lua adapter only selects the analysis mode;
the implementation and its regression tests remain editor-independent in Rust.
The full commit pin makes builds independent of branch movement or merge timing.

## Cache and precision boundaries

Each compiler invocation caches compact field read/write and input/output
summaries by resolved callee instance. Batched roots and repeated calls reuse
them. These session-owned summaries end with the compiler process. A separate
[persistent focus cache](cache.md) reuses completed analysis across processes
after validating the function and callees. There is no background whole-project
analysis. The editor's memory cache handles cursor movement, unchanged saves,
and off/on without compiler requests.

Direct local functions, inherent methods and statically resolved local trait
implementations can be summarized. Dynamic or unresolved calls, external bodies,
unsupported memory operations, and edges within recursive call cycles retain
conservative signature effects. Some projections widen to an enclosing field.
This remains a conservative reading aid under Flowistry's alias-model assumptions.

`Recurse` may cost more than signature-only analysis. It stays opt-in; caching
improves repeated callee analysis but does not eliminate compiler startup or
crate checking. Highlighting may still include a call if its transitive callees
really read the selected field.

`nix flake check` verifies actual Neovim decorations with the packaged compiler:
the default and explicit `SigOnly` retain signature effects, `Recurse` separates
untouched fields, nested writes remain relevant, and saving an altered callee
changes the dependent field. It covers whole-file, selected-body and unbatched
requests. Run `nix develop -c make test-summaries` for the same test interactively.
