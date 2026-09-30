# Source selection and constructor highlighting

The gameplay report exposed two source-display problems: a constructor's aggregate
MIR span kept independent fields readable when focusing an input, and editor
extmarks recolored comments covered by broad source ranges. The user also requested
that selecting a parameter type behave like selecting its binding, with a toggle.

The backend now trims plain independent fields from forward-only struct/enum
constructors, using HIR field indices and the operands' incoming provenance. It
retains complete backward dependencies, fields with effects or adjustments, macro
expansions, update bases, unions and control-dependent aggregates conservatively.
This changes human-facing slices; the solver and underlying dependency relation
are unchanged. It does not promise that every readable expression is necessary.

Focus responses add optional `comments` and `parameter_aliases`, indexing the
existing range table. Comments come from rustc's lexer, including nested block
comments and Unicode, while string contents remain ordinary code. Aliases map
HIR parameter type spans to single binding identifiers, including references,
lifetimes and generics. Destructured arguments have no arbitrary chosen binding.
Older clients can ignore both fields. The paired Neovim change excludes comments
from every decoration and offers `parameter_types` (default true) and `:Flow types`.

The portable focus cache is schema 4. Both metadata fields participate in integrity
checks, index validation and token-based relocation. The comparison harness resolves
the additional indices instead of comparing incidental table order. Old frozen
corpus runs remain unchanged; their equality evidence predates this intended slice
refinement and does not prove equality of the new outputs.

## Validation

Compiler source: `ceeabfd97ac5271d676e89cc8007ff061106ce2d`, frozen at
`target/continuation-validation/source-selection-v1` with `build.json`.

- 112 core and 35 IDE tests pass, including both analysis modes, reordered fields,
  backward selection, effectful fields, control dependencies and update syntax.
- 55 Python harness tests pass, including metadata table permutation, streaming
  decoding, invalid references and detection of changed alias targets.
- Existing real cache suite: 51 cross-process cases plus error rejection.
- `scripts/test-source-selection.py`: eight compiler requests verify fresh output,
  snapshot replay, whitespace relocation, changed comments and corrupt metadata.
- Paired editor: 242 frontend assertions; a real-compiler selection test covers
  constructor dimming, comments, type aliases, pins and the live toggle. The actual
  rust.vim formatter test passes 73 assertions across 11 saves, including Insert mode.
- Read-only analysis of the user's saved `continue_saved_game` in gameplay.rs
  includes both health assignments and excludes `simulation_time_millis: 0`,
  `last_wall_clock_millis: 0` and `frame_delta_millis: 0` when selecting `saved`.
  Its metadata includes 23 comments and the requested `&LevelMap` → `map` alias.

Detailed local logs: `/tmp/flowistry-source-selection-{core,tests,python,cache,
metadata-cache,live,save}.log`; actual-game result and checks are
`/tmp/flowistry-source-selection-stalker.{out,log}` and
`/tmp/flowistry-source-selection-stalker-check.json`.

This is a bug-fix addition after project worker PR #51. The project coordinator
remains separate WIP at `/tmp/flowistry-project-coordinator`; its uncommitted changes
are preserved. The thirteen-step continuation plan remains unfinished.
