//! Utilities for analyzing places: children, aliases, etc.

use std::{cell::Cell, ops::ControlFlow, rc::Rc};

use indexical::ToIndex;
use rustc_borrowck::consumers::BodyWithBorrowckFacts;
use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_hir::def_id::DefId;
use rustc_middle::{
  mir::*,
  ty::{
    Region, RegionKind, RegionVid, Ty, TyCtxt, TyKind, TypeSuperVisitable, TypeVisitor,
  },
};
use rustc_utils::{
  BodyExt, MutabilityExt, PlaceExt, block_timer,
  cache::{Cache, CopyCache},
  mir::{
    location_or_arg::{
      LocationOrArg,
      index::{LocationOrArgDomain, LocationOrArgIndex},
    },
    place::UNKNOWN_REGION,
  },
};

use super::{
  aliases::Aliases,
  utils::{ErasedTy, MAX_ARG_POINTER_DEPTH, PlaceSet},
};
use crate::extensions::{EvalMode, MutabilityMode};

/// A place in normal form, used as the row key of a
/// [`FlowDomain`](crate::infoflow::FlowDomain).
///
/// A `NormPlace` is produced only by [`PlaceInfo::normalize`], which erases regions,
/// normalizes associated types, collapses every index projection to `[_0]` and drops
/// subslices (see [`PlaceExt::normalize`]).
///
/// A `NormPlace` is *not* a valid place of the body: its types have no region
/// variables, so alias and loan queries on it would silently find nothing. For that
/// reason no API that takes a [`Place`] accepts a `NormPlace`, and there is no public
/// conversion back to [`Place`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NormPlace<'tcx>(Place<'tcx>);

impl<'tcx> NormPlace<'tcx> {
  /// The row of state that no place of a body names (see
  /// [`infoflow::hidden`](crate::infoflow)), keyed by a local past the locals of the
  /// body. It is not a place of the body: no place query may be made on it.
  pub(crate) fn hidden(local: Local) -> Self {
    NormPlace(Place::from(local))
  }

  /// The base local of the place.
  pub fn local(self) -> Local {
    self.0.local
  }

  /// The (normalized) projection of the place.
  pub fn projection(self) -> &'tcx [PlaceElem<'tcx>] {
    self.0.projection
  }

  /// The type of the place in `body`, with all regions erased (including those of
  /// the base local's type, which normalization does not touch).
  pub fn ty(self, body: &Body<'tcx>, tcx: TyCtxt<'tcx>) -> ErasedTy<'tcx> {
    ErasedTy::new(tcx, self.0.ty(body.local_decls(), tcx).ty)
  }
}

/// How often one of the [`PlaceInfo`] caches was queried, and how often it had to
/// compute the answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
  /// Number of queries.
  pub lookups: usize,
  /// Number of queries whose answer was not cached yet.
  pub misses: usize,
}

#[derive(Default)]
struct CacheCounter {
  lookups: Cell<usize>,
  misses: Cell<usize>,
}

impl CacheCounter {
  fn lookup(&self) {
    self.lookups.set(self.lookups.get() + 1);
  }

  fn miss(&self) {
    self.misses.set(self.misses.get() + 1);
  }

  fn stats(&self) -> CacheStats {
    CacheStats {
      lookups: self.lookups.get(),
      misses: self.misses.get(),
    }
  }
}

/// Counters of the place queries of a [`PlaceInfo`], see [`PlaceInfo::cache_stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaceCacheStats {
  /// [`PlaceInfo::normalize`].
  pub normalize: CacheStats,
  /// [`PlaceInfo::aliases`].
  pub aliases: CacheStats,
  /// [`PlaceInfo::conflicts`].
  pub conflicts: CacheStats,
  /// [`PlaceInfo::reachable_values`].
  pub reachable: CacheStats,
  /// [`PlaceInfo::children`] (not cached: every lookup is a miss).
  pub children: CacheStats,
}

impl PlaceCacheStats {
  /// The counters as `(name, value)` pairs, in a fixed order.
  pub fn counters(&self) -> Vec<(&'static str, usize)> {
    vec![
      ("normalize.lookups", self.normalize.lookups),
      ("normalize.misses", self.normalize.misses),
      ("aliases.lookups", self.aliases.lookups),
      ("aliases.misses", self.aliases.misses),
      ("conflicts.lookups", self.conflicts.lookups),
      ("conflicts.misses", self.conflicts.misses),
      ("reachable.lookups", self.reachable.lookups),
      ("reachable.misses", self.reachable.misses),
      ("children.computed", self.children.misses),
    ]
  }
}

