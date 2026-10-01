//! Possible writes through separately held shared handles to interior-mutable state
//! (`compute_flow_with_shared_handles`), in both context modes.

#![feature(rustc_private)]
extern crate rustc_span;

mod common;

use common::slices;
use flowistry::{extensions::ContextMode, infoflow::Direction};

fn check(input: &str, exact: &[&str], only_maybe: &[&str], absent: &[&str]) {
  for mode in [ContextMode::SigOnly, ContextMode::Recurse] {
    let (e, m) = slices(input, mode, Direction::Backward);
    let ctx = format!("{mode:?}\nexact:\n{e}\nonly maybe:\n{m}");
    for text in exact {
      assert!(e.contains(text), "{text:?} not exact in {ctx}");
    }
    for text in only_maybe {
      assert!(!e.contains(text), "{text:?} unexpectedly exact in {ctx}");
      assert!(m.contains(text), "{text:?} not a maybe in {ctx}");
    }
    for text in absent {
      assert!(
        !e.contains(text) && !m.contains(text),
        "unexpected {text:?} in {ctx}"
      );
    }
  }
}

#[test]
fn rc_refcell_clones_may_alias() {
  check(
    r#"
use std::{cell::RefCell, rc::Rc};
fn main() {
  let a = Rc::new(RefCell::new(0));
  let b = a.clone();
  let input = 73;
  *a.borrow_mut() = input;
  let v = `(*b.borrow())`;
}"#,
    &["let b = a.clone()"],
    &["*a.borrow_mut() = input", "let input = 73"],
    &[],
  );
}

#[test]
fn arc_mutex_readme_example() {
  check(
    r#"
use std::sync::{Arc, Mutex};
fn main() {
  let x = Arc::new(Mutex::new(0));
  let y = x.clone();
  let input = 1;
  *x.lock().unwrap() = input;
  let v = `(*y.lock().unwrap())`;
}"#,
    &[],
    &["*x.lock().unwrap() = input", "let input = 1"],
    &[],
  );
}

#[test]
fn handles_in_different_structs() {
  check(
    r#"
use std::{cell::RefCell, rc::Rc};
struct Random { seed: u64 }
struct App { rng: Rc<RefCell<Random>> }
struct Config { rng: Rc<RefCell<Random>> }
impl App { fn reseed(&mut self, x: u64) { self.rng.borrow_mut().seed = x; } }
fn main() {
  let r = Rc::new(RefCell::new(Random { seed: 0 }));
  let mut app = App { rng: r.clone() };
  let config = Config { rng: r };
  let input = 5;
  app.reseed(input);
  let v = `(config.rng.borrow().seed)`;
}"#,
    &[],
    &["app.reseed(input)", "let input = 5"],
    &[],
  );
}

#[test]
fn rc_cell_set_through_other_handle() {
  check(
    r#"
use std::{cell::Cell, rc::Rc};
fn main() {
  let a = Rc::new(Cell::new(0));
  let b = Rc::clone(&a);
  let input = 73;
  a.set(input);
  let v = `(b.get())`;
}"#,
    &[],
    &["a.set(input)", "let input = 73"],
    &[],
  );
}

#[test]
fn rc_cell_set_on_same_handle_is_exact() {
  check(
    r#"
use std::{cell::Cell, rc::Rc};
fn main() {
  let c = Rc::new(Cell::new(0));
  let input = 73;
  c.set(input);
  let v = `(c.get())`;
}"#,
    &["c.set(input)", "let input = 73"],
    &[],
    &[],
  );
}

#[test]
fn shared_references_passed_separately() {
  check(
    r#"
use std::cell::RefCell;
fn f(a: &RefCell<i32>, b: &RefCell<i32>, input: i32) -> i32 {
  *a.borrow_mut() = input;
  let v = `(*b.borrow())`;
  v
}
fn main() {}"#,
    &[],
    &["*a.borrow_mut() = input"],
    &[],
  );
}

