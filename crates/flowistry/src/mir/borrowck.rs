//! Access to borrow-checked bodies and their borrowck facts.

use rustc_borrowck::consumers::BodyWithBorrowckFacts;
use rustc_hir::def_id::LocalDefId;
use rustc_middle::ty::TyCtxt;
use rustc_utils::mir::borrowck_facts;

/// The body of `def_id` with its borrowck facts, like
/// [`borrowck_facts::get_body_with_borrowck_facts`], which it calls.
///
/// rustc borrow-checks a closure (or another typeck child) together with its typeck
/// root, and cannot borrow-check it alone. Unless the whole crate was checked in
/// advance, the root of a child may not be checked yet (the IDE runs its analyses on
/// demand, right after expansion): check the root first, which also stores the bodies
/// of its children.
pub fn body_with_borrowck_facts<'tcx>(
  tcx: TyCtxt<'tcx>,
  def_id: LocalDefId,
) -> &'tcx BodyWithBorrowckFacts<'tcx> {
  let root = tcx.typeck_root_def_id(def_id.to_def_id()).expect_local();
  if root != def_id {
    borrowck_facts::get_body_with_borrowck_facts(tcx, root);
  }
  borrowck_facts::get_body_with_borrowck_facts(tcx, def_id)
}
