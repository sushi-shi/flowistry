# Neovim and backend in one repository

The user requested one repository so the Neovim plugin and Rust backend cannot
silently drift through independently maintained source pins.

`nvim/` imports the complete `sushi-shi/flowistry.nvim` history through
`e2394b5`, including editor PRs #1–#5 and the live highlighting fixes. The subtree
merge retains the original commits and license. The separate editor flake and
lock were removed. The source repositories/worktrees and old branches remain
available as historical records; no source or experimental work was deleted.

## Package contract

The root flake builds both `backend` and `plugin` from this source tree, using
one compiler/dependency lock. The plugin's generated `flowistry.packaged` module
binds that exact backend and gzip store path, so direct plugin installation also
gets the paired backend automatically. An explicit `command` override remains
available for backend development; it deliberately opts out of that binding.

- `nix run .#nvim -- FILE`: paired editor launcher, preserving user configuration.
- `tools/flowistry FILE`: local-checkout launcher; run from the project's shell.
- `packages.<system>.plugin`: install directly in Neovim and call `setup()`.
- `packages.<system>.backend` / `default`: existing editor-independent backend.
- `nix develop .#nvim`: frontend development shell with the paired backend.
- `make -C nvim test`: frontend/protocol regressions without compiling Rust.
- `nix flake check`: compiler/editor/cache/package tests for the same source tree.

The existing Rust default and smoke development shells remain available. The
Neovim demo already declares its own Cargo workspace and remains independent.

## Review and validation

The migration follows backend #52. Its initial subtree commit is the exact editor
import; the next commit consolidates packaging, tests, installation docs and CI.
Original editor PRs become historical reviews superseded by this migration. No
PR has been merged. Future editor/backend changes belong in the same PR chain.

The root CI workflow runs all Nix checks on pull requests, including stacked PRs.
`nvim-package` tests the installed plugin with no supplied backend command, then
starts the actual packaged launcher. Both assert that analysis invokes the exact
backend path from this build and renders compiler-derived highlights. Other
checks cover frontend behavior, source selection, call precision, callee summaries
and persistent caches.

Full package validation results will be recorded after the clean candidate build.
The full optimization/background/incremental plan remains unfinished. Preserve
the coordinator WIP in `/tmp/flowistry-project-coordinator` and continue it from
the migration tip when resumed.
