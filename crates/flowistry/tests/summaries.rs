#![feature(rustc_private)]
extern crate rustc_middle;
extern crate rustc_span;

use flowistry::{
  extensions::{ContextMode, EVAL_MODE, EvalMode},
  infoflow::{self, AnalysisSession, Direction},
  test_utils,
};
use fluid_let::fluid_set;
use rustc_utils::{
  mir::borrowck_facts,
  source_map::{find_bodies::find_bodies, range::ToSpan, spanner::Spanner},
  test_utils::CompileBuilder,
};

fn check_slice(input: &str, direction: Direction, included: &[&str], excluded: &[&str]) {
  let input = input.to_owned();
  let (clean, _) = test_utils::parse_ranges(&input, [("`(", ")`")]).unwrap();
  test_utils::compile_body_with_range(
    clean,
    || test_utils::parse_ranges(&input, [("`(", ")`")]).unwrap().1["`("][0],
    |tcx, body_id, facts, target| {
      let mode = EvalMode {
        context_mode: ContextMode::Recurse,
        ..EvalMode::default()
      };
      fluid_set!(EVAL_MODE, mode);
      let results = infoflow::compute_flow(tcx, body_id, facts);
      let spanner = Spanner::new(tcx, body_id, &facts.body);
      let targets = spanner
        .span_to_places(target.to_span(tcx).unwrap())
        .iter()
        .map(|span| {
          span
            .locations
            .iter()
            .map(|loc| (span.place, *loc))
            .collect()
        })
        .collect();
      let spans = match direction {
        Direction::Both => {
          infoflow::compute_focus_spans(&results, targets, &spanner, &[])
        }
        _ => infoflow::compute_dependency_spans(&results, targets, direction, &spanner),
      };
      let snippets = spans
        .iter()
        .flatten()
        .map(|span| tcx.sess.source_map().span_to_snippet(*span).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
      for text in included {
        assert!(snippets.contains(text), "missing {text:?} in:\n{snippets}");
      }
      for text in excluded {
        assert!(
          !snippets.contains(text),
          "unexpected {text:?} in:\n{snippets}"
        );
      }
    },
  );
}

#[test]
fn untouched_receiver_field_backward() {
  check_slice(
    r#"
struct State { a: i32, b: i32 }
impl State { fn update_b(&mut self, x: i32) { self.b = x; } }
fn main() {
  let mut s = State { a: 1, b: 2 };
  let input = 73;
  s.update_b(input);
  `(s.a)`;
}"#,
    Direction::Both,
    &["a: 1"],
    &["s.update_b(input)", "input = 73"],
  );
}

#[test]
fn untouched_receiver_field_forward() {
  check_slice(
    r#"
struct State { a: i32, b: i32 }
impl State { fn update_b(&mut self, x: i32) { self.b = x; } }
fn main() {
  let mut s = State { a: 1, b: 2 };
  `(s.a)` = 37;
  s.update_b(73);
  let after = s.b;
}"#,
    Direction::Forward,
    &[],
    &["s.update_b(73)", "after = s.b"],
  );
}

#[test]
fn nested_calls_keep_real_reads_and_exclude_siblings() {
  check_slice(
    r#"
struct State { a: i32, b: i32, c: i32 }
impl State {
 fn leaf(&mut self) { self.c = self.b; }
 fn middle(&mut self) { self.leaf(); }
 fn outer(&mut self) { self.middle(); }
}
fn main() {
 let mut s = State { a: 0, b: 0, c: 0 };
 s.a = 11;
 s.b = 29;
 s.outer();
 `(s.c)`;
}"#,
    Direction::Backward,
    &["s.b = 29", "s.outer()"],
    &["s.a = 11"],
  );
}

#[test]
fn unit_return_still_reads_inputs() {
  check_slice(
    r#"
fn consume(x: &i32) { std::hint::black_box(*x); }
fn main() {
 let `(x)` = 23;
 consume(&x);
}"#,
    Direction::Forward,
    &["consume(&x)"],
    &[],
  );
}

#[test]
fn conditions_and_aliases_reach_written_fields() {
  check_slice(
    r#"
struct State { a: i32, b: i32 }
impl State { fn update(&mut self, flag: bool, x: i32) {
 let p = &mut self.b;
 if flag { *p = x; }
} }
fn main() {
 let mut s = State { a: 0, b: 0 };
 let flag = true;
 let value = 49;
 s.update(flag, value);
 `(s.b)`;
}"#,
    Direction::Backward,
    &["flag = true", "value = 49", "s.update(flag, value)"],
    &[],
  );
}

#[test]
fn separate_return_fields() {
  check_slice(
    r#"
struct State { a: i32, b: i32 }
impl State { fn pair(&self) -> (i32, i32) { (self.a, self.b) } }
fn main() {
 let mut s = State { a: 0, b: 0 };
 s.a = 17;
 s.b = 91;
 let pair = s.pair();
 `(pair.0)`;
}"#,
    Direction::Backward,
    &["s.a = 17"],
    &["s.b = 91"],
  );
}

#[test]
fn opaque_call_on_one_field_does_not_widen_receiver() {
  check_slice(
    r#"
struct State { a: i32, b: i32 }
impl State { fn opaque(&mut self, f: fn(&mut i32)) { f(&mut self.b); } }
fn main() {
 let mut s = State { a: 0, b: 0 };
 `(s.a)` = 17;
 s.opaque(|b| *b = 9);
}"#,
    Direction::Forward,
    &[],
    &["s.opaque"],
  );
}

#[test]
fn unresolved_dispatch_is_conservative() {
  check_slice(
    r#"
struct State { a: i32, b: i32 }
trait Update { fn update(&mut self, state: &mut State); }
fn main(f: &mut dyn Update) {
 let mut s = State { a: 0, b: 0 };
 `(s.a)` = 17;
 f.update(&mut s);
}"#,
    Direction::Forward,
    &["f.update(&mut s)"],
    &[],
  );
}

#[test]
fn statically_resolved_trait_implementation() {
  check_slice(
    r#"
struct State { a: i32, b: i32 }
trait Update { fn update(&mut self); }
impl Update for State { fn update(&mut self) { self.b = 5; } }
fn main() {
 let mut s = State { a: 0, b: 0 };
 s.update();
 `(s.a)`;
}"#,
    Direction::Backward,
    &[],
    &["s.update()"],
  );
}

#[test]
fn raw_pointer_body_falls_back() {
  check_slice(
    r#"
struct State { a: i32, b: i32 }
fn unknown(s: &mut State) { unsafe { *(&mut s.b as *mut i32) = 9; } }
fn main() {
 let mut s = State { a: 0, b: 0 };
 `(s.a)` = 17;
 unknown(&mut s);
}"#,
    Direction::Forward,
    &["unknown(&mut s)"],
    &[],
  );
}

#[test]
fn shared_diamond_and_batch_roots_compute_once() {
  borrowck_facts::enable_mir_simplification();
  CompileBuilder::new(
    r#"
fn leaf(x: i32) -> i32 { x + 1 }
fn left(x: i32) -> i32 { leaf(x) }
fn right(x: i32) -> i32 { leaf(x) }
fn root1(x: i32) -> i32 { left(x) + right(x) + left(x) }
fn root2(x: i32) -> i32 { right(x) + leaf(x) }
"#,
  )
  .compile(|result| {
    let tcx = result.tcx;
    let mode = EvalMode {
      context_mode: ContextMode::Recurse,
      ..EvalMode::default()
    };
    fluid_set!(EVAL_MODE, mode);
    let session = AnalysisSession::new(tcx);
    let roots = find_bodies(tcx)
      .into_iter()
      .filter(|(_, id)| {
        tcx
          .item_name(tcx.hir_body_owner_def_id(*id).to_def_id())
          .as_str()
          .starts_with("root")
      })
      .collect::<Vec<_>>();
    for _ in 0 .. 2 {
      for (_, id) in &roots {
        let facts = borrowck_facts::get_body_with_borrowck_facts(
          tcx,
          tcx.hir_body_owner_def_id(*id),
        );
        infoflow::compute_flow_with_session(session.clone(), tcx, *id, facts);
      }
    }
    assert_eq!(session.stats().computations, 3);
    assert!(session.stats().cache_hits >= 5);
  });
}

#[test]
fn gameplay_dependency_is_real_transitively() {
  check_slice(
    r#"
struct Runtime { interactive_visual_bounds: i32, unrelated: i32, aim: i32 }
impl Runtime {
 fn compute_interactive_aim_bounds(&mut self) { self.aim = self.interactive_visual_bounds; }
 fn configure_camera_and_aim_bounds(&mut self) { self.compute_interactive_aim_bounds(); }
 fn load_sublevel_runtime(&mut self) { self.configure_camera_and_aim_bounds(); }
}

fn main() {
 let mut r = Runtime { interactive_visual_bounds: 1, unrelated: 2, aim: 0 };
 `(r.interactive_visual_bounds)` = 0;
 r.load_sublevel_runtime();
}"#,
    Direction::Both,
    &["r.load_sublevel_runtime()"],
    &[],
  );
}

#[test]
fn generic_receiver_field_types_are_instantiated() {
  check_slice(
    r#"
struct Pair<T, U> { a: T, b: U }
impl<T: Copy, U> Pair<T, U> { fn first(&self) -> T { self.a } }
fn main() {
 let mut s = Pair { a: 0i32, b: 0u8 };
 s.a = 13;
 s.b = 97;
 let output = s.first();
 `(output)`;
}"#,
    Direction::Backward,
    &["s.a = 13"],
    &["s.b = 97"],
  );
}

#[test]
fn constant_write_is_not_mistaken_for_no_effect() {
  check_slice(
    r#"
struct State { a: i32, b: i32 }
impl State { fn constant(&mut self) { self.b = 73; } }
fn main() {
 let mut s = State { a: 0, b: 0 };
 s.constant();
 `(s.b)`;
}"#,
    Direction::Backward,
    &["s.constant()"],
    &[],
  );
}

#[test]
fn writes_before_panicking_path_are_retained() {
  check_slice(
    r#"
struct State { a: i32, b: i32 }
impl State { fn update(&mut self, flag: bool, x: i32) {
 if flag { self.b = x; panic!("stop"); }
} }
fn main() {
 let mut s = State { a: 0, b: 0 };
 let input = 73;
 s.update(false, input);
 `(s.b)`;
}"#,
    Direction::Backward,
    &["input = 73", "s.update(false, input)"],
    &[],
  );
}

#[test]
fn never_returning_callee_keeps_prior_writes() {
  check_slice(
    r#"
fn update(x: &mut i32, v: i32) -> ! { *x = v; panic!("stop"); }
fn main() {
 let mut x = 0;
 let `(input)` = 73;
 update(&mut x, input);
}"#,
    Direction::Forward,
    &["update(&mut x, input)"],
    &[],
  );
}

#[test]
fn loops_preserve_control_and_input_dependencies() {
  check_slice(
    r#"
struct State { a: i32, b: i32 }
impl State { fn repeat(&mut self, n: i32, x: i32) {
 let mut i = 0;
 while i < n { self.b += x; i += 1; }
} }
fn main() {
 let mut s = State { a: 0, b: 0 };
 let count = 3;
 let amount = 73;
 s.repeat(count, amount);
 `(s.b)`;
}"#,
    Direction::Backward,
    &["count = 3", "amount = 73"],
    &[],
  );
}

#[test]
fn recursive_components_are_independent_of_root_order() {
  borrowck_facts::enable_mir_simplification();
  CompileBuilder::new(
    r#"
struct State { a: i32, b: i32 }
fn a(s: &mut State, n: u32) { if n > 0 { b(s, n - 1); } s.a = s.b; }
fn b(s: &mut State, n: u32) { if n > 0 { a(s, n - 1); } s.b = s.a; }
fn root_a(s: &mut State) { a(s, 3); }
fn root_b(s: &mut State) { b(s, 3); }
"#,
  )
  .compile(|result| {
    let tcx = result.tcx;
    let mode = EvalMode {
      context_mode: ContextMode::Recurse,
      ..EvalMode::default()
    };
    fluid_set!(EVAL_MODE, mode);
    let mut roots = find_bodies(tcx)
      .into_iter()
      .filter(|(_, id)| {
        tcx
          .item_name(tcx.hir_body_owner_def_id(*id).to_def_id())
          .as_str()
          .starts_with("root")
      })
      .collect::<Vec<_>>();
    roots.sort_by_key(|(_, id)| {
      tcx.def_path_str(tcx.hir_body_owner_def_id(*id).to_def_id())
    });
    let analyze = |reverse: bool| {
      let session = AnalysisSession::new(tcx);
      let mut snapshots = Vec::new();
      let mut order = roots.clone();
      if reverse {
        order.reverse();
      }
      for (_, id) in order {
        let def = tcx.hir_body_owner_def_id(id);
        let facts = borrowck_facts::get_body_with_borrowck_facts(tcx, def);
        let results =
          infoflow::compute_flow_with_session(session.clone(), tcx, id, facts);
        let mut snapshot = Vec::new();
        results
          .analysis
          .visit_effects(|location, mutations, reads| {
            for mutation in mutations {
              let mut deps = results
                .analysis
                .deps_for(results.state_at(location), mutation.mutated)
                .iter()
                .map(|p| format!("{p:?}"))
                .collect::<Vec<_>>();
              deps.sort();
              snapshot.push(format!("{location:?} {:?} {deps:?}", mutation.mutated));
            }
            for read in reads {
              snapshot.push(format!("{location:?} read {read:?}"));
            }
          });
        snapshot.sort();
        snapshots.push((tcx.def_path_str(def.to_def_id()), snapshot));
      }
      assert_eq!(session.stats().computations, 2);
      assert!(session.stats().fallbacks["recursive edge"] >= 2);
      snapshots.sort();
      snapshots
    };
    assert_eq!(analyze(false), analyze(true));
  });
}

#[test]
fn fresh_compiler_observes_changed_callee() {
  let program = r#"
struct State { a: i32, b: i32 }
impl State { fn read(&self) -> i32 { self.FIELD } }
fn main() {
 let mut s = State { a: 0, b: 0 };
 s.a = 13;
 s.b = 97;
 let output = s.read();
 `(output)`;
}"#;
  check_slice(
    &program.replace("FIELD", "a"),
    Direction::Backward,
    &["s.a = 13"],
    &["s.b = 97"],
  );
  check_slice(
    &program.replace("FIELD", "b"),
    Direction::Backward,
    &["s.b = 97"],
    &["s.a = 13"],
  );
}
#[test]
fn opaque_return_fields_merge_instead_of_overwriting() {
  check_slice(
    r#"
mod private {
 pub struct Pair { a: i32, b: i32 }
 pub fn pair(a: i32, b: i32) -> Pair { Pair { a, b } }
}
fn main() {
 let first = 13;
 let second = 97;
 let out = private::pair(first, second);
 `(out)`;
}"#,
    Direction::Backward,
    &["first = 13", "second = 97"],
    &[],
  );
}

#[test]
fn field_effects_translate_through_nested_actual_projection() {
  check_slice(
    r#"
struct Inner { a: i32, b: i32 }
struct Outer { inner: Inner, other: i32 }
fn update(s: &mut Inner, x: i32) { s.b = x; }
fn main() {
 let mut s = Outer { inner: Inner { a: 0, b: 0 }, other: 0 };
 s.inner.a = 13;
 s.other = 97;
 let value = 73;
 update(&mut s.inner, value);
 `(s.inner.b)`;
}"#,
    Direction::Backward,
    &["value = 73"],
    &["s.inner.a = 13", "s.other = 97"],
  );
}

#[test]
fn generic_instances_have_separate_cache_entries() {
  borrowck_facts::enable_mir_simplification();
  CompileBuilder::new("fn identity<T>(x: T) -> T { x } fn main() { identity(1u8); identity(2u32); identity(3u8); }")
    .compile(|result| {
      let tcx = result.tcx;
      let mode = EvalMode { context_mode: ContextMode::Recurse, ..EvalMode::default() };
      fluid_set!(EVAL_MODE, mode);
      let session = AnalysisSession::new(tcx);
      let (_, id) = find_bodies(tcx).into_iter().find(|(_, id)| tcx.item_name(tcx.hir_body_owner_def_id(*id).to_def_id()).as_str() == "main").unwrap();
      let facts = borrowck_facts::get_body_with_borrowck_facts(tcx, tcx.hir_body_owner_def_id(id));
      // The session owns its mode, even if the calling API's ambient mode changes.
      fluid_set!(EVAL_MODE, EvalMode::default());
      infoflow::compute_flow_with_session(session.clone(), tcx, id, facts);
      assert_eq!(session.stats().computations, 2);
      assert!(session.stats().cache_hits >= 1);
    });
}

#[test]
fn opaque_borrowed_inputs_affect_return_values() {
  check_slice(
    r#"
struct State { items: Vec<i32>, other: i32 }
impl State { fn count(&self) -> usize { self.items.len() } }
fn main() {
 let mut s = State { items: Vec::new(), other: 0 };
 s.items.push(19);
 s.other = 97;
 let count = s.count();
 `(count)`;
}"#,
    Direction::Backward,
    &["s.items.push(19)", "s.count()"],
    &["s.other = 97"],
  );
}

#[test]
fn deep_acyclic_chain_has_no_depth_cutoff() {
  borrowck_facts::enable_mir_simplification();
  let mut source = String::from("fn f0(x: i32) -> i32 { x }\n");
  for n in 1 .. 40 {
    source.push_str(&format!("fn f{n}(x: i32) -> i32 {{ f{}(x) }}\n", n - 1));
  }
  source.push_str("fn main() { f39(1); }");
  CompileBuilder::new(source).compile(|result| {
    let tcx = result.tcx;
    let mode = EvalMode {
      context_mode: ContextMode::Recurse,
      ..EvalMode::default()
    };
    fluid_set!(EVAL_MODE, mode);
    let session = AnalysisSession::new(tcx);
    let (_, id) = find_bodies(tcx)
      .into_iter()
      .find(|(_, id)| {
        tcx
          .item_name(tcx.hir_body_owner_def_id(*id).to_def_id())
          .as_str()
          == "main"
      })
      .unwrap();
    let facts =
      borrowck_facts::get_body_with_borrowck_facts(tcx, tcx.hir_body_owner_def_id(id));
    infoflow::compute_flow_with_session(session.clone(), tcx, id, facts);
    assert_eq!(session.stats().computations, 40);
    assert!(session.stats().fallbacks.is_empty());
  });
}

#[test]
fn deeply_nested_borrowed_value_retains_its_contents() {
  check_slice(
    r#"
struct Inner<'a> { value: &'a i32 }
struct State<'a> { inner: Inner<'a>, other: i32 }
fn read(s: &State<'_>) -> i32 { *s.inner.value }
fn main() {
 let value = 49;
 let s = State { inner: Inner { value: &value }, other: 0 };
 let result = read(&s);
 `(result)`;
}"#,
    Direction::Backward,
    &["value = 49"],
    &[],
  );
}

