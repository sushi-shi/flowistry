#!/usr/bin/env python3
"""Smoke-test the Flowistry backend on real crates from the local cargo registry.

For every crate, the script copies the published sources into a scratch
directory, strips tests/benches/examples/dev-dependencies, resolves a lockfile
offline, and then runs `cargo flowistry focus` at deterministically sampled
positions inside function bodies, once per context mode. Each run is
classified as:

  ok      the backend answered {"Ok": ...}
  benign  a known, expected analysis error (e.g. the position is not in a body)
  error   any other {"Err": ...} answer (reported, but not a crash)
  crash   a panic / ICE on stderr, or no decodable response
  timeout the run exceeded --timeout

With --compare, every run is repeated with a second backend build and the
answers are compared after canonicalising the order of `place_info` (the IDE
emits it in hash-map order).

The script only uses the Python standard library but needs `cargo` and the
pinned nightly toolchain, so run it inside the dev shell, e.g.

  nix develop github:sushi-shi/flowistry -c python3 scripts/smoke-real-crates.py \\
      target/smoke-worker/debug

Exit status is 1 if any run crashed or timed out (or, with --compare, if the
two backends disagree), else 0.
"""

import argparse
import base64
import concurrent.futures
import glob
import gzip
import json
import os
import random
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
import zlib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

DEFAULT_CRATES = [
    "itertools",
    "either",
    "smallvec",
    "indexmap",
    "hashbrown",
    "serde_json",
    "regex-syntax",
    "anyhow",
    "bitflags",
    "memchr",
]

MODES = ["SigOnly", "Recurse"]

# Analysis errors that are expected for arbitrary positions and say nothing
# about the health of the analysis itself.
BENIGN_ERRORS = [
    "Selection did not map to a body",
    "Could not find SourceFile for path",
]

CRASH_MARKERS = ["panicked", "internal compiler error"]

PROBE_FILE = "__flowistry_smoke_probe__.rs"

_print_lock = threading.Lock()


def log(msg):
    with _print_lock:
        print(msg, file=sys.stderr, flush=True)


# --------------------------------------------------------------------------
# Locating and preparing crates


def parse_version(text):
    m = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?", text)
    if not m:
        return None
    return (int(m[1]), int(m[2]), int(m[3])), m[4]


def registry_dirs(args):
    if args.registry:
        return [Path(r) for r in args.registry]
    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    return [Path(p) for p in sorted(glob.glob(str(cargo_home / "registry" / "src" / "*")))]


def find_candidates(spec, registries):
    """Return [(label, source_dir)] for a crate spec, best candidate first.

    A spec is `name`, `name@version`, or a path to a crate directory.
    """
    if os.sep in spec or spec.startswith("."):
        path = Path(spec).resolve()
        return [(path.name, path)]
    name, _, pinned = spec.partition("@")
    found = {}
    for reg in registries:
        for entry in reg.glob(f"{name}-*"):
            version = entry.name[len(name) + 1 :]
            parsed = parse_version(version)
            if parsed is None or not (entry / "Cargo.toml").is_file():
                continue
            if pinned and version != pinned:
                continue
            if parsed[1] is not None and not pinned:
                continue  # skip pre-releases unless asked for explicitly
            found[version] = entry
    ordered = sorted(found, key=lambda v: parse_version(v)[0], reverse=True)
    return [(f"{name}-{v}", found[v]) for v in ordered]


# A minimal line-based editor for the normalised Cargo.toml that `cargo
# publish` writes (one table per dependency, no multi-line inline tables),
# because the standard library can read TOML but not write it.

DEP_HEADER = re.compile(r"^\[(?:target\.(.+)\.)?(dev-|build-)?dependencies(?:\.([^\]]+))?\]$")
DEV_TARGET_HEADER = re.compile(r"^\[\[(bench|test|example)\]\]$")


def manifest_sections(text):
    """Split a manifest into [header, body_lines] pairs; the first header is None."""
    sections = [[None, []]]
    for line in text.splitlines():
        if line.startswith("["):
            sections.append([line.strip(), []])
        else:
            sections[-1][1].append(line)
    return sections


