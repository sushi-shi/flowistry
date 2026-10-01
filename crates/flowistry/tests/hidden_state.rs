//! Dependencies carried by memory that no place of the caller names: globals,
//! interior mutability, shared handles and raw pointers. Each test records the
//! tier that reports the dependency today, in both context modes.

#![feature(rustc_private)]
extern crate rustc_span;

mod common;

use common::slices;
use flowistry::{extensions::ContextMode, infoflow::Direction};

/// Where the dependency of the target on `deps` is reported.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Tier {
  /// In the exact slice.
  Exact,
  /// Only in the pessimistic shared-handle slice.
  Maybe,
  /// Nowhere: a known unsoundness.
  Missed,
}

fn check(tier: Tier, input: &str, direction: Direction, kept: &[&str], deps: &[&str]) {
  check_modes([tier, tier], input, direction, kept, deps);
}

/// [`check`] with the expected tier of `SigOnly` and of `Recurse` respectively.
fn check_modes(
  tiers: [Tier; 2],
  input: &str,
  direction: Direction,
  kept: &[&str],
  deps: &[&str],
) {
  for (mode, tier) in [ContextMode::SigOnly, ContextMode::Recurse]
    .into_iter()
    .zip(tiers)
  {
    let (exact, maybe) = slices(input, mode, direction);
    let ctx = format!("{mode:?}\nexact:\n{exact}\nonly maybe:\n{maybe}");
    for text in kept {
      assert!(exact.contains(text), "{text:?} not exact in {ctx}");
    }
    for text in deps {
      let found = if exact.contains(text) {
        Tier::Exact
      } else if maybe.contains(text) {
        Tier::Maybe
      } else {
        Tier::Missed
      };
      assert_eq!(found, tier, "{text:?} in {ctx}");
    }
  }
}

// Global state reached without any argument, in one body or across calls: no
// place of the caller names it, so neither tier reports it.

// SigOnly mode does not scan callee bodies for statics.
#[test]
fn static_mut_written_and_read_by_callees() {
  check_modes(
    [Tier::Missed, Tier::Exact],
    r#"
static mut G: i32 = 0;
fn stash(x: i32) { unsafe { G = x; } }
fn fetch() -> i32 { unsafe { G } }
fn main() {
  let input = 73;
  stash(input);
  let `(y)` = fetch();
}"#,
    Direction::Backward,
    &["fetch()"],
    &["stash(input)", "input = 73"],
  );
}

// SigOnly mode does not scan callee bodies for statics.
#[test]
fn safe_atomic_static_written_and_read_by_callees() {
  check_modes(
    [Tier::Missed, Tier::Exact],
    r#"
use std::sync::atomic::{AtomicI32, Ordering};
static G: AtomicI32 = AtomicI32::new(0);
fn stash(x: i32) { G.store(x, Ordering::SeqCst); }
fn fetch() -> i32 { G.load(Ordering::SeqCst) }
fn main() {
  let input = 73;
  stash(input);
  let `(y)` = fetch();
}"#,
    Direction::Backward,
    &["fetch()"],
    &["stash(input)", "input = 73"],
  );
}

#[test]
fn static_atomic_in_same_function() {
  check(
    Tier::Exact,
    r#"
use std::sync::atomic::{AtomicI32, Ordering};
static G: AtomicI32 = AtomicI32::new(0);
fn main() {
  let input = 73;
  G.store(input, Ordering::SeqCst);
  let `(y)` = G.load(Ordering::SeqCst);
}"#,
    Direction::Backward,
    &["G.load"],
    &["G.store", "input = 73"],
  );
}

// SigOnly mode does not scan callee bodies for statics.
#[test]
fn static_mutex_written_and_read_by_callees() {
  check_modes(
    [Tier::Missed, Tier::Exact],
    r#"
use std::sync::Mutex;
static M: Mutex<i32> = Mutex::new(0);
fn stash(x: i32) { *M.lock().unwrap() = x; }
fn fetch() -> i32 { *M.lock().unwrap() }
fn main() {
  let input = 73;
  stash(input);
  let `(y)` = fetch();
}"#,
    Direction::Backward,
    &["fetch()"],
    &["stash(input)", "input = 73"],
  );
}

