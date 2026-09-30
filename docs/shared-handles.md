# Possible writes through shared handles

Flowistry connects pointers through lifetimes. A `RefCell` guard is tied to the
handle it was borrowed from, so `*a.borrow_mut() = x` is a write to `a`. Two
`Rc<RefCell<T>>` values share no lifetime, even when one is a clone of the other,
so a write through `a` never reaches a later read through `b`. This is the
interior-mutability limitation described in the README.

Whether two handles point to the same object cannot be decided in general. For
example, `App` and `Config` may each receive an `Rc<RefCell<Random>>` from
elsewhere. Instead of claiming either answer, focus responses carry a separate
`maybe_slice`: code that is relevant only if such handles alias. Editors can show
it in a different color than the exact `slice`.

## Analysis

A *shared handle* is a place of type `Rc<T>`, `Arc<T>`, one of their `Weak`s, or
`&T`, where `T` is not `Freeze` (it contains a `Cell`, `RefCell`, `Mutex`, atomic,
or another `UnsafeCell`). The *state* of a `&T` handle is its pointee; an `Rc` or
`Arc` handle stands for its state, which has no place of its own. Handles in a
body, including fields of locals and state reachable from arguments, are grouped
by the region-erased type of their state.

When no group has two members, nothing more is computed
(`compute_flow_with_shared_handles` returns `None`). Otherwise a second flow
analysis runs in which a write to the state of one handle also possibly writes
the state of every other handle in its group. `maybe_slice` is the second
analysis's slice minus the exact one: a maybe span that contains or overlaps an
exact span keeps only its uncovered parts, so `maybe_slice` and `slice` are
disjoint. The exact analysis and its `slice` are unchanged by the second
analysis.

A write reaches the state of a handle when it is inside that state, or when it
may write something containing the handle without a pointer in between, such as a
`&mut self` method on `App` holding `App::rng`. The kind of the write decides
whether it goes *through* the handle: writing a new value into the handle itself
(`let b = a.clone()`, `b = make()`, the destination of a call returning a
handle, in either context mode) rebinds it, and is not a write to the state
behind it. Rewriting a reference is not a write to its pointee either. Known
read-only calls such as `borrow`, `get`, `clone` of an `Rc`, and atomic `load`
are not writes (see [callee summaries](callee-summaries.md)), and dropping a
handle is not a write through it.

The exact analysis also treats a write reaching an `Rc`/`Arc` to interior-mutable
state (`Cell::set` through an `Rc`) as a write to that handle, in both modes. In
`Recurse` mode, a callee writing through a handle it received by value (e.g.
`fn set(a: Rc<RefCell<i32>>, ..)`) writes the caller's operand, like a callee
writing through a pointer argument.

Handle groups are per body. Handles are grouped only by type, so two genuinely
separate `Rc<RefCell<i32>>` values in one function are also reported as possibly
related. Different state types, and handles to `Freeze` data, are never related.
Aliasing between a body's handles and handles held elsewhere is not represented,
except through the callee effects already present in the exact analysis.

## Protocol

`place_info[].maybe_slice` is a list of ranges, disjoint from `slice`. It is
omitted when empty, so responses for code without such handle pairs are
unchanged, and editors that do not know the field ignore it.

## Cost

The second analysis runs only for bodies with at least two handles to the same
interior-mutable type, and reuses the session's callee summaries in `Recurse`
mode. For those bodies, flow analysis and slicing roughly double.

## Validation

`crates/flowistry/tests/shared_handles.rs` checks both context modes: `Rc` and
`Arc` clones, the README's `Arc<Mutex>` example, handles in different structs
written by a method, `Cell` writes through another handle, separately passed
`&RefCell` arguments, a handle passed by value and written by the callee, and
writes through the same handle staying exact. It also checks that different
state types, `Freeze` states, reads through another handle, binding new handles
(also from a local function), and dropping handles add nothing. The IDE tests
check that `maybe_slice` is serialized only when present and is disjoint from
`slice`.