def render_manifest(sections):
    lines = []
    for header, body in sections:
        if header is not None:
            lines.append(header)
        lines += body
    return "\n".join(lines) + "\n"


def manifest_deps(sections):
    """Yield (section, kind, key, package, optional, inline_line) for every dependency."""
    for sec in sections:
        m = DEP_HEADER.match(sec[0] or "")
        if not m:
            continue
        kind = m[2] or ""
        if m[3]:
            body = "\n".join(sec[1])
            key = m[3].strip("\"'")
            pkg = re.search(r'^package\s*=\s*"([^"]+)"', body, re.M)
            optional = re.search(r"^optional\s*=\s*true", body, re.M) is not None
            yield sec, kind, key, pkg[1] if pkg else key, optional, None
        else:
            for line in sec[1]:
                dm = re.match(r"""^\s*["']?([\w-]+)["']?\s*=""", line)
                if dm:
                    pkg = re.search(r'package\s*=\s*"([^"]+)"', line)
                    optional = re.search(r"optional\s*=\s*true", line) is not None
                    yield sec, kind, dm[1], pkg[1] if pkg else dm[1], optional, line


def remove_deps(sections, predicate):
    """Remove dependencies matching predicate(kind, key, package, optional); return removed keys."""
    removed, dead = set(), []
    for sec, kind, key, pkg, optional, line in list(manifest_deps(sections)):
        if predicate(kind, key, pkg, optional):
            removed.add(key)
            if line is None:
                dead.append(sec)
            else:
                sec[1].remove(line)
    sections[:] = [s for s in sections if not any(s is d for d in dead)]
    # Features must not mention dependencies that no longer exist.
    remaining = {key for _, _, key, _, _, _ in manifest_deps(sections)}
    for sec in sections:
        if sec[0] == "[features]":
            body = "\n".join(sec[1])
            for key in removed - remaining:
                body = re.sub(r'"(?:dep:)?%s\??(?:/[^"]*)?"\s*,?' % re.escape(key), "", body)
            sec[1] = body.split("\n")
    return removed


def strip_dev_only(sections):
    """Drop dev-only targets and dependencies and make the crate its own workspace."""
    sections[:] = [s for s in sections if not DEV_TARGET_HEADER.match(s[0] or "")]
    remove_deps(sections, lambda kind, key, pkg, optional: kind == "dev-")
    if not any(s[0] == "[workspace]" for s in sections):
        sections.append(["[workspace]", []])


def run(cmd, cwd, env, timeout=None):
    """subprocess.run, but a timeout kills the whole process group (cargo and its rustc children)."""
    with subprocess.Popen(cmd, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          text=True, errors="replace", start_new_session=True) as proc:
        try:
            stdout, stderr = proc.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.communicate()
            raise
    return subprocess.CompletedProcess(cmd, proc.returncode, stdout, stderr)


def prepare_copy(src, dest, env, fresh):
    """Copy a registry crate to `dest` and make it buildable on its own, offline.

    Optional dependencies that cannot be resolved offline are removed (and
    returned); a missing non-optional dependency raises RuntimeError.
    """
    marker = dest / ".flowistry-smoke-source"
    if not fresh and marker.is_file():
        source, _, pruned = marker.read_text().partition("\n")
        if source == str(src):
            return pruned.split()
    if dest.exists():
        shutil.rmtree(dest)
    shutil.copytree(src, dest, symlinks=True)
    for d in ["benches", "tests", "examples"]:
        shutil.rmtree(dest / d, ignore_errors=True)
    for f in ["Cargo.toml.orig", "Cargo.lock", ".cargo-ok", ".cargo_vcs_info.json"]:
        (dest / f).unlink(missing_ok=True)
    manifest = dest / "Cargo.toml"
    sections = manifest_sections(manifest.read_text())
    strip_dev_only(sections)
    pruned = []
    for _ in range(50):
        manifest.write_text(render_manifest(sections))
        res = run(["cargo", "generate-lockfile", "--offline"], dest, env)
        if res.returncode == 0:
            break
        m = re.search(r"no matching package named `([^`]+)`"
                      r"|failed to select a version for the requirement `([^` =]+)", res.stderr)
        missing = m and (m[1] or m[2])
        removed = missing and remove_deps(
            sections, lambda kind, key, pkg, optional: optional and missing in (key, pkg))
        if not removed:
            raise RuntimeError("cargo generate-lockfile --offline failed: " + cargo_error(res.stderr))
        pruned += sorted(removed)
    else:
        raise RuntimeError("cargo generate-lockfile --offline kept failing")
    marker.write_text(str(src) + "\n" + " ".join(pruned))
    return pruned


