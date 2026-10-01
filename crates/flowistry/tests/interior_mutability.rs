//! Writes to interior-mutable state (`Cell`, `RefCell`, atomics, ...) through
//! shared references, in both context modes.

#![feature(rustc_private)]
extern crate rustc_span;

mod common;

use common::{slice, slice_with_args};
use flowistry::{
  extensions::ContextMode, infoflow::Direction, test_utils::IncrementalDir,
};

const MODES: [ContextMode; 2] = [ContextMode::SigOnly, ContextMode::Recurse];

/// Checks the backward slice of `input` in both modes.
fn check(input: &str, included: &[&str], excluded: &[&str]) {
  for mode in MODES {
    let snippets = slice(input, mode, Direction::Backward);
    for text in included {
      assert!(
        snippets.contains(text),
        "{mode:?}: missing {text:?} in:\n{snippets}"
      );
    }
    for text in excluded {
      assert!(
        !snippets.contains(text),
        "{mode:?}: unexpected {text:?} in:\n{snippets}"
      );
    }
  }
}

#[test]
fn interior_mutability_through_shared_receiver_field() {
  for (field, init, call, read) in [
    ("Cell<i32>", "Cell::new(0)", "self.c.set(x)", "s.c.get()"),
    (
      "RefCell<i32>",
      "RefCell::new(0)",
      "self.c.replace(x)",
      "*s.c.borrow()",
    ),
    (
      "AtomicI32",
      "AtomicI32::new(0)",
      "self.c.store(x, SeqCst)",
      "s.c.load(SeqCst)",
    ),
  ] {
    for receiver in ["&mut self", "&self"] {
      check(
        &format!(
          r#"
use std::{{cell::{{Cell, RefCell}}, sync::atomic::{{AtomicI32, Ordering::SeqCst}}}};
struct State {{ a: i32, c: {field} }}
impl State {{ fn poke({receiver}, x: i32) {{ {call}; }} }}
fn main() {{
  let mut s = State {{ a: 0, c: {init} }};
  let input = 73;
  s.poke(input);
  let v = `({read})`;
}}"#
        ),
        &["s.poke(input)", "input = 73"],
        &[],
      );
    }
  }
}

#[test]
fn interior_mutation_leaves_frozen_siblings_independent() {
  check(
    r#"
use std::cell::Cell;
struct State { a: i32, c: Cell<i32> }
impl State { fn poke(&self, x: i32) { self.c.set(x); } }
fn main() {
  let s = State { a: 1, c: Cell::new(0) };
  let input = 73;
  s.poke(input);
  `(s.a)`;
}"#,
    &["a: 1"],
    &["s.poke(input)", "input = 73"],
  );
}

#[test]
fn cell_set_in_same_body() {
  check(
    r#"
use std::cell::Cell;
fn main() {
  let c = Cell::new(0);
  let input = 73;
  c.set(input);
  `(c.get())`;
}"#,
    &["c.set(input)", "input = 73"],
    &[],
  );
}

#[test]
fn interior_reads_are_not_writes() {
  for (ty, init, read) in [
    ("Cell<i32>", "Cell::new(0)", "c.get()"),
    ("Cell<i32>", "Cell::new(0)", "c.clone()"),
    ("RefCell<i32>", "RefCell::new(0)", "*c.borrow()"),
    ("AtomicI32", "AtomicI32::new(0)", "c.load(SeqCst)"),
    ("Mutex<i32>", "Mutex::new(0)", "*c.lock().unwrap()"),
    (
      "Rc<Cell<i32>>",
      "Rc::new(Cell::new(0))",
      "Rc::strong_count(&c)",
    ),
  ] {
    check(
      &format!(
        r#"
use std::{{cell::{{Cell, RefCell}}, rc::Rc, sync::{{Mutex, atomic::{{AtomicI32, Ordering::SeqCst}}}}}};
fn main() {{
  let c: {ty} = {init};
  let first = {read};
  let second = `({read})`;
}}"#
      ),
      &[init],
      &["first"],
    );
  }
}

