//! More dependencies carried by memory that no place of the caller names, and
//! user-level implementations of shared-state libraries (locks, lock-free
//! structures, arenas, channels). Each test records the tier that reports the
//! dependency today, in both context modes (see `hidden_state.rs`).

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

// ---------------------------------------------------------------------------
// Part A: further hidden-state patterns.
// ---------------------------------------------------------------------------

// The clone moved into the spawned closure stays a handle of its group.
#[test]
fn thread_spawn_join_arc_mutex_clone() {
  check(
    Tier::Maybe,
    r#"
use std::{sync::{Arc, Mutex}, thread};
fn main() {
  let a = Arc::new(Mutex::new(0));
  let b = Arc::clone(&a);
  let input = 73;
  thread::spawn(move || { *b.lock().unwrap() = input; }).join().unwrap();
  let `(y)` = *a.lock().unwrap();
}"#,
    Direction::Backward,
    &["a.lock()"],
    &["thread::spawn", "input = 73"],
  );
}

// The scoped closure borrows the mutex, so lifetimes relate them.
#[test]
fn scoped_thread_writes_borrowed_mutex() {
  check(
    Tier::Exact,
    r#"
use std::{sync::Mutex, thread};
fn main() {
  let m = Mutex::new(0);
  let input = 73;
  thread::scope(|s| { s.spawn(|| { *m.lock().unwrap() = input; }); });
  let `(y)` = *m.lock().unwrap();
}"#,
    Direction::Backward,
    &["m.lock()"],
    &["thread::scope", "input = 73"],
  );
}

#[test]
fn rc_cell_in_fields_of_two_structs() {
  check(
    Tier::Maybe,
    r#"
use std::{cell::Cell, rc::Rc};
struct A { c: Rc<Cell<i32>> }
struct B { c: Rc<Cell<i32>> }
fn main() {
  let shared = Rc::new(Cell::new(0));
  let a = A { c: shared.clone() };
  let b = B { c: shared };
  let input = 73;
  b.c.set(input);
  let `(y)` = a.c.get();
}"#,
    Direction::Backward,
    &["a.c.get()"],
    &["b.c.set(input)", "input = 73"],
  );
}

// The signature does not write an `Rc` in a field of the pointee of a shared argument.
#[test]
fn known_miss_rc_cell_in_struct_field_written_by_callee() {
  check_modes(
    [Tier::Missed, Tier::Maybe],
    r#"
use std::{cell::Cell, rc::Rc};
struct A { c: Rc<Cell<i32>> }
struct B { c: Rc<Cell<i32>> }
fn poke(b: &B, x: i32) { b.c.set(x); }
fn main() {
  let shared = Rc::new(Cell::new(0));
  let a = A { c: shared.clone() };
  let b = B { c: shared };
  let input = 73;
  poke(&b, input);
  let `(y)` = a.c.get();
}"#,
    Direction::Backward,
    &["a.c.get()"],
    &["poke(&b, input)", "input = 73"],
  );
}

// The `&Cell` a callee derives from an `Rc` field aliases the loan of `w2`, which is
// not a handle of the group.
#[test]
fn known_miss_cell_ref_into_rc_returned_by_callee() {
  check(
    Tier::Missed,
    r#"
use std::{cell::Cell, rc::Rc};
struct W { c: Rc<Cell<i32>> }
fn cell(w: &W) -> &Cell<i32> { &w.c }
fn main() {
  let w1 = W { c: Rc::new(Cell::new(0)) };
  let w2 = W { c: w1.c.clone() };
  let input = 73;
  cell(&w2).set(input);
  let `(y)` = w1.c.get();
}"#,
    Direction::Backward,
    &["w1.c.get()"],
    &["input = 73"],
  );
}

#[test]
fn weak_upgrade_writes_strong_handle() {
  check(
    Tier::Maybe,
    r#"
use std::{cell::Cell, rc::Rc};
fn main() {
  let a = Rc::new(Cell::new(0));
  let w = Rc::downgrade(&a);
  let input = 73;
  w.upgrade().unwrap().set(input);
  let `(y)` = a.get();
}"#,
    Direction::Backward,
    &["a.get()"],
    &["input = 73"],
  );
}

// `Pin<Rc<_>>` is not recognized as a handle.
#[test]
fn known_miss_pinned_rc_clone() {
  check(
    Tier::Missed,
    r#"
use std::{cell::Cell, rc::Rc};
fn main() {
  let a = Rc::pin(Cell::new(0));
  let b = a.clone();
  let input = 73;
  b.set(input);
  let `(y)` = a.get();
}"#,
    Direction::Backward,
    &["a.get()"],
    &["b.set(input)", "input = 73"],
  );
}

// The raw round trip is invisible, but its result is an `Rc` of the same type.
#[test]
fn rc_from_raw_round_trip() {
  check(
    Tier::Maybe,
    r#"
use std::{cell::Cell, rc::Rc};
fn main() {
  let a = Rc::new(Cell::new(0));
  let p = Rc::into_raw(a.clone());
  let b = unsafe { Rc::from_raw(p) };
  let input = 73;
  b.set(input);
  let `(y)` = a.get();
}"#,
    Direction::Backward,
    &["a.get()"],
    &["b.set(input)", "input = 73"],
  );
}