#[test]
fn generic_opaque_values_keep_borrowed_contents() {
  check_slice(
    r#"
struct State<'a> { value: &'a i32 }
trait Read { fn read(&self) -> i32; }
impl Read for State<'_> { fn read(&self) -> i32 { *self.value } }
fn read<T: Read>(s: &T) -> i32 { s.read() }
fn main() {
 let value = 49;
 let s = State { value: &value };
 let result = read(&s);
 `(result)`;
}"#,
    Direction::Backward,
    &["value = 49"],
    &[],
  );
}

#[test]
fn unsupported_summary_is_cached() {
  borrowck_facts::enable_mir_simplification();
  CompileBuilder::new(
    r#"
fn opaque(x: &mut i32) { unsafe { *(x as *mut i32) = 17; } }
fn main() { let mut x = 0; opaque(&mut x); opaque(&mut x); }
"#,
  )
  .compile(|result| {
    let tcx = result.tcx;
    let mode = EvalMode {
      context_mode: ContextMode::Recurse,
      ..EvalMode::default()
    };
    fluid_set!(EVAL_MODE, mode);
    let session = AnalysisSession::new(tcx);
    let (_, id) = find_bodies(tcx)
      .into_iter()
      .find(|(_, id)| {
        tcx
          .item_name(tcx.hir_body_owner_def_id(*id).to_def_id())
          .as_str()
          == "main"
      })
      .unwrap();
    let facts =
      borrowck_facts::get_body_with_borrowck_facts(tcx, tcx.hir_body_owner_def_id(id));
    infoflow::compute_flow_with_session(session.clone(), tcx, id, facts);
    assert_eq!(session.stats().computations, 1);
    assert_eq!(session.stats().cache_hits, 1);
    assert_eq!(session.stats().fallbacks["unsupported MIR operation"], 1);
  });
}

