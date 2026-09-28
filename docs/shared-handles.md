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

A *shared handle* is a place of type `Rc<T>`, `Arc<T>`, their `Weak`s, or `&T`,
where `T` is not `Freeze` (it contains a `Cell`, `RefCell`, `Mutex`, atomic, or
another `UnsafeCell`). Handles in a body, including fields of locals and state
reachable from arguments, are grouped by their region-erased pointee type.

When no group has two members, nothing more is computed. Otherwise a second flow
analysis runs in which a write to one handle's state also possibly writes every
other handle in its group. `maybe_slice` is the second analysis's slice minus the
exact one. The exact analysis and its `slice` are unchanged.

A write reaches a handle's state when it is inside that state (for `&T`, behind
the reference), or when it may write something owning the handle directly, such
as a `&mut self` method on `App` owning `App::rng`. An `Rc`/`Arc` pointee has no
place of its own, so the handle stands for it, as it already does for guard
writes. Binding a handle (`let b = a.clone()`) or rewriting a reference is not a
write to the state behind it. Known read-only calls such as `borrow`, `get`, and
atomic `load` are not writes; see [callee summaries](callee-summaries.md).

Handle groups are per body. Handles are grouped only by type, so two genuinely
separate `Rc<RefCell<i32>>` values in one function are also reported as possibly
related. Different pointee types, and handles to `Freeze` data, are never
related. Aliasing between a body's handles and handles held elsewhere is not
represented, except through the callee effects already present in the exact
analysis.

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
`&RefCell` arguments, and writes through the same handle staying exact. It also
checks that different pointee types, `Freeze` pointees, reads through another
handle, and binding new handles add nothing. The IDE test checks that
`maybe_slice` is serialized only when present.
