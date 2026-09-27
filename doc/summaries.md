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

`patches/cached-callee-summaries.patch` is the Rust-crate diff of local Flowistry
fork commit `54f8e9e` against its `fork-base` commit `6097c62`. The preceding two
patches reproduce that base on pinned upstream `693ceda925bd1d39d8de413ce239cfa6a87bb665`.
The patch includes the backend regression tests. Analysis implementation remains
in the editor-independent Rust backend; the Lua adapter only selects the mode.
This packaging PR is separate from the fork's backend implementation branch.

This carries a reproducible snapshot while the backend fork is local. A later
package update can pin the published fork commit and remove all three patches.
Installing vanilla upstream via the README's Cargo command does not include
the new summary implementation.

## Cache and precision boundaries

Each compiler invocation caches compact field read/write and input/output
summaries by resolved callee instance. Batched roots and repeated calls reuse
them. The cache ends with the compiler process; saving starts fresh analysis,
including updated callees. There is no persistent disk cache or background
whole-project analysis. The editor's existing function-result cache still
handles cursor movement without compiler requests.

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
