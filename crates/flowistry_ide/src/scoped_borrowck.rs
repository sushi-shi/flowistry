//! Collect Polonius facts only for bodies the IDE may analyze. Ordinary rustc
//! borrow checking still runs for every body, using its original provider.
//!
//! `mir_borrowck` runs on typeck roots; collecting the facts of a root also collects
//! those of its nested bodies (closures, inline constants). A root is relevant if its
//! span contains the requested position (so the root of the innermost body at the
//! position, which the focus analyzes, is always relevant), or with no position, if it
//! lies in the requested file. Without a request, every root is relevant, which is the
//! behavior of [`borrowck_facts::override_queries`].
use std::{
  cell::{Cell, RefCell},
  sync::Mutex,
};

use rustc_hir::def_id::LocalDefId;
use rustc_middle::{ty::TyCtxt, util::Providers};
use rustc_span::{FileName, Span};
use rustc_utils::{
  mir::borrowck_facts,
  source_map::{
    filename::Filename,
    range::{CharPos, CharRange, ToSpan},
  },
};

type Borrowck = for<'tcx> fn(
  TyCtxt<'tcx>,
  LocalDefId,
) -> rustc_middle::queries::mir_borrowck::ProvidedValue<'tcx>;

struct Scope {
  filename: String,
  position: Option<CharPos>,
  resolved: Option<(FileName, Option<Span>)>,
}

thread_local! {
  static SCOPE: RefCell<Option<Scope>> = const { RefCell::new(None) };
  static PROVIDERS: Cell<Option<(Borrowck, Borrowck)>> = const { Cell::new(None) };
  static COLLECTED: Cell<usize> = const { Cell::new(0) };
}

// rustc config and query installation can run on different threads. Transfer
// only owned request data; compiler spans remain local to the query thread.
static REQUEST: Mutex<Option<(String, Option<CharPos>)>> = Mutex::new(None);

pub fn configure(filename: String, position: Option<CharPos>) {
  *REQUEST.lock().unwrap() = Some((filename, position));
}

/// The number of typeck roots whose facts were collected in this compiler session.
pub fn collected_roots() -> usize {
  COLLECTED.with(Cell::get)
}

fn relevant(tcx: TyCtxt<'_>, def_id: LocalDefId) -> bool {
  SCOPE.with(|scope| {
    let mut scope = scope.borrow_mut();
    let Some(scope) = scope.as_mut() else {
      return true;
    };
    if scope.resolved.is_none() {
      let filename = Filename::intern(&scope.filename);
      let Ok(file) = filename.find_source_file(tcx.sess.source_map()) else {
        return true;
      };
      let target = if let Some(position) = scope.position {
        // `to_span` panics on a position outside the file. Collect everything then, so
        // that the analysis fails on it exactly as it would without a scope.
        let in_file = position.line < file.count_lines()
          && file
            .get_line(position.line)
            .is_some_and(|line| position.column <= line.chars().count());
        if !in_file {
          return true;
        }
        let Ok(span) = (CharRange {
          filename,
          start: position,
          end: position,
        })
        .to_span(tcx) else {
          return true;
        };
        Some(span)
      } else {
        None
      };
      scope.resolved = Some((file.name.clone(), target));
    }
    let (filename, target) = scope.resolved.as_ref().unwrap();
    let span = tcx
      .hir_span_with_body(tcx.local_def_id_to_hir_id(def_id))
      .source_callsite();
    if span.is_dummy() {
      return true;
    }
    if let Some(target) = target {
      span.contains(*target)
    } else {
      tcx.sess.source_map().lookup_source_file(span.lo()).name == *filename
    }
  })
}

pub fn override_queries(session: &rustc_session::Session, providers: &mut Providers) {
  // Each request is for one compiler session.
  let request = REQUEST.lock().unwrap().take();
  SCOPE.with(|scope| {
    *scope.borrow_mut() = request.map(|(filename, position)| Scope {
      filename,
      position,
      resolved: None,
    });
  });
  COLLECTED.with(|collected| collected.set(0));
  let original = providers.queries.mir_borrowck;
  borrowck_facts::override_queries(session, providers);
  let collecting = providers.queries.mir_borrowck;
  PROVIDERS.with(|saved| saved.set(Some((original, collecting))));
  providers.queries.mir_borrowck = |tcx, def_id| {
    let (original, collecting) = PROVIDERS.with(|saved| saved.get().unwrap());
    if relevant(tcx, def_id) {
      COLLECTED.with(|collected| collected.set(collected.get() + 1));
      collecting(tcx, def_id)
    } else {
      original(tcx, def_id)
    }
  };
}