#[test]
fn different_pointee_types_stay_independent() {
  check(
    r#"
use std::{cell::RefCell, rc::Rc};
fn main() {
  let a = Rc::new(RefCell::new(0i32));
  let b = Rc::new(RefCell::new(0u32));
  let input = 73;
  *a.borrow_mut() = input;
  let v = `(*b.borrow())`;
}"#,
    &[],
    &[],
    &["*a.borrow_mut() = input", "let input = 73"],
  );
}

#[test]
fn frozen_pointees_are_not_shared_state() {
  check(
    r#"
use std::rc::Rc;
fn main() {
  let a = Rc::new(1);
  let b = Rc::new(2);
  let v = `(*b)`;
  let w = *a;
}"#,
    &["Rc::new(2)"],
    &[],
    &["Rc::new(1)"],
  );
}

#[test]
fn reads_through_other_handles_are_not_writes() {
  check(
    r#"
use std::{cell::RefCell, rc::Rc};
fn main() {
  let a = Rc::new(RefCell::new(0));
  let b = a.clone();
  let first = *a.borrow();
  let v = `(*b.borrow())`;
}"#,
    &[],
    &[],
    &["first"],
  );
}

#[test]
fn binding_a_new_handle_is_not_a_write() {
  check(
    r#"
use std::{cell::RefCell, rc::Rc};
fn main() {
  let a = Rc::new(RefCell::new(0));
  let other = Rc::new(RefCell::new(1));
  let b = other.clone();
  let v = `(*a.borrow())`;
}"#,
    &["Rc::new(RefCell::new(0))"],
    &[],
    &["other.clone()", "Rc::new(RefCell::new(1))"],
  );
}

#[test]
fn freeze_checks_accept_types_with_lifetimes() {
  check(
    r#"
use std::{cell::RefCell, rc::Rc};
struct Random { seed: u64 }
struct App { rng: Rc<RefCell<Random>> }
struct Config { rng: Rc<RefCell<Random>> }
impl App { fn reseed(&mut self, x: u64) { self.rng.borrow_mut().seed = x; } }
fn main() {
  let r = Rc::new(RefCell::new(Random { seed: 0 }));
  let mut app = App { rng: r.clone() };
  let config = Config { rng: r };
  let input = 5;
  app.reseed(input);
  let seen = `(config.rng.borrow().seed)`;
  println!("{seen}");
}"#,
    &[],
    &["app.reseed(input)", "let input = 5"],
    &[],
  );
}

// R2: a handle passed by value and written through in the callee.

#[test]
fn handle_written_by_value_in_callee() {
  check(
    r#"
use std::{cell::RefCell, rc::Rc};
fn set(a: Rc<RefCell<i32>>, v: i32) { *a.borrow_mut() = v; }
fn main() {
  let a = Rc::new(RefCell::new(0));
  let input = 73;
  set(a.clone(), input);
  let seen = `(*a.borrow())`;
}"#,
    &[],
    &["set(a.clone(), input)", "let input = 73"],
    &[],
  );
}

// R5b: binding a handle returned by a local function is not a write through it.

#[test]
fn handle_returned_by_local_function_is_a_binding() {
  check(
    r#"
use std::{cell::RefCell, rc::Rc};
fn make() -> Rc<RefCell<i32>> { Rc::new(RefCell::new(0)) }
fn main() {
  let s = String::new();
  let a = Rc::new(RefCell::new(1));
  let b = make();
  let seen = `(*a.borrow())`;
  drop(s);
  drop(b);
}"#,
    &["Rc::new(RefCell::new(1))"],
    &[],
    &["make()"],
  );
}

// R7: dropping a handle is not a write through it.

#[test]
fn dropping_a_handle_is_not_a_write() {
  check(
    r#"
use std::{cell::RefCell, rc::Rc};
fn main() {
  let shared = Rc::new(RefCell::new(0));
  for i in 0 .. 2 {
    let h = shared.clone();
    let n = Rc::strong_count(&h);
  }
  let seen = `(*shared.borrow())`;
}"#,
    &["Rc::new(RefCell::new(0))"],
    &[],
    &["shared.clone()", "strong_count"],
  );
}