def lib_target(crate_dir, env):
    res = run(["cargo", "metadata", "--offline", "--no-deps", "--format-version", "1"], crate_dir, env)
    if res.returncode != 0:
        raise RuntimeError("cargo metadata failed: " + cargo_error(res.stderr))
    pkg = json.loads(res.stdout)["packages"][0]
    for target in pkg["targets"]:
        if "lib" in target["kind"] or "rlib" in target["kind"] or "proc-macro" in target["kind"]:
            return Path(target["src_path"])
    raise RuntimeError("crate has no library target")


# --------------------------------------------------------------------------
# Running the backend


def last_lines(text, n):
    lines = [line for line in text.strip().splitlines() if line.strip()]
    return " | ".join(lines[-n:])


def cargo_error(stderr):
    """The `error: ...` lines of cargo's stderr (or its tail if there are none)."""
    errors = [line.strip() for line in stderr.splitlines() if line.startswith("error")]
    return " | ".join(errors[:3]) if errors else last_lines(stderr, 3)


def backend_env(bin_dir, target_dir):
    env = dict(os.environ)
    env["PATH"] = str(bin_dir) + os.pathsep + env.get("PATH", "")
    env["CARGO_NET_OFFLINE"] = "true"
    env["CARGO_TARGET_DIR"] = str(target_dir)
    env.pop("RUSTC_WRAPPER", None)
    env.pop("RUSTC_WORKSPACE_WRAPPER", None)
    return env


def crash_signature(stderr):
    """A short, stable description of a panic/ICE, suitable for grouping."""
    lines = stderr.splitlines()
    for i, line in enumerate(lines):
        if "panicked at" in line:
            loc = line.split("panicked at", 1)[1].strip().rstrip(":")
            msg = lines[i + 1].strip() if i + 1 < len(lines) else ""
            return f"panicked at {loc}: {msg}"
    for line in lines:
        if "internal compiler error" in line:
            return line.strip()
    return "no decodable response: " + last_lines(stderr, 2)


def flowistry_focus(crate_dir, env, rel_file, line, col, mode, timeout):
    cmd = ["cargo", "flowistry", "--context-mode", mode, "focus", rel_file, str(line), str(col)]
    start = time.monotonic()
    try:
        res = run(cmd, crate_dir, env, timeout)
    except subprocess.TimeoutExpired:
        return {"status": "timeout", "message": f"timed out after {timeout}s",
                "seconds": round(time.monotonic() - start, 2)}
    seconds = round(time.monotonic() - start, 2)
    response = None
    tail = res.stdout.strip().splitlines()
    if tail:
        try:
            response = json.loads(gzip.decompress(base64.b64decode(tail[-1].strip(), validate=True)))
        except Exception:
            response = None
    stderr = res.stderr
    if any(m in stderr for m in CRASH_MARKERS) or response is None:
        return {"status": "crash", "message": crash_signature(stderr), "seconds": seconds,
                "returncode": res.returncode, "stderr_tail": stderr[-4000:]}
    if "Ok" in response:
        return {"status": "ok", "seconds": seconds, "output": response["Ok"],
                "places": len(response["Ok"].get("place_info", []))}
    err = response.get("Err", response)
    message = err.get("error") or err.get("type") or json.dumps(err)
    status = "benign" if any(b in message for b in BENIGN_ERRORS) else "error"
    return {"status": status, "message": message, "seconds": seconds}