// Unsizing to `Rc<dyn Store>` moves the clone to the group of `dyn Store`.
#[test]
fn known_miss_rc_dyn_trait_unsized_clone() {
  check(
    Tier::Missed,
    r#"
use std::{cell::Cell, rc::Rc};
trait Store { fn put(&self, v: i32); }
impl Store for Cell<i32> { fn put(&self, v: i32) { self.set(v); } }
fn main() {
  let a = Rc::new(Cell::new(0));
  let d: Rc<dyn Store> = a.clone();
  let input = 73;
  d.put(input);
  let `(y)` = a.get();
}"#,
    Direction::Backward,
    &["a.get()"],
    &["d.put(input)", "input = 73"],
  );
}

// The downcast `&Cell` aliases the loan of `d`, whose group is that of `dyn Any`.
#[test]
fn known_miss_rc_dyn_any_downcast() {
  check(
    Tier::Missed,
    r#"
use std::{any::Any, cell::Cell, rc::Rc};
fn main() {
  let a = Rc::new(Cell::new(0));
  let d: Rc<dyn Any> = a.clone();
  let input = 73;
  d.downcast_ref::<Cell<i32>>().unwrap().set(input);
  let `(y)` = a.get();
}"#,
    Direction::Backward,
    &["a.get()"],
    &["input = 73"],
  );
}

// A pointer returned by a callee from `&mut m` aliases nothing.
#[test]
fn known_miss_maybe_uninit_write_through_as_mut_ptr() {
  check(
    Tier::Missed,
    r#"
use std::mem::MaybeUninit;
fn main() {
  let mut m = MaybeUninit::<i32>::uninit();
  let p = m.as_mut_ptr();
  let input = 73;
  unsafe { p.write(input); }
  let `(y)` = unsafe { m.assume_init() };
}"#,
    Direction::Backward,
    &["m.assume_init()"],
    &["p.write(input)", "input = 73"],
  );
}

#[test]
fn maybe_uninit_write_method() {
  check(
    Tier::Exact,
    r#"
use std::mem::MaybeUninit;
fn main() {
  let mut m = MaybeUninit::<i32>::uninit();
  let input = 73;
  m.write(input);
  let `(y)` = unsafe { m.assume_init() };
}"#,
    Direction::Backward,
    &["m.assume_init()"],
    &["m.write(input)", "input = 73"],
  );
}