#[test]
fn static_rwlock_in_same_function() {
  check(
    Tier::Maybe,
    r#"
use std::sync::RwLock;
static M: RwLock<i32> = RwLock::new(0);
fn main() {
  let input = 73;
  *M.write().unwrap() = input;
  let `(y)` = *M.read().unwrap();
}"#,
    Direction::Backward,
    &["M.read()"],
    &["input = 73"],
  );
}

// SigOnly mode does not scan callee bodies for statics.
#[test]
fn static_oncelock_set_and_get_by_callees() {
  check_modes(
    [Tier::Missed, Tier::Exact],
    r#"
use std::sync::OnceLock;
static O: OnceLock<i32> = OnceLock::new();
fn stash(x: i32) { let _ = O.set(x); }
fn fetch() -> i32 { *O.get().unwrap() }
fn main() {
  let input = 73;
  stash(input);
  let `(y)` = fetch();
}"#,
    Direction::Backward,
    &["fetch()"],
    &["stash(input)", "input = 73"],
  );
}

#[test]
fn static_lazylock_mutex_in_same_function() {
  check(
    Tier::Maybe,
    r#"
use std::sync::{LazyLock, Mutex};
static L: LazyLock<Mutex<i32>> = LazyLock::new(|| Mutex::new(0));
fn main() {
  let input = 73;
  *L.lock().unwrap() = input;
  let `(y)` = *L.lock().unwrap();
}"#,
    Direction::Backward,
    &["L.lock()"],
    &["input = 73"],
  );
}

// SigOnly mode does not scan callee bodies for statics.
#[test]
fn mutex_guard_from_static_returned_by_callee() {
  check_modes(
    [Tier::Missed, Tier::Maybe],
    r#"
use std::sync::{Mutex, MutexGuard};
static M: Mutex<i32> = Mutex::new(0);
fn guard() -> MutexGuard<'static, i32> { M.lock().unwrap() }
fn main() {
  let input = 73;
  *guard() = input;
  let `(y)` = *guard();
}"#,
    Direction::Backward,
    &["guard()"],
    &["input = 73"],
  );
}

// SigOnly mode does not scan callee bodies for statics.
#[test]
fn thread_local_cell() {
  check_modes(
    [Tier::Missed, Tier::Exact],
    r#"
use std::cell::Cell;
thread_local! { static T: Cell<i32> = Cell::new(0); }
fn stash(x: i32) { T.with(|t| t.set(x)); }
fn main() {
  let input = 73;
  stash(input);
  let `(y)` = T.with(|t| t.get());
}"#,
    Direction::Backward,
    &["T.with"],
    &["stash(input)", "input = 73"],
  );
}

#[test]
fn thread_local_refcell_in_same_function() {
  check(
    Tier::Exact,
    r#"
use std::cell::RefCell;
thread_local! { static T: RefCell<i32> = RefCell::new(0); }
fn main() {
  let input = 73;
  T.with_borrow_mut(|t| *t = input);
  let `(y)` = T.with_borrow(|t| *t);
}"#,
    Direction::Backward,
    &["T.with_borrow"],
    &["with_borrow_mut", "input = 73"],
  );
}

#[test]
fn extern_c_hidden_state() {
  check(
    Tier::Maybe,
    r#"
unsafe extern "C" { fn ext_store(x: i32); fn ext_load() -> i32; }
fn main() {
  let input = 73;
  unsafe { ext_store(input); }
  let `(y)` = unsafe { ext_load() };
}"#,
    Direction::Backward,
    &["ext_load()"],
    &["ext_store(input)", "input = 73"],
  );
}