def canonical(output):
    """Order-insensitive form of a focus output, for comparing two backends."""
    def key(x):
        return json.dumps(x, sort_keys=True)

    if not isinstance(output, dict):
        return output
    places = []
    for p in output.get("place_info", []):
        p = dict(p)
        for field in ["ranges", "slice", "direct_influence"]:
            if field in p:
                p[field] = sorted(p[field], key=key)
        places.append(p)
    out = dict(output)
    out["place_info"] = sorted(places, key=key)
    if "containers" in out:
        out["containers"] = sorted(out["containers"], key=key)
    return out


# --------------------------------------------------------------------------
# Sampling positions


# Blocks whose contents are not compiled into the library (or not as bodies).
SKIPPED_BLOCK = re.compile(r"macro_rules!|#\[cfg\((all\()?test\b|#\[(\w+::)*test\]")


def rust_lines_in_bodies(text):
    """Yield (line_no, line) for lines that are (approximately) inside fn bodies.

    A tiny lexer tracks braces while ignoring comments, strings and char
    literals; each `{` is labelled by the text that precedes it, so blocks
    under `fn` count as bodies, while `macro_rules!` bodies, `#[test]`
    functions and `#[cfg(test)]` items are excluded.
    """
    stack = []  # labels: "fn", "skip", "other"
    header = ""
    in_block_comment = 0
    in_str = None  # None, '"', or a raw-string terminator such as '"##'
    for no, line in enumerate(text.splitlines()):
        in_body = "fn" in stack and "skip" not in stack
        i, n = 0, len(line)
        while i < n:
            c = line[i]
            if in_block_comment:
                if line.startswith("*/", i):
                    in_block_comment -= 1
                    i += 2
                elif line.startswith("/*", i):
                    in_block_comment += 1
                    i += 2
                else:
                    i += 1
                continue
            if in_str:
                if in_str == '"' and c == "\\":
                    i += 2
                elif line.startswith(in_str, i):
                    i += len(in_str)
                    in_str = None
                else:
                    i += 1
                continue
            if line.startswith("//", i):
                break
            if line.startswith("/*", i):
                in_block_comment = 1
                i += 2
                continue
            m = re.match(r'b?r(#*)"', line[i:])
            if m and (i == 0 or not (line[i - 1].isalnum() or line[i - 1] == "_")):
                in_str = '"' + m[1]
                i += len(m[0])
                continue
            if c == '"':
                in_str = '"'
                i += 1
                continue
            m = re.match(r"b?'(\\.[^']*|[^\\'])'", line[i:])
            if m:
                i += len(m[0])
                continue
            if c == "{":
                h = header
                if SKIPPED_BLOCK.search(h):
                    label = "skip"
                elif re.search(r"\bfn\b", h) or "fn" in stack:
                    label = "fn"
                else:
                    label = "other"
                stack.append(label)
                header = ""
            elif c == "}":
                if stack:
                    stack.pop()
                header = ""
            elif c == ";":
                header = ""
            else:
                header += c
            i += 1
        if not line.strip().startswith("//"):
            header += " "
        if in_body:
            yield no, line


STATEMENT_START = re.compile(
    r"^(let |if |match |while |for |return\b|[A-Za-z_][A-Za-z0-9_:.]*\s*(\(|\.|=|\+=|-=|\[|!))"
)


# Start of a closure's parameter list, e.g. `.map(|x| ...`, `= move |a, b| ...`.
CLOSURE = re.compile(r"(?:[(,=]\s*|\bmove\s+)(\|[^|]*\|)\s*")


