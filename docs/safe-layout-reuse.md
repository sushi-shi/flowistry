# Safe layout reuse before compiler startup

The snapshot cache can now relocate eligible whitespace edits without invoking
Cargo, rustc or the Flowistry solver. It reuses the existing token-relative range
anchors, shared bounded store and revision/generation publication protocol.
Compiler validation remains the fallback whenever the proof is unavailable.
This implements step 12 above #58; final-stack corpus and performance gates
remain separate requirements.

## Eligibility and proof

A successful compiler result records an optional proof alongside its normal
complete input snapshot. This first supported subset requires all of the
following:

- Cargo's selected package has no dependencies, build script or procedural-macro
  target. The compiler also attests that every linked external crate is from its
  sysroot; a package's implicitly linked sibling library is therefore excluded.
- All compiler-parsed local sources are ordinary Rust files within the watched
  package roots. Imported sysroot source files remain watched inputs, but do not
  count as local source for this lexical proof.
- Local source contains no macro invocation/definition punctuation, attributes,
  doc comments, external linkage, or source-location access. The conservative
  scanner rejects `!`, `#`, `extern`, `Location` and `caller_location`, including
  raw identifiers. It also rejects harmless uses of those spellings; they fall
  back to the compiler. Ordinary comments remain exact tokens.
- There are no compiler-injected crate attributes, Cargo configuration files,
  custom compiler wrappers or Rust flag overrides. An explicit compiler override
  must resolve to the same compiler as `rustc` on PATH.
- Each source is at most 1 MiB, with at most 8 MiB of retained source text. CRLF,
  BOM and whitespace other than spaces, tabs and newlines fall back rather than
  assuming rustc's source normalization matches raw disk coordinates.

This is deliberately not a general claim that identical tokens make arbitrary
Rust formatting safe. Dependencies, macros and build scripts can observe source
bytes or locations, and stable `Location::caller()` can observe positions even
without a macro in the edited body. The pinned compiler/standard library is the
trusted language implementation; arbitrary external crates are not admitted by
this first proof.

On an edit, the complete snapshot must retain exactly the same input membership.
Every changed digest must correspond to one of the compiler-attested local Rust
sources. Every other manifest, config, executable, dependency, generated and
external input remains unchanged. Old proof text and newly read source text are
checked against their respective snapshot digests.

The lexer must produce the same token kinds and exact spellings, including
comments and literals. Punctuation jointness must also match: `> >` and `>>`
cannot be treated as interchangeable lexer tokens. Some harmless rustfmt
punctuation changes therefore fall back. No comment is newly ignored.

All range tables, container ranges, file body ranges and result-index body ranges
are relocated through the existing character-aware token anchors. Numeric range
indices—including maybe-slices, comment exclusions and parameter aliases—stay
intact. Every cached response must relocate successfully, or the request falls
back. The shared publication lock is held, and a second full snapshot checks
stamps as well as contents before accepting the result; source/input changes
and ABA writes during relocation reject publication.

A successful relocation updates the stored source proof and snapshot and obtains
a ticket for the new revision. The first returned response identifies
`cache.validation = "layout"`; a later unchanged read identifies `"snapshot"`.
`FLOWISTRY_CACHE=off`, `refresh` and summary verification retain compiler behavior.
Debug logs explain eligibility failures and fallback reasons; audit logs record
`audit layout-hit`. No new editor toggle or independent cache is introduced.
The response-cache key version advances to 8; old response entries safely miss.

## Validation

The runtime candidate is frozen at `e25fe7ecb` in
`target/continuation-validation/layout-reuse-v2`. Exact checksums and raw-report
references are retained in [the report](measurements/safe-layout-reuse.json).

- 46 process cases pass across both modes, including library/binary targets,
  Unicode, closures, indexed maybe-slices, single/multiple source edits and
  rustfmt. Eligible cases each invoke zero compilers and zero solvers, publish a
  new revision/generation, and match an independently frozen #57 cache-off
  backend. Comment/semantic edits, source-sensitive constructs, dependencies,
  configuration, wrappers, new inputs, corruption and refresh invoke the
  compiler and match the same independent oracle.
- All 16 existing versioned-publication cases and all 84 existing edit/concurrency
  cases pass, including curated real-project edits and cancellation/recovery.
- All 168 workspace tests pass, including punctuation/Unicode checks and an
  input change after the initial proof snapshot that must reject publication.
- The real headless Neovim rustfmt-save test passes 18 assertions across both
  modes. Layout-reused highlights exactly match fresh compiler extmarks after
  resetting editor state and disabling reuse. The test is included in the root
  paired-package cache check.

The saved editor times in the live report are observations under host load, not
quiet latency distributions or proof of the provisional 300 ms performance gate.
Most dependency-heavy projects are currently ineligible and continue using
compiler-validated semantic reuse. Broader eligibility requires a stronger proof,
not removal of these checks. The final full corpus, package CI and remaining
performance/resource acceptance gates must still pass at the final stack.