// A reference reborrowed from a raw pointer aliases only raw dereferences.
#[test]
fn known_miss_mem_replace_through_raw_pointer() {
  check(
    Tier::Missed,
    r#"
fn main() {
  let mut x = 0;
  let p = &raw mut x;
  let input = 73;
  let _old = unsafe { std::mem::replace(&mut *p, input) };
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["mem::replace", "input = 73"],
  );
}

#[test]
fn known_miss_mem_swap_through_raw_pointer() {
  check(
    Tier::Missed,
    r#"
fn main() {
  let mut x = 0;
  let p = &raw mut x;
  let mut input = 73;
  unsafe { std::mem::swap(&mut *p, &mut input) };
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["mem::swap", "input = 73"],
  );
}

#[test]
fn known_miss_ptr_copy_nonoverlapping_between_locals() {
  check(
    Tier::Missed,
    r#"
fn main() {
  let input = 73;
  let mut x = 0;
  unsafe { std::ptr::copy_nonoverlapping(&raw const input, &raw mut x, 1); }
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["copy_nonoverlapping", "input = 73"],
  );
}

// Raw dereferences alias each other, and `from_raw(p)` reads `*p`.
#[test]
fn box_into_raw_written_then_from_raw() {
  check(
    Tier::Exact,
    r#"
fn main() {
  let p = Box::into_raw(Box::new(0));
  let input = 73;
  unsafe { *p = input; }
  let b = unsafe { Box::from_raw(p) };
  let `(y)` = *b;
}"#,
    Direction::Backward,
    &["Box::from_raw(p)"],
    &["input = 73"],
  );
}

// The pointer into the buffer of `v` returned by `as_mut_ptr` aliases nothing.
#[test]
fn known_miss_vec_as_mut_ptr_then_set_len() {
  check(
    Tier::Missed,
    r#"
fn main() {
  let mut v: Vec<i32> = Vec::with_capacity(1);
  let p = v.as_mut_ptr();
  let input = 73;
  unsafe { p.write(input); v.set_len(1); }
  let `(y)` = v[0];
}"#,
    Direction::Backward,
    &["v[0]"],
    &["p.write(input)", "input = 73"],
  );
}

#[test]
fn known_miss_nonnull_from_mut_written() {
  check(
    Tier::Missed,
    r#"
use std::ptr::NonNull;
fn main() {
  let mut x = 0;
  let nn = NonNull::from(&mut x);
  let input = 73;
  unsafe { *nn.as_ptr() = input; }
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["input = 73"],
  );
}

#[test]
fn known_miss_atomic_ptr_to_local_written() {
  check(
    Tier::Missed,
    r#"
use std::sync::atomic::{AtomicPtr, Ordering};
fn main() {
  let mut x = 0;
  let ap = AtomicPtr::new(&raw mut x);
  let input = 73;
  unsafe { *ap.load(Ordering::SeqCst) = input; }
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["input = 73"],
  );
}

#[test]
fn unsafe_cell_get_in_same_function() {
  check(
    Tier::Exact,
    r#"
use std::cell::UnsafeCell;
fn main() {
  let c = UnsafeCell::new(0);
  let input = 73;
  unsafe { *c.get() = input; }
  let `(y)` = unsafe { *c.get() };
}"#,
    Direction::Backward,
    &["c.get()"],
    &["input = 73"],
  );
}

// Both accesses dereference the address of the same static.
#[test]
fn static_mut_in_same_function() {
  check(
    Tier::Exact,
    r#"
static mut G: i32 = 0;
fn main() {
  let input = 73;
  unsafe { G = input; }
  let `(y)` = unsafe { G };
}"#,
    Direction::Backward,
    &["G }"],
    &["input = 73"],
  );
}

#[test]
fn static_mut_array_indexed_in_same_function() {
  check(
    Tier::Exact,
    r#"
static mut A: [i32; 4] = [0; 4];
fn main() {
  let (i, j) = (1, 1);
  let input = 73;
  unsafe { A[i] = input; }
  let `(y)` = unsafe { A[j] };
}"#,
    Direction::Backward,
    &["A[j]"],
    &["input = 73"],
  );
}

#[test]
fn known_miss_static_mut_array_indexed_by_callees() {
  check(
    Tier::Missed,
    r#"
static mut A: [i32; 4] = [0; 4];
fn put(i: usize, v: i32) { unsafe { A[i] = v; } }
fn get(i: usize) -> i32 { unsafe { A[i] } }
fn main() {
  let input = 73;
  put(1, input);
  let `(y)` = get(1);
}"#,
    Direction::Backward,
    &["get(1)"],
    &["put(1, input)", "input = 73"],
  );
}

// A `&'static` read out of a static is not related to the static it points to.
#[test]
fn known_miss_static_registry_of_references_to_static_atomic() {
  check(
    Tier::Missed,
    r#"
use std::sync::atomic::{AtomicI32, Ordering};
static A: AtomicI32 = AtomicI32::new(0);
static REG: [&AtomicI32; 1] = [&A];
fn main() {
  let input = 73;
  REG[0].store(input, Ordering::SeqCst);
  let `(y)` = A.load(Ordering::SeqCst);
}"#,
    Direction::Backward,
    &["A.load"],
    &["input = 73"],
  );
}

// The callee is only known as a function pointer read from a static.
#[test]
fn known_miss_static_fn_pointer_callback_writes_static() {
  check(
    Tier::Missed,
    r#"
static mut CB: Option<fn(i32)> = None;
static mut OUT: i32 = 0;
fn sink(x: i32) { unsafe { OUT = x; } }
fn main() {
  unsafe { CB = Some(sink); }
  let input = 73;
  unsafe { (CB.unwrap())(input); }
  let `(y)` = unsafe { OUT };
}"#,
    Direction::Backward,
    &["OUT }"],
    &["input = 73"],
  );
}

// The clone escapes into a boxed closure in a thread-local, run by a later call.
#[test]
fn known_miss_thread_local_hook_registry_writes_captured_rc() {
  check(
    Tier::Missed,
    r#"
use std::{cell::{Cell, RefCell}, rc::Rc};
thread_local! { static HOOKS: RefCell<Vec<Box<dyn Fn(i32)>>> = RefCell::new(Vec::new()); }
fn main() {
  let c = Rc::new(Cell::new(0));
  let c2 = c.clone();
  HOOKS.with_borrow_mut(|h| h.push(Box::new(move |v| c2.set(v))));
  let input = 73;
  HOOKS.with_borrow(|h| for f in h { f(input) });
  let `(y)` = c.get();
}"#,
    Direction::Backward,
    &["c.get()"],
    &["input = 73"],
  );
}

// Both references returned by `alloc` point into the same static.
#[test]
fn known_miss_static_pool_recycles_slot() {
  check(
    Tier::Missed,
    r#"
static mut SLOT: i32 = 0;
fn alloc() -> &'static mut i32 { unsafe { &mut *(&raw mut SLOT) } }
fn main() {
  let input = 73;
  {
    let a = alloc();
    *a = input;
  }
  let b = alloc();
  let `(y)` = *b;
}"#,
    Direction::Backward,
    &["alloc()"],
    &["input = 73"],
  );
}

#[test]
fn once_cell_set_through_shared_reference() {
  check(
    Tier::Exact,
    r#"
use std::cell::OnceCell;
fn main() {
  let o = OnceCell::new();
  let r = &o;
  let input = 73;
  let _ = r.set(input);
  let `(y)` = *o.get().unwrap();
}"#,
    Direction::Backward,
    &["o.get()"],
    &["r.set(input)", "input = 73"],
  );
}

#[test]
fn rc_once_cell_clone() {
  check(
    Tier::Maybe,
    r#"
use std::{cell::OnceCell, rc::Rc};
fn main() {
  let a: Rc<OnceCell<i32>> = Rc::new(OnceCell::new());
  let b = a.clone();
  let input = 73;
  let _ = b.set(input);
  let `(y)` = *a.get().unwrap();
}"#,
    Direction::Backward,
    &["a.get()"],
    &["b.set(input)", "input = 73"],
  );
}

#[test]
fn iterator_adaptor_closure_holding_cell() {
  check(
    Tier::Exact,
    r#"
use std::cell::Cell;
fn main() {
  let c = Cell::new(0);
  let input = 73;
  let it = (0..1).map(|_| c.set(input));
  for _ in it {}
  let `(y)` = c.get();
}"#,
    Direction::Backward,
    &["c.get()"],
    &["input = 73"],
  );
}

#[test]
fn catch_unwind_panic_payload() {
  check(
    Tier::Exact,
    r#"
fn main() {
  let input = 73;
  let r = std::panic::catch_unwind(|| std::panic::panic_any(input));
  let `(y)` = *r.unwrap_err().downcast::<i32>().unwrap();
}"#,
    Direction::Backward,
    &["catch_unwind"],
    &["input = 73"],
  );
}

// A miss of the exact tier in safe code: the `&Cell<i32>` view of `&mut x` does not
// alias `x`.
#[test]
fn known_miss_cell_from_mut() {
  check(
    Tier::Missed,
    r#"
use std::cell::Cell;
fn main() {
  let mut x = 0;
  let input = 73;
  {
    let c = Cell::from_mut(&mut x);
    c.set(input);
  }
  let `(y)` = x;
}"#,
    Direction::Backward,
    &["x = 0"],
    &["c.set(input)", "input = 73"],
  );
}

#[test]
fn async_block_on_future_holding_cell() {
  check(
    Tier::Exact,
    r#"
use std::{cell::Cell, future::Future, pin::pin, task::{Context, Poll, Waker}};
fn block_on<F: Future>(f: F) -> F::Output {
  let mut f = pin!(f);
  let mut cx = Context::from_waker(Waker::noop());
  loop { if let Poll::Ready(v) = f.as_mut().poll(&mut cx) { return v; } }
}
async fn stash(c: &Cell<i32>, x: i32) { c.set(x); }
fn main() {
  let c = Cell::new(0);
  let input = 73;
  block_on(stash(&c, input));
  let `(y)` = c.get();
}"#,
    Direction::Backward,
    &["c.get()"],
    &["input = 73"],
  );
}

// The `Arc` moved into the `Waker` is reached only through its raw data pointer.
#[test]
fn known_miss_async_waker_from_arc_wake() {
  check(
    Tier::Missed,
    r#"
use std::{future::Future, pin::Pin, sync::{Arc, atomic::{AtomicI32, Ordering}}, task::{Context, Poll, Wake, Waker}};
struct Flag(AtomicI32);
impl Wake for Flag { fn wake(self: Arc<Self>) { self.0.fetch_add(1, Ordering::SeqCst); } }
struct Fut(i32);
impl Future for Fut {
  type Output = ();
  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    if self.0 > 0 { cx.waker().wake_by_ref(); }
    Poll::Ready(())
  }
}
fn main() {
  let flag = Arc::new(Flag(AtomicI32::new(0)));
  let waker = Waker::from(flag.clone());
  let mut cx = Context::from_waker(&waker);
  let input = 73;
  let mut f = Fut(input);
  let _ = Pin::new(&mut f).poll(&mut cx);
  let `(y)` = flag.0.load(Ordering::SeqCst);
}"#,
    Direction::Backward,
    &["flag.0.load"],
    &["input = 73"],
  );
}

// The address of `flag` escapes into the raw data pointer of the waker, called
// through its vtable.
#[test]
fn known_miss_async_raw_waker_data_pointer() {
  check(
    Tier::Missed,
    r#"
use std::{future::Future, pin::Pin, sync::atomic::{AtomicI32, Ordering}, task::{Context, Poll, RawWaker, RawWakerVTable, Waker}};
static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);
fn clone(p: *const ()) -> RawWaker { RawWaker::new(p, &VTABLE) }
fn wake(p: *const ()) { wake_by_ref(p) }
fn wake_by_ref(p: *const ()) { unsafe { (*(p as *const AtomicI32)).fetch_add(1, Ordering::SeqCst); } }
fn drop_waker(_: *const ()) {}
struct Fut(i32);
impl Future for Fut {
  type Output = ();
  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    if self.0 > 0 { cx.waker().wake_by_ref(); }
    Poll::Ready(())
  }
}
fn main() {
  let flag = AtomicI32::new(0);
  let waker = unsafe { Waker::from_raw(RawWaker::new(&raw const flag as *const (), &VTABLE)) };
  let mut cx = Context::from_waker(&waker);
  let input = 73;
  let mut f = Fut(input);
  let _ = Pin::new(&mut f).poll(&mut cx);
  let `(y)` = flag.load(Ordering::SeqCst);
}"#,
    Direction::Backward,
    &["flag.load"],
    &["input = 73"],
  );
}

#[test]
fn known_miss_set_current_dir_round_trip() {
  check(
    Tier::Missed,
    r#"
fn main() {
  let input = "/tmp";
  std::env::set_current_dir(input).unwrap();
  let `(y)` = std::env::current_dir().unwrap();
}"#,
    Direction::Backward,
    &["current_dir()"],
    &["set_current_dir", "input = \"/tmp\""],
  );
}

// A miss of the exact tier in safe code: the write through a copy of a `&'static`
// reference without loans is lost, in both tiers.
#[test]
fn known_miss_box_leak_static_cell_copied_reference() {
  check(
    Tier::Missed,
    r#"
use std::cell::Cell;
fn main() {
  let r: &'static Cell<i32> = Box::leak(Box::new(Cell::new(0)));
  let r2 = r;
  let input = 73;
  r2.set(input);
  let `(y)` = r.get();
}"#,
    Direction::Backward,
    &["r.get()"],
    &["r2.set(input)", "input = 73"],
  );
}

// ---------------------------------------------------------------------------
// Part B: user-level implementations of shared-state libraries. A write through
// one handle and a read through another.
// ---------------------------------------------------------------------------

const RCU: &str = r#"
use std::sync::{Arc, atomic::{AtomicPtr, Ordering::SeqCst}};
struct Rcu { p: AtomicPtr<i32> }
impl Rcu {
  fn new(v: i32) -> Rcu { Rcu { p: AtomicPtr::new(Box::into_raw(Box::new(v))) } }
  fn read(&self) -> i32 { unsafe { *self.p.load(SeqCst) } }
  fn update(&self, v: i32) { let _old = self.p.swap(Box::into_raw(Box::new(v)), SeqCst); }
  fn update_in_place(&self, v: i32) { unsafe { *self.p.load(SeqCst) = v; } }
}
"#;

#[test]
fn lib_rcu_update_via_shared_ref() {
  check(
    Tier::Exact,
    &format!(
      "{RCU}{}",
      r#"
fn main() {
  let r = Rcu::new(0);
  let input = 73;
  r.update(input);
  let `(y)` = r.read();
}"#
    ),
    Direction::Backward,
    &["r.read()"],
    &["r.update(input)", "input = 73"],
  );
}

#[test]
fn lib_rcu_update_via_arc_clone() {
  check(
    Tier::Maybe,
    &format!(
      "{RCU}{}",
      r#"
fn main() {
  let r = Arc::new(Rcu::new(0));
  let w = r.clone();
  let input = 73;
  w.update(input);
  let `(y)` = r.read();
}"#
    ),
    Direction::Backward,
    &["r.read()"],
    &["w.update(input)", "input = 73"],
  );
}

// `AtomicPtr` is not `Freeze`, so a callee given `&Rcu` may write it, and its write
// through the loaded pointer is attributed to it.
#[test]
fn lib_rcu_update_in_place_via_shared_ref() {
  check(
    Tier::Exact,
    &format!(
      "{RCU}{}",
      r#"
fn main() {
  let r = Rcu::new(0);
  let input = 73;
  r.update_in_place(input);
  let `(y)` = r.read();
}"#
    ),
    Direction::Backward,
    &["r.read()"],
    &["r.update_in_place(input)", "input = 73"],
  );
}

const TREIBER: &str = r#"
use std::{ptr::null_mut, sync::{Arc, atomic::{AtomicPtr, Ordering::SeqCst}}};
struct Node { val: i32, next: *mut Node }
struct Stack { head: AtomicPtr<Node> }
impl Stack {
  fn new() -> Stack { Stack { head: AtomicPtr::new(null_mut()) } }
  fn push(&self, v: i32) {
    let n = Box::into_raw(Box::new(Node { val: v, next: null_mut() }));
    loop {
      let h = self.head.load(SeqCst);
      unsafe { (*n).next = h; }
      if self.head.compare_exchange(h, n, SeqCst, SeqCst).is_ok() { return; }
    }
  }
  fn pop(&self) -> Option<i32> {
    loop {
      let h = self.head.load(SeqCst);
      if h.is_null() { return None; }
      let next = unsafe { (*h).next };
      if self.head.compare_exchange(h, next, SeqCst, SeqCst).is_ok() {
        return Some(unsafe { Box::from_raw(h) }.val);
      }
    }
  }
  fn bump_top(&self, v: i32) { let h = self.head.load(SeqCst); if !h.is_null() { unsafe { (*h).val = v; } } }
}
"#;

#[test]
fn lib_treiber_stack_via_arc_clone() {
  check(
    Tier::Maybe,
    &format!(
      "{TREIBER}{}",
      r#"
fn main() {
  let s = Arc::new(Stack::new());
  let s2 = s.clone();
  let input = 73;
  s2.push(input);
  let `(y)` = s.pop();
}"#
    ),
    Direction::Backward,
    &["s.pop()"],
    &["s2.push(input)", "input = 73"],
  );
}

#[test]
fn lib_treiber_stack_in_place_write_via_shared_ref() {
  check(
    Tier::Exact,
    &format!(
      "{TREIBER}{}",
      r#"
fn main() {
  let s = Stack::new();
  s.push(0);
  let input = 73;
  s.bump_top(input);
  let `(y)` = s.pop();
}"#
    ),
    Direction::Backward,
    &["s.pop()"],
    &["s.bump_top(input)", "input = 73"],
  );
}

const SEQLOCK: &str = r#"
use std::{cell::UnsafeCell, sync::{Arc, atomic::{AtomicUsize, Ordering::SeqCst}}};
struct SeqLock { seq: AtomicUsize, data: UnsafeCell<i32> }
unsafe impl Sync for SeqLock {}
impl SeqLock {
  fn new(v: i32) -> SeqLock { SeqLock { seq: AtomicUsize::new(0), data: UnsafeCell::new(v) } }
  fn write(&self, v: i32) {
    self.seq.fetch_add(1, SeqCst);
    unsafe { *self.data.get() = v; }
    self.seq.fetch_add(1, SeqCst);
  }
  fn read(&self) -> i32 {
    loop {
      let s1 = self.seq.load(SeqCst);
      if s1 % 2 == 1 { continue; }
      let v = unsafe { std::ptr::read_volatile(self.data.get()) };
      if self.seq.load(SeqCst) == s1 { return v; }
    }
  }
}
"#;

#[test]
fn lib_seqlock_via_shared_ref() {
  check(
    Tier::Exact,
    &format!(
      "{SEQLOCK}{}",
      r#"
fn main() {
  let l = SeqLock::new(0);
  let input = 73;
  l.write(input);
  let `(y)` = l.read();
}"#
    ),
    Direction::Backward,
    &["l.read()"],
    &["l.write(input)", "input = 73"],
  );
}

#[test]
fn lib_seqlock_via_arc_clone() {
  check(
    Tier::Maybe,
    &format!(
      "{SEQLOCK}{}",
      r#"
fn main() {
  let l = Arc::new(SeqLock::new(0));
  let w = l.clone();
  let input = 73;
  w.write(input);
  let `(y)` = l.read();
}"#
    ),
    Direction::Backward,
    &["l.read()"],
    &["w.write(input)", "input = 73"],
  );
}

const ARC_SWAP: &str = r#"
use std::sync::{Arc, atomic::{AtomicPtr, Ordering::SeqCst}};
struct ArcSwap { p: AtomicPtr<i32> }
impl ArcSwap {
  fn new(v: Arc<i32>) -> ArcSwap { ArcSwap { p: AtomicPtr::new(Arc::into_raw(v) as *mut i32) } }
  fn store(&self, v: Arc<i32>) {
    let old = self.p.swap(Arc::into_raw(v) as *mut i32, SeqCst);
    unsafe { drop(Arc::from_raw(old)) }
  }
  fn load(&self) -> Arc<i32> {
    let p = self.p.load(SeqCst);
    unsafe { Arc::increment_strong_count(p); Arc::from_raw(p) }
  }
}
"#;

#[test]
fn lib_arc_swap_via_shared_ref() {
  check(
    Tier::Exact,
    &format!(
      "{ARC_SWAP}{}",
      r#"
fn main() {
  let s = ArcSwap::new(Arc::new(0));
  let input = 73;
  s.store(Arc::new(input));
  let `(y)` = *s.load();
}"#
    ),
    Direction::Backward,
    &["s.load()"],
    &["s.store(", "input = 73"],
  );
}

#[test]
fn lib_arc_swap_via_arc_clone() {
  check(
    Tier::Maybe,
    &format!(
      "{ARC_SWAP}{}",
      r#"
fn main() {
  let s = Arc::new(ArcSwap::new(Arc::new(0)));
  let w = s.clone();
  let input = 73;
  w.store(Arc::new(input));
  let `(y)` = *s.load();
}"#
    ),
    Direction::Backward,
    &["s.load()"],
    &["w.store(", "input = 73"],
  );
}

const SPIN: &str = r#"
use std::{cell::UnsafeCell, ops::{Deref, DerefMut}, sync::{Arc, atomic::{AtomicBool, Ordering}}};
struct Spin<T> { locked: AtomicBool, data: UnsafeCell<T> }
unsafe impl<T: Send> Sync for Spin<T> {}
struct Guard<'a, T> { lock: &'a Spin<T> }
impl<T> Spin<T> {
  const fn new(v: T) -> Self { Spin { locked: AtomicBool::new(false), data: UnsafeCell::new(v) } }
  fn lock(&self) -> Guard<'_, T> {
    while self.locked.swap(true, Ordering::Acquire) {}
    Guard { lock: self }
  }
}
impl<T> Deref for Guard<'_, T> { type Target = T; fn deref(&self) -> &T { unsafe { &*self.lock.data.get() } } }
impl<T> DerefMut for Guard<'_, T> { fn deref_mut(&mut self) -> &mut T { unsafe { &mut *self.lock.data.get() } } }
impl<T> Drop for Guard<'_, T> { fn drop(&mut self) { self.lock.locked.store(false, Ordering::Release); } }
"#;