def sample_positions(crate_dir, files, count, seed, crate_label):
    """Pick `count` (file, line, column) positions, deterministically.

    Candidates are statement-like lines inside function bodies, with the
    column at the line's indentation. Lines that bind (`let`) or contain a
    closure are preferred (about 3/4 of the sample); a closure line also
    contributes a second candidate whose column is the start of the closure
    body, so closure bodies themselves get analysed too.
    """
    preferred, other = [], []
    for rel in files:
        try:
            text = (crate_dir / rel).read_text(errors="replace")
        except OSError:
            continue
        for no, line in rust_lines_in_bodies(text):
            stripped = line.strip()
            if not stripped or stripped.startswith(("//", "#", "*", "/*", "}", "{", ")", "]")):
                continue
            if not STATEMENT_START.match(stripped):
                continue
            pos = (rel, no, len(line) - len(line.lstrip()))
            closure = CLOSURE.search(line)
            if closure and closure.end() < len(line):
                preferred += [pos, (rel, no, closure.end())]
            elif stripped.startswith("let "):
                preferred.append(pos)
            else:
                other.append(pos)
    rng = random.Random(seed ^ zlib.crc32(crate_label.encode()))
    n_pref = min(len(preferred), round(count * 0.75))
    picked = rng.sample(preferred, n_pref)
    picked += rng.sample(other, min(len(other), count - n_pref))
    if len(picked) < count:
        rest = [p for p in preferred if p not in picked]
        picked += rng.sample(rest, min(len(rest), count - len(picked)))
    return sorted(picked)


def compiled_files(crate_dir, lib_dir, env, timeout):
    """Probe the backend with an uncompiled file to learn which files the crate compiles.

    The probe also builds dependencies and checks that the crate builds at all.
    """
    probe = lib_dir / PROBE_FILE
    probe.write_text("")
    try:
        rel = os.path.relpath(probe, crate_dir)
        result = flowistry_focus(crate_dir, env, rel, 0, 0, "SigOnly", timeout)
    finally:
        probe.unlink(missing_ok=True)
    msg = result.get("message", "")
    m = re.search(r"Available SourceFiles were: \[(.*)\]", msg, re.S)
    if not m:
        raise RuntimeError(f"probe did not list source files ({result['status']}): {msg[:500]}")
    root = crate_dir.resolve()
    files = set()
    for name in m[1].split(", "):
        path = Path(name) if os.path.isabs(name) else root / name
        try:
            rel = path.resolve().relative_to(root)
        except (ValueError, OSError):
            continue  # dependency or std file
        if rel.parts[0] == "target" or rel.suffix != ".rs" or not (root / rel).is_file():
            continue
        files.add(str(rel))
    return sorted(files)


# --------------------------------------------------------------------------
# Per-crate driver


def smoke_crate(spec, args, registries, backends):
    candidates = find_candidates(spec, registries)
    report = {"spec": spec, "crate": None, "skipped": [], "records": [], "files": []}
    if not candidates:
        report["skipped"].append((spec, "not found in the cargo registry"))
        return report
    for label, src in candidates[: args.version_attempts]:
        dest = args.work_dir / label
        base_env = backend_env(backends[0][1], dest / "target" / "smoke-base")
        try:
            pruned = prepare_copy(src, dest, base_env, args.fresh)
            lib_dir = lib_target(dest, base_env).parent
            files = compiled_files(dest, lib_dir, base_env, args.timeout)
            if args.compare:
                # Warm up the second backend's target dir too.
                compiled_files(dest, lib_dir, backend_env(backends[1][1], dest / "target" / "smoke-cmp"),
                               args.timeout)
        except Exception as e:  # noqa: BLE001 - any failure means "try the next version"
            log(f"[{label}] skipped: {e}")
            report["skipped"].append((label, str(e)))
            continue
        report.update(crate=label, dir=str(dest), files=files, pruned=pruned)
        if pruned:
            log(f"[{label}] pruned optional dependencies unavailable offline: {', '.join(pruned)}")
        break
    if report["crate"] is None:
        return report

    label, dest = report["crate"], Path(report["dir"])
    positions = sample_positions(dest, report["files"], args.positions, args.seed, label.rsplit("-", 1)[0])
    log(f"[{label}] {len(report['files'])} compiled files, {len(positions)} positions")
    envs = [(name, backend_env(bin_dir, dest / "target" / ("smoke-base" if i == 0 else "smoke-cmp")))
            for i, (name, bin_dir) in enumerate(backends)]
    started = time.monotonic()
    for idx, (rel, line, col) in enumerate(positions):
        for mode in args.modes:
            rec = {"crate": label, "file": rel, "line": line, "column": col, "mode": mode}
            for name, env in envs:
                result = flowistry_focus(dest, env, rel, line, col, mode, args.timeout)
                rec[name] = result
                if result["status"] in ("crash", "timeout"):
                    log(f"[{label}] {result['status'].upper()} ({name}) {mode} {rel}:{line}:{col}: "
                        f"{result['message']}")
            if len(envs) == 2:
                a, b = rec[envs[0][0]], rec[envs[1][0]]
                same = a["status"] == b["status"] and (
                    canonical(a.get("output")) == canonical(b.get("output"))
                    if a["status"] == "ok" else a.get("message") == b.get("message"))
                rec["same"] = same
            if not args.keep_outputs:
                for name, _ in envs:
                    rec[name].pop("output", None)
            report["records"].append(rec)
        if (idx + 1) % 10 == 0:
            log(f"[{label}] {idx + 1}/{len(positions)} positions")
    report["seconds"] = round(time.monotonic() - started, 1)
    return report


