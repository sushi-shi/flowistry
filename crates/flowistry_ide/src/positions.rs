//! Conversions between spans and character positions.
//!
//! `rustc_utils`' `CharRange::from_span` and `CharRange::to_span` first build, for
//! every file they touch, two hash maps with an entry for each character of the file:
//! about 3% of a typical focus request, and more in large files. rustc's source map
//! answers the same questions for one position at a time from its table of line
//! starts. These functions give the same results.

use anyhow::{Context, Result, bail, ensure};
use rustc_middle::ty::TyCtxt;
use rustc_span::{
  BytePos, FileName, RemapPathScopeComponents, Span, source_map::SourceMap,
};
use rustc_utils::source_map::{
  filename::Filename,
  range::{CharPos, CharRange, ToSpan},
};

/// The characters of `span`, like `CharRange::from_span`.
pub fn char_range(span: Span, source_map: &SourceMap) -> Result<CharRange> {
  let (lo, hi) = (
    source_map.lookup_char_pos(span.lo()),
    source_map.lookup_char_pos(span.hi()),
  );
  let FileName::Real(name) = &lo.file.name else {
    bail!("unsupported file {:?}", lo.file.name);
  };
  ensure!(lo.file.name == hi.file.name, "{span:?} crosses files");
  ensure!(
    source_map.ensure_source_file_source_present(&lo.file),
    "could not load source for file {:?}",
    lo.file.name
  );
  let position = |loc: &rustc_span::Loc| CharPos {
    line: loc.line - 1,
    column: loc.col.0,
  };
  Ok(CharRange {
    start: position(&lo),
    end: position(&hi),
    filename: Filename::intern(name.path(RemapPathScopeComponents::DOCUMENTATION)),
  })
}

/// A character range that converts to a span like `CharRange::to_span`, erring on
/// positions outside the file instead of panicking.
pub struct Chars(pub CharRange);

impl ToSpan for Chars {
  fn to_span(&self, tcx: TyCtxt) -> Result<Span> {
    let file = self.0.filename.find_source_file(tcx.sess.source_map())?;
    let byte = |pos: CharPos| -> Result<BytePos> {
      ensure!(
        pos.line < file.count_lines(),
        "line {} is outside the file",
        pos.line
      );
      let line = file.get_line(pos.line).context("missing source")?;
      let offset = match line.char_indices().nth(pos.column) {
        Some((offset, _)) => offset,
        None if line.chars().count() == pos.column => line.len(),
        None => bail!("column {} is outside line {}", pos.column, pos.line),
      };
      Ok(file.line_bounds(pos.line).start + BytePos(u32::try_from(offset)?))
    };
    Ok(Span::with_root_ctxt(byte(self.0.start)?, byte(self.0.end)?))
  }
}