#[test]
fn lib_spinlock_local() {
  check(
    Tier::Exact,
    &format!(
      "{SPIN}{}",
      r#"
fn main() {
  let m = Spin::new(0);
  let input = 73;
  *m.lock() = input;
  let `(y)` = *m.lock();
}"#
    ),
    Direction::Backward,
    &["m.lock()"],
    &["input = 73"],
  );
}

#[test]
fn lib_spinlock_via_arc_clone() {
  check(
    Tier::Maybe,
    &format!(
      "{SPIN}{}",
      r#"
fn main() {
  let m = Arc::new(Spin::new(0));
  let w = m.clone();
  let input = 73;
  *w.lock() = input;
  let `(y)` = *m.lock();
}"#
    ),
    Direction::Backward,
    &["m.lock()"],
    &["input = 73"],
  );
}

// A static is not a handle.
#[test]
fn known_miss_lib_spinlock_static() {
  check(
    Tier::Missed,
    &format!(
      "{SPIN}{}",
      r#"
static M: Spin<i32> = Spin::new(0);
fn main() {
  let input = 73;
  *M.lock() = input;
  let `(y)` = *M.lock();
}"#
    ),
    Direction::Backward,
    &["M.lock()"],
    &["input = 73"],
  );
}

const ARENA: &str = r#"
use std::{cell::RefCell, rc::Rc};
struct Arena { items: Vec<i32> }
impl Arena {
  fn new() -> Arena { Arena { items: Vec::new() } }
  fn alloc(&mut self, v: i32) -> usize { self.items.push(v); self.items.len() - 1 }
  fn get(&self, h: usize) -> i32 { self.items[h] }
  fn set(&mut self, h: usize, v: i32) { self.items[h] = v; }
}
"#;

