//! Plain call inputs and constructor fields that human-facing slices may trim.
use rustc_abi::FieldIdx;
use rustc_data_structures::fx::FxHashMap;
use rustc_hir::{
  self as hir,
  def_id::LocalDefId,
  intravisit::{self, Visitor},
};
use rustc_middle::ty::{self, TyCtxt, TypeckResults};
use rustc_span::Span;

pub(super) struct SimpleInputs {
  pub calls: Vec<Span>,
  pub fields: FxHashMap<Span, Vec<(FieldIdx, Span)>>,
}

struct Arguments<'tcx> {
  typeck: &'tcx TypeckResults<'tcx>,
  inputs: SimpleInputs,
}

impl Arguments<'_> {
  fn simple(&self, expr: &hir::Expr<'_>) -> bool {
    if expr.span.from_expansion() || !self.typeck.expr_adjustments(expr).is_empty() {
      return false;
    }
    match expr.kind {
      hir::ExprKind::Path(_) | hir::ExprKind::Lit(_) => true,
      hir::ExprKind::Field(base, _) => self.simple(base),
      hir::ExprKind::Index(base, index, _) => {
        matches!(
          self.typeck.expr_ty_adjusted(base).kind(),
          ty::Array(..) | ty::Slice(..)
        ) && self.simple(base)
          && self.simple(index)
      }
      _ => false,
    }
  }
}

impl<'tcx> Visitor<'tcx> for Arguments<'tcx> {
  fn visit_expr(&mut self, expr: &'tcx hir::Expr<'tcx>) {
    if let hir::ExprKind::Call(_, args) = expr.kind {
      for arg in args {
        if self.simple(arg) {
          self.inputs.calls.push(arg.span);
        }
      }
    }
    if let hir::ExprKind::Struct(_, fields, _) = expr.kind {
      if !expr.span.from_expansion() {
        let simple = fields
          .iter()
          .filter(|field| self.simple(field.expr) && !field.span.from_expansion())
          .map(|field| (self.typeck.field_index(field.hir_id), field.span))
          .collect();
        self.inputs.fields.insert(expr.span, simple);
      }
    }
    // Nested bodies are analyzed independently; the default visitor does not
    // enter them. Keep methods, macros and effectful expressions conservative.
    intravisit::walk_expr(self, expr);
  }
}

/// Spans of function-call arguments and struct fields that are plain reads:
/// paths, literals, and field or array/slice index projections of them. Arguments
/// with adjustments (such as the reborrow of a reference argument) or from macro
/// expansions are excluded, as are all method-call arguments.
pub(super) fn collect(tcx: TyCtxt<'_>, def_id: LocalDefId) -> SimpleInputs {
  let mut visitor = Arguments {
    typeck: tcx.typeck(def_id),
    inputs: SimpleInputs {
      calls: Vec::new(),
      fields: FxHashMap::default(),
    },
  };
  visitor.visit_body(tcx.hir_body_owned_by(def_id));
  visitor.inputs
}