#[test]
fn file_round_trip() {
  check(
    Tier::Maybe,
    r#"
fn main() {
  let input = 73;
  std::fs::write("/tmp/flowistry-known-miss", input.to_string()).unwrap();
  let `(y)` = std::fs::read_to_string("/tmp/flowistry-known-miss").unwrap();
}"#,
    Direction::Backward,
    &["read_to_string"],
    &["fs::write", "input = 73"],
  );
}

#[test]
fn env_var_round_trip() {
  check(
    Tier::Maybe,
    r#"
fn main() {
  let input = 73;
  unsafe { std::env::set_var("FLOWISTRY_KNOWN_MISS", input.to_string()); }
  let `(y)` = std::env::var("FLOWISTRY_KNOWN_MISS").unwrap();
}"#,
    Direction::Backward,
    &["env::var"],
    &["set_var", "input = 73"],
  );
}

// Interior mutability behind a shared reference is a write to the cell.

#[test]
fn cell_written_through_shared_reference() {
  check(
    Tier::Exact,
    r#"
use std::cell::Cell;
fn stash(c: &Cell<i32>, x: i32) { c.set(x); }
fn main() {
  let c = Cell::new(0);
  let input = 73;
  stash(&c, input);
  let `(y)` = c.get();
}"#,
    Direction::Backward,
    &["c.get()"],
    &["stash(&c, input)", "input = 73"],
  );
}

#[test]
fn cell_set_in_same_function() {
  check(
    Tier::Exact,
    r#"
use std::cell::Cell;
fn main() {
  let c = Cell::new(0);
  let input = 73;
  c.set(input);
  let `(y)` = c.get();
}"#,
    Direction::Backward,
    &["c.get()"],
    &["c.set(input)", "input = 73"],
  );
}

#[test]
fn custom_unsafe_cell_through_shared_receiver() {
  check(
    Tier::Exact,
    r#"
use std::cell::UnsafeCell;
struct MyCell(UnsafeCell<i32>);
impl MyCell {
  fn put(&self, v: i32) { unsafe { *self.0.get() = v; } }
  fn take(&self) -> i32 { unsafe { *self.0.get() } }
}
fn main() {
  let c = MyCell(UnsafeCell::new(0));
  let input = 73;
  c.put(input);
  let `(y)` = c.take();
}"#,
    Direction::Backward,
    &["c.take()"],
    &["c.put(input)", "input = 73"],
  );
}

// Distinct values sharing hidden state: lifetimes relate neither Rc clones nor
// the two ends of a channel. The shared-handle pass reports a write through one
// clone in the same body as possible for the others.

#[test]
fn rc_refcell_write_through_clone() {
  check(
    Tier::Maybe,
    r#"
use std::{cell::RefCell, rc::Rc};
fn main() {
  let a = Rc::new(RefCell::new(0));
  let b = Rc::clone(&a);
  let input = 73;
  *b.borrow_mut() = input;
  let `(y)` = *a.borrow();
}"#,
    Direction::Backward,
    &["a.borrow()"],
    &["input = 73"],
  );
}

#[test]
fn rc_refcell_clone_written_by_callee() {
  check(
    Tier::Maybe,
    r#"
use std::{cell::RefCell, rc::Rc};
fn stash(c: &Rc<RefCell<i32>>, x: i32) { *c.borrow_mut() = x; }
fn main() {
  let a = Rc::new(RefCell::new(0));
  let b = Rc::clone(&a);
  let input = 73;
  stash(&b, input);
  let `(y)` = *a.borrow();
}"#,
    Direction::Backward,
    &["a.borrow()"],
    &["stash(&b, input)", "input = 73"],
  );
}

#[test]
fn rc_refcell_clone_forward() {
  check(
    Tier::Maybe,
    r#"
use std::{cell::RefCell, rc::Rc};
fn main() {
  let a = Rc::new(RefCell::new(0));
  let b = Rc::clone(&a);
  let `(input)` = 73;
  *b.borrow_mut() = input;
  let y = *a.borrow();
}"#,
    Direction::Forward,
    &["input"],
    &["a.borrow()"],
  );
}

