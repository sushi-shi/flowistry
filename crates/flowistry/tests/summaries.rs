//! Callee summaries in `Recurse` mode.

#![feature(rustc_private)]
extern crate rustc_hir;
extern crate rustc_middle;

mod common;

use std::rc::Rc;

use common::{check_recurse, sees, slice, slice_with_args};
use flowistry::{
  extensions::{ContextMode, EvalMode},
  infoflow::{self, AnalysisSession, Direction, FallbackReason, UnsupportedOp},
  test_utils::{self, IncrementalDir},
};
use rustc_hir::BodyId;
use rustc_middle::ty::TyCtxt;
use rustc_utils::{mir::borrowck_facts, source_map::find_bodies::find_bodies};

fn recurse() -> EvalMode {
  EvalMode {
    context_mode: ContextMode::Recurse,
    ..EvalMode::default()
  }
}

/// The bodies whose name satisfies `filter`, sorted by path.
fn bodies(tcx: TyCtxt<'_>, filter: impl Fn(&str) -> bool) -> Vec<BodyId> {
  let mut bodies = find_bodies(tcx)
    .into_iter()
    .map(|(_, id)| id)
    .filter(|id| {
      let def_id = tcx.hir_body_owner_def_id(*id).to_def_id();
      tcx
        .opt_item_name(def_id)
        .is_some_and(|name| filter(name.as_str()))
    })
    .collect::<Vec<_>>();
  bodies.sort_by_key(|id| tcx.def_path_str(tcx.hir_body_owner_def_id(*id).to_def_id()));
  bodies
}

fn analyze<'tcx>(
  tcx: TyCtxt<'tcx>,
  session: &Rc<AnalysisSession<'tcx>>,
  id: BodyId,
) -> infoflow::FlowResults<'tcx, 'tcx> {
  let facts =
    borrowck_facts::get_body_with_borrowck_facts(tcx, tcx.hir_body_owner_def_id(id));
  infoflow::compute_flow_with_session(session, id, facts)
}

// Field sensitivity.

