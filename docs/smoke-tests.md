# Real-crate smoke tests

Hand-written test fixtures only cover the code shapes someone thought of.
`scripts/smoke-real-crates.py` runs the Flowistry backend over real, widely used
crates from the local cargo registry at many sampled positions, in both context
modes, and reports panics / ICEs. It can also compare two backend builds (for
example `master` against a branch) position by position.

It is a smoke test, not a correctness test: it answers "does the backend crash
on real code?" and, with `--compare`, "did this change alter any output?". Because
the corpus is locked (below), its per-run timings are also the basis for performance
comparisons.

## The locked corpus

Without `--crate`, the script runs the corpus in `scripts/smoke-corpus/`:

- **Registry crates** (itertools, either, smallvec, indexmap, hashbrown, serde_json,
  regex-syntax, anyhow, bitflags, memchr), each pinned to an exact version and the
  sha256 of its `.crate` archive, with the optional dependencies pruned to build
  offline and the resulting `Cargo.lock`.
- **Applications from git**, each pinned to a commit and analysing one package
  (`package`): just, tokei, alacritty, niri, helix (`helix-term`), bevy (`crates/bevy_ecs`),
  brains (`open/stratum-proxy`) and bosminer (`open/bosminer`, the Braiins OS miner), local_lru, cargo-inspect, biodiff, objdiff (`objdiff-cli`),
  boxxy and sudo-rs. Their own `Cargo.lock` pins dependencies; for repositories without one
  (bevy) a generated lock is stored in the corpus. Optional fields: `root` (the cargo
  workspace's subdirectory, e.g. brains), `submodules` (check out git submodules, e.g. biodiff's
  bundled WFA2), and `env` (build environment, e.g. `CFLAGS=-std=gnu17` for old C code that
  gcc 15 rejects under C23, or `RUSTFLAGS=--cap-lints=warn` for old crates that deny warnings).
- For every entry, the sampled positions (`positions.tsv`), so a change to the sampler
  does not change what is measured.

`--update-corpus` re-resolves it: git entries move to the current head of their `ref`,
locks are regenerated, positions resampled and checksums recorded. `--fetch` downloads
what is not available offline (git checkouts, crates, dependencies); runs are offline
otherwise. `--prepare-only` stops after preparing.

The git applications need native libraries (niri, alacritty), so use the flake's
`smoke` shell: `nix develop github:sushi-shi/flowistry#smoke`. It uses the same pinned
toolchain as the default shell.

Known limitation (upstream `rustc_plugin`): a binary whose crate name differs from its
package name (e.g. `dust`, package `du-dust`) is never recognised as the target crate,
so it cannot be analysed. Binaries are also cached by cargo after one run; the script
touches a binary's root file before each run to force the analysis.

## Running

The script uses only the Python standard library, but needs `cargo` and the
pinned toolchain, so run it inside the dev shell. First build the backend into
a dedicated target directory:

```sh
nix develop github:sushi-shi/flowistry -c env CARGO_TARGET_DIR=target/smoke-worker \
    cargo build --locked -p flowistry_ide

nix develop github:sushi-shi/flowistry#smoke -c python3 scripts/smoke-real-crates.py \
    target/smoke-worker/debug --fetch   # --fetch only needed the first time
```

The positional argument is the directory containing `cargo-flowistry` and
`flowistry-driver`; it is prepended to `PATH` for every run.

Useful options (see `--help` for all of them):