#[test]
fn arc_atomic_clone_same_function() {
  check(
    Tier::Maybe,
    r#"
use std::sync::{Arc, atomic::{AtomicI32, Ordering}};
fn main() {
  let a = Arc::new(AtomicI32::new(0));
  let b = Arc::clone(&a);
  let input = 73;
  b.store(input, Ordering::SeqCst);
  let `(y)` = a.load(Ordering::SeqCst);
}"#,
    Direction::Backward,
    &["a.load"],
    &["b.store", "input = 73"],
  );
}

#[test]
fn channel_send_then_recv() {
  check(
    Tier::Maybe,
    r#"
use std::sync::mpsc::channel;
fn main() {
  let (tx, rx) = channel();
  let input = 73;
  tx.send(input).unwrap();
  let `(y)` = rx.recv().unwrap();
}"#,
    Direction::Backward,
    &["rx.recv()"],
    &["tx.send(input)", "input = 73"],
  );
}

#[test]
fn channel_send_then_recv_forward() {
  check(
    Tier::Maybe,
    r#"
use std::sync::mpsc::channel;
fn main() {
  let (tx, rx) = channel();
  let `(input)` = 73;
  tx.send(input).unwrap();
  let y = rx.recv().unwrap();
}"#,
    Direction::Forward,
    &["tx.send(input)"],
    &["rx.recv()"],
  );
}

// Raw-pointer escape: a dereferenced raw pointer aliases only other raw
// dereferences, never the place it was derived from.

