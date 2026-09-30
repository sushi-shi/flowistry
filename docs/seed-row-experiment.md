# Seed-row construction candidate

Continuation step 4 is an experiment, not an accepted performance improvement.
The candidate follows the row-group port and retains the original argument-place
and conflict-parent semantics.

Repeated argument subtrees often have identical types. For each distinct full,
unnormalized type, compute one fresh `interior_places` traversal and retain its
relative projection paths temporarily. Reapply that template at other roots of
the same type. The body and visibility DefId are fixed by the enclosing PlaceInfo;
every cached traversal starts with an empty type stack. No template is harvested
from a parent traversal, where ancestor types can cut off recursion earlier.
References, enums, array indices and closure projections use the existing pinned
rustc-utils traversal, rather than a separately reimplemented type visitor.

Retain at most 65,536 template paths during construction; beyond the cap, compute
fresh paths without retaining them. The templates are dropped before solving.
Deduplicate normalized seeds before constructing the matrix. A `seed rows` timer
exposes this work separately in phase reports. This still enumerates each output
projection and does not promise linear complexity in the number of final seeds.

Validation: 108 core unit/integration tests pass with engine-diff and shadow-eager.
Under shadow-eager, every optimized seed set is compared to the original full
conflict enumeration. A dedicated fixture includes recursive boxed nodes, nested
references, enums and arrays, and verifies that restarting at a child really can
reach deeper than the parent traversal. Existing argument/strong-update/slicing
and callee-summary fixtures also pass without changing expected outputs.

Pending: full corpus/reference checks and repeated normal-release measurements
against the row-group predecessor, including just's large argument type and peak
memory. Retain this implementation only if those measurements establish a useful
gain; otherwise record the rejected experiment. The current combined/reference
corpus jobs occupy both heavy-run slots, so large comparisons are queued.