// Index handles alias harmlessly: the arena is a single place.
#[test]
fn lib_arena_index_handles_local() {
  check(
    Tier::Exact,
    &format!(
      "{ARENA}{}",
      r#"
fn main() {
  let mut a = Arena::new();
  let h1 = a.alloc(0);
  let h2 = h1;
  let input = 73;
  a.set(h1, input);
  let `(y)` = a.get(h2);
}"#
    ),
    Direction::Backward,
    &["a.get(h2)"],
    &["a.set(h1, input)", "input = 73"],
  );
}

#[test]
fn lib_arena_behind_rc_refcell_clones() {
  check(
    Tier::Maybe,
    &format!(
      "{ARENA}{}",
      r#"
fn main() {
  let a = Rc::new(RefCell::new(Arena::new()));
  let b = a.clone();
  let h = a.borrow_mut().alloc(0);
  let input = 73;
  b.borrow_mut().set(h, input);
  let `(y)` = a.borrow().get(h);
}"#
    ),
    Direction::Backward,
    &["a.borrow().get(h)"],
    &["input = 73"],
  );
}

// The handle is a plain index into a thread-local arena.
#[test]
fn known_miss_lib_interner_index_into_thread_local_arena() {
  check(
    Tier::Missed,
    &format!(
      "{ARENA}{}",
      r#"
thread_local! { static ARENA: RefCell<Arena> = RefCell::new(Arena::new()); }
#[derive(Clone, Copy)] struct Id(usize);
impl Id {
  fn new(v: i32) -> Id { Id(ARENA.with_borrow_mut(|a| a.alloc(v))) }
  fn set(self, v: i32) { ARENA.with_borrow_mut(|a| a.set(self.0, v)) }
  fn get(self) -> i32 { ARENA.with_borrow(|a| a.get(self.0)) }
}
fn main() {
  let id = Id::new(0);
  let alias = id;
  let input = 73;
  id.set(input);
  let `(y)` = alias.get();
}"#
    ),
    Direction::Backward,
    &["alias.get()"],
    &["id.set(input)", "input = 73"],
  );
}