#[test]
fn raw_pointer_stashed_in_struct_field() {
  check(
    Tier::Maybe,
    r#"
struct Holder { p: *mut i32 }
fn main() {
  let mut x = 0;
  let h = Holder { p: &raw mut x };
  let input = 73;
  unsafe { *h.p = input; }
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["input = 73"],
  );
}

#[test]
fn raw_pointer_field_written_by_callee_through_shared_ref() {
  check(
    Tier::Maybe,
    r#"
struct Holder { p: *mut i32 }
fn poke(h: &Holder, v: i32) { unsafe { *h.p = v; } }
fn main() {
  let mut x = 0;
  let h = Holder { p: &raw mut x };
  let input = 73;
  poke(&h, input);
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["poke(&h, input)", "input = 73"],
  );
}

#[test]
fn raw_pointer_returned_by_callee_then_written() {
  check(
    Tier::Maybe,
    r#"
fn escape(x: &mut i32) -> *mut i32 { x as *mut i32 }
fn main() {
  let mut x = 0;
  let p = escape(&mut x);
  let input = 73;
  unsafe { *p = input; }
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["input = 73"],
  );
}

#[test]
fn raw_pointer_written_by_callee() {
  check(
    Tier::Maybe,
    r#"
fn put(p: *mut i32, v: i32) { unsafe { *p = v; } }
fn main() {
  let mut x = 0;
  let input = 73;
  put(&raw mut x, input);
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["put(", "input = 73"],
  );
}

#[test]
fn ptr_write_in_same_function() {
  check(
    Tier::Maybe,
    r#"
fn main() {
  let mut x = 0;
  let p = &raw mut x;
  let input = 73;
  unsafe { std::ptr::write(p, input); }
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["ptr::write", "input = 73"],
  );
}

#[test]
fn ptr_write_in_same_function_forward() {
  check(
    Tier::Maybe,
    r#"
fn main() {
  let mut x = 0;
  let p = &raw mut x;
  let `(input)` = 73;
  unsafe { std::ptr::write(p, input); }
  let y = x + 1;
}"#,
    Direction::Forward,
    &["ptr::write(p, input)"],
    &["x + 1"],
  );
}

#[test]
fn ptr_read_after_direct_write() {
  check(
    Tier::Maybe,
    r#"
fn main() {
  let mut x = 0;
  let p = &raw const x;
  let input = 73;
  x = input;
  let `(y)` = unsafe { std::ptr::read(p) };
}"#,
    Direction::Backward,
    &["ptr::read"],
    &["x = input", "input = 73"],
  );
}

#[test]
fn pointer_round_tripped_through_integer() {
  check(
    Tier::Maybe,
    r#"
fn main() {
  let mut x = 0;
  let addr = &raw mut x as usize;
  let input = 73;
  unsafe { *(addr as *mut i32) = input; }
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["input = 73"],
  );
}

#[test]
fn transmuted_reference_written() {
  check(
    Tier::Maybe,
    r#"
fn main() {
  let mut x = 0;
  let p: *mut i32 = unsafe { std::mem::transmute(&mut x) };
  let input = 73;
  unsafe { *p = input; }
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["input = 73"],
  );
}

#[test]
fn slice_from_raw_parts_mut() {
  check(
    Tier::Maybe,
    r#"
fn main() {
  let mut x = 0;
  let input = 73;
  {
    let s = unsafe { std::slice::from_raw_parts_mut(&raw mut x, 1) };
    s[0] = input;
  }
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["input = 73"],
  );
}

#[test]
fn pointer_arithmetic_to_sibling_field() {
  check(
    Tier::Maybe,
    r#"
#[repr(C)] struct S { a: i32, b: i32 }
fn main() {
  let mut s = S { a: 0, b: 0 };
  let p = &raw mut s.a;
  let input = 73;
  unsafe { *p.add(1) = input; }
  let `(y)` = s.b;
}"#,
    Direction::Backward,
    &["s.b"],
    &["input = 73"],
  );
}

#[test]
fn union_field_punning() {
  check(
    Tier::Exact,
    r#"
union U { a: i32, b: u32 }
fn main() {
  let mut u = U { b: 0 };
  let input = 73;
  u.a = input;
  let `(y)` = unsafe { u.b };
}"#,
    Direction::Backward,
    &["u.b"],
    &["u.a = input", "input = 73"],
  );
}

// Writes through a pointee the callee cannot see: a type parameter, a `dyn`
// object, a by-value generic argument, or a raw pointer handed to std.

#[test]
fn callee_writes_field_via_ptr_write() {
  check(
    Tier::Exact,
    r#"
struct State { a: i32, b: i32 }
fn put(s: &mut State, v: i32) { let p = &raw mut s.b; unsafe { std::ptr::write(p, v); } }
fn main() {
  let mut s = State { a: 0, b: 0 };
  let input = 73;
  put(&mut s, input);
  let `(y)` = s.b;
}"#,
    Direction::Backward,
    &["s.b"],
    &["put(&mut s, input)", "input = 73"],
  );
}

#[test]
fn callee_writes_field_via_ptr_as_mut() {
  check(
    Tier::Exact,
    r#"
struct State { a: i32, b: i32 }
fn put(s: &mut State, v: i32) { let p = &raw mut s.b; *unsafe { p.as_mut() }.unwrap() = v; }
fn main() {
  let mut s = State { a: 0, b: 0 };
  let input = 73;
  put(&mut s, input);
  let `(y)` = s.b;
}"#,
    Direction::Backward,
    &["s.b"],
    &["put(&mut s, input)", "input = 73"],
  );
}

#[test]
fn generic_deref_mut_callee() {
  check(
    Tier::Exact,
    r#"
use std::ops::DerefMut;
fn set<T: DerefMut<Target = i32>>(t: &mut T, x: i32) { **t = x; }
fn main() {
  let mut x = 0;
  let input = 73;
  {
    let mut r = &mut x;
    set(&mut r, input);
  }
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["set(&mut r, input)", "input = 73"],
  );
}

#[test]
fn generic_trait_callee_writes_through_reference_held_by_argument() {
  check(
    Tier::Exact,
    r#"
trait Sink { fn put(&mut self, v: i32); }
struct S<'a>(&'a mut i32);
impl Sink for S<'_> { fn put(&mut self, v: i32) { *self.0 = v; } }
fn apply<T: Sink>(t: &mut T, x: i32) { t.put(x); }
fn main() {
  let mut x = 0;
  let input = 73;
  {
    let mut s = S(&mut x);
    apply(&mut s, input);
  }
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["apply(&mut s, input)", "input = 73"],
  );
}

#[test]
fn generic_callee_by_value_writes_through_contained_reference() {
  check(
    Tier::Exact,
    r#"
trait Sink { fn put(&mut self, v: i32); }
struct S<'a>(&'a mut i32);
impl Sink for S<'_> { fn put(&mut self, v: i32) { *self.0 = v; } }
fn apply<T: Sink>(mut t: T, x: i32) { t.put(x); }
fn main() {
  let mut x = 0;
  let input = 73;
  apply(S(&mut x), input);
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["apply(S(&mut x), input)", "input = 73"],
  );
}

#[test]
fn generic_writer_passed_mut_ref_by_value() {
  check(
    Tier::Exact,
    r#"
use std::io::Write;
fn emit<W: Write>(mut w: W, x: u8) { w.write_all(&[x]).unwrap(); }
fn main() {
  let mut buf: Vec<u8> = Vec::new();
  let input = 73;
  emit(&mut buf, input);
  let `(y)` = buf.len();
}"#,
    Direction::Backward,
    &["buf.len()"],
    &["emit(&mut buf, input)", "input = 73"],
  );
}

#[test]
fn generic_writer_passed_mut_ref_by_value_forward() {
  check(
    Tier::Exact,
    r#"
use std::io::Write;
fn emit<W: Write>(mut w: W, x: u8) { w.write_all(&[x]).unwrap(); }
fn main() {
  let mut buf: Vec<u8> = Vec::new();
  let `(input)` = 73;
  emit(&mut buf, input);
  let y = buf.len();
}"#,
    Direction::Forward,
    &["emit(&mut buf, input)"],
    &["buf.len()"],
  );
}

#[test]
fn dyn_trait_callee_writes_through_reference_held_by_argument() {
  check(
    Tier::Exact,
    r#"
trait Sink { fn put(&mut self, v: i32); }
struct S<'a>(&'a mut i32);
impl Sink for S<'_> { fn put(&mut self, v: i32) { *self.0 = v; } }
fn apply(t: &mut dyn Sink, x: i32) { t.put(x); }
fn main() {
  let mut x = 0;
  let input = 73;
  {
    let mut s = S(&mut x);
    apply(&mut s, input);
  }
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["apply(&mut s, input)", "input = 73"],
  );
}

#[test]
fn dyn_fnmut_passed_to_callee() {
  check(
    Tier::Exact,
    r#"
fn call(f: &mut dyn FnMut(i32), x: i32) { f(x); }
fn main() {
  let mut x = 0;
  let input = 73;
  {
    let mut g = |v| x = v;
    call(&mut g, input);
  }
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["call(&mut g, input)", "input = 73"],
  );
}

#[test]
fn boxed_dyn_by_value_callee() {
  check(
    Tier::Exact,
    r#"
trait Sink { fn put(&mut self, v: i32); }
struct S<'a>(&'a mut i32);
impl Sink for S<'_> { fn put(&mut self, v: i32) { *self.0 = v; } }
fn apply(mut t: Box<dyn Sink + '_>, x: i32) { t.put(x); }
fn main() {
  let mut x = 0;
  let input = 73;
  apply(Box::new(S(&mut x)), input);
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["input = 73"],
  );
}

#[test]
fn cell_read_through_reference_taken_before_the_write() {
  check(
    Tier::Exact,
    r#"
use std::cell::Cell;
fn main() {
  let c = Cell::new(0);
  let r = &c;
  let input = 73;
  r.set(input);
  let `(y)` = r.get();
}"#,
    Direction::Backward,
    &["r.get()"],
    &["r.set(input)", "input = 73"],
  );
}
