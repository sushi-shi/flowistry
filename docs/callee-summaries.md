# Cached callee summaries

This fork makes the opt-in `Recurse` context mode precise and cheap enough to use
on real code. A local method that only uses `self.b` no longer connects an
unrelated `self.a` operation to the call because its receiver is `&mut self`, and
dependencies through nested calls are kept. The default mode stays `SigOnly`,
which is unchanged.

```sh
cargo flowistry --context-mode Recurse file-focus /absolute/path/to/file.rs LINE COLUMN
```

Positions are zero-based. Without a position, `file-focus` analyzes every body of
the file in one compiler session, and the bodies share their callee summaries.
`focus` supports the same flag. The JSON protocol is unchanged.

## What a summary is

`AnalysisSession` (in `flowistry::infoflow`) owns the summaries of one compiler
session and one `EvalMode`. `compute_flow_with_session` analyzes a body with a
session; `compute_flow` and `compute_flow_with_mode` create a fresh one.
Summaries are keyed by the callee's definition: bodies are analyzed
parametrically in their own typing environment, so all instances of a generic
function share one summary.

A summary is the result of a flow analysis of the callee whose columns are
*origins* instead of locations: every leaf place of a parameter, including the
places behind the parameters' pointers, has its own origin. It records:

- the parameter places the callee may read (for forward slices of calls that
  write nothing, e.g. that return `()`),
- the places behind pointer parameters that the callee may write, accumulated
  at every write, including on paths that panic, with the origins of each
  written value,
- the parts of the return place at the normal exits, with their origins; the
  unit parts (e.g. of `Option<()>`) give the dependencies of the return value
  as a whole,
- the call operands passed to parameters of an opaque type (a type parameter,
  an alias or a trait object, possibly inside fields), whose writes keep the
  modular approximation because the callee cannot see the pointers in them.

Places are stored as body-independent paths (`EffectPath`) rooted at the return
place or at a parameter position, and each call site translates them into its
own places with `CallSite::translate`. Closure bodies receive their arguments as
a tuple (the "rust-call" ABI), which `CalleeAbi::ClosureBody` accounts for.
Translation is total: a path the caller cannot follow (a private field, a field
of a trait object, an opaque projection) degrades to the longest prefix the
caller can name. If the rest of the path went through a pointer, everything
mutably reachable from that prefix is possibly written.

The whole call destination is always written first; the effects on its parts
then refine it. Writes behind pointers are weak updates. A return effect that
had to be coarsened is weak as well, since several of them can land on the same
place.

## Borrows read addresses

In `Recurse` mode, a borrow `&p` reads only the pointers dereferenced to form the
address of `p` (and index locals), not what `p` holds. Code that reads through the
reference reaches `p` through its aliases. Without this, every write through
`&mut s` would inherit the dependencies of all of `s` as provenance, and the field
precision of summaries would be lost at the first reborrow. Consumers of a
reference that do not read through it, and whose aliases the analysis cannot
follow, read everything reachable from it instead: calls analyzed with the
modular approximation, operands of opaque parameters, destructors, and casts of
references to raw pointers or integers.

## Conservative boundaries

A call is analyzed with the modular approximation (as in `SigOnly`) when:

- it does not resolve statically to an item of the local crate: calls through
  function pointers or `dyn Trait`, compiler shims, intrinsics, calls that are
  too generic to resolve in the caller, and external functions (`FallbackReason`
  says which);
- the callee is instantiated with an `FnMut` or `FnOnce` closure type;
- the callee is in the same strongly connected component of the call graph as
  the caller (recursion). The component structure is computed once per session
  from the resolved calls, so results do not depend on the order in which
  bodies are analyzed, and there is no depth limit for acyclic call chains;
- the callee never returns;
- the callee body holds a raw pointer, accesses a union field, contains inline
  assembly, a transmute, a tail call or a suspension point, or is a coroutine:
  a summary could miss writes through any of them;
- the callee's parameters hold pointers nested more deeply than the alias
  analysis tracks (`MAX_ARG_POINTER_DEPTH`).

Statically resolved trait implementations, default trait methods that the call
really runs, and directly called local closures are summarized.

A `Drop` whose drop glue may run a destructor of the source code possibly writes
what is mutably reachable from the dropped value, e.g. a guard writing through
its `&mut` field. Drops of standard-library types (e.g. an `Rc` clone) have no
effect besides reading the value.

The analysis keeps Flowistry's alias-model assumptions; it is not a soundness
guarantee for arbitrary unsafe code. A test checks that `Recurse` sees every
write that `SigOnly` sees on a matrix of programs
(`crates/flowistry/tests/summaries.rs`).

## Instrumentation

`AnalysisSession::stats()` reports the number of summaries computed, cache hits,
fallbacks by reason, and the time spent computing summaries. With
`RUST_LOG=flowistry::infoflow=info` they are logged to stderr, never to the
protocol's stdout.

## Benchmark

`scripts/bench-summaries.py` compares two `cargo-flowistry` builds on a generated
diamond-shaped call graph, or on real code:

```sh
python3 scripts/bench-summaries.py \
  --baseline /path/to/baseline/target/release/cargo-flowistry \
  --candidate /path/to/fork/target/release/cargo-flowistry \
  --output target/summary-bench
```

The baseline must support `file-focus`. Add `--project WORKSPACE --file SOURCE
--position LINE COLUMN` for real code, from that project's build environment.