const SLAB: &str = r#"
#[derive(Clone, Copy, PartialEq)] struct Key { idx: usize, generation: u32 }
struct Slab { slots: Vec<(u32, Option<i32>)> }
impl Slab {
  fn insert(&mut self, v: i32) -> Key {
    for (i, (g, s)) in self.slots.iter_mut().enumerate() {
      if s.is_none() { *g += 1; *s = Some(v); return Key { idx: i, generation: *g }; }
    }
    self.slots.push((0, Some(v)));
    Key { idx: self.slots.len() - 1, generation: 0 }
  }
  fn remove(&mut self, k: Key) { if self.slots[k.idx].0 == k.generation { self.slots[k.idx].1 = None; } }
  fn get(&self, k: Key) -> Option<i32> {
    let (g, s) = self.slots[k.idx];
    if g == k.generation { s } else { None }
  }
}
"#;

#[test]
fn lib_slab_generational_keys() {
  check(
    Tier::Exact,
    &format!(
      "{SLAB}{}",
      r#"
fn main() {
  let mut s = Slab { slots: Vec::new() };
  let old = s.insert(0);
  s.remove(old);
  let input = 73;
  let _new = s.insert(input);
  let `(y)` = s.get(old);
}"#
    ),
    Direction::Backward,
    &["s.get(old)"],
    &["s.insert(input)", "input = 73"],
  );
}