| option | meaning |
| --- | --- |
| `--crate SPEC` | crate to test, repeatable: a corpus entry name, or `NAME` (newest version in the registry), `NAME@VERSION`, or a path to a crate directory. Defaults to the locked corpus. |
| `--positions N` | positions sampled per crate (default 60) |
| `--seed S` | sampling seed (default 0); the same seed always picks the same positions |
| `--modes M1,M2` | context modes (default `SigOnly,Recurse`) |
| `-j N` | crates processed in parallel (default 4) |
| `--compare DIR` | a second backend bin dir; every run is repeated with it and the answers are compared |
| `--json FILE` | write every run record (and, with `--keep-outputs`, the full focus output) |
| `--fresh` | re-copy crates instead of reusing prepared copies |
| `--update-corpus` | re-resolve the locked corpus (see above) |
| `--fetch` | download corpus sources and dependencies not available offline |
| `--prepare-only` | prepare crates (and positions) without running the analysis |
| `--bump` | with `--update-corpus`, move git entries to the current head of their ref |
| `--phases` | record the backend's per-phase timers; adds a timing section (per-phase totals, and with `--compare` the ratio of totals and the geometric mean of per-run ratios). Use release builds. |
| `--repeat N` | run every position N times and keep the fastest, to reduce timing noise |

The exit status is 1 if any run crashed or timed out, or if the two backends
disagree under `--compare`.

### Comparing two builds

Build each backend into its own target directory, then:

```sh
python3 scripts/smoke-real-crates.py target/smoke-master/debug --compare target/smoke-branch/debug
```

`place_info` is emitted in hash-map order, so outputs are canonicalised before
comparison: `ranges`, `slice` and `direct_influence` are sorted within each
entry, and entries are sorted by their full JSON content. With that, a build
compared against itself is stable.

## What it does

1. **Prepare.** Each crate is copied from `$CARGO_HOME/registry/src/*/` into
   `target/smoke-crates/<name>-<version>` (inside the repository: running from
   a symlinked temp directory produced SourceFile path mismatches). The copy
   drops `tests/`, `benches/`, `examples/`, `[[test]]`/`[[bench]]`/`[[example]]`
   targets and dev-dependencies, gets an empty `[workspace]` table (it lives
   inside the Flowistry workspace), and a lockfile is generated with
   `--offline`. Optional dependencies that are not available offline are
   removed along with the features that mention them; the report lists them.
   If the newest version cannot be prepared or built, older versions are tried.
   Prepared copies are reused on later runs; each backend gets its own
   `CARGO_TARGET_DIR` inside the copy.
2. **Probe.** A focus request on an empty, uncompiled file in the library's
   source directory builds the dependencies and makes the backend list the
   source files the crate actually compiles with its default features.
3. **Sample.** A small lexer finds lines inside function bodies of the
   compiled files (skipping comments, `macro_rules!`, `#[test]` functions and
   `#[cfg(test)]` items) that start a statement. About three quarters of the
   positions come from lines with a `let` or a closure. The column is the
   line's indentation; a line with a closure is also a candidate at the start
   of the closure body, so closure bodies get analysed too. Lines are passed
   0-based. The sample depends only on the
   crate name, its sources and `--seed`.
4. **Run.** `cargo flowistry --context-mode MODE focus FILE LINE COLUMN` from
   the crate root, one compiler run per position and mode. The response is the
   last stdout line (base64 of gzipped JSON).

## Reading the results

Each run is classified as:

- **ok**: the backend answered `{"Ok": ...}`.
- **benign**: an expected error for an arbitrary position, e.g. `Selection did
  not map to a body` (the position is not inside a body the analysis maps,
  such as some closures and constants).
- **error**: any other `{"Err": ...}`; not a crash, but worth a look.
- **crash**: stderr contains `panicked` or `internal compiler error`, or there
  is no decodable response. The report groups crashes by panic location and
  message and prints a reproduction command (run it in the dev shell with the
  backend's bin dir on `PATH`; `--json` keeps the tail of stderr).
- **timeout**: the run exceeded `--timeout` seconds.

Each run is a full compiler invocation of the crate plus the analysis of one
body: about 0.2 s for `either`, 1–2 s for `itertools` or `regex-syntax`. The
outlier is `memchr`, whose SIMD search loops in `src/arch/generic/memchr.rs`
take 45–95 s per position in either mode; with the defaults (60 positions,
`-j 4`) it alone accounts for most of a roughly 40-minute run, while the other
nine crates finish in about five minutes. Leave it out with explicit `--crate`
options for a quick check.