#[test]
fn untouched_receiver_field_backward() {
  check_recurse(
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
fn conditions_and_aliases_reach_written_fields() {
  check_recurse(
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
fn separate_return_fields_by_value() {
  check_recurse(
    r#"
struct State { a: i32, b: i32 }
fn pair(s: State) -> (i32, i32) { (s.a, s.b) }
fn main() {
 let mut s = State { a: 0, b: 0 };
 s.a = 17;
 s.b = 91;
 let pair = pair(s);
 `(pair.0)`;
}"#,
    Direction::Backward,
    &["s.a = 17"],
    &["s.b = 91"],
  );
}

#[test]
fn unresolved_dispatch_is_conservative() {
  check_recurse(
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
  check_recurse(
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
  check_recurse(
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
fn raw_pointer_passed_to_opaque_call_falls_back() {
  for write in [
    "std::ptr::write(&raw mut self.b, x)",
    "std::ptr::write(&mut self.b as *mut i32, x)",
  ] {
    check_recurse(
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

#[test]
fn gameplay_dependency_is_real_transitively() {
  check_recurse(
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
  check_recurse(
    r#"
struct Pair<T, U> { a: T, b: U }
impl<T: Copy, U> Pair<T, U> { fn first(self) -> T { self.a } }
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
  check_recurse(
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
  check_recurse(
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
  check_recurse(
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
  check_recurse(
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
fn fresh_compiler_observes_changed_callee() {
  let program = r#"
struct State { a: i32, b: i32 }
fn read(s: State) -> i32 { s.FIELD }
fn main() {
 let mut s = State { a: 0, b: 0 };
 s.a = 13;
 s.b = 97;
 let output = read(s);
 `(output)`;
}"#;
  check_recurse(
    &program.replace("FIELD", "a"),
    Direction::Backward,
    &["s.a = 13"],
    &["s.b = 97"],
  );
  check_recurse(
    &program.replace("FIELD", "b"),
    Direction::Backward,
    &["s.b = 97"],
    &["s.a = 13"],
  );
}

#[test]
fn opaque_return_fields_merge_instead_of_overwriting() {
  check_recurse(
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
fn opaque_borrowed_inputs_affect_return_values() {
  check_recurse(
    r#"
struct State { items: Vec<i32>, other: i32 }
impl State { fn count(&self) -> usize { self.items.len() } }
fn main() {
 let mut s = State { items: Vec::new(), other: 0 };
 s.items.push(19);
 let count = s.count();
 `(count)`;
}"#,
    Direction::Backward,
    &["s.items.push(19)", "s.count()"],
    &[],
  );
}

#[test]
fn deeply_nested_borrowed_value_retains_its_contents() {
  check_recurse(
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
  check_recurse(
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
fn user_destructor_writes_through_its_guard() {
  check_recurse(
    r#"
struct Guard<'a>(&'a mut i32, i32);
impl Drop for Guard<'_> { fn drop(&mut self) { *self.0 = self.1; } }
fn scoped(x: &mut i32, v: i32) { let _g = Guard(x, v); }
fn main() {
 let mut x = 0;
 let input = 73;
 scoped(&mut x, input);
 `(x)`;
}"#,
    Direction::Backward,
    &["scoped(&mut x, input)", "input = 73"],
    &[],
  );
}

// A borrow reads only its address: its contents are reached through aliases.

#[test]
fn separate_return_fields() {
  check_recurse(
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
fn untouched_receiver_field_forward() {
  check_recurse(
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
fn field_effects_translate_through_nested_actual_projection() {
  check_recurse(
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
fn opaque_call_on_one_field_does_not_widen_receiver() {
  check_recurse(
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
fn nested_calls_keep_real_reads_and_exclude_siblings() {
  check_recurse(
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
  check_recurse(
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
fn reference_cast_to_raw_pointer_keeps_contents() {
  check_recurse(
    r#"
fn main() {
 let mut x = 0;
 x = 41;
 let r = &x;
 let p = r as *const i32;
 let v = unsafe { *p };
 `(v)`;
}"#,
    Direction::Backward,
    &["x = 41"],
    &[],
  );
}

#[test]
fn reference_passed_to_opaque_call_keeps_contents() {
  check_recurse(
    r#"
fn main() {
 let mut v = vec![1];
 v.push(41);
 let r = &v;
 let n = r.len();
 `(n)`;
}"#,
    Direction::Backward,
    &["v.push(41)"],
    &[],
  );
}

#[test]
fn transmuted_reference_keeps_contents() {
  check_recurse(
    r#"
fn main() {
 let mut x = 0u32;
 x = 41;
 let r = &x;
 let n: usize = unsafe { std::mem::transmute(r) };
 `(n)`;
}"#,
    Direction::Backward,
    &["x = 41"],
    &[],
  );
}

// Closures, called directly: their bodies take their arguments as a tuple (R1).

#[test]
fn closure_arguments_are_untupled() {
  // R1: the old mapping of callee `_k` to operand `k - 1` sent `b` to no operand
  // and `a` to the tuple.
  check_recurse(
    r#"
fn main() {
 let g = |a: i32, b: i32| b;
 let x = 1;
 let yy = 2;
 let r = g(x, yy);
 `(r)`;
}"#,
    Direction::Backward,
    &["yy = 2", "g(x, yy)"],
    &["x = 1"],
  );
}

#[test]
fn closure_writing_through_argument() {
  // R1: the old mapping dereferenced the argument tuple, an ICE.
  check_recurse(
    r#"
fn main() {
 let mut n = 0;
 let g = |v: &mut i32| *v = 1;
 g(&mut n);
 `(n)`;
}"#,
    Direction::Backward,
    &["g(&mut n)"],
    &[],
  );
}

#[test]
fn closure_body_is_summarized() {
  test_utils::compile_crate(
    r#"
fn main() {
 let mut n = 0;
 let g = |v: &mut i32, w: i32| *v = w;
 g(&mut n, 1);
 let mut k = 0;
 let mut h = |w: i32| k = w;
 h(2);
 let s = String::new();
 let once = move |w: i32| { drop(s); w };
 once(3);
}
"#,
    &[],
    |tcx| {
      let session = AnalysisSession::new(tcx, recurse());
      for id in bodies(tcx, |name| name == "main") {
        analyze(tcx, &session, id);
      }
      let stats = session.stats();
      // The three closure bodies (Fn, FnMut and FnOnce). Only `String::new` and
      // `drop` are not summarized.
      assert_eq!(stats.computations, 3, "{stats:?}");
      assert_eq!(stats.fallbacks.into_iter().collect::<Vec<_>>(), vec![(
        FallbackReason::NotLocal,
        2
      )]);
    },
  );
}

#[test]
fn closure_upvar_writes_reach_captured_place() {
  check_recurse(
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
  check_recurse(
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
  check_recurse(
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
  check_recurse(
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

// The return value as a whole (R3).

#[test]
fn unit_parts_of_return_value_keep_dependencies() {
  // R3: `Some(())` has no non-unit part, so the old summary exported no return
  // effect at all.
  check_recurse(
    r#"
fn check(x: i32) -> Option<()> { if x > 0 { Some(()) } else { None } }
fn main() {
 let xx = 5;
 let r = check(xx);
 `(r)`;
}"#,
    Direction::Backward,
    &["xx = 5", "check(xx)"],
    &[],
  );
}

// Writes through parameters whose type the callee cannot see through (R4).

#[test]
fn writes_through_dyn_closure_parameter() {
  // R4: the callee sees `f: &mut dyn FnMut()`, not the `&mut count` it captures.
  check_recurse(
    r#"
fn run(f: &mut dyn FnMut()) { f() }
fn main() {
 let mut count = 0;
 run(&mut || count += 1);
 `(count)`;
}"#,
    Direction::Backward,
    &["run(&mut || count += 1)"],
    &[],
  );
}

#[test]
fn writes_through_generic_parameter() {
  // R4: the callee sees `x: &mut T`, not the `&mut i32` that `T` is.
  check_recurse(
    r#"
trait Bump { fn bump(&mut self); }
impl Bump for &mut i32 { fn bump(&mut self) { **self += 1; } }
fn call<T: Bump>(x: &mut T) { x.bump() }
fn main() {
 let mut n = 0;
 {
   let mut r = &mut n;
   call(&mut r);
 }
 `(n)`;
}"#,
    Direction::Backward,
    &["call(&mut r)"],
    &[],
  );
}

// Sessions.

#[test]
fn shared_diamond_and_batch_roots_compute_once() {
  test_utils::compile_crate(
    r#"
fn leaf(x: i32) -> i32 { x + 1 }
fn left(x: i32) -> i32 { leaf(x) }
fn right(x: i32) -> i32 { leaf(x) }
fn root1(x: i32) -> i32 { left(x) + right(x) + left(x) }
fn root2(x: i32) -> i32 { right(x) + leaf(x) }
"#,
    &[],
    |tcx| {
      let session = AnalysisSession::new(tcx, recurse());
      let roots = bodies(tcx, |name| name.starts_with("root"));
      for _ in 0 .. 2 {
        for id in &roots {
          analyze(tcx, &session, *id);
        }
      }
      let stats = session.stats();
      assert_eq!(stats.computations, 3, "{stats:?}");
      assert!(stats.cache_hits >= 5, "{stats:?}");
    },
  );
}

#[test]
fn recursive_components_are_independent_of_root_order() {
  test_utils::compile_crate(
    r#"
struct State { a: i32, b: i32 }
fn a(s: &mut State, n: u32) { if n > 0 { b(s, n - 1); } s.a = s.b; }
fn b(s: &mut State, n: u32) { if n > 0 { a(s, n - 1); } s.b = s.a; }
fn root_a(s: &mut State) { a(s, 3); }
fn root_b(s: &mut State) { b(s, 3); }
"#,
    &[],
    |tcx| {
      let roots = bodies(tcx, |name| name.starts_with("root"));
      let analyze_all = |reverse: bool| {
        let session = AnalysisSession::new(tcx, recurse());
        let mut order = roots.clone();
        if reverse {
          order.reverse();
        }
        let mut snapshots = order
          .into_iter()
          .map(|id| {
            let results = analyze(tcx, &session, id);
            let body = results.analysis.body;
            let mut snapshot = rustc_utils::BodyExt::all_locations(body)
              .flat_map(|location| {
                results
                  .state_at(location)
                  .rows()
                  .map(|(row, deps)| {
                    let mut deps = deps
                      .iter()
                      .map(|dep| format!("{dep:?}"))
                      .collect::<Vec<_>>();
                    deps.sort();
                    format!("{location:?} {row:?} {deps:?}")
                  })
                  .collect::<Vec<_>>()
              })
              .collect::<Vec<_>>();
            snapshot.sort();
            (
              tcx.def_path_str(tcx.hir_body_owner_def_id(id).to_def_id()),
              snapshot,
            )
          })
          .collect::<Vec<_>>();
        let stats = session.stats();
        // `a` and `b` call each other: neither is summarized from the other.
        assert_eq!(stats.computations, 2, "{stats:?}");
        assert!(
          stats.fallbacks[&FallbackReason::RecursiveCall] >= 2,
          "{stats:?}"
        );
        snapshots.sort();
        snapshots
      };
      assert_eq!(analyze_all(false), analyze_all(true));
    },
  );
}

#[test]
fn generic_instances_share_one_summary() {
  // Bodies are summarized parametrically, so all instances share a summary.
  test_utils::compile_crate(
    "fn identity<T>(x: T) -> T { x } fn main() { identity(1u8); identity(2u32); identity(3u8); }",
    &[],
    |tcx| {
      let session = AnalysisSession::new(tcx, recurse());
      for id in bodies(tcx, |name| name == "main") {
        analyze(tcx, &session, id);
      }
      let stats = session.stats();
      assert_eq!(stats.computations, 1, "{stats:?}");
      assert_eq!(stats.cache_hits, 2, "{stats:?}");
    },
  );
}

#[test]
fn deep_acyclic_chain_has_no_depth_cutoff() {
  let mut source = String::from("fn f0(x: i32) -> i32 { x }\n");
  for n in 1 .. 40 {
    source.push_str(&format!("fn f{n}(x: i32) -> i32 {{ f{}(x) }}\n", n - 1));
  }
  source.push_str("fn main() { f39(1); }");
  test_utils::compile_crate(source, &[], |tcx| {
    let session = AnalysisSession::new(tcx, recurse());
    for id in bodies(tcx, |name| name == "main") {
      analyze(tcx, &session, id);
    }
    let stats = session.stats();
    assert_eq!(stats.computations, 40, "{stats:?}");
    assert!(stats.fallbacks.is_empty(), "{stats:?}");
  });
}

#[test]
fn unsupported_summary_is_cached() {
  test_utils::compile_crate(
    r#"
fn opaque(x: &mut i32) { unsafe { *(x as *mut i32) = 17; } }
fn main() { let mut x = 0; opaque(&mut x); opaque(&mut x); }
"#,
    &[],
    |tcx| {
      let session = AnalysisSession::new(tcx, recurse());
      for id in bodies(tcx, |name| name == "main") {
        analyze(tcx, &session, id);
      }
      let stats = session.stats();
      assert_eq!(stats.computations, 1, "{stats:?}");
      assert_eq!(stats.cache_hits, 1, "{stats:?}");
      assert_eq!(
        stats.fallbacks[&FallbackReason::UnsupportedBody(UnsupportedOp::RawPointer)],
        2,
        "{stats:?}"
      );
    },
  );
}

/// Incremental compilation (as in the IDE) hashes query keys, which must not contain
/// region variables of borrow-checked MIR.
#[test]
fn summaries_under_incremental_compilation() {
  let incremental = IncrementalDir::new();
  let input = r#"
use std::{cell::{Cell, RefCell}, rc::Rc};
struct Random { seed: u64 }
struct Guard<'a>(&'a mut u64);
impl Drop for Guard<'_> { fn drop(&mut self) { *self.0 += 1; } }
struct App<'a> { rng: Rc<RefCell<Random>>, hits: Cell<u32>, seen: &'a mut u64 }
impl App<'_> {
  fn reseed(&mut self, x: u64) { self.rng.borrow_mut().seed = x; self.hits.set(1); let _g = Guard(self.seen); }
  fn peek(&self) -> u64 { self.rng.borrow().seed }
}
fn main() {
  let mut seen = 0;
  let mut app = App { rng: Rc::new(RefCell::new(Random { seed: 0 })), hits: Cell::new(0), seen: &mut seen };
  let f = |x: u64| x + 1;
  app.reseed(f(5));
  let `(v)` = app.peek();
}
"#;
  for mode in [ContextMode::SigOnly, ContextMode::Recurse] {
    let with = slice_with_args(input, mode, Direction::Backward, &incremental.args());
    let without = slice(input, mode, Direction::Backward);
    assert_eq!(with, without, "{mode:?}");
  }
}

// Recurse may only be more precise than SigOnly: it must see every write SigOnly
// sees.

/// Programs where `WRITE` is written, directly or not, into the place the target
/// reads.
const WRITES: &[(&str, &str)] = &[
  (
    "atomic store via &mut self",
    r#"use std::sync::atomic::{AtomicI32, Ordering};
struct S { a: i32, n: AtomicI32 }
impl S { fn poke(&mut self, x: i32) { self.n.store(x, Ordering::SeqCst); } }
fn main() { let mut s = S { a: 0, n: AtomicI32::new(0) }; let WRITE = 5; s.poke(WRITE);
  let v = `(s.n.load(Ordering::SeqCst))`; }"#,
  ),
  (
    "Mutex guard via &mut self",
    r#"use std::sync::Mutex;
struct S { a: i32, m: Mutex<i32> }
impl S { fn poke(&mut self, x: i32) { *self.m.lock().unwrap() = x; } }
fn main() { let mut s = S { a: 0, m: Mutex::new(0) }; let WRITE = 5; s.poke(WRITE);
  let v = `(*s.m.lock().unwrap())`; }"#,
  ),
  (
    "callee returns &mut field, caller writes",
    r#"struct S { a: i32, b: i32 }
impl S { fn b_mut(&mut self) -> &mut i32 { &mut self.b } }
fn main() { let mut s = S { a: 0, b: 0 }; let WRITE = 5; *s.b_mut() = WRITE;
  let v = `(s.b)`; }"#,
  ),
  (
    "write through &mut stored in struct arg",
    r#"struct H<'a> { r: &'a mut i32 }
fn f(h: &mut H, x: i32) { *h.r = x; }
fn main() { let mut t = 0; let WRITE = 5; { let mut h = H { r: &mut t }; f(&mut h, WRITE); }
  let v = `(t)`; }"#,
  ),
  (
    "Vec push on field",
    r#"struct S { a: i32, v: Vec<i32> }
impl S { fn poke(&mut self, x: i32) { self.v.push(x); } }
fn main() { let mut s = S { a: 0, v: vec![] }; let WRITE = 5; s.poke(WRITE);
  let v = `(s.v.len())`; }"#,
  ),
  (
    "FnMut closure capturing field",
    r#"struct S { a: i32, b: i32 }
impl S { fn poke(&mut self, x: i32) { let mut g = |y| self.b = y; g(x); } }
fn main() { let mut s = S { a: 0, b: 0 }; let WRITE = 5; s.poke(WRITE);
  let v = `(s.b)`; }"#,
  ),
  (
    "mem::replace on field",
    r#"struct S { a: i32, b: i32 }
impl S { fn poke(&mut self, x: i32) { std::mem::replace(&mut self.b, x); } }
fn main() { let mut s = S { a: 0, b: 0 }; let WRITE = 5; s.poke(WRITE);
  let v = `(s.b)`; }"#,
  ),
  (
    "Option<&mut> by value",
    r#"fn f(o: Option<&mut i32>, x: i32) { if let Some(r) = o { *r = x; } }
fn main() { let mut t = 0; let WRITE = 5; f(Some(&mut t), WRITE);
  let v = `(t)`; }"#,
  ),
  (
    "nested Cell field",
    r#"use std::cell::Cell;
struct I { c: Cell<i32> } struct S { a: i32, i: I }
impl S { fn poke(&mut self, x: i32) { self.i.c.set(x); } }
fn main() { let mut s = S { a: 0, i: I { c: Cell::new(0) } }; let WRITE = 5; s.poke(WRITE);
  let v = `(s.i.c.get())`; }"#,
  ),
  (
    "ptr::write via &raw mut",
    r#"struct S { a: i32, b: i32 }
impl S { fn poke(&mut self, x: i32) { unsafe { std::ptr::write(&raw mut self.b, x); } } }
fn main() { let mut s = S { a: 0, b: 0 }; let WRITE = 5; s.poke(WRITE);
  let v = `(s.b)`; }"#,
  ),
  (
    "ptr::write via cast",
    r#"struct S { a: i32, b: i32 }
impl S { fn poke(&mut self, x: i32) { unsafe { std::ptr::write(&mut self.b as *mut i32, x); } } }
fn main() { let mut s = S { a: 0, b: 0 }; let WRITE = 5; s.poke(WRITE);
  let v = `(s.b)`; }"#,
  ),
  (
    "slice write",
    r#"fn f(s: &mut [i32], x: i32) { s[0] = x; }
fn main() { let mut t = [0; 3]; let WRITE = 5; f(&mut t, WRITE);
  let v = `(t[0])`; }"#,
  ),
  (
    "RefCell::replace via &mut self",
    r#"use std::cell::RefCell;
struct S { a: i32, c: RefCell<i32> }
impl S { fn poke(&mut self, x: i32) { self.c.replace(x); } }
fn main() { let mut s = S { a: 0, c: RefCell::new(0) }; let WRITE = 5; s.poke(WRITE);
  let v = `(*s.c.borrow())`; }"#,
  ),
  (
    "generic &mut T callee calls trait method",
    r#"trait T { fn set(&mut self, x: i32); }
struct S { b: i32 } impl T for S { fn set(&mut self, x: i32) { self.b = x; } }
fn f<X: T>(t: &mut X, x: i32) { t.set(x); }
fn main() { let mut s = S { b: 0 }; let WRITE = 5; f(&mut s, WRITE);
  let v = `(s.b)`; }"#,
  ),
  (
    "swap two fields, read a",
    r#"struct S { a: i32, b: i32 }
impl S { fn poke(&mut self, x: i32) { self.b = x; std::mem::swap(&mut self.a, &mut self.b); } }
fn main() { let mut s = S { a: 0, b: 0 }; let WRITE = 5; s.poke(WRITE);
  let v = `(s.a)`; }"#,
  ),
  (
    "same body FnMut closure",
    r#"fn main() { let mut b = 0; let WRITE = 5; let mut g = |y| b = y; g(WRITE);
  let v = `(b)`; }"#,
  ),
  (
    "same body ptr::write",
    r#"fn main() { let mut b = 0; let WRITE = 5; unsafe { std::ptr::write(&raw mut b, WRITE); }
  let v = `(b)`; }"#,
  ),
  (
    "closure calls local fn via &mut self",
    r#"struct S { a: i32, b: i32 }
fn set(s: &mut S, x: i32) { s.b = x; }
impl S { fn poke(&mut self, x: i32) { let mut g = |y| set(self, y); g(x); } }
fn main() { let mut s = S { a: 0, b: 0 }; let WRITE = 5; s.poke(WRITE);
  let v = `(s.b)`; }"#,
  ),
  (
    "&mut self method writes via own RefCell field",
    r#"use std::cell::RefCell;
struct S { a: i32, c: RefCell<i32> }
impl S { fn poke(&mut self, x: i32) { *self.c.borrow_mut() = x; } }
fn main() { let mut s = S { a: 0, c: RefCell::new(0) }; let WRITE = 5; s.poke(WRITE);
  let v = `(*s.c.borrow())`; }"#,
  ),
  (
    "&self method writes via own RefCell field",
    r#"use std::cell::RefCell;
struct S { a: i32, c: RefCell<i32> }
impl S { fn poke(&self, x: i32) { *self.c.borrow_mut() = x; } }
fn main() { let s = S { a: 0, c: RefCell::new(0) }; let WRITE = 5; s.poke(WRITE);
  let v = `(*s.c.borrow())`; }"#,
  ),
  (
    "same body, direct borrow_mut",
    r#"use std::cell::RefCell;
fn main() { let c = RefCell::new(0); let WRITE = 5; *c.borrow_mut() = WRITE;
  let v = `(*c.borrow())`; }"#,
  ),
  (
    "two shared refs to one RefCell",
    r#"use std::cell::RefCell;
fn main() { let c = RefCell::new(0); let a = &c; let b = &c; let WRITE = 5;
  *a.borrow_mut() = WRITE; let v = `(*b.borrow())`; }"#,
  ),
  (
    "Rc<RefCell> clone",
    r#"use std::{cell::RefCell, rc::Rc};
fn main() { let a = Rc::new(RefCell::new(0)); let b = a.clone(); let WRITE = 5;
  *a.borrow_mut() = WRITE; let v = `(*b.borrow())`; }"#,
  ),
  (
    "&mut self method, Cell::set on field",
    r#"use std::cell::Cell;
struct S { a: i32, c: Cell<i32> }
impl S { fn poke(&mut self, x: i32) { self.c.set(x); } }
fn main() { let mut s = S { a: 0, c: Cell::new(0) }; let WRITE = 5; s.poke(WRITE);
  let v = `(s.c.get())`; }"#,
  ),
  (
    "&mut self helper passes &RefCell to local fn",
    r#"use std::cell::RefCell;
struct S { a: i32, c: RefCell<i32> }
fn set(c: &RefCell<i32>, x: i32) { *c.borrow_mut() = x; }
impl S { fn poke(&mut self, x: i32) { set(&self.c, x); } }
fn main() { let mut s = S { a: 0, c: RefCell::new(0) }; let WRITE = 5; s.poke(WRITE);
  let v = `(*s.c.borrow())`; }"#,
  ),
  (
    "free fn(&RefCell) writes",
    r#"use std::cell::RefCell;
fn set(c: &RefCell<i32>, x: i32) { *c.borrow_mut() = x; }
fn main() { let c = RefCell::new(0); let WRITE = 5; set(&c, WRITE);
  let v = `(*c.borrow())`; }"#,
  ),
  (
    "&mut self method: RefMut held in local then written",
    r#"use std::cell::RefCell;
struct S { a: i32, c: RefCell<Vec<i32>> }
impl S { fn poke(&mut self, x: i32) { let mut g = self.c.borrow_mut(); g.push(x); } }
fn main() { let mut s = S { a: 0, c: RefCell::new(vec![]) }; let WRITE = 5; s.poke(WRITE);
  let v = `(s.c.borrow().len())`; }"#,
  ),
  (
    "same body Cell::set",
    r#"use std::cell::Cell;
fn main() { let c = Cell::new(0); let WRITE = 5; c.set(WRITE); let v = `(c.get())`; }"#,
  ),
  (
    "Arc<Mutex> clone",
    r#"use std::sync::{Arc, Mutex};
fn main() { let x = Arc::new(Mutex::new(0)); let y = x.clone(); let WRITE = 1;
  *x.lock().unwrap() = WRITE; let v = `(*y.lock().unwrap())`; }"#,
  ),
  (
    "App and Config both hold Rc<RefCell<Random>>",
    r#"use std::{cell::RefCell, rc::Rc};
struct Random { seed: u64 }
struct App { rng: Rc<RefCell<Random>> }
struct Config { rng: Rc<RefCell<Random>> }
impl App { fn reseed(&mut self, x: u64) { self.rng.borrow_mut().seed = x; } }
fn main() { let r = Rc::new(RefCell::new(Random { seed: 0 })); let mut app = App { rng: r.clone() };
  let config = Config { rng: r }; let WRITE = 5; app.reseed(WRITE);
  let v = `(config.rng.borrow().seed)`; }"#,
  ),
  (
    "user destructor writes through its guard",
    r#"struct G<'a>(&'a mut i32, i32);
impl Drop for G<'_> { fn drop(&mut self) { *self.0 = self.1; } }
fn f(x: &mut i32, v: i32) { let _g = G(x, v); }
fn main() { let mut t = 0; let WRITE = 5; f(&mut t, WRITE); let v = `(t)`; }"#,
  ),
];

/// Writes that SigOnly sees and Recurse does not, yet: interior mutability behind a
/// shared reference is not modeled.
const KNOWN_RECURSE_MISSES: &[&str] = &[
  "nested Cell field",
  "RefCell::replace via &mut self",
  "&mut self method, Cell::set on field",
  "&mut self helper passes &RefCell to local fn",
];

#[test]
fn recurse_sees_every_write_sig_only_sees() {
  let mut misses = Vec::new();
  for (name, input) in WRITES {
    let sig_only = sees(input, ContextMode::SigOnly, "WRITE");
    let recurse = sees(input, ContextMode::Recurse, "WRITE");
    if sig_only && !recurse {
      misses.push(*name);
    }
  }
  assert_eq!(misses, KNOWN_RECURSE_MISSES);
}