const LIST: &str = r#"
use std::ptr::null_mut;
struct Node { val: i32, next: *mut Node }
struct List { head: *mut Node }
impl List {
  fn push(&mut self, v: i32) { self.head = Box::into_raw(Box::new(Node { val: v, next: self.head })); }
  fn set_first(&self, v: i32) { unsafe { (*self.head).val = v; } }
  fn first(&self) -> i32 { unsafe { (*self.head).val } }
}
"#;

// The address of `a` escapes into a raw pointer field.
#[test]
fn known_miss_lib_intrusive_list_stack_nodes() {
  check(
    Tier::Missed,
    &format!(
      "{LIST}{}",
      r#"
fn main() {
  let mut a = Node { val: 0, next: null_mut() };
  let b = Node { val: 0, next: &raw mut a };
  let input = 73;
  unsafe { (*b.next).val = input; }
  let `(y)` = a.val;
}"#
    ),
    Direction::Backward,
    &["a.val"],
    &["input = 73"],
  );
}

// Raw dereferences alias each other.
#[test]
fn lib_intrusive_list_two_cursors() {
  check(
    Tier::Exact,
    &format!(
      "{LIST}{}",
      r#"
fn main() {
  let mut l = List { head: null_mut() };
  l.push(0);
  let (c1, c2) = (l.head, l.head);
  let input = 73;
  unsafe { (*c1).val = input; }
  let `(y)` = unsafe { (*c2).val };
}"#
    ),
    Direction::Backward,
    &["(*c2).val"],
    &["input = 73"],
  );
}

