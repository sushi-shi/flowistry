//! Syntax metadata for the editor; this never changes the dataflow analysis.
use rustc_hir::{BodyId, PatKind};
use rustc_middle::ty::TyCtxt;
use rustc_span::{BytePos, Span};

use super::{ParameterAlias, RangeTable};

pub(super) fn collect(
  tcx: TyCtxt<'_>,
  body_id: BodyId,
  table: &mut RangeTable,
) -> (Vec<u32>, Vec<ParameterAlias>) {
  let source_map = tcx.sess.source_map();
  let index = |table: &mut RangeTable, span: Span| {
    crate::positions::char_range(span, source_map)
      .ok()
      .map(|range| table.index(range))
  };
  let mut aliases = Vec::new();
  for param in tcx.hir_body(body_id).params {
    // A tuple/struct pattern has no single binding corresponding to its type.
    let PatKind::Binding(_, _, ident, None) = param.pat.kind else {
      continue;
    };
    if param.ty_span.is_dummy()
      || param.ty_span.is_empty()
      || param.ty_span.from_expansion()
      || ident.span.from_expansion()
      || param.ty_span.overlaps(ident.span)
    {
      continue;
    }
    if let (Some(range), Some(target)) =
      (index(table, param.ty_span), index(table, ident.span))
    {
      aliases.push(ParameterAlias { range, target });
    }
  }

  let mut comments = Vec::new();
  let span = tcx.hir_span_with_body(tcx.hir_body_owner(body_id));
  if !span.from_expansion() {
    if let Ok(text) = source_map.span_to_snippet(span) {
      let mut offset = 0;
      for token in rustc_lexer::tokenize(&text, rustc_lexer::FrontmatterAllowed::No) {
        if matches!(
          token.kind,
          rustc_lexer::TokenKind::LineComment { .. }
            | rustc_lexer::TokenKind::BlockComment { .. }
        ) {
          let comment = span
            .with_lo(span.lo() + BytePos(offset))
            .with_hi(span.lo() + BytePos(offset + token.len));
          if let Some(range) = index(table, comment) {
            comments.push(range);
          }
        }
        offset += token.len;
      }
    }
  }
  (comments, aliases)
}