#[cfg(test)]
mod test {
  use std::{fs, process::Command};

  use rustc_hir::def_id::LOCAL_CRATE;
  use rustc_utils::{
    mir::borrowck_facts::get_body_with_borrowck_facts,
    source_map::find_bodies::find_enclosing_bodies,
  };

  use super::*;

  const INPUT: &str = r#"
fn a(x: i32) -> i32 { x + 1 }

fn b(v: Vec<i32>) -> i32 {
  let f = |y: i32| y * 2;
  v.iter().map(|z| f(*z)).sum()
}

fn c() {}

fn outer() -> i32 {
  fn inner() -> i32 {
    let q = 1;
    q
  }
  inner()
}
"#;

  struct Callbacks {
    file: String,
    position: Option<CharPos>,
    /// Whether the position is in the file.
    valid: bool,
    /// The number of typeck roots whose facts were collected, and whether the facts of
    /// the body at the position are available.
    result: Option<(usize, bool)>,
  }

  impl rustc_driver::Callbacks for Callbacks {
    fn config(&mut self, config: &mut rustc_interface::Config) {
      borrowck_facts::enable_mir_simplification();
      if let Some(position) = self.position {
        configure(self.file.clone(), Some(position));
      }
      config.override_queries = Some(override_queries);
    }

    fn after_analysis<'tcx>(
      &mut self,
      _compiler: &rustc_interface::interface::Compiler,
      tcx: TyCtxt<'tcx>,
    ) -> rustc_driver::Compilation {
      let has_target_facts =
        self
          .position
          .filter(|_| self.valid)
          .is_some_and(|position| {
            let target = CharRange {
              filename: Filename::intern(&self.file),
              start: position,
              end: position,
            }
            .to_span(tcx)
            .unwrap();
            let body = find_enclosing_bodies(tcx, target).next().unwrap();
            let body = get_body_with_borrowck_facts(tcx, tcx.hir_body_owner_def_id(body));
            body.input_facts.is_some()
          });
      assert_eq!(tcx.crate_name(LOCAL_CRATE).as_str(), "scoped");
      self.result = Some((collected_roots(), has_target_facts));
      rustc_driver::Compilation::Stop
    }
  }

  fn collect(file: &str, position: Option<CharPos>, valid: bool) -> (usize, bool) {
    let sysroot = Command::new("rustc")
      .args(["--print", "sysroot"])
      .output()
      .unwrap()
      .stdout;
    let args = [
      "rustc",
      file,
      "--crate-name",
      "scoped",
      "--crate-type",
      "lib",
      "--edition=2024",
      "-Zidentify-regions",
      "-Zmir-opt-level=0",
      "-Zmaximal-hir-to-mir-coverage",
      "--allow",
      "warnings",
      "--sysroot",
      String::from_utf8(sysroot).unwrap().trim(),
    ]
    .map(str::to_owned);
    let mut callbacks = Callbacks {
      file: file.to_owned(),
      position,
      valid,
      result: None,
    };
    rustc_driver::catch_fatal_errors(|| {
      rustc_driver::run_compiler(&args, &mut callbacks);
    })
    .unwrap();
    callbacks.result.unwrap()
  }

  /// All cases run in one test: the request is process-global.
  #[test]
  fn facts_only_for_enclosing_roots() {
    let file = std::env::temp_dir().join(format!(
      "flowistry-scoped-borrowck-{}.rs",
      std::process::id()
    ));
    fs::write(&file, INPUT).unwrap();
    let file = file.to_str().unwrap();
    let at = |line, column| collect(file, Some(CharPos { line, column }), true);

    // Without a request, every root: a, b, c, outer and inner.
    assert_eq!(collect(file, None, false), (5, false));
    // In a closure of `b`: only its root `b`, whose facts include the closure's.
    assert_eq!(at(4, 19), (1, true));
    // In `c`.
    assert_eq!(at(8, 8), (1, true));
    // In `inner`, which is nested in `outer`: both roots enclose the position.
    assert_eq!(at(12, 4), (2, true));
    // Past the end of a line: every root, as without a request.
    let past_end = Some(CharPos {
      line: 8,
      column: 10,
    });
    assert_eq!(collect(file, past_end, false), (5, false));

    fs::remove_file(file).unwrap();
  }
}
