# Shared result index and publication

Continuation feature 7 extends the existing compiler-validated focus cache and
snapshot response cache. It does not add another semantic cache or another set
of dependency fingerprints. The response entry now includes compiler-derived
body identities, package/target/configuration/mode provenance, an input revision,
and the generation that published it. Existing focus/file-focus wire formats
remain unchanged unless the new envelope is explicitly requested.

## Reading and canceling

Use the same working directory, cache directory and launcher configuration as
the analysis request:

```sh
cargo flowistry --context-mode Recurse file-focus /project/src/lib.rs 12 4
cargo flowistry --context-mode Recurse result-index /project/src/lib.rs
cargo flowistry --context-mode Recurse cancel-results /project/src/lib.rs
```

`result-index` performs no Cargo metadata/build work and never starts rustc.
A `current` result lists each body's
compiler-derived identity, display name, current range and availability, together
with the package, crate types, target, configuration, mode and validation
provenance. Its input snapshot must still match. Otherwise it reports `miss`.
Coincident, ambiguous body ranges are not advertised as available. The existing
file-focus command retrieves an available body without compiling.

`cancel-results` invalidates active publication tickets for that file and launcher
configuration. It does not terminate compiler processes; the worker/editor owner
must also stop its process group when it wants to reclaim running resources.
Completed results may be reused after current-input validation. Cancellation
does not erase valid semantic cache entries.

## Versioned delivery

Set `FLOWISTRY_RESULT_PROTOCOL=1` to receive a JSON envelope instead of the raw
base64 response. It carries `schema`, `status`, `revision`, `generation` and
`output`; successful `output` contains the unchanged encoded file-focus response.

| Status | Meaning |
|---|---|
| `current` | The response passed input and publication checks; revision and generation are present. |
| `superseded` | Inputs changed or the publication ticket was invalidated; no output is delivered, exit status 75. |
| `uncached` | Snapshot validation is unavailable or disabled; output is present without a claim of current indexed publication. |
| `error` | The compiler/request failed; normal failure status and stderr are preserved. |

A consumer must compare the revision and generation with its current request
before applying a completion. The envelope does not replace the editor's buffer
change-tick check: saved files and unsaved buffers are different states. The
project and Neovim features will use this protocol and add their delivery tests.

Requests for the same input revision share a generation and can fill different
bodies concurrently. A changed revision receives a fresh random generation.
Publication checks the ticket under a filesystem lock and merges already
completed bodies from that revision. Eviction, restart and corrupt generation
state cannot recreate an older ticket. The lock is not held during compilation
or solving. It covers generation changes, atomic replacement and eviction.

## Input validation

Warm reads retain the existing content-based snapshot check, so an unchanged save
can replay. In-flight writers additionally check source stamps: an intervening
edit followed by an undo must not hide bytes that a compiler could have read.
Cargo may regenerate its target outputs while validating the watched build
inputs. Compiler source files are checked against rustc's hash of the original,
unnormalized bytes, then checked again before publication. This follows the
[pinned compiler's source-hash definition](https://github.com/rust-lang/rust/blob/f53b654a8882fd5fc036c4ca7a4ff41ce32497a6/compiler/rustc_span/src/lib.rs#L2118).

New external build/dependency inputs sometimes become visible only after Cargo
has run. If neither the preflight snapshot nor rustc's source hash attests them,
the first request retains only their watch list. A subsequent compiler-validated
request must succeed before a response is published. This deliberately adds a
validation request for that case. An external-input race regression reproduces
incorrect publication in the earlier prototype and rejects it in this version.

Body IDs contain stable compiler hashes and names, not process-local rustc IDs.
Every result still requires the complete matching input snapshot; a body move
cannot make an old range current. The semantic cache can relocate the result only
after its normal compiler validation, at which point the index records new ranges.

## Storage and recovery

`FLOWISTRY_CACHE_MAX_BYTES` sets the combined semantic/response/generation storage
limit in bytes; the default is 256 MiB. Individual result entries remain capped
at 32 MiB and generation metadata at 1 MiB/256 scopes. All current writers share
one lock, so concurrent writes cannot independently exceed the combined budget.
Old entries are evicted before replacement. Temporary atomic writes can briefly
require an additional entry's worth of space; incomplete writes are never read
as completed entries. Corrupt entries fall back to analysis.

Compiler input sidecars use an immediately unlinked file held by the parent and
accessed through procfs. Killing the request closes it instead of leaving a named
file behind. The sidecar is bounded to 32 MiB per active request. Completed,
compiler-validated semantic body entries survive an interrupted response
publication and can be reused after restart.

## Validation

Frozen build `235760027` passes all 16 publication/index cases in both modes,
all 84 current edit/concurrency/real-project cases, 51 semantic-cache cases plus
compile-error rejection, and 32 snapshot cases (17 hits invoke no compiler).
The workspace suite passes; all IDE targets were rerun after the final private
storage changes. All 48 Python oracle regressions pass.

The publication suite covers concurrent bodies, supersession, cancellation,
generation corruption, relocation, newly discovered external-input races,
restart after an interrupted publication and eviction under a configured
128 KiB limit. Maximum measured committed file contents were 121,774 bytes in
SigOnly and 121,127 in Recurse; each retained ten of twenty semantic entries.
Filesystem allocation overhead and active temporary writes are separate from
that content budget. A killed publisher leaves no named input sidecar, and its
completed semantic result is reused without another solve after restart.

The [committed report](measurements/versioned-result-index.json) pins the build,
harnesses and detailed local evidence. Runs overlapped corpus work, so their
timings are not quiet performance claims. Full final corpus/cache gates and
project/editor acceptance requirements remain in
[continuation-plan.md](continuation-plan.md).
