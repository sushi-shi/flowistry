#!/usr/bin/env python3
"""Compare two cargo-flowistry builds; run with the pinned Rust compiler on PATH.

Both builds must support the `file-focus` command (e.g. a baseline built from the
file-focus branch, and a candidate with callee summaries). The baseline runs in
SigOnly and Recurse modes, the candidate in Recurse mode.

By default this creates a dependency-free diamond-call-graph crate. To benchmark
real code, pass --project, --file and an optional zero-based --position LINE COL.
Each sample launches a fresh compiler; 'warm' refers to Cargo artifacts only.
Requires GNU time. Outputs decoded protocol results, stderr logs and JSON metrics,
including the candidate's callee-summary counters when it logs them.
"""

import argparse
import base64
import gzip
import json
import os
from pathlib import Path
import shutil
import signal
import statistics
import subprocess
import tempfile


def diamond(project):
    (project / "src").mkdir(parents=True)
    (project / "Cargo.toml").write_text(
        '[package]\nname = "summary-bench"\nversion = "0.0.0"\nedition = "2024"\n'
    )
    lines = [
        "struct State { used: i64, untouched: i64 }",
        "fn leaf(s: &mut State, v: i64) -> i64 { s.used += v; s.used }",
    ]
    for level in range(7):
        left = "leaf" if level == 0 else f"left{level - 1}"
        right = "leaf" if level == 0 else f"right{level - 1}"
        for name in ("left", "right"):
            lines.append(
                f"fn {name}{level}(s: &mut State, v: i64) -> i64 {{ "
                f"{left}(s, v) + {right}(s, v) }}"
            )
    line = len(lines)
    lines += [
        "fn main() {",
        " let mut s = State { used: 1, untouched: 2 };",
        " s.untouched = 3;",
        " let value = left6(&mut s, 4) + right6(&mut s, 5);",
        " std::hint::black_box((s.untouched, value));",
        "}",
    ]
    source = project / "src/main.rs"
    source.write_text("\n".join(lines) + "\n")
    return source, [line + 3, 5]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--project", type=Path)
    parser.add_argument("--file", type=Path)
    parser.add_argument("--position", nargs=2, type=int)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--timeout", type=int, default=180)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if bool(args.project) != bool(args.file):
        parser.error("--project and --file must be supplied together")
    if args.repeats < 1:
        parser.error("--repeats must be positive")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    temporary = tempfile.TemporaryDirectory(prefix="flowistry-summary-bench-")
    project = args.project.resolve() if args.project else Path(temporary.name)
    source, position = (args.file.resolve(), args.position) if args.file else diamond(project)
    timer = shutil.which("time")
    if not timer:
        parser.error("GNU time is required on PATH")
    report = {"project": str(project), "file": str(source), "position": position, "runs": []}
    variants = [("baseline-sigonly", args.baseline, "SigOnly"),
                ("baseline-recurse", args.baseline, "Recurse"),
                ("candidate-recurse", args.candidate, "Recurse")]
    for name, executable, mode in variants:
        samples = []
        for run in range(args.repeats + 1):
            stem = output / f"{name}-{run}"
            resource_path = stem.with_suffix(".time")
            command = [timer, "-f", "%e %M", "-o", str(resource_path),
                       str(executable.resolve()), "flowistry", "--context-mode", mode,
                       "file-focus", str(source)]
            if position:
                command.extend(map(str, position))
            env = dict(os.environ, RUST_LOG="flowistry::infoflow::session=info")
            with stem.with_suffix(".stdout").open("wb") as stdout, stem.with_suffix(".log").open("wb") as stderr:
                process = subprocess.Popen(command, cwd=project, env=env, stdout=stdout, stderr=stderr, start_new_session=True)
                try:
                    status = process.wait(timeout=args.timeout)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
                    raise RuntimeError(f"{name} exceeded {args.timeout}s; see {stem}.log")
            if status:
                raise RuntimeError(f"{name} exited {status}; see {stem}.log")
            payload = json.loads(gzip.decompress(base64.b64decode(stem.with_suffix(".stdout").read_bytes())))
            if "Ok" not in payload or any(body.get("focus", {}).get("Err") for body in payload["Ok"]["bodies"] if body.get("focus")):
                raise RuntimeError(f"{name} returned an analysis error: {payload}")
            stem.with_suffix(".json").write_text(json.dumps(payload))
            wall, rss = resource_path.read_text().split()
            # The session logs its counters when it is dropped.
            stats = [line.split("Callee summaries: ", 1)[1].strip()
                     for line in stem.with_suffix(".log").read_text(errors="replace").splitlines()
                     if "Callee summaries: " in line]
            record = {"variant": name, "run": run, "warm_cargo": run > 0,
                      "wall_seconds": float(wall), "peak_rss_kib": int(rss),
                      "summary_stats": stats[-1] if stats else None}
            report["runs"].append(record)
            (output / "metrics.json").write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps(record), flush=True)
            if run > 0:
                samples.append(float(wall))
        print(f"{name}: median warm-Cargo wall time {statistics.median(samples):.3f}s", flush=True)
    temporary.cleanup()


if __name__ == "__main__":
    main()
