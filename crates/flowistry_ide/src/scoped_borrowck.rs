//! Collect Polonius facts only for bodies the IDE may analyze. Ordinary rustc
//! borrow checking still runs for every body, using its original provider.
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
}

// rustc config and query installation can run on different threads. Transfer
// only owned request data; compiler spans remain local to the query thread.
static REQUEST: Mutex<Option<(String, Option<CharPos>)>> = Mutex::new(None);

pub fn configure(filename: String, position: Option<CharPos>) {
  *REQUEST.lock().unwrap() = Some((filename, position));
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
  let request = REQUEST.lock().unwrap().clone();
  SCOPE.with(|scope| {
    *scope.borrow_mut() = request.map(|(filename, position)| Scope {
      filename,
      position,
      resolved: None,
    });
  });
  let original = providers.queries.mir_borrowck;
  borrowck_facts::override_queries(session, providers);
  let collecting = providers.queries.mir_borrowck;
  PROVIDERS.with(|saved| saved.set(Some((original, collecting))));
  providers.queries.mir_borrowck = |tcx, def_id| {
    let (original, collecting) = PROVIDERS.with(|saved| saved.get().unwrap());
    if relevant(tcx, def_id) {
      collecting(tcx, def_id)
    } else {
      original(tcx, def_id)
    }
  };
}
