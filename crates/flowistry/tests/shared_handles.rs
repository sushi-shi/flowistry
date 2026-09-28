#![feature(rustc_private)]
extern crate rustc_middle;
extern crate rustc_span;

use flowistry::{
  extensions::{ContextMode, EVAL_MODE, EvalMode},
  infoflow::{self, AnalysisSession, Direction},
  test_utils,
};
use fluid_let::fluid_set;
use rustc_utils::source_map::{range::ToSpan, spanner::Spanner};

/// Snippets in the exact slice, and those only the shared-handle pass adds.
fn slices(input: &str, mode: ContextMode, direction: Direction) -> (String, String) {
  let input = input.to_owned();
  let (clean, _) = test_utils::parse_ranges(&input, [("`(", ")`")]).unwrap();
  let mut out = Default::default();
  test_utils::compile_body_with_range(
    clean,
    || test_utils::parse_ranges(&input, [("`(", ")`")]).unwrap().1["`("][0],
    |tcx, body_id, facts, target| {
      let mode = EvalMode {
        context_mode: mode,
        ..EvalMode::default()
      };
      fluid_set!(EVAL_MODE, mode);
      let session = AnalysisSession::new(tcx);
      let spanner = Spanner::new(tcx, body_id, &facts.body);
      let targets = spanner
        .span_to_places(target.to_span(tcx).unwrap())
        .iter()
        .map(|s| {
          s.locations
            .iter()
            .map(|l| (s.place, *l))
            .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
      let results =
        infoflow::compute_flow_with_session(session.clone(), tcx, body_id, facts);
      let exact = infoflow::compute_dependency_spans(
        &results,
        targets.clone(),
        direction,
        &spanner,
      )
      .into_iter()
      .flatten()
      .collect::<Vec<_>>();
      let maybe =
        match infoflow::compute_flow_with_shared_handles(session, tcx, body_id, facts) {
          Some(results) => {
            infoflow::compute_dependency_spans(&results, targets, direction, &spanner)
              .into_iter()
              .flatten()
              .collect::<Vec<_>>()
          }
          None => Vec::new(),
        };
      let text = |spans: Vec<_>| {
        spans
          .into_iter()
          .map(|span| tcx.sess.source_map().span_to_snippet(span).unwrap())
          .collect::<Vec<_>>()
          .join("\n")
      };
      let only_maybe = maybe
        .iter()
        .filter(|span| !exact.iter().any(|e| e.contains(**span)))
        .copied()
        .collect();
      out = (text(exact), text(only_maybe));
    },
  );
  out
}

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