# --------------------------------------------------------------------------
# Reporting


def short_message(msg):
    msg = msg.split(". Available SourceFiles were:")[0]
    msg = re.sub(r"for path: \S+", "for path: <file>", msg)
    return msg if len(msg) <= 160 else msg[:157] + "..."


def summarize(reports, backends, args, total_seconds):
    names = [b[0] for b in backends]
    out = []
    header = f"{'crate':<22} {'files':>5} {'pos':>4} {'runs':>5} {'ok':>5} {'benign':>6} {'error':>5} " \
             f"{'crash':>5} {'t/o':>4} {'secs':>7}"
    if len(names) == 2:
        header += f" {'diff':>5}"
    for name in names:
        if len(names) == 2:
            out.append(f"== backend '{name}' ==")
        out.append(header)
        for r in reports:
            if r["crate"] is None:
                continue
            recs = r["records"]
            counts = {s: sum(1 for x in recs if x[name]["status"] == s)
                      for s in ["ok", "benign", "error", "crash", "timeout"]}
            npos = len({(x["file"], x["line"], x["column"]) for x in recs})
            row = f"{r['crate']:<22} {len(r['files']):>5} {npos:>4} {len(recs):>5} {counts['ok']:>5} " \
                  f"{counts['benign']:>6} {counts['error']:>5} {counts['crash']:>5} {counts['timeout']:>4} " \
                  f"{r.get('seconds', 0):>7}"
            if len(names) == 2:
                row += f" {sum(1 for x in recs if not x['same']):>5}"
            out.append(row)
        out.append("")

    for r in reports:
        for label, why in r["skipped"]:
            out.append(f"skipped {label}: {why}")
        if r.get("pruned"):
            out.append(f"note {r['crate']}: removed optional deps unavailable offline: {', '.join(r['pruned'])}")
    out.append("")

    groups = {}
    for r in reports:
        for rec in r["records"]:
            for name in names:
                res = rec[name]
                if res["status"] == "ok":
                    continue
                key = (res["status"], name, short_message(res["message"]) if res["status"] != "crash"
                       else res["message"])
                groups.setdefault(key, []).append(rec)
    order = {"crash": 0, "timeout": 1, "error": 2, "benign": 3}
    for (status, name, msg), recs in sorted(groups.items(), key=lambda kv: (order[kv[0][0]], -len(kv[1]))):
        suffix = f" [{name}]" if len(names) == 2 else ""
        out.append(f"{status.upper()}{suffix} x{len(recs)}: {msg}")
        if status in ("crash", "timeout", "error"):
            for rec in recs[: args.examples]:
                out.append(f"    {rec['crate']} {rec['mode']} {rec['file']}:{rec['line']}:{rec['column']}")
            first = recs[0]
            crate_dir = next(r["dir"] for r in reports if r["crate"] == first["crate"])
            out.append(f"    repro: (cd {crate_dir} && cargo flowistry --context-mode {first['mode']} "
                       f"focus {first['file']} {first['line']} {first['column']})")
    if len(names) == 2:
        diffs = [rec for r in reports for rec in r["records"] if not rec["same"]]
        out.append("")
        out.append(f"{len(diffs)} run(s) differ between '{names[0]}' and '{names[1]}'")
        for rec in diffs[: args.examples * 4]:
            a, b = rec[names[0]], rec[names[1]]
            out.append(f"    {rec['crate']} {rec['mode']} {rec['file']}:{rec['line']}:{rec['column']}: "
                       f"{a['status']} vs {b['status']}")
    out.append(f"total runtime: {total_seconds:.0f}s")
    return "\n".join(out)


