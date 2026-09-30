# Measurement implementation and evidence

The measurement harness is implemented; continuation-plan step 2 is not complete.
The combined corpus gate is still running. Immediate-base optimization comparisons,
quiet representative/stress measurements, project/save distributions and measured
resource budgets remain required.

The focused real-executable proof used identical frozen integrated binaries
(`2630ee356`) on locked `either` position `src/into_either.rs:58:8`, SigOnly, cache
off, four repetitions per backend with alternating A/B order. All eight samples
returned equal output and valid instruction/cycle counters. The raw report is
`target/continuation-validation/perf-interleaved-proof.json`; its summary is
`perf-interleaved-summary.json` in the same directory. These were functional probes
on a loaded host, not performance gains. In particular, the instruction counts
include both roughly 231M and 606M samples from the same binary: warm Cargo command
replay must be controlled before interpreting a ratio.

The cause was a shared replay store, keyed by source directory, whose record was
replaced whenever the A/B compiler environment changed. A second real probe used
per-backend replay stores and one warmup (`--cargo-replay on --cache-dir ...
--warmup 1 --phases`). All eight samples reported direct compiler replay and
231.72–231.78M instructions, equal semantic output, wire size and phase/counter
data. This verifies the measurement isolation; it still makes no latency claim.
Its compact durable report is [measurements/replay-isolation-proof.json](measurements/replay-isolation-proof.json).

The inherited master-versus-measure report finished successfully: 704 successful
pairs, zero output differences and 16 paired no-body results. It used older branch
tips and overlapped other work, so it is historical evidence only. The full report
and load log remain under the Claude scratchpad's `engine/results/` as
`timing-master-measure.{json,txt,load,log}`.

Harness validation: 42 Python regression tests pass, including repeated-output
stability, alternating order, unavailable/partial counters, missing repetitions,
tail latency/peak memory preservation, checkpoint rejection and missing phases.
See [smoke-tests.md](smoke-tests.md) for reproducible commands and interpretation.