#[derive(Default)]
struct PlaceCacheCounters {
  normalize: CacheCounter,
  aliases: CacheCounter,
  conflicts: CacheCounter,
  reachable: CacheCounter,
  children: CacheCounter,
}

/// Utilities for analyzing places: children, aliases, etc.
pub struct PlaceInfo<'a, 'tcx> {
  pub(crate) tcx: TyCtxt<'tcx>,
  pub(crate) body: &'a Body<'tcx>,
  pub(crate) def_id: DefId,
  location_domain: Rc<LocationOrArgDomain>,
  mode: EvalMode,

  // Core computed data structure
  aliases: Aliases<'a, 'tcx>,

  // Caching for derived analysis
  normalized_cache: CopyCache<Place<'tcx>, NormPlace<'tcx>>,
  aliases_cache: Cache<NormPlace<'tcx>, PlaceSet<'tcx>>,
  conflicts_cache: Cache<Place<'tcx>, PlaceSet<'tcx>>,
  reachable_cache: Cache<(Place<'tcx>, Mutability), PlaceSet<'tcx>>,
  counters: PlaceCacheCounters,
}

impl<'a, 'tcx> PlaceInfo<'a, 'tcx> {
  fn build_location_arg_domain(body: &Body) -> Rc<LocationOrArgDomain> {
    let all_locations = body.all_locations().map(LocationOrArg::Location);
    let all_locals = body.args_iter().map(LocationOrArg::Arg);
    let domain = all_locations.chain(all_locals).collect::<Vec<_>>();
    Rc::new(LocationOrArgDomain::from_iter(domain))
  }

  /// Computes all the metadata about places used within the infoflow analysis,
  /// with the ambient [`EvalMode`] (see [`EvalMode::from_ambient`]).
  ///
  /// The mode is read once, here, and not when the returned value is queried.
  pub fn build(
    tcx: TyCtxt<'tcx>,
    def_id: DefId,
    body_with_facts: &'a BodyWithBorrowckFacts<'tcx>,
  ) -> Self {
    Self::build_with_mode(tcx, def_id, body_with_facts, EvalMode::from_ambient())
  }

  /// Computes all the metadata about places used within the infoflow analysis,
  /// with an explicit [`EvalMode`].
  pub fn build_with_mode(
    tcx: TyCtxt<'tcx>,
    def_id: DefId,
    body_with_facts: &'a BodyWithBorrowckFacts<'tcx>,
    mode: EvalMode,
  ) -> Self {
    block_timer!("aliases");
    let body = &body_with_facts.body;
    let location_domain = Self::build_location_arg_domain(body);
    let aliases =
      Aliases::build_with_mode(tcx, def_id, body_with_facts, mode.pointer_mode);

    PlaceInfo {
      aliases,
      tcx,
      body,
      def_id,
      location_domain,
      mode,
      aliases_cache: Cache::default(),
      normalized_cache: CopyCache::default(),
      conflicts_cache: Cache::default(),
      reachable_cache: Cache::default(),
      counters: PlaceCacheCounters::default(),
    }
  }

  /// How often the place queries were made and computed so far.
  pub fn cache_stats(&self) -> PlaceCacheStats {
    let c = &self.counters;
    PlaceCacheStats {
      normalize: c.normalize.stats(),
      aliases: c.aliases.stats(),
      conflicts: c.conflicts.stats(),
      reachable: c.reachable.stats(),
      children: c.children.stats(),
    }
  }

  /// The [`EvalMode`] this analysis was built with.
  pub fn mode(&self) -> EvalMode {
    self.mode
  }