def main():
    parser = argparse.ArgumentParser(
        description=__doc__.split("\n\n")[0],
        epilog="See docs/smoke-tests.md for details.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("backend", type=Path,
                        help="directory containing cargo-flowistry and flowistry-driver (e.g. target/debug)")
    parser.add_argument("--compare", type=Path, metavar="BACKEND2",
                        help="second backend bin dir; compare its answers against the first")
    parser.add_argument("--crate", dest="crates", action="append", metavar="SPEC",
                        help="crate to test: NAME, NAME@VERSION or a path to a crate directory "
                             "(repeatable; default: %s)" % ", ".join(DEFAULT_CRATES))
    parser.add_argument("--registry", action="append", metavar="DIR",
                        help="registry source dir to search (default: $CARGO_HOME/registry/src/*)")
    parser.add_argument("--work-dir", type=Path, default=REPO_ROOT / "target" / "smoke-crates",
                        help="where crate copies are prepared; must be under the repository's target/ "
                             "or another non-symlinked path (default: %(default)s)")
    parser.add_argument("--positions", type=int, default=60, help="positions per crate (default: %(default)s)")
    parser.add_argument("--seed", type=int, default=0, help="sampling seed (default: %(default)s)")
    parser.add_argument("--modes", default=",".join(MODES),
                        help="comma-separated context modes (default: %(default)s)")
    parser.add_argument("--jobs", "-j", type=int, default=4, help="crates processed in parallel (default: %(default)s)")
    parser.add_argument("--timeout", type=int, default=600, help="seconds per compiler run (default: %(default)s)")
    parser.add_argument("--version-attempts", type=int, default=3,
                        help="older registry versions to try if the newest does not build (default: %(default)s)")
    parser.add_argument("--fresh", action="store_true", help="re-copy crates even if a prepared copy exists")
    parser.add_argument("--json", type=Path, metavar="FILE", help="write every run record to FILE")
    parser.add_argument("--keep-outputs", action="store_true", help="include full focus outputs in --json")
    parser.add_argument("--examples", type=int, default=5, help="example positions shown per problem group")
    args = parser.parse_args()

    args.modes = [m.strip() for m in args.modes.split(",") if m.strip()]
    args.work_dir = args.work_dir.resolve()
    backends = [("base", args.backend.resolve())]
    if args.compare:
        backends.append(("compare", args.compare.resolve()))
    for _, bin_dir in backends:
        for exe in ["cargo-flowistry", "flowistry-driver"]:
            if not (bin_dir / exe).is_file():
                parser.error(f"{bin_dir / exe} does not exist")
    if shutil.which("cargo") is None:
        parser.error("cargo is not on PATH; run inside the dev shell (nix develop)")
    args.work_dir.mkdir(parents=True, exist_ok=True)

    registries = registry_dirs(args)
    specs = args.crates or DEFAULT_CRATES
    started = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        reports = list(pool.map(lambda s: smoke_crate(s, args, registries, backends), specs))
    total = time.monotonic() - started

    print(summarize(reports, backends, args, total))
    if args.json:
        args.json.write_text(json.dumps({
            "backends": {n: str(d) for n, d in backends},
            "seed": args.seed, "positions": args.positions, "modes": args.modes,
            "total_seconds": round(total, 1),
            "crates": reports,
        }, indent=1))

    bad = any(rec[n]["status"] in ("crash", "timeout") for r in reports for rec in r["records"]
              for n, _ in backends)
    bad |= any(not rec.get("same", True) for r in reports for rec in r["records"])
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