// `*mut Node` is `Freeze`, so a callee given `&List` is assumed not to write through
// it.
#[test]
fn known_miss_lib_intrusive_list_methods_through_shared_ref() {
  check(
    Tier::Missed,
    &format!(
      "{LIST}{}",
      r#"
fn main() {
  let mut l = List { head: null_mut() };
  l.push(0);
  let input = 73;
  l.set_first(input);
  let `(y)` = l.first();
}"#
    ),
    Direction::Backward,
    &["l.first()"],
    &["l.set_first(input)", "input = 73"],
  );
}

const CHAN: &str = r#"
use std::{cell::UnsafeCell, collections::VecDeque, sync::Arc};
struct Chan { q: UnsafeCell<VecDeque<i32>> }
struct Sender(Arc<Chan>);
struct Receiver(Arc<Chan>);
fn channel() -> (Sender, Receiver) {
  let c = Arc::new(Chan { q: UnsafeCell::new(VecDeque::new()) });
  (Sender(c.clone()), Receiver(c))
}
impl Sender { fn send(&self, v: i32) { unsafe { (*self.0.q.get()).push_back(v) } } }
impl Receiver { fn recv(&self) -> Option<i32> { unsafe { (*self.0.q.get()).pop_front() } } }
struct RawSender(*const Chan);
struct RawReceiver(*const Chan);
fn raw_channel() -> (RawSender, RawReceiver) {
  let c = Arc::new(Chan { q: UnsafeCell::new(VecDeque::new()) });
  (RawSender(Arc::into_raw(c.clone())), RawReceiver(Arc::into_raw(c)))
}
impl RawSender { fn send(&self, v: i32) { unsafe { (*(*self.0).q.get()).push_back(v) } } }
impl RawReceiver { fn recv(&self) -> Option<i32> { unsafe { (*(*self.0).q.get()).pop_front() } } }
"#;

// Both ends hold the `Arc` in a field (see
// `known_miss_rc_cell_in_struct_field_written_by_callee`).
#[test]
fn known_miss_lib_channel_arc_unsafe_cell() {
  check(
    Tier::Missed,
    &format!(
      "{CHAN}{}",
      r#"
fn main() {
  let (tx, rx) = channel();
  let input = 73;
  tx.send(input);
  let `(y)` = rx.recv();
}"#
    ),
    Direction::Backward,
    &["rx.recv()"],
    &["tx.send(input)", "input = 73"],
  );
}

// Both ends hold a raw pointer, which is `Freeze`, as the ends of `std::sync::mpsc` do.
#[test]
fn known_miss_lib_channel_raw_pointer_ends() {
  check(
    Tier::Missed,
    &format!(
      "{CHAN}{}",
      r#"
fn main() {
  let (tx, rx) = raw_channel();
  let input = 73;
  tx.send(input);
  let `(y)` = rx.recv();
}"#
    ),
    Direction::Backward,
    &["rx.recv()"],
    &["tx.send(input)", "input = 73"],
  );
}

// As `known_miss_box_leak_static_cell_copied_reference`, with the `&'static` returned
// by a local callee.
#[test]
fn known_miss_static_ref_returned_by_callee_copied() {
  check(
    Tier::Missed,
    r#"
use std::cell::Cell;
fn leak() -> &'static Cell<i32> { Box::leak(Box::new(Cell::new(0))) }
fn main() {
  let r = leak();
  let r2 = r;
  let input = 73;
  r2.set(input);
  let `(y)` = r.get();
}"#,
    Direction::Backward,
    &["r.get()"],
    &["r2.set(input)", "input = 73"],
  );
}

// As `rc_refcell_clone_written_by_callee` in `hidden_state.rs`, with the handle in a
// field.
#[test]
fn rc_field_passed_directly_to_callee() {
  check(
    Tier::Maybe,
    r#"
use std::{cell::Cell, rc::Rc};
struct B { c: Rc<Cell<i32>> }
fn poke(c: &Rc<Cell<i32>>, x: i32) { c.set(x); }
fn main() {
  let a = Rc::new(Cell::new(0));
  let b = B { c: a.clone() };
  let input = 73;
  poke(&b.c, input);
  let `(y)` = a.get();
}"#,
    Direction::Backward,
    &["a.get()"],
    &["poke(&b.c, input)", "input = 73"],
  );
}

#[test]
fn lib_spinlock_arc_clone_written_by_callee() {
  check(
    Tier::Maybe,
    &format!(
      "{SPIN}{}",
      r#"
fn put(m: &Arc<Spin<i32>>, v: i32) { *m.lock() = v; }
fn main() {
  let m = Arc::new(Spin::new(0));
  let w = m.clone();
  let input = 73;
  put(&w, input);
  let `(y)` = *m.lock();
}"#
    ),
    Direction::Backward,
    &["m.lock()"],
    &["put(&w, input)", "input = 73"],
  );
}

#[test]
fn lib_treiber_stack_arc_clone_pushed_by_callee() {
  check(
    Tier::Maybe,
    &format!(
      "{TREIBER}{}",
      r#"
fn produce(s: &Arc<Stack>, v: i32) { s.push(v); }
fn main() {
  let s = Arc::new(Stack::new());
  let s2 = s.clone();
  let input = 73;
  produce(&s2, input);
  let `(y)` = s.pop();
}"#
    ),
    Direction::Backward,
    &["s.pop()"],
    &["produce(&s2, input)", "input = 73"],
  );
}