#[test]
fn interior_write_through_shared_reference_keeps_written_value() {
  for callee in [
    "impl State { fn poke(&self, x: i32) { *self.c.borrow_mut() = x; } }",
    "impl State { fn poke(&self, x: i32) { set(&self.c, x); } }",
  ] {
    check(
      &format!(
        r#"
use std::cell::RefCell;
struct State {{ a: i32, c: RefCell<i32> }}
fn set(c: &RefCell<i32>, x: i32) {{ *c.borrow_mut() = x; }}
{callee}
fn main() {{
  let s = State {{ a: 0, c: RefCell::new(0) }};
  let input = 73;
  s.poke(input);
  let v = `(*s.c.borrow())`;
}}"#
      ),
      &["s.poke(input)", "input = 73"],
      &[],
    );
  }
}

// R8: whether a call leaves interior state unchanged is decided on the implementation
// that runs, not on the trait method it names.

#[test]
fn user_clone_mutating_a_cell_is_a_write() {
  check(
    r#"
use std::cell::Cell;
struct Counter { n: Cell<i32> }
impl Clone for Counter {
  fn clone(&self) -> Self { self.n.set(self.n.get() + 1); Counter { n: Cell::new(0) } }
}
fn main() {
  let c = Counter { n: Cell::new(0) };
  let d = c.clone();
  let v = `(c.n.get())`;
}"#,
    &["c.clone()"],
    &[],
  );
}

#[test]
fn std_clone_calling_user_clone_is_a_write() {
  // `Option<T>: Clone` is implemented in the standard library, but calls `T::clone`.
  check(
    r#"
use std::cell::Cell;
struct Counter { n: Cell<i32> }
impl Clone for Counter {
  fn clone(&self) -> Self { self.n.set(self.n.get() + 1); Counter { n: Cell::new(0) } }
}
fn main() {
  let c = Some(Counter { n: Cell::new(0) });
  let d = c.clone();
  let v = `(c.is_some())`;
}"#,
    &["c.clone()"],
    &[],
  );
}

#[test]
fn user_debug_mutating_a_cell_is_a_write() {
  check(
    r#"
use std::{cell::Cell, fmt};
struct Counter { n: Cell<i32> }
impl fmt::Debug for Counter {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.n.set(1); Ok(()) }
}
fn main() {
  let c = Counter { n: Cell::new(0) };
  let s = format!("{c:?}");
  let v = `(c.n.get())`;
}"#,
    &["format!(\"{c:?}\")"],
    &[],
  );
}

/// Incremental compilation (as in the IDE) hashes query keys, which must not contain
/// region variables: the `Freeze` checks run on region-erased types.
#[test]
fn freeze_checks_under_incremental_compilation() {
  let incremental = IncrementalDir::new();
  let input = r#"
use std::{cell::{Cell, RefCell}, rc::Rc};
struct Random<'a> { seed: &'a Cell<u64> }
struct App<'a> { rng: Rc<RefCell<Random<'a>>>, hits: &'a Cell<u32> }
impl<'a> App<'a> {
  fn reseed(&self, x: u64) { self.rng.borrow_mut().seed.set(x); self.hits.set(1); }
  fn peek(&self) -> u64 { self.rng.borrow().seed.get() }
}
fn main() {
  let seed = Cell::new(0);
  let hits = Cell::new(0);
  let app = App { rng: Rc::new(RefCell::new(Random { seed: &seed })), hits: &hits };
  app.reseed(5);
  let `(v)` = app.peek();
}
"#;
  for mode in MODES {
    let with = slice_with_args(input, mode, Direction::Backward, &incremental.args());
    let without = slice(input, mode, Direction::Backward);
    assert_eq!(with, without, "{mode:?}");
  }
}
