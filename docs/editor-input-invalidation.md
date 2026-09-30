# Editor invalidation for compiler inputs

Draft [#57](https://github.com/sushi-shi/flowistry/pull/57) follows #56 in the
unified backend/editor stack as the next feature-11 increment.
It connects the editor's save handling to the input paths already collected by
the shared response cache. It does not introduce another validity cache.

The negotiated publication envelope now includes optional `inputs` metadata:
schema 1, package/dependency roots and individual external paths. Paths come from
the existing Cargo/compiler snapshot, including absent ancestor configuration,
build-script inputs, compiler includes and physical aliases of symlinked inputs.
Legacy focus/file-focus payloads remain unchanged. The project stream forwards
these hints; paired batch requests negotiate the same publication protocol.

The editor invalidates and cancels analysis when a normal file buffer under a
watched root or at a watched external path changes, irrespective of file type.
Unsaved inputs pause analysis and leave existing highlights explicitly stale.
Saving schedules revalidation and retains #56's current-body/saved-file/caller
priorities. Configuration and manifest edits also rediscover compiler/target
context. A workspace is canceled once per editor event, even with many enabled
buffers. Foreground and background completions reject intervening editor edits
during discovery, before newly encountered external input paths are known.

`FocusGained` conservatively discards retained reuse and rediscovers context,
covering changes made outside Neovim while it was unfocused. There is no recursive
filesystem monitor in this increment: external writes while Neovim stays focused
require the existing explicit refresh command (or a later focus return). Backend
input and publication checks still apply to every new request.

Watch metadata is bounded to 256 KiB per publication. The editor unions known
paths across target/file scopes, capped at 8,192 paths/1 MiB per workspace. Missing
or invalid metadata conservatively treats any normal edited file as potentially
relevant until that scope receives valid metadata. One scope cannot clear another
scope's unknown status. More than 128 unknown scopes, or a path-budget overflow,
keeps conservative matching until setup resets the session. Old paths remain
watched, so removed dependencies can cause extra invalidation rather than stale
results. `input_status()` and `:Flowistry log` expose the current fallback reason.
Optional local undo restoration reads at most 1 MiB per input; larger files use
backend revalidation instead of retaining another large file copy.

`nvim/tests/inputs.lua` covers non-Rust inputs, external includes/configuration,
dirty-before-discovery and edit-during-discovery races, unrelated buffers,
focus return, fallback recovery and path budgets. `inputs_live.lua` exercises
actual compiler/backend/editor saves of an external include, path dependency,
external build input and ancestor Cargo config in both modes, with background
workers enabled for Recurse. Its recorded timings are loaded-machine functional
observations, not the repeated quiet save distributions required by the plan.

Frozen candidate `fda9c7e8efca3e338334b5e7ff243205c990f7c9` passes 366 frontend
assertions, 31 real-editor assertions covering eight saves in both modes, 20 IDE
unit tests and 16 publication cases. Its backend executables are byte-identical
to the earlier `689c60f09` archive used by the publication and Rust tests; later
changes refined editor workspace identity and test fixtures. The
[durable evidence](measurements/editor-input-invalidation.json) records both
builds, exact report/harness hashes, passing case coverage and the unsuccessful
fixture attempt retained separately. A parent-workspace regression verifies that
configuration rediscovery cannot resume compilation while that input is dirty.
The [paired-package gate](https://github.com/sushi-shi/flowistry/actions/runs/36731870757/job/109943332290)
passes at this runtime revision. Five supplemental scheduler assertions verify
that background input discovery rejects intervening edits before decoding and
resumes only for the new generation; runtime files are unchanged.

The broader [continuation plan](continuation-plan.md) remains active: final edit
and corpus gates, the rustc-incremental experiment, safe layout reuse and measured
save/project/resource budgets are still required.
