# Human-tool review — 2026-09-27

This is a historical record from before the monorepo migration. Current packaging
and validation are documented in [the migration guide](../../docs/neovim-monorepo.md).

Reviewed the Neovim state lifecycle, protocol/error handling, source-range
rendering, airline/CoC integration, and the packaged Flowistry backend. No
agent-query interface was added.

## Findings and changes

- Opening another Rust file invalidated existing analysis and pins. Initial
  reads are now distinguished from reloads; buffers retain independent state.
- Editing an unrelated project could invalidate the current project's state.
  Invalidation is now restricted to the affected root, including old paths on
  rename.
- Editing cleared highlights and pins. The last successful display now remains
  visible, explicitly labelled `[saved analysis]`. An extmark tracks the pin as
  text moves. Saving refreshes automatically; a compile failure keeps the last
  successful display until the code is repaired or Flowistry is disabled.
- Follow-up: rust.vim's formatter replaces entire lines on save, moving the
  extmark from a variable to EOL even when its source text is unchanged. The pin
  now retains a source snapshot and remaps line/byte positions through edits.
  Missing or changed tokens get an explicit `pinned target unavailable` state;
  restoring the text can recover the pin. This is source tracking, not semantic
  binding identity across arbitrary refactors.
- Repeating `:Flow pin` on the pinned token now unpins it, including after it
  moves during formatting. Pinning another token moves the pin. Bare `:Flow`
  and `:Flowistry` show an action menu; cancelling it preserves the current state.
- Saved Rust buffers in Cargo projects enable automatically by default. The
  `auto_enable` setting disables this behavior, and the launcher accepts
  `vim.g.flowistry_config` overrides. Explicitly disabled buffers stay off across
  entering, changing filetype, and saving.
- A dot in `.unwrap_or` could select a whole multiline call expression.
  Punctuation and whitespace now produce no focus selection. The selection
  background is limited to the word under the cursor; dependency spans remain
  unchanged, so selecting a method result still includes its contributing code.
- Appending to an expression-based `%!` statusline could break it or duplicate
  the Flowistry label. Expressions and repeated setup are now handled.
- Adjacent statusline expressions stripped the separator's leading space.
  A conditional group preserves `rust-analyzer | flowistry`; both labels use
  airline's language-server accent. Disabled Flowistry has no label.
- Analysis lacked matching progress UI. CoC's actual animated notification
  renderer now displays phase and elapsed time, without taking keyboard focus.
  Completion, failure, cancellation and reconfiguration close progress windows.
- Empty compiler output could hide diagnostics behind a base64 error. The
  original compiler diagnostics are now retained.
- A relevant call made independent inputs look relevant. The backend now uses
  pre-call provenance to dim independent simple arguments in forward uses.
  Backward slices retain all contributing inputs. Complex/effectful arguments,
  methods and macros retain conservative highlighting.
- Every request collected extra borrow facts for unrelated bodies. Combined
  requests now collect these facts only for the requested file or enclosing
  bodies, while preserving ordinary Rust checking throughout the crate.
  Later uncached functions use the same scoped request and retain earlier caches.
  Target dependencies and source-range conversions are also reused.

## Verification

| Check | Result |
| --- | --- |
| Frontend/protocol suite | 188 assertions passed |
| Real compiler precision suite, including baseline comparison | 279 assertions passed |
| Real compiler editing/recovery suite | 21 assertions passed |
| Actual airline/CoC UI suite | 27 assertions passed |
| Actual rustfmt-on-save with real compiler | 68 assertions passed, ten saves |
| Nested closure: scoped vs per-function output | Matching results |
| Packaged Neovim with stalker-mobile | Camera arguments, backward inputs, text styling, separator, popup cleanup and buffer state checked |

The precision suite covers ordinary values, builtin arrays, control dependencies,
references, effectful arguments, Unicode and closures. It checks that refinement
does not introduce highlighted regions absent from the baseline, and that complete
backward slices for the fixture's call results remain unchanged. An error in an
unselected function must still fail compilation.

The live suite creates a temporary crate, inserts a line before a pinned variable,
saves with the cursor elsewhere, and checks the pin still selects the intended
variable. It also saves invalid Rust, verifies the old display survives, then
repairs the code and verifies automatic recovery. User source is not edited.

The formatter suite starts with a nested `dy` closure at line 5000 in a temporary
crate. It repeatedly inserts blank lines, changes indentation, saves while the
cursor is on `dx`, and checks that `dy` stays selected. Repeated unchanged saves
are covered too. This same test fails against the preceding packaged plugin with
`Flowistry: ON - select a variable`, and passes with the pin fix. The suite uses
the installed rust.vim save hook and actual rustfmt, rather than a formatter mock.
It also verifies pin toggling after formatting and dot/method-name selection
on the resulting `.unwrap_or(0)` call chain.

Commands are documented in the README and exposed as `make test`, `make test-ui`,
`make test-live`, and `make test-precision`. Real backend tests accept
`FLOWISTRY_BACKEND_EXE`; baseline comparison also accepts `FLOWISTRY_BASELINE_EXE`.

## Measured performance

Target: `gameplay.rs`, `compute_enemy_aim_bounds`, selecting `section` at line 5441.
Baseline is the prior combined backend, before precision/scoped-collection changes.
Each backend was warmed once, then three samples were alternated in the same
project/compiler environment. These timings include compiler invocation and
response decoding; they are not cached cursor-motion timings.

| Backend | Samples (ms) | Median |
| --- | --- | --- |
| Baseline | 2317, 2604, 2492 | 2492 ms |
| Revised | 1859, 1916, 1955 | 1916 ms |

That is approximately 23% lower latency for this measured function. A separate
instrumented run collected extra borrow facts for 1 body instead of 1437, with
peak RSS of 386924 KiB instead of 638636 KiB (about 39% lower). Nested closures
may require their enclosing body's facts too.

## Remaining limits

These checks cover concrete regressions and representative functions, not every
Rust program. First use after a backend/compiler change can still be slower.
Each uncached request still starts the compiler and checks the crate; a persistent
compiler service would be a larger latency improvement to investigate separately.
Unsaved code is not analyzed: retained highlights describe the last successful
saved version. Live unsaved analysis would require a compiler file overlay plus
handling temporarily incomplete Rust. Complex calls/macros still have conservative
source ranges. The tool is a reading aid, not proof that dimmed code is irrelevant.