#[test]
fn closure_upvar_writes_reach_captured_place() {
  check_slice(
    r#"
fn main() {
  let mut b = 0;
  let input = 73;
  let mut g = |y| b = y;
  g(input);
  `(b)`;
}"#,
    Direction::Backward,
    &["g(input)", "input = 73"],
    &[],
  );
}

#[test]
fn closure_capturing_receiver_field_in_method() {
  check_slice(
    r#"
struct State { a: i32, b: i32 }
impl State { fn poke(&mut self, x: i32) { let mut g = |y| self.b = y; g(x); } }
fn main() {
  let mut s = State { a: 0, b: 0 };
  let input = 73;
  s.poke(input);
  `(s.b)`;
}"#,
    Direction::Backward,
    &["s.poke(input)", "input = 73"],
    &[],
  );
}

#[test]
fn closure_capturing_receiver_calls_local_function() {
  check_slice(
    r#"
struct State { a: i32, b: i32 }
fn set(s: &mut State, x: i32) { s.b = x; }
impl State { fn poke(&mut self, x: i32) { let mut g = |y| set(self, y); g(x); } }
fn main() {
  let mut s = State { a: 0, b: 0 };
  let input = 73;
  s.poke(input);
  `(s.b)`;
}"#,
    Direction::Backward,
    &["s.poke(input)", "input = 73"],
    &[],
  );
}