  /// Normalizes a place via [`PlaceExt::normalize`] (cached).
  ///
  /// This is the only way to construct a [`NormPlace`], the row key of a
  /// [`FlowDomain`](crate::infoflow::FlowDomain).
  /// See the `PlaceExt` documentation for details on how normalization works.
  pub fn normalize(&self, place: Place<'tcx>) -> NormPlace<'tcx> {
    self.counters.normalize.lookup();
    self.normalized_cache.get(&place, |place| {
      self.counters.normalize.miss();
      NormPlace(place.normalize(self.tcx, self.def_id))
    })
  }

  /// Returns the [children](Self::children) of a normalized place, as row keys.
  ///
  /// The children are computed from the normalized place and are not renormalized.
  pub(crate) fn norm_children(
    &self,
    place: NormPlace<'tcx>,
  ) -> impl Iterator<Item = NormPlace<'tcx>> + use<'tcx> {
    self.children(place.0).into_iter().map(NormPlace)
  }

  /// Computes the aliases of a place (cached).
  ///
  /// For example, if `x = &y`, then `*x` aliases `y`.
  /// Note that an alias is NOT guaranteed to be of the same type as `place`!
  pub fn aliases(&self, place: Place<'tcx>) -> &PlaceSet<'tcx> {
    // note: important that aliases are computed on the unnormalized place
    // which contains region information
    self.counters.aliases.lookup();
    self.aliases_cache.get(&self.normalize(place), move |_| {
      self.counters.aliases.miss();
      self.aliases.aliases(place)
    })
  }

  /// Returns all reachable fields of `place` without going through references.
  ///
  /// For example, if `x = (0, 1)` then `children(x) = {x, x.0, x.1}`.
  pub fn children(&self, place: Place<'tcx>) -> PlaceSet<'tcx> {
    self.counters.children.lookup();
    self.counters.children.miss();
    PlaceSet::from_iter(place.interior_places(self.tcx, self.body, self.def_id))
  }

  /// Returns all places that *directly* conflict with `place`, i.e. that a mutation to `place`
  /// would also be a mutation to the conflicting place.
  ///
  /// For example, if `x = ((0, 1), 2)` then `conflicts(x.0) = {x, x.0, x.0.0, x.0.1}`, but not `x.1`.
  ///
  /// For indirect places, this function follows conflicting parents up until a reference point.
  /// So if `x = (0, &(box 1, 2))` then conflicts(*(*(x.1).0)) = {*(*(x.1).0), *(x.1).0, *(x.1)}
  pub fn conflicts(&self, place: Place<'tcx>) -> &PlaceSet<'tcx> {
    self.counters.conflicts.lookup();
    self.conflicts_cache.get(&place, |place| {
      self.counters.conflicts.miss();
      self.compute_conflicts(place)
    })
  }

  /// Computes [`conflicts`](Self::conflicts) without caching the result, for places
  /// that are queried once (e.g. when seeding the rows of the arguments).
  pub(crate) fn compute_conflicts(&self, place: Place<'tcx>) -> PlaceSet<'tcx> {
    let children = self.children(place);
    // The fields of a union overlap: every place under a union containing `place`
    // conflicts with it. (`children` does not enter unions.)
    let union_members = self.conflict_parents(place).flat_map(|parent| {
      let TyKind::Adt(adt_def, args) =
        parent.ty(self.body.local_decls(), self.tcx).ty.kind()
      else {
        return Vec::new();
      };
      if !adt_def.is_union() {
        return Vec::new();
      }
      let fields = adt_def.non_enum_variant().fields.iter_enumerated();
      fields
        .flat_map(|(field, def)| {
          self.children(
            self
              .tcx
              .mk_place_field(parent, field, def.ty(self.tcx, args)),
          )
        })
        .collect()
    });
    children
      .into_iter()
      .chain(self.conflict_parents(place))
      .chain(union_members)
      .collect()
  }

  fn conflict_parents(
    &self,
    place: Place<'tcx>,
  ) -> impl Iterator<Item = Place<'tcx>> + '_ {
    place
      .projection
      .iter()
      .enumerate()
      .map(move |(i, elem)| {
        let place = PlaceRef {
          local: place.local,
          projection: &place.projection[.. i],
        };
        (place, elem)
      })
      .take_while(|(place, elem)| {
        place.ty(self.body.local_decls(), self.tcx).ty.is_box()
          || !matches!(elem, PlaceElem::Deref)
      })
      .map(|(place_ref, _)| Place::from_ref(place_ref, self.tcx))
  }

  /// Construct argument seeds using independently rooted child traversals.
  ///
  /// Every template starts with an empty type stack, exactly like `children`.
  /// Templates are keyed by the full, unnormalized type within this body/DefId;
  /// they are never extracted from a parent's context-dependent traversal.
  pub(crate) fn seed_rows(&'a self) -> FxHashSet<(NormPlace<'tcx>, LocationOrArgIndex)> {
    block_timer!("seed rows");
    // Bound temporary template storage; a miss beyond the cap still computes the
    // same children. Projection slices refer to rustc's existing interned places.
    const MAX_TEMPLATE_PATHS: usize = 65_536;
    let mut templates = FxHashMap::<Ty<'tcx>, Rc<[&'tcx [PlaceElem<'tcx>]]>>::default();
    let mut retained_paths = 0;
    let mut rows = FxHashSet::default();
    for (arg, location) in self.all_args() {
      let ty = arg.ty(self.body.local_decls(), self.tcx).ty;
      let paths = match templates.get(&ty) {
        Some(paths) => Rc::clone(paths),
        None => {
          let paths = self
            .children(arg)
            .into_iter()
            .map(|child| {
              debug_assert_eq!(child.local, arg.local);
              debug_assert!(child.projection.starts_with(arg.projection));
              &child.projection[arg.projection.len() ..]
            })
            .collect::<Rc<[_]>>();
          if paths.len() <= MAX_TEMPLATE_PATHS - retained_paths {
            retained_paths += paths.len();
            templates.insert(ty, Rc::clone(&paths));
          }
          paths
        }
      };
      for path in paths.iter() {
        rows.insert((self.normalize(arg.project_deeper(path, self.tcx)), location));
      }
      for parent in self.conflict_parents(arg) {
        rows.insert((self.normalize(parent), location));
      }
    }
    #[cfg(feature = "shadow-eager")]
    {
      let reference = self
        .all_args()
        .flat_map(|(arg, location)| {
          self
            .compute_conflicts(arg)
            .into_iter()
            .map(move |place| (self.normalize(place), location))
        })
        .collect::<FxHashSet<_>>();
      assert_eq!(
        rows, reference,
        "seed rows differ from independently enumerated conflicts"
      );
    }
    rows
  }

  /// Returns all [direct](PlaceExt::is_direct) places that are reachable from `place`
  /// and can be used at the provided level of [`Mutability`] (cached).
  ///
  /// For example, if `x = 0` and `y = (0, &x)`, then `reachable_values(y, Mutability::Not)`
  /// is `{y, x}`. With `Mutability::Mut`, then the output is `{y}` (no `x`).
  pub fn reachable_values(
    &self,
    place: Place<'tcx>,
    mutability: Mutability,
  ) -> &PlaceSet<'tcx> {
    self.counters.reachable.lookup();
    self.reachable_cache.get(&(place, mutability), |_| {
      self.counters.reachable.miss();
      let ty = place.ty(self.body.local_decls(), self.tcx).ty;
      let loans = self.collect_loans(ty, mutability);
      loans
        .into_iter()
        .chain([place])
        .filter(|place| {
          if let Some((place, _)) = place.refs_in_projection(self.body, self.tcx).last() {
            let ty = place.ty(self.body.local_decls(), self.tcx).ty;
            if ty.is_box() || ty.is_raw_ptr() {
              return true;
            }
          }
          place.is_direct(self.body, self.tcx)
        })
        .collect()
    })
  }

  fn collect_loans(&self, ty: Ty<'tcx>, mutability: Mutability) -> PlaceSet<'tcx> {
    let mut collector = LoanCollector {
      aliases: &self.aliases,
      unknown_region: Region::new_var(self.tcx, UNKNOWN_REGION),
      target_mutability: mutability,
      mutability_mode: self.mode.mutability_mode,
      stack: vec![],
      loans: PlaceSet::default(),
    };
    let _ = collector.visit_ty(ty);
    collector.loans
  }

  /// Returns all [direct](PlaceExt::is_direct) places reachable from arguments
  /// to the current body.
  pub fn all_args(
    &'a self,
  ) -> impl Iterator<Item = (Place<'tcx>, LocationOrArgIndex)> + 'a {
    self.body.args_iter().flat_map(|local| {
      let location = local.to_index(&self.location_domain);
      let place = Place::from_local(local, self.tcx);
      let ptrs = place
        .interior_pointers(self.tcx, self.body, self.def_id)
        .into_values()
        .flat_map(|ptrs| {
          ptrs
            .into_iter()
            .filter(|(ptr, _)| ptr.projection.len() <= MAX_ARG_POINTER_DEPTH)
            .map(|(ptr, _)| self.tcx.mk_place_deref(ptr))
        });
      ptrs
        .chain([place])
        .flat_map(|place| place.interior_places(self.tcx, self.body, self.def_id))
        .map(move |place| (place, location))
    })
  }

  /// Whether some argument holds a pointer nested more deeply than
  /// [`MAX_ARG_POINTER_DEPTH`], i.e. whether the analysis ignores loans that the
  /// arguments of this body may hold.
  pub(crate) fn arg_pointers_truncated(&self) -> bool {
    self.body.args_iter().any(|local| {
      Place::from_local(local, self.tcx)
        .interior_pointers(self.tcx, self.body, self.def_id)
        .into_values()
        .flatten()
        .any(|(ptr, _)| ptr.projection.len() > MAX_ARG_POINTER_DEPTH)
    })
  }

  /// Returns the [`LocationOrArgDomain`] for the current body.
  pub fn location_domain(&self) -> &Rc<LocationOrArgDomain> {
    &self.location_domain
  }
}

// TODO: this visitor shares some structure with the PlaceCollector in mir utils.
// Can we consolidate these?
struct LoanCollector<'a, 'tcx> {
  aliases: &'a Aliases<'a, 'tcx>,
  unknown_region: Region<'tcx>,
  target_mutability: Mutability,
  mutability_mode: MutabilityMode,
  stack: Vec<Mutability>,
  loans: PlaceSet<'tcx>,
}

impl<'tcx> TypeVisitor<TyCtxt<'tcx>> for LoanCollector<'_, 'tcx> {
  type Result = ControlFlow<()>;

  fn visit_ty(&mut self, ty: Ty<'tcx>) -> Self::Result {
    match ty.kind() {
      TyKind::Ref(_, _, mutability) => {
        self.stack.push(*mutability);
        ty.super_visit_with(self)?;
        self.stack.pop();
      }
      _ if ty.is_box() || ty.is_raw_ptr() => {
        self.visit_region(self.unknown_region)?;
        ty.super_visit_with(self)?;
      }
      _ => ty.super_visit_with(self)?,
    };

    ControlFlow::Continue(())
  }

  fn visit_region(&mut self, region: Region<'tcx>) -> Self::Result {
    let region = match region.kind() {
      RegionKind::ReVar(region) => region,
      RegionKind::ReStatic => RegionVid::from_usize(0),
      // TODO: do we need to handle bound regions?
      // e.g. shows up with closures, for<'a> ...
      RegionKind::ReErased | RegionKind::ReBound(..) => {
        return ControlFlow::Continue(());
      }
      _ => unreachable!("{region:?}"),
    };
    if let Some(loans) = self.aliases.loans.get(&region) {
      let under_immut_ref = self.stack.contains(&Mutability::Not);
      self
        .loans
        .extend(loans.iter().filter_map(
          |(place, mutability)| match self.mutability_mode {
            MutabilityMode::IgnoreMut => Some(place),
            MutabilityMode::DistinguishMut => {
              let loan_mutability = if under_immut_ref {
                Mutability::Not
              } else {
                *mutability
              };
              self
                .target_mutability
                .is_permissive_as(loan_mutability)
                .then_some(place)
            }
          },
        ))
    }

    ControlFlow::Continue(())
  }
}

#[cfg(test)]
mod test {
  use rustc_middle::ty::TypeVisitableExt;
  use rustc_utils::{
    hashset,
    test_utils::{Placer, compare_sets},
  };

  use super::*;
  use crate::test_utils;

  fn placeinfo_harness(
    input: &str,
    f: impl for<'tcx> FnOnce(TyCtxt<'tcx>, &Body<'tcx>, PlaceInfo<'_, 'tcx>) + Send,
  ) {
    test_utils::compile_body(input, |tcx, body_id, body_with_facts| {
      let body = &body_with_facts.body;
      let def_id = tcx.hir_body_owner_def_id(body_id);
      let place_info = PlaceInfo::build(tcx, def_id.to_def_id(), body_with_facts);

      f(tcx, body, place_info)
    });
  }

  #[test]
  fn seed_templates_preserve_independent_type_stack_cutoffs() {
    let input = r#"
struct Node { value: u32, next: Option<Box<Node>> }
enum Payload<'a> { Array([u32; 3]), Borrowed(&'a mut Node), Nested((&'a u8, u16)) }
fn f(a: Node, b: Node, x: &mut Payload<'_>, y: &&Node) -> u32 {
  a.value + b.value + y.value
}
"#;
    placeinfo_harness(input, |tcx, body, place_info| {
      let reference = place_info
        .all_args()
        .flat_map(|(arg, location)| {
          let place_info = &place_info;
          place_info
            .compute_conflicts(arg)
            .into_iter()
            .map(move |place| (place_info.normalize(place), location))
        })
        .collect::<FxHashSet<_>>();
      assert_eq!(place_info.seed_rows(), reference);

      // This fixture must exercise the exact trap that makes parent-derived
      // subtree reuse invalid: restarting at a child can visit deeper places.
      let root = Place::from_local(body.args_iter().next().unwrap(), tcx);
      let outer = place_info.children(root);
      assert!(outer.iter().any(|child| {
        place_info
          .children(*child)
          .iter()
          .any(|nested| !outer.contains(nested))
      }));
    });
  }

  #[test]
  fn test_placeinfo_basic() {
    let input = r#"
fn main() {
  let a = 0;
  let mut b = 1;
  let c = ((0, &a), &mut b);
  let d = 0;
  let e = &d;
  let f = &e;
}
    "#;
    placeinfo_harness(input, |tcx, body, place_info| {
      let p = Placer::new(tcx, body);
      let c = p.local("c");
      compare_sets(place_info.children(c.mk()), hashset! {
        c.mk(),
        c.field(0).mk(),
        c.field(0).field(0).mk(),
        c.field(0).field(1).mk(),
        c.field(1).mk(),
      });

      compare_sets(place_info.conflicts(c.field(0).mk()), &hashset! {
        c.mk(),
        c.field(0).mk(),
        c.field(0).field(0).mk(),
        c.field(0).field(1).mk(),
        // c.field(1) not part of the set
      });

      // a and b are reachable at immut-level
      compare_sets(
        place_info.reachable_values(c.mk(), Mutability::Not),
        &hashset! {
          c.mk(),
          p.local("a").mk(),
          p.local("b").mk()
        },
      );

      // only b is reachable at mut-level
      compare_sets(
        place_info.reachable_values(c.mk(), Mutability::Mut),
        &hashset! {
          c.mk(),
          p.local("b").mk()
        },
      );

      // handles transitive references
      compare_sets(
        place_info.reachable_values(p.local("f").mk(), Mutability::Not),
        &hashset! {
          p.local("f").mk(),
          p.local("e").mk(),
          p.local("d").mk()
        },
      )
    });
  }

  #[test]
  fn test_normalize_collapses_indices() {
    let input = r#"
fn main() {
  let n = 0;
  let x = [&n, &n];
  let i = 0;
  let j = 1;
  let a = x[i];
  let b = x[j];
}
    "#;
    placeinfo_harness(input, |tcx, body, place_info| {
      let p = Placer::new(tcx, body);
      let x = p.local("x");
      let i = p.local("i").mk().local.as_usize();
      let j = p.local("j").mk().local.as_usize();

      let xi = place_info.normalize(x.index(i).mk());
      let xj = place_info.normalize(x.index(j).mk());
      assert_eq!(xi, xj);
      assert_eq!(xi.local(), x.mk().local);
      assert_eq!(xi.projection(), &[ProjectionElem::Index(
        Local::from_usize(0)
      )]);
      assert_ne!(xi, place_info.normalize(x.mk()));

      // The element type comes from the local's type, whose regions normalization
      // does not erase: `ty` must erase them.
      let raw_ty = x.index(i).mk().ty(body.local_decls(), tcx).ty;
      assert!(raw_ty.has_infer_regions());
      let ty = xi.ty(body, tcx).ty();
      assert!(!ty.has_infer_regions());
      assert_eq!(ty, tcx.erase_and_anonymize_regions(raw_ty));
    });
  }

  #[test]
  fn test_arg_pointers_truncated() {
    let shallow = "fn f(x: &&&i32, y: (&i32, &mut &i32)) {}";
    placeinfo_harness(shallow, |_, _, place_info| {
      assert!(!place_info.arg_pointers_truncated());
    });

    let deep = "fn f(x: &&&&i32) {}";
    placeinfo_harness(deep, |_, _, place_info| {
      assert!(place_info.arg_pointers_truncated());
    });
  }
}
