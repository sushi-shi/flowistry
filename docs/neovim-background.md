# Neovim workspace background analysis

This feature extends the unified chain after coordinator #54. The plugin can run
one bounded project queue per canonical Cargo workspace while keeping foreground
requests ahead of background warming. Enable it with `:Flow project` or
`project = { enabled = true }`. It remains opt-in until the large-project and
latency acceptance gates pass.

The new `project-targets` backend command returns schema-1 workspace/target metadata
through the ordinary compressed Result protocol, without invoking rustc. The editor
uses the same packaged backend and Cargo environment for discovery and analysis.
It visits supported library/binary workspace targets; explicit configuration can
select example/test/bench targets too. Feature-gated targets require opt-in, and
multi-crate-type targets remain unsupported. Full Cargo package IDs disambiguate
workspace members. Exact target source paths win; other module paths prefer their
package's library, with stable tie breaking and explicit selection overrides.

An enabled buffer registers with its workspace queue. Cursor and open-file
priorities are supplied to each project coordinator. A foreground request reserves
its slot before canceling background work and waits for coordinator exit before
launching. Duplicate foreground requests with identical arguments, environment,
mode and cache policy share an operation; one canceled subscriber does not cancel
another. Idle warming resumes through the existing validated backend store.
Different workspaces have independent queues. A global `:Flow stop` disables all
buffers and automatic enabling; `:Flow start` restores it. Per-buffer off remains
available.

Each stream enforces schema, run identity, sequence, event kinds and completion.
Transport queues are bounded; gzip decoding is separately cancelable. The editor
only decodes results for open enabled buffers and the target selected for that
buffer. Generation, buffer tick, modified state and saved-byte equality guard
publication, including asynchronous decompression. Saves currently restart the
inventory conservatively; manifest changes rediscover targets. Dependency-selective
save planning is the next feature, not a claim of this implementation.

Backend workers retain #54's per-body memory/time scopes. Editor background
decoding defaults to at most 8 MiB uncompressed JSON per body, and retained
background objects have a 16 MiB aggregate JSON-byte budget. These byte limits
bound inputs/retention decisions; Lua heap cost is representation-dependent and
is not a hard RSS guarantee. Old background objects are evicted, including copies
retained during edits, while their validated backend entries remain reusable.
Large results stay backend-only until requested in the foreground. The existing
600-line foreground batch guard remains. Foreground objects are not discarded
merely to satisfy the speculative background budget.

`project_status()` exposes target/body progress, failed outcomes, diagnostics and
retained background bytes. A partial body failure never becomes a globally
successful project completion. Background results from different revisions are
not represented as one current workspace snapshot. Existing highlight behavior,
comment exclusion, argument-type aliases and saved-analysis labels remain active.

## Focused validation and remaining gates

The frontend suite covers existing behavior plus fragmented/malformed streams,
bounded decompression, independent workspace queues, foreground coalescing and
cancellation barriers, late callbacks after edits/disable, output-only warming
for unopened files, retention eviction, partial failure and target rediscovery.
Real subprocess fixtures cover foreground preemption, navigation, saves, global
disable and re-enable. A real-backend headless test runs both modes with actual
systemd-scoped workers: all four fixture bodies complete and navigation to a
warmed function invokes no compiler. The backend target inventory and existing
body worker tests pass together.

These checks do not replace whole-project resource-growth, throughput, foreground
latency, rapid-save/dependency-matrix or final corpus gates. Package validation and
exact candidate evidence are recorded separately when complete. The previous
coordinator's two full cache corpus runs continue against their own immutable
executables and independent prepared roots.