#[test]
fn closure_upvar_write_keeps_sibling_fields_independent() {
  check_slice(
    r#"
struct State { a: i32, b: i32 }
impl State { fn poke(&mut self, x: i32) { let mut g = |y| self.b = y; g(x); } }
fn main() {
  let mut s = State { a: 1, b: 0 };
  let input = 73;
  s.poke(input);
  `(s.a)`;
}"#,
    Direction::Backward,
    &["a: 1"],
    &["s.poke(input)", "input = 73"],
  );
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
    check_slice(
      &format!(
        r#"
use std::{{cell::{{Cell, RefCell}}, sync::atomic::{{AtomicI32, Ordering::SeqCst}}}};
struct State {{ a: i32, c: {field} }}
impl State {{ fn poke(&mut self, x: i32) {{ {call}; }} }}
fn main() {{
  let mut s = State {{ a: 0, c: {init} }};
  let input = 73;
  s.poke(input);
  let v = `({read})`;
}}"#
      ),
      Direction::Backward,
      &["s.poke(input)", "input = 73"],
      &[],
    );
  }
}

#[test]
fn interior_mutation_leaves_frozen_siblings_independent() {
  check_slice(
    r#"
use std::cell::Cell;
struct State { a: i32, c: Cell<i32> }
impl State { fn poke(&mut self, x: i32) { self.c.set(x); } }
fn main() {
  let mut s = State { a: 1, c: Cell::new(0) };
  let input = 73;
  s.poke(input);
  `(s.a)`;
}"#,
    Direction::Backward,
    &["a: 1"],
    &["s.poke(input)", "input = 73"],
  );
}

#[test]
fn cell_set_in_same_body() {
  check_slice(
    r#"
use std::cell::Cell;
fn main() {
  let c = Cell::new(0);
  let input = 73;
  c.set(input);
  `(c.get())`;
}"#,
    Direction::Backward,
    &["c.set(input)", "input = 73"],
    &[],
  );
}

#[test]
fn raw_pointer_passed_to_opaque_call_falls_back() {
  for write in [
    "std::ptr::write(&raw mut self.b, x)",
    "std::ptr::write(&mut self.b as *mut i32, x)",
  ] {
    check_slice(
      &format!(
        r#"
struct State {{ a: i32, b: i32 }}
impl State {{ fn poke(&mut self, x: i32) {{ unsafe {{ {write}; }} }} }}
fn main() {{
  let mut s = State {{ a: 0, b: 0 }};
  let input = 73;
  s.poke(input);
  `(s.b)`;
}}"#
      ),
      Direction::Backward,
      &["s.poke(input)", "input = 73"],
      &[],
    );
  }
}
