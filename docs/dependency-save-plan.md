# Save scheduling from compiler observations

Draft [#56](https://github.com/sushi-shi/flowistry/pull/56) is the first feature-11
increment above #55. The full
[continuation plan](continuation-plan.md) remains the acceptance contract.

Saving a Rust file now passes a deduplicated list of recent saved files to the
project coordinator. Rapid saves cancel obsolete generations and restart one
queue. Within each target the order is the cursor body, saved files (most recent
first), explicit priority bodies, changed/affected/unknown bodies, other priority
files, and the remaining inventory. Without saved-file hints, the previous
cursor/body/file order is preserved. All inventoried bodies still get validated;
an ordering hint never skips a worker or authorizes a result.

The compiler inventory compares expanded HIR body fingerprints with observations
written while building the existing compiler-validated semantic dependency keys.
Observations contain portable body identities, direct resolved callee edges,
the declaration/configuration context, evaluation mode, semantic-key provenance
and an integrity checksum. They occupy the existing bounded `dependencies-v1`
namespace; they are not a separate result cache or a project-current index.
Compiler queries finish before the store lock is acquired. Cache-off inventory
does not compute these optional fingerprints or read observations.

`save_plan` on inventory and streamed body metadata describes an **unvalidated
scheduling hint**, with one of these statuses:

| Status | Meaning |
|---|---|
| `changed` | Expanded body differs from the previous observation. |
| `affected` | A previous resolved Recurse callee is changed or unknown. |
| `unchanged-input-hint` | Expanded body matches; compiler/MIR validation remains necessary. |
| `unknown` | Observation is absent/invalid or a callee lies outside the inventory. |

Recurse propagates changes through previous reverse edges, including cycles.
SigOnly does not couple ordinary caller bodies to callee implementation edits.
Declaration, compiler, configuration and external-crate context changes select
different observation keys, producing conservative unknown hints. Missing,
evicted, corrupt or raced observations can change ordering only. Normal worker
input validation rebuilds current call resolution and determines focus/summary
reuse and actual solver work. A snapshot hit can use a valid result without
rebuilding an evicted hint; subsequent compiler validation repairs the hint.
This intentionally makes no claim that matching HIR alone proves semantic equality.

The editor retains at most 64 distinct saved-file hints per workspace and clears
them after finishing all queued targets. Discarding older hints loses priority
only, because the complete target inventory is still visited. Existing dirty
buffer, generation, changed-tick, source-text and publication checks continue to
guard delivery; background analysis remains opt-in.

`scripts/test-save-plan.py` checks real compiler observations, a cross-file callee
edit, exact streamed output against independent cache-off analysis, mode-specific
caller order, compiler/solver counters, corruption, declaration fallback and
revert. The Rust regressions cover cyclic propagation and integrity/mode/context
isolation. The editor scheduler test covers deduplicated rapid-save ordering.

Frozen candidate `fc4d659fe77821b8fb50d5a99935f3fb35d18297` passes all eight
save-plan scenarios, all 84 existing modification/concurrency cases (including
copied real projects), 14 project-worker scenarios and 16 coordinator scenarios.
All 164 workspace tests, 55 Python harness regressions and 324 frontend assertions
pass. The [durable evidence](measurements/dependency-save-plan.json) includes
binary/build provenance, harness and raw-report hashes, exact case coverage and
per-body work counters. In the callee-edit fixture, unrelated bodies solve zero
times in both modes; the caller solves only in Recurse. These are correctness
and work-avoidance observations, not quiet latency or throughput measurements.
The [paired-package CI](https://github.com/sushi-shi/flowistry/actions/runs/36728715359/job/109932276105)
also passes on the candidate.

Remaining feature-11 gates include extending editor save/concurrency coverage,
non-Rust input/editor invalidation coverage, real-project save latency
distributions and a measured rustc-incremental experiment. Large-project cost of
the inventory fingerprints and optional observation writes must be measured.
Safe no-compile layout reuse remains feature 12; final corpus, package and
performance acceptance remain feature 13. Focused fixture results do not close
those gates.
