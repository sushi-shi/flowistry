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
  oom     the run was killed for exceeding --memory-limit

With --compare, every run is repeated with a second backend build and the
answers are compared after canonicalising the order of `place_info` (the IDE
emits it in hash-map order).

Every run records its peak resident memory (the largest RSS among cargo and the
compiler processes it ran). With --budgets, only the stress positions listed in
scripts/smoke-corpus/budgets.tsv are run, and each must stay within its budget
of peak memory and seconds.

The script only uses the Python standard library but needs `cargo` and the
pinned nightly toolchain, so run it inside the dev shell, e.g.

  nix develop github:sushi-shi/flowistry -c python3 scripts/smoke-real-crates.py \\
      target/smoke-worker/debug

Exit status is 1 if any run crashed, timed out or ran out of memory (or, with
--compare, if the two backends disagree; with --budgets, if a budget is
exceeded), or if a corpus entry was skipped, else 0.
"""

import argparse
import base64
import codecs
import concurrent.futures
import glob
import gzip
import hashlib
import json
import math
import os
import random
import re
import shutil
import signal
import subprocess
import sys
import tarfile
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

# A Rust panic ("thread 'rustc' (…) panicked at …") or an ICE. Bare words would match
# identifiers in timer logs, e.g. sudo-rs's `has_panicked`.
CRASH_MARKER = re.compile(r"^thread '[^']*'(?: \(\d+\))? panicked at |internal compiler error:", re.M)

# Timer output of the backend: `[<time> INFO  rustc_utils::timer] <phase> took <n>s`.
PHASE_LOG = "rustc_utils::timer=info,flowistry_ide=info,flowistry::stats=info"
PHASE_LINE = re.compile(r"\] (.+?) took ([0-9.]+)s$", re.M)
# Counters of the backend: `[<time> INFO  flowistry::stats] stat <name> = <n>`.
STAT_LINE = re.compile(r"\] stat ([\w.]+) = (\d+)$", re.M)

PROBE_FILE = "__flowistry_smoke_probe__.rs"

# The locked corpus: exact crate versions (with the sha256 of their .crate archives),
# the manifest edits and Cargo.lock that make each build offline, and the sampled
# positions. Runs without --crate use it, so results and timings stay comparable
# across time, machines and changes to this script's sampler.
CORPUS_DIR = REPO_ROOT / "scripts" / "smoke-corpus"
CORPUS_FILE = CORPUS_DIR / "corpus.json"

# Stress positions of the corpus with their budgets (see read_budgets).
BUDGETS_FILE = CORPUS_DIR / "budgets.tsv"

# Run statuses that count as failures (exit status 1), worst first.
FAILURES = ["crash", "oom", "timeout"]

# cargo's report of a compiler process killed by SIGKILL, e.g. by the kernel's OOM killer.
SIGKILL_MARKER = re.compile(r"signal: 9, SIGKILL")

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


def cargo_home():
    return Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))


def registry_dirs(args):
    if args.registry:
        return [Path(r) for r in args.registry]
    return [Path(p) for p in sorted(glob.glob(str(cargo_home() / "registry" / "src" / "*")))]


def cached_crate_file(name, version):
    """The downloaded `.crate` archive of a registry crate, if cargo still has it."""
    matches = sorted(glob.glob(str(cargo_home() / "registry" / "cache" / "*" / f"{name}-{version}.crate")))
    return Path(matches[0]) if matches else None


def sha256_file(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def fetch_crate(name, version, work_dir, env):
    """Download an exact crate version into cargo's registry cache (needs the network)."""
    proj = work_dir / "_fetch" / f"{name}-{version}"
    shutil.rmtree(proj, ignore_errors=True)
    (proj / "src").mkdir(parents=True)
    (proj / "src" / "lib.rs").write_text("")
    (proj / "Cargo.toml").write_text(
        '[package]\nname = "flowistry-smoke-fetch"\nversion = "0.0.0"\nedition = "2021"\n\n'
        f'[dependencies]\n{name} = "={version}"\n\n[workspace]\n')
    res = run(["cargo", "fetch"], proj, env)
    if res.returncode != 0:
        raise RuntimeError("cargo fetch failed: " + cargo_error(res.stderr))


def locked_source(entry, args, registries, env):
    """The source directory of a corpus crate, unpacked or fetched if needed.

    The `.crate` archive, when cargo still has it, must match the corpus checksum.
    """
    name, version = entry["name"], entry["version"]
    for attempt in range(2):
        archive = cached_crate_file(name, version)
        if archive is not None and entry.get("sha256") and sha256_file(archive) != entry["sha256"]:
            raise RuntimeError(f"{archive} does not match the corpus checksum")
        found = find_candidates(f"{name}@{version}", registries)
        if found:
            return found[0][1]
        if archive is not None:
            sources = args.work_dir / "_sources"
            sources.mkdir(parents=True, exist_ok=True)
            with tarfile.open(archive, "r:gz") as tar:
                tar.extractall(sources, filter="data")
            return sources / f"{name}-{version}"
        if attempt == 0 and args.fetch:
            fetch_crate(name, version, args.work_dir, env)
            continue
        break
    raise RuntimeError(f"{name} {version} is not in the local cargo registry; rerun with --fetch")


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


def memory_scope(memory_limit, environment=None):
    """The command prefix that runs a command in a transient systemd scope whose memory
    (without swap) is capped at `memory_limit` (e.g. "6G"). When the cap is hit, the kernel
    kills the largest process in the scope (the compiler) and the rest keep running
    (OOMPolicy=continue), so cargo reports the compiler's SIGKILL."""
    if not memory_limit:
        return []
    scope = ["systemd-run", "--user", "--scope", "--quiet", "--collect",
             "-p", f"MemoryMax={memory_limit}", "-p", "MemorySwapMax=0", "-p", "OOMPolicy=continue"]
    # The memory cap must not invent a new semantic environment for every run.
    # Restore the caller's invocation ID (or absence) after entering the scope.
    environment = os.environ if environment is None else environment
    if "INVOCATION_ID" in environment:
        return scope + ["env", "INVOCATION_ID=" + environment["INVOCATION_ID"]]
    return scope + ["env", "-u", "INVOCATION_ID"]


# Runs argv[2:] and writes the peak RSS (KiB) of its process tree to the file descriptor
# argv[1]; exits like its child. A forked process starts with the peak RSS of its parent (the
# kernel keeps the high-water mark of the pre-exec memory), so the harness, whose memory grows
# with the outputs it decodes, cannot measure its children directly: this small process forks
# the command instead.
RSS_WRAPPER = """
import os, signal, subprocess, sys
fd = int(sys.argv[1])
child = subprocess.Popen(sys.argv[2:])
_, status, usage = os.wait4(child.pid, 0)
os.write(fd, str(usage.ru_maxrss).encode())
os.close(fd)
code = os.waitstatus_to_exitcode(status)
if code < 0:
    signal.signal(-code, signal.SIG_DFL)
    os.kill(os.getpid(), -code)
sys.exit(code)
"""


def run(cmd, cwd, env, timeout=None, memory_limit=None):
    """subprocess.run, but a timeout kills the whole process group (cargo and its rustc children),
    and the result records the peak resident memory of the process tree (`max_rss_kb`: the
    largest RSS of the command and the descendants it waited for, from wait4; 0 if the run
    timed out).

    With `memory_limit`, the command runs in a systemd scope capped at that much memory.
    """
    rss_read, rss_write = os.pipe()
    cmd = [sys.executable, "-S", "-c", RSS_WRAPPER, str(rss_write)] + memory_scope(memory_limit, env) + list(cmd)
    try:
        proc = subprocess.Popen(cmd, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                text=True, errors="replace", start_new_session=True, pass_fds=(rss_write,))
    finally:
        os.close(rss_write)
    output = {}

    def drain(name, stream):
        output[name] = stream.read()
        stream.close()

    readers = [threading.Thread(target=drain, args=(name, stream), daemon=True)
               for name, stream in (("stdout", proc.stdout), ("stderr", proc.stderr))]
    for reader in readers:
        reader.start()
    lock = threading.Lock()
    state = {"reaped": False, "timed_out": False}

    def kill():
        with lock:
            if not state["reaped"]:
                state["timed_out"] = True
                os.killpg(proc.pid, signal.SIGKILL)

    timer = threading.Timer(timeout, kill) if timeout is not None else None
    if timer is not None:
        timer.start()
    proc.wait()
    with lock:
        state["reaped"] = True
    if timer is not None:
        timer.cancel()
    for reader in readers:
        reader.join()
    with os.fdopen(rss_read, "rb") as rss:
        reported = rss.read().decode()
    max_rss_kb = int(reported) if reported.isdigit() else 0
    if state["timed_out"]:
        expired = subprocess.TimeoutExpired(cmd, timeout)
        expired.max_rss_kb = max_rss_kb
        raise expired
    res = subprocess.CompletedProcess(cmd, proc.returncode, output.get("stdout", ""), output.get("stderr", ""))
    res.max_rss_kb = max_rss_kb
    return res


def prepare_copy(src, dest, env, fresh, locked=None):
    """Copy a registry crate to `dest` and make it buildable on its own, offline.

    Optional dependencies that cannot be resolved offline are removed (and
    returned); a missing non-optional dependency raises RuntimeError. With
    `locked` ({"dir": corpus entry dir, "prune": [...]}), the corpus' pruned
    dependencies and Cargo.lock are reproduced instead of being resolved.
    """
    marker = dest / ".flowistry-smoke-source"
    key = str(src)
    if locked is not None:
        key += " locked " + sha256_file(locked["dir"] / "Cargo.lock") + " " + " ".join(locked["prune"])
    if not fresh and marker.is_file():
        source, _, pruned = marker.read_text().partition("\n")
        if source == key:
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
    if locked is not None:
        prune = set(locked["prune"])
        pruned = sorted(remove_deps(
            sections, lambda kind, key, pkg, optional: optional and (key in prune or pkg in prune)))
        manifest.write_text(render_manifest(sections))
        shutil.copyfile(locked["dir"] / "Cargo.lock", dest / "Cargo.lock")
        res = run(["cargo", "metadata", "--offline", "--locked", "--format-version", "1"], dest, env)
        if res.returncode != 0:
            raise RuntimeError("the corpus Cargo.lock does not resolve offline: " + cargo_error(res.stderr))
        marker.write_text(key + "\n" + " ".join(pruned))
        return pruned
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
    marker.write_text(key + "\n" + " ".join(pruned))
    return pruned


def lib_target(crate_dir, env):
    """The root source file of the package in `crate_dir` (its library, else its binary),
    and whether that is a binary."""
    res = run(["cargo", "metadata", "--offline", "--no-deps", "--format-version", "1"], crate_dir, env)
    if res.returncode != 0:
        raise RuntimeError("cargo metadata failed: " + cargo_error(res.stderr))
    manifest = (Path(crate_dir) / "Cargo.toml").resolve()
    packages = json.loads(res.stdout)["packages"]
    pkg = next((p for p in packages if Path(p["manifest_path"]).resolve() == manifest), packages[0])
    for kinds in (("lib", "rlib", "proc-macro"), ("bin",)):
        for target in pkg["targets"]:
            if any(k in target["kind"] for k in kinds):
                return Path(target["src_path"]), kinds == ("bin",)
    raise RuntimeError("package has no library or binary target")


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
    # An ICE's panic payload is often just `Box<dyn Any>`; the ICE line itself
    # names the actual cause, so prefer it.
    for line in lines:
        if "internal compiler error" in line:
            return line.strip()
    for i, line in enumerate(lines):
        if "panicked at" in line:
            loc = line.split("panicked at", 1)[1].strip().rstrip(":")
            msg = lines[i + 1].strip() if i + 1 < len(lines) else ""
            return f"panicked at {loc}: {msg}"
    return "no decodable response: " + last_lines(stderr, 2)


OK_PREFIX = '{"Ok":{"place_info":['
RANGE_LIST_FIELDS = ("ranges", "slice", "direct_influence", "maybe_slice")


def canonical_entry(entry, table=None):
    """The canonical JSON of one `place_info` entry (see `canonical`)."""
    def key(x):
        return json.dumps(x, sort_keys=True)

    entry = dict(entry)
    def resolve(index):
        # Reject malformed references rather than accepting Python's negative indices.
        if type(index) is not int or not 0 <= index < len(table):
            raise ValueError(f"invalid range-table index: {index!r}")
        return table[index]

    if table is not None and "range" in entry:
        entry["range"] = resolve(entry["range"])
    for field in RANGE_LIST_FIELDS:
        if field in entry:
            ranges = entry[field]
            if table is not None:
                ranges = [resolve(index) for index in ranges]
            # The editor treats highlight ranges as sets. Duplicate spans and
            # their iteration order do not change the displayed analysis.
            unique = {key(r): r for r in ranges}
            entry[field] = [unique[value] for value in sorted(unique)]
    return key(entry)


def decode_response(encoded, keep_output):
    """Decode a focus response (base64 of gzipped JSON).

    With `keep_output`, returns the parsed response. Otherwise an `Ok` response is not
    kept: it becomes {"Ok": {"digest": ..., "places": n}}, where the digest identifies the
    canonical form of the output (`canonical`), so two outputs have the same digest if and
    only if their canonical forms are equal (barring SHA-256 collisions). The output is
    decompressed and parsed entry by entry, so the harness never holds a whole output: large
    outputs (hundreds of MB of JSON) would otherwise take many GB of Python objects.
    """
    data = base64.b64decode(encoded, validate=True)
    if keep_output:
        return json.loads(gzip.decompress(data))
    decompressor = zlib.decompressobj(wbits=31)
    text_decoder = codecs.getincrementaldecoder("utf-8")()
    chunks = (data[i:i + (1 << 20)] for i in range(0, len(data), 1 << 20))
    finished = False

    def more():
        nonlocal finished
        for chunk in chunks:
            text = text_decoder.decode(decompressor.decompress(chunk))
            if text:
                return text
        finished = True
        return text_decoder.decode(decompressor.flush(), final=True)

    buffer = ""
    while len(buffer) < len(OK_PREFIX) and not finished:
        buffer += more()
    if not buffer.startswith(OK_PREFIX):
        # Not the compact form the backend writes: parse it whole.
        while not finished:
            buffer += more()
        response = json.loads(buffer)
        if "Ok" not in response:
            return response
        if "bodies" in response["Ok"]:
            output = response["Ok"]
            normalized = canonical(output)
            count = sum(len(body.get("focus", {}).get("Ok", {}).get("place_info", []))
                        for body in output["bodies"] if isinstance(body.get("focus"), dict))
            return {"Ok": {"digest": hashlib.sha256(json.dumps(normalized, sort_keys=True).encode()).hexdigest(),
                           "places": count}, "cache": output.get("cache")}
        tail = dict(response["Ok"])
        entries = tail.pop("place_info", [])
        table = tail.pop("ranges", None)
        return output_digest([entry_digest(entry, table) for entry in entries], tail)

    decoder = json.JSONDecoder()
    digests = []
    indexed_entries = []
    pos = len(OK_PREFIX)
    while True:
        while pos < len(buffer) and buffer[pos] in " \t\r\n,":
            pos += 1
        if pos == len(buffer):
            if finished:
                raise ValueError("truncated focus output")
            buffer = buffer[pos:] + more()
            pos = 0
            continue
        if buffer[pos] == "]":
            break
        try:
            entry, end = decoder.raw_decode(buffer, pos)
        except json.JSONDecodeError:
            if finished:
                raise
            # The entry is incomplete: drop what was parsed and read more.
            buffer = buffer[pos:] + more()
            pos = 0
            continue
        if type(entry.get("range")) is int:
            # A reordered object can put its table after place_info. Retain only
            # these compact index lists until the table is available.
            indexed_entries.append(entry)
        else:
            digests.append(entry_digest(entry))
        pos = end
    rest = buffer[pos + 1:]
    while not finished:
        rest += more()
    # `rest` is the end of the output object, e.g. `,"containers":[...]}}`.
    tail = json.loads("{" + rest.lstrip().lstrip(",").rstrip()[:-1])
    table = tail.pop("ranges", None)
    if indexed_entries and table is None:
        raise ValueError("indexed focus output has no range table")
    digests.extend(entry_digest(entry, table) for entry in indexed_entries)
    return output_digest(digests, tail)


def entry_digest(entry, table=None):
    return hashlib.sha256(canonical_entry(entry, table).encode()).hexdigest()


def output_digest(entry_digests, tail):
    """The digest of an output from the digests of its `place_info` entries and its other
    fields (`tail`)."""
    tail = dict(tail)
    if "containers" in tail:
        tail["containers"] = sorted(tail["containers"], key=lambda x: json.dumps(x, sort_keys=True))
    digest = hashlib.sha256()
    for entry in sorted(entry_digests):
        digest.update(entry.encode())
    digest.update(json.dumps(tail, sort_keys=True).encode())
    return {"Ok": {"digest": digest.hexdigest(), "places": len(entry_digests)}}


def flowistry_focus(crate_dir, env, rel_file, line, col, mode, timeout, touch=None, phases=False,
                    memory_limit=None, keep_output=True, command="focus"):
    if phases:
        env = dict(env, RUST_LOG=PHASE_LOG)
    # rustc_plugin clears cargo's cached metadata for library targets only; for a binary,
    # cargo would consider the target fresh and never run the plugin again.
    if touch is not None:
        os.utime(touch)
    cmd = ["cargo", "flowistry", "--context-mode", mode, command, rel_file, str(line), str(col)]
    start = time.monotonic()
    try:
        res = run(cmd, crate_dir, env, timeout, memory_limit)
    except subprocess.TimeoutExpired as expired:
        return {"status": "timeout", "message": f"timed out after {timeout}s",
                "seconds": round(time.monotonic() - start, 2),
                "max_rss_mb": round(getattr(expired, "max_rss_kb", 0) / 1024)}
    seconds = round(time.monotonic() - start, 2)
    max_rss_mb = round(res.max_rss_kb / 1024)
    response = None
    tail = res.stdout.strip().splitlines()
    if tail:
        try:
            response = decode_response(tail[-1].strip(), keep_output)
        except Exception:
            response = None
    stderr = res.stderr
    if memory_limit and response is None and (SIGKILL_MARKER.search(stderr) or res.returncode in (-9, 137)):
        return {"status": "oom", "message": f"killed at the memory limit of {memory_limit}",
                "seconds": seconds, "max_rss_mb": max_rss_mb, "returncode": res.returncode,
                "stderr_tail": stderr[-4000:]}
    if CRASH_MARKER.search(stderr) or response is None:
        return {"status": "crash", "message": crash_signature(stderr), "seconds": seconds,
                "max_rss_mb": max_rss_mb, "returncode": res.returncode, "stderr_tail": stderr[-4000:]}
    timings = parse_phases(stderr) if phases else None
    stats = parse_stats(stderr) if phases else None
    if "Ok" in response:
        output = response["Ok"]
        places = output["places"] if "digest" in output else len(output.get("place_info", []))
        if "bodies" in output:
            places = sum(len(body.get("focus", {}).get("Ok", {}).get("place_info", []))
                         for body in output["bodies"] if isinstance(body.get("focus"), dict))
        return {"status": "ok", "seconds": seconds, "max_rss_mb": max_rss_mb, "output": output,
                "phases": timings, "stats": stats, "places": places,
                "cache": response.get("cache", output.get("cache"))}
    err = response.get("Err", response)
    message = err.get("error") or err.get("type") or json.dumps(err)
    status = "benign" if any(b in message for b in BENIGN_ERRORS) else "error"
    return {"status": status, "message": message, "seconds": seconds, "max_rss_mb": max_rss_mb,
            "phases": timings, "stats": stats}


def parse_phases(stderr):
    """Sum the backend's `<phase> took <n>s` timer lines by phase name.

    Per-body timers (`get_bodies_with_borrowck_facts for <body>`) are summed under one name.
    """
    totals = {}
    for m in PHASE_LINE.finditer(stderr):
        name = re.sub(r" for .*$", "", m[1])
        totals[name] = totals.get(name, 0.0) + float(m[2])
    return {k: round(v, 4) for k, v in totals.items()}


def parse_stats(stderr):
    """Sum the backend's `stat <name> = <n>` counter lines by name (one line per analyzed body
    and counter, see `FlowResults::stats`)."""
    totals = {}
    for m in STAT_LINE.finditer(stderr):
        totals[m[1]] = totals.get(m[1], 0) + int(m[2])
    return totals


def focus_repeated(crate_dir, env, rel_file, line, col, mode, args, touch=None):
    """Run a position `--repeat` times; keep the fastest run (the least disturbed by noise),
    unless some run failed, in which case that failure is the result."""
    runs = [flowistry_focus(crate_dir, env, rel_file, line, col, mode, args.timeout, touch, args.phases,
                            args.memory_limit, args.keep_outputs, getattr(args, "command", "focus"))
            for _ in range(max(1, args.repeat))]
    worst = {"crash": 0, "oom": 1, "timeout": 2, "error": 3}
    failed = sorted((r for r in runs if r["status"] in worst), key=lambda r: worst[r["status"]])
    if failed:
        return failed[0]
    best = min(runs, key=lambda r: r["seconds"])
    if len(runs) > 1:
        best["all_seconds"] = [r["seconds"] for r in runs]
    return best


def canonical(output):
    """Order-insensitive form of a focus output, for comparing two backends."""
    def key(x):
        return json.dumps(x, sort_keys=True)

    if not isinstance(output, dict) or "digest" in output:
        return output
    out = dict(output)
    if "bodies" in out:
        out.pop("cache", None)
        bodies = []
        for body in out["bodies"]:
            body = dict(body)
            body.pop("cached", None)
            if isinstance(body.get("focus"), dict) and "Ok" in body["focus"]:
                body["focus"] = dict(body["focus"], Ok=canonical(body["focus"]["Ok"]))
            bodies.append(body)
        out["bodies"] = sorted(bodies, key=key)
        return out
    table = out.pop("ranges", None)
    places = [json.loads(canonical_entry(p, table)) for p in output.get("place_info", [])]
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


def read_positions(path):
    """Positions stored as `file<TAB>line<TAB>column` (0-based line, as passed to focus)."""
    positions = []
    for row in path.read_text().splitlines():
        if row and not row.startswith("#"):
            rel, line, col = row.split("\t")
            positions.append((rel, int(line), int(col)))
    return positions


def write_positions(path, positions):
    rows = ["# file\tline (0-based)\tcolumn; generated by smoke-real-crates.py --update-corpus"]
    rows += [f"{rel}\t{line}\t{col}" for rel, line, col in positions]
    path.write_text("\n".join(rows) + "\n")


def in_nested_package(root, rel):
    """Whether `rel` lies in another package nested under `root` (a directory with its own
    Cargo.toml). rustc_plugin cannot tell such packages apart when the outer one has a build
    script (\"Too many matching targets\"), and they are not this package's code anyway."""
    d = (root / rel).parent
    while d != root:
        if (d / "Cargo.toml").is_file():
            return True
        d = d.parent
    return False


def compiled_files(crate_dir, lib_dir, env, timeout, touch=None, workspace_root=None):
    """Probe the backend with an uncompiled file to learn which files the crate compiles.

    The probe also builds dependencies and checks that the crate builds at all.
    """
    probe = lib_dir / PROBE_FILE
    probe.write_text("")
    try:
        rel = os.path.relpath(probe, crate_dir)
        result = flowistry_focus(crate_dir, env, rel, 0, 0, "SigOnly", timeout, touch)
    finally:
        probe.unlink(missing_ok=True)
    msg = result.get("message", "")
    m = re.search(r"Available SourceFiles were: \[(.*)\]", msg, re.S)
    if not m:
        raise RuntimeError(f"probe did not list source files ({result['status']}): {msg[:500]}")
    root = crate_dir.resolve()
    # rustc names files relative to the workspace root, which may be above the package.
    bases = [root] + ([Path(workspace_root).resolve()] if workspace_root is not None else [])
    files = set()
    for name in m[1].split(", "):
        path = Path(name) if os.path.isabs(name) else next(
            (b / name for b in bases if (b / name).is_file()), root / name)
        try:
            rel = path.resolve().relative_to(root)
        except (ValueError, OSError):
            continue  # dependency or std file
        if rel.parts[0] == "target" or rel.suffix != ".rs" or not (root / rel).is_file():
            continue
        if in_nested_package(root, rel):
            continue  # another workspace member nested inside this package's directory
        files.add(str(rel))
    return sorted(files)


# --------------------------------------------------------------------------
# Per-crate driver


def smoke_crate(spec, args, registries, backends):
    """Run one crate. `spec` is a --crate string, or a corpus entry (a dict)."""
    entry = spec if isinstance(spec, dict) else None
    if entry is not None and "git" in entry:
        return smoke_git_entry(entry, args, backends)
    report = {"spec": spec if entry is None else f"{entry['name']}@{entry['version']}",
              "crate": None, "skipped": [], "records": [], "files": []}
    entry_dir = locked = None
    if entry is not None:
        label = f"{entry['name']}-{entry['version']}"
        entry_dir = CORPUS_DIR / label
        try:
            src = locked_source(entry, args, registries,
                                backend_env(backends[0][1], args.work_dir / "_fetch" / "target"))
        except Exception as e:  # noqa: BLE001 - reported as a skipped crate
            report["skipped"].append((label, str(e)))
            return report
        candidates = [(label, src)]
        if not args.update_corpus:
            if not (entry_dir / "Cargo.lock").is_file():
                report["skipped"].append((label, f"{entry_dir}/Cargo.lock is missing; run --update-corpus"))
                return report
            locked = {"dir": entry_dir, "prune": entry.get("prune", [])}
    else:
        candidates = find_candidates(spec, registries)
    if not candidates:
        report["skipped"].append((spec, "not found in the cargo registry"))
        return report
    for label, src in candidates[: args.version_attempts]:
        dest = args.work_dir / label
        base_env = backend_env(backends[0][1], dest / "target" / "smoke-base")
        try:
            pruned = prepare_copy(src, dest, base_env, args.fresh or args.update_corpus, locked)
            root_src, is_bin = lib_target(dest, base_env)
            touch = root_src if is_bin else None
            files = compiled_files(dest, root_src.parent, base_env, args.timeout, touch)
            if args.compare:
                # Warm up the second backend's target dir too.
                compiled_files(dest, root_src.parent,
                               backend_env(backends[1][1], dest / "target" / "smoke-cmp"), args.timeout, touch)
        except Exception as e:  # noqa: BLE001 - any failure means "try the next version"
            log(f"[{label}] skipped: {e}")
            report["skipped"].append((label, str(e)))
            continue
        report.update(crate=label, name=entry["name"] if entry is not None else None, dir=str(dest),
                      files=files, pruned=pruned, touch=str(touch) if touch else None)
        if pruned:
            log(f"[{label}] pruned optional dependencies unavailable offline: {', '.join(pruned)}")
        break
    if report["crate"] is None:
        return report
    if entry is not None and args.update_corpus:
        entry_dir.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(Path(report["dir"]) / "Cargo.lock", entry_dir / "Cargo.lock")
        archive = cached_crate_file(entry["name"], entry["version"])
        entry["prune"] = report["pruned"]
        entry["sha256"] = sha256_file(archive) if archive is not None else entry.get("sha256")
        if archive is None:
            log(f"[{report['crate']}] no .crate archive in the cargo cache; checksum not recorded")
    return run_positions(report, entry_dir, Path(report["dir"]), args, backends)


def git(argv, cwd):
    return subprocess.run(["git", *argv], cwd=cwd, capture_output=True, text=True)


def resolve_git_ref(url, ref):
    res = git(["ls-remote", url, ref], REPO_ROOT)
    for line in res.stdout.splitlines():
        sha, name = line.split("\t")
        if name in (ref, f"refs/heads/{ref}", f"refs/tags/{ref}"):
            return sha
    raise RuntimeError(f"cannot resolve {ref} in {url}: {res.stderr.strip() or 'no such ref'}")


def git_checkout(entry, args, env):
    """A checkout of a git corpus entry at its pinned commit.

    Cloning and downloading dependencies need the network, so they only happen with
    --fetch or --update-corpus; the repository's own Cargo.lock pins dependencies.
    Returns the cargo workspace root: the checkout, or its `root` subdirectory.
    """
    dest = args.work_dir / "_git" / entry["name"]
    head = git(["rev-parse", "HEAD"], dest).stdout.strip() if (dest / ".git").is_dir() else None
    online = args.fetch or args.update_corpus
    if head != entry["rev"]:
        if not online:
            raise RuntimeError(f"{entry['git']} is not checked out at {entry['rev'][:12]}; rerun with --fetch")
        if not (dest / ".git").is_dir():
            dest.mkdir(parents=True, exist_ok=True)
            git(["init", "-q"], dest)
            git(["remote", "add", "origin", entry["git"]], dest)
        for argv in (["fetch", "-q", "--depth", "1", "origin", entry["rev"]],
                     ["checkout", "-q", "--force", "--detach", "FETCH_HEAD"]):
            res = git(argv, dest)
            if res.returncode != 0:
                raise RuntimeError(f"git {argv[0]} failed: {res.stderr.strip()}")
    if online and entry.get("submodules"):
        res = git(["submodule", "update", "-q", "--init", "--recursive", "--depth", "1"], dest)
        if res.returncode != 0:
            raise RuntimeError(f"git submodule update failed: {res.stderr.strip()}")
    # The cargo workspace may live in a subdirectory of the repository (`root`).
    dest = dest / entry.get("root", ".")
    # The checkout lies inside this repository, whose workspace cargo would otherwise use.
    manifest = dest / "Cargo.toml"
    text = manifest.read_text()
    if not re.search(r"^\[workspace\]", text, re.M):
        manifest.write_text(text + "\n[workspace]\n")
    fetch_env = dict(env)
    fetch_env.pop("CARGO_NET_OFFLINE", None)
    # Repositories that do not commit a Cargo.lock (e.g. libraries) get one stored in the
    # corpus, so their dependencies are pinned too.
    if git(["ls-files", "--error-unmatch", "Cargo.lock"], dest).returncode != 0:
        stored = CORPUS_DIR / entry["name"] / "Cargo.lock"
        if args.update_corpus:
            res = run(["cargo", "generate-lockfile"], dest, fetch_env)
            if res.returncode != 0:
                raise RuntimeError("cargo generate-lockfile failed: " + cargo_error(res.stderr))
            stored.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(dest / "Cargo.lock", stored)
        elif stored.is_file():
            shutil.copyfile(stored, dest / "Cargo.lock")
        else:
            raise RuntimeError(f"{stored} is missing; run --update-corpus")
    if online:
        res = run(["cargo", "fetch", "--locked"], dest, fetch_env)
        if res.returncode != 0:
            raise RuntimeError("cargo fetch --locked failed: " + cargo_error(res.stderr))
    return dest


def smoke_git_entry(entry, args, backends):
    """Run a corpus entry that is a git repository pinned to a commit."""
    report = {"spec": entry["git"], "crate": None, "skipped": [], "records": [], "files": []}
    try:
        if args.update_corpus and entry.get("ref") and (args.bump or not entry.get("rev")):
            entry["rev"] = resolve_git_ref(entry["git"], entry["ref"])
        if not entry.get("rev"):
            raise RuntimeError("no pinned rev; run --update-corpus")
        label = f"{entry['name']}-{entry['rev'][:12]}"
        entry_dir = CORPUS_DIR / entry["name"]
        checkout = git_checkout(entry, args, backend_env(backends[0][1], args.work_dir / "_fetch" / "target"))
        dest = checkout / entry.get("package", ".")
        # Per-entry build environment, e.g. CFLAGS for old C code or --cap-lints for old crates.
        extra_env = entry.get("env", {})
        base_env = dict(backend_env(backends[0][1], checkout / "target" / "smoke-base"), **extra_env)
        root_src, is_bin = lib_target(dest, base_env)
        touch = root_src if is_bin else None
        files = compiled_files(dest, root_src.parent, base_env, args.timeout, touch, checkout)
        if args.compare:
            compiled_files(dest, root_src.parent,
                           dict(backend_env(backends[1][1], checkout / "target" / "smoke-cmp"), **extra_env),
                           args.timeout,
                           touch, checkout)
    except Exception as e:  # noqa: BLE001 - reported as a skipped crate
        report["skipped"].append((entry["name"], str(e)))
        log(f"[{entry['name']}] skipped: {e}")
        return report
    report.update(crate=label, name=entry["name"], dir=str(dest), target_root=str(checkout), files=files,
                  pruned=[], env=entry.get("env", {}),
                  touch=str(touch) if touch else None)
    return run_positions(report, entry_dir, dest, args, backends)


def run_positions(report, entry_dir, dest, args, backends):
    """Pick (or load) the positions of a prepared crate and run the backend(s) on them."""
    label = report["crate"]
    positions_file = entry_dir / "positions.tsv" if entry_dir is not None else None
    if positions_file is not None and not args.update_corpus:
        if not positions_file.is_file():
            report["skipped"].append((label, f"{positions_file} is missing; run --update-corpus"))
            report["crate"] = None
            return report
        positions = read_positions(positions_file)
    else:
        positions = sample_positions(dest, report["files"], args.positions, args.seed,
                                     label.rsplit("-", 1)[0])
        if positions_file is not None:
            entry_dir.mkdir(parents=True, exist_ok=True)
            write_positions(positions_file, positions)
    if args.budgets is not None:
        # Only the stress positions of this entry, each in its own mode.
        runs = [(b["file"], b["line"], b["column"], [b["mode"]])
                for b in args.budgets if b["crate"] == report.get("name")]
    else:
        runs = [(rel, line, col, args.modes) for rel, line, col in positions]
    log(f"[{label}] {len(report['files'])} compiled files, {len(runs)} positions")
    if args.prepare_only:
        report["seconds"] = 0.0
        return report
    target_root = Path(report.get("target_root", dest))
    envs = [(name, dict(backend_env(bin_dir, target_root / "target" / ("smoke-base" if i == 0 else "smoke-cmp")),
                        **report.get("env", {})))
            for i, (name, bin_dir) in enumerate(backends)]
    checkpoints = getattr(args, "checkpoints", None)
    source_identity = None
    if checkpoints:
        from smoke_checkpoint import tree_digest
        source_identity = tree_digest(target_root)
    for name, env in envs:
        if getattr(args, "cache_dir", None):
            env["FLOWISTRY_CACHE_DIR"] = str(args.cache_dir / name)
        cache_mode = getattr(args, f"{name}_cache", "inherit")
        if cache_mode != "inherit":
            env["FLOWISTRY_CACHE"] = "on" if cache_mode == "warm" else cache_mode
    started = time.monotonic()
    for idx, (rel, line, col, modes) in enumerate(runs):
        for mode in modes:
            rec = {"crate": label, "file": rel, "line": line, "column": col, "mode": mode}
            checkpoint_key = dict(rec, source=source_identity)
            if checkpoints:
                if tree_digest(target_root) != source_identity:
                    raise RuntimeError(f"{label}: source inputs changed during validation")
                previous = checkpoints.load(checkpoint_key)
                if previous is not None:
                    previous["checkpoint_reused"] = True
                    report["records"].append(previous)
                    continue
            rec["recorded_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
            for name, env in envs:
                # Inside a workspace member, cargo-flowistry resolves relative paths from the
                # package but the driver from the workspace root; an absolute path works for both.
                file_arg = str(dest / rel) if report.get("target_root") not in (None, str(dest)) else rel
                warmup = None
                if getattr(args, f"{name}_cache", "inherit") == "warm":
                    warmup = focus_repeated(dest, env, file_arg, line, col, mode, args, report.get("touch"))
                result = focus_repeated(dest, env, file_arg, line, col, mode, args, report.get("touch"))
                if warmup is not None:
                    result["warmup_status"] = warmup["status"]
                    if warmup["status"] != "ok":
                        result["warmup_message"] = warmup.get("message")
                rec[name] = result
                if result["status"] in FAILURES:
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
                    output = rec[name].pop("output", None)
                    if isinstance(output, dict) and "digest" in output:
                        rec[name]["output_digest"] = output["digest"]
            if checkpoints:
                if tree_digest(target_root) != source_identity:
                    raise RuntimeError(f"{label}: source inputs changed during validation")
                checkpoints.save(checkpoint_key, rec)
            report["records"].append(rec)
        if (idx + 1) % 10 == 0:
            log(f"[{label}] {idx + 1}/{len(runs)} positions")
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
             f"{'crash':>5} {'oom':>4} {'t/o':>4} {'maxMB':>6} {'secs':>7}"
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
                      for s in ["ok", "benign", "error", "crash", "oom", "timeout"]}
            npos = len({(x["file"], x["line"], x["column"]) for x in recs})
            max_rss = max((x[name].get("max_rss_mb", 0) for x in recs), default=0)
            row = f"{r['crate']:<22} {len(r['files']):>5} {npos:>4} {len(recs):>5} {counts['ok']:>5} " \
                  f"{counts['benign']:>6} {counts['error']:>5} {counts['crash']:>5} {counts['oom']:>4} " \
                  f"{counts['timeout']:>4} {max_rss:>6} {r.get('seconds', 0):>7}"
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
    order = {"crash": 0, "oom": 1, "timeout": 2, "error": 3, "benign": 4}
    for (status, name, msg), recs in sorted(groups.items(), key=lambda kv: (order[kv[0][0]], -len(kv[1]))):
        suffix = f" [{name}]" if len(names) == 2 else ""
        out.append(f"{status.upper()}{suffix} x{len(recs)}: {msg}")
        if status in FAILURES + ["error"]:
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
    if args.phases or args.repeat > 1 or len(names) == 2:
        out.append("")
        out += timing_summary(reports, names)
    if args.budgets is not None:
        out.append("")
        out += budget_summary(reports, names, args.budgets)[0]
    out.append(f"total runtime: {total_seconds:.0f}s")
    return "\n".join(out)


def timing_summary(reports, names):
    """Wall time, peak memory, per-phase totals and backend counters over runs that succeeded
    for every backend.

    With two backends, also the ratio of totals and the geometric mean of per-run ratios
    (so a few huge positions do not hide a change on typical ones). For peak memory, the
    "total" is the largest peak over the runs.
    """
    runs = [rec for r in reports for rec in r["records"]
            if all(rec[n]["status"] == "ok" for n in names)]
    out = [f"timing over {len(runs)} run(s) that succeeded for every backend:"]
    phase_names = sorted({p for rec in runs for n in names for p in (rec[n].get("phases") or {})})
    stat_names = sorted({s for rec in runs for n in names for s in (rec[n].get("stats") or {})})
    # (label, value of one run, unit, how to combine the runs)
    rows = [("wall", lambda res: res["seconds"], "s", sum),
            ("peak RSS", lambda res: res.get("max_rss_mb", 0), "M", max)]
    rows += [(p, lambda res, p=p: (res.get("phases") or {}).get(p, 0.0), "s", sum) for p in phase_names]
    rows += [(f"stat {s}", lambda res, s=s: (res.get("stats") or {}).get(s, 0), "", sum) for s in stat_names]
    header = f"  {'phase':<34}" + "".join(f" {n:>12}" for n in names)
    if len(names) == 2:
        header += f" {'ratio':>7} {'geomean':>8}"
    out.append(header)
    for label, get, unit, combine in rows:
        totals = [combine([get(rec[n]) for rec in runs] or [0]) for n in names]
        line = f"  {label:<34}" + "".join(
            f" {t:>11.2f}{unit}" if unit == "s" else f" {t:>11}{unit or ' '}" for t in totals)
        if len(names) == 2:
            pairs = [(get(rec[names[0]]), get(rec[names[1]])) for rec in runs]
            pairs = [(a, b) for a, b in pairs if a > 0 and b > 0]
            ratio = totals[1] / totals[0] if totals[0] else float("nan")
            geo = (2 ** (sum(math.log2(b / a) for a, b in pairs) / len(pairs))) if pairs else float("nan")
            line += f" {ratio:>7.3f} {geo:>8.3f}"
        out.append(line)
    return out


def read_budgets(path):
    """The stress positions and their budgets, one per line:
    `crate<TAB>mode<TAB>file<TAB>line<TAB>column<TAB>max RSS (MiB)<TAB>max seconds`.

    `crate` is a corpus entry name; `line` is 0-based, as passed to focus. A run within
    budget answers {"Ok": ...} with a peak RSS and a wall time no larger than the budget.
    """
    budgets = []
    for row in path.read_text().splitlines():
        if not row.strip() or row.startswith("#"):
            continue
        crate, mode, rel, line, col, max_rss_mb, max_seconds = row.split("\t")
        budgets.append({"crate": crate, "mode": mode, "file": rel, "line": int(line), "column": int(col),
                        "max_rss_mb": int(max_rss_mb), "max_seconds": float(max_seconds)})
    return budgets


def budget_summary(reports, names, budgets):
    """Check every stress run against its budget. Returns (report lines, any exceeded)."""
    out = ["budgets (peak RSS MiB / seconds):"]
    exceeded = False
    records = {(r.get("name"), rec["mode"], rec["file"], rec["line"], rec["column"]): rec
               for r in reports for rec in r["records"]}
    for b in budgets:
        rec = records.get((b["crate"], b["mode"], b["file"], b["line"], b["column"]))
        where = f"{b['crate']} {b['mode']} {b['file']}:{b['line']}:{b['column']}"
        for name in names:
            res = rec[name] if rec is not None else None
            if res is None:
                verdict, detail = "MISSING", "not run"
            else:
                rss, secs = res.get("max_rss_mb", 0), res["seconds"]
                ok = res["status"] == "ok" and rss <= b["max_rss_mb"] and secs <= b["max_seconds"]
                verdict = "ok" if ok else "OVER"
                detail = f"{res['status']} {rss}/{b['max_rss_mb']} MiB, {secs}/{b['max_seconds']} s"
            exceeded |= verdict != "ok"
            suffix = f" [{name}]" if len(names) == 2 else ""
            out.append(f"  {verdict:<7} {where}{suffix}: {detail}")
    return out, exceeded


def checked_smoke_crate(spec, args, registries, backends):
    """Retain an explicit failed entry if harness preparation/checkpointing fails."""
    try:
        return smoke_crate(spec, args, registries, backends)
    except Exception as error:
        name = spec.get('name') if isinstance(spec, dict) else str(spec)
        message = f'harness failure ({type(error).__name__}): {error}'
        log(f'[{name}] {message}')
        return {'name': name, 'spec': spec, 'crate': None, 'files': [], 'records': [],
                'skipped': [(name, message)]}


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
                             "(repeatable; default: the locked corpus in scripts/smoke-corpus)")
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
    parser.add_argument("--update-corpus", action="store_true",
                        help="re-resolve the locked corpus (scripts/smoke-corpus): re-prune dependencies, "
                             "regenerate each Cargo.lock, resample positions, record checksums")
    parser.add_argument("--bump", action="store_true",
                        help="with --update-corpus, move git entries to the current head of their ref "
                             "(otherwise their pinned commit is kept)")
    parser.add_argument("--prepare-only", action="store_true",
                        help="prepare crates (and with --update-corpus, write the corpus) without "
                             "running the analysis")
    parser.add_argument("--fetch", action="store_true",
                        help="download corpus crates and dependencies that are not available offline")
    parser.add_argument("--phases", action="store_true",
                        help="record the backend's per-phase timers for every run (a timing section is "
                             "added to the report; use release builds of the backend)")
    parser.add_argument("--repeat", type=int, default=1,
                        help="run every position N times and keep the fastest, to reduce timing noise")
    parser.add_argument("--memory-limit", metavar="SIZE",
                        help="cap the memory (without swap) of every focus run at SIZE, e.g. 6G, by running "
                             "it in a systemd user scope; a run killed at the cap has the status `oom`")
    parser.add_argument("--budgets", action="store_true",
                        help=f"run only the stress positions in {BUDGETS_FILE.relative_to(REPO_ROOT)}, one crate "
                             "at a time, and check each against its peak-memory and time budget")
    parser.add_argument("--skip", action="append", default=[], metavar="NAME",
                        help="leave the corpus entry NAME out (repeatable)")
    parser.add_argument("--json", type=Path, metavar="FILE", help="write every run record to FILE")
    parser.add_argument("--cache-dir", type=Path, help="isolated cache root, required for explicit cache reuse checks")
    parser.add_argument("--command", choices=("focus", "file-focus"), default="focus",
                        help="protocol to compare at each locked position")
    for backend_name in ("base", "compare"):
        parser.add_argument(f"--{backend_name}-cache", choices=("inherit", "off", "refresh", "on", "warm"),
                            default="inherit", help="cache policy; warm primes the position before recording it")
    parser.add_argument("--checkpoint-dir", type=Path,
                        help="resume correctness runs with the same binaries, inputs and settings; not for timing")
    parser.add_argument("--keep-outputs", action="store_true", help="include full focus outputs in --json")
    parser.add_argument("--examples", type=int, default=5, help="example positions shown per problem group")
    args = parser.parse_args()

    args.modes = [m.strip() for m in args.modes.split(",") if m.strip()]
    if any(mode in ("refresh", "on", "warm") for mode in (args.base_cache, args.compare_cache)) and not args.cache_dir:
        parser.error("explicit cache reuse checks require --cache-dir")
    if args.cache_dir:
        args.cache_dir = args.cache_dir.resolve()
    if args.checkpoint_dir:
        # Nix launchers allocate fresh temporary paths each time. Use one real,
        # stable directory for the compiler rather than ignoring observable env.
        scratch = args.checkpoint_dir.resolve() / "tmp"
        scratch.mkdir(parents=True, exist_ok=True)
        for name in ("TMPDIR", "TMP", "TEMP", "TEMPDIR", "NIX_BUILD_TOP"):
            os.environ[name] = str(scratch)
    if args.memory_limit and shutil.which("systemd-run") is None:
        parser.error("--memory-limit needs systemd-run")
    if args.budgets:
        if args.update_corpus:
            parser.error("--budgets cannot be combined with --update-corpus")
        args.budgets = read_budgets(BUDGETS_FILE)
        # Stress runs must never overlap: they are the runs most likely to exhaust memory.
        args.jobs = 1
    else:
        args.budgets = None
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
    corpus = json.loads(CORPUS_FILE.read_text()) if CORPUS_FILE.is_file() else None
    if args.crates and not args.update_corpus:
        # Corpus entries by name (locked); anything else is an ad-hoc NAME[@VERSION] or path.
        by_name = {e["name"]: e for e in (corpus or {}).get("crates", [])}
        specs = [dict(by_name[c]) if c in by_name else c for c in args.crates]
        if corpus is not None:
            args.seed, args.positions = corpus["seed"], corpus["positions"]
    elif corpus is not None:
        if args.update_corpus:
            # Entries are updated in place; with --crate, only the named ones.
            specs = [e for e in corpus["crates"] if not args.crates or e["name"] in args.crates]
            unknown = set(args.crates or []) - {e["name"] for e in corpus["crates"]}
            if unknown:
                parser.error(f"not in the corpus: {', '.join(sorted(unknown))}")
            if not args.crates:
                corpus["seed"], corpus["positions"] = args.seed, args.positions
        else:
            specs = [dict(e) for e in corpus["crates"]]
        args.seed, args.positions = corpus["seed"], corpus["positions"]
    elif args.update_corpus:
        # Start a corpus from the newest registry version of each default crate.
        specs = []
        for name in DEFAULT_CRATES:
            found = find_candidates(name, registries)
            if not found:
                parser.error(f"{name} is not in the local cargo registry")
            specs.append({"name": name, "version": found[0][0][len(name) + 1:]})
        corpus = {"seed": args.seed, "positions": args.positions}
    else:
        parser.error(f"{CORPUS_FILE} does not exist; create it with --update-corpus")
    if args.skip and not args.update_corpus:
        specs = [s for s in specs if not (isinstance(s, dict) and s["name"] in args.skip)]
    if args.budgets is not None:
        budgeted = {b["crate"] for b in args.budgets}
        specs = [s for s in specs if isinstance(s, dict) and s["name"] in budgeted]
    args.checkpoints = None
    if args.checkpoint_dir:
        if args.update_corpus or args.prepare_only or args.keep_outputs or args.repeat != 1:
            parser.error("checkpoints require a fixed correctness run (no update, prepare, full output, or repeats)")
        if any(not isinstance(spec, dict) for spec in specs):
            parser.error("checkpoints require locked corpus entries")
        from smoke_checkpoint import Checkpoints, manifest
        args.checkpoints = Checkpoints(args.checkpoint_dir, manifest(args, backends, CORPUS_DIR, __file__))
    started = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        reports = list(pool.map(lambda s: checked_smoke_crate(s, args, registries, backends), specs))
    total = time.monotonic() - started
    if args.update_corpus:
        if not args.crates:
            corpus["crates"] = specs
        CORPUS_DIR.mkdir(parents=True, exist_ok=True)
        CORPUS_FILE.write_text(json.dumps(corpus, indent=2) + "\n")
        log(f"wrote {CORPUS_FILE}")

    print(summarize(reports, backends, args, total))
    if args.json:
        args.json.write_text(json.dumps({
            "backends": {n: str(d) for n, d in backends},
            "seed": args.seed, "positions": args.positions, "modes": args.modes,
            "total_seconds": round(total, 1),
            "validation_manifest": args.checkpoints.manifest if args.checkpoints else None,
            "checkpoint_id": args.checkpoints.identity if args.checkpoints else None,
            "command": args.command,
            "cache_modes": {"base": args.base_cache, "compare": args.compare_cache},
            "crates": reports,
        }, indent=1))

    bad = any(r["skipped"] for r in reports)
    bad |= any(rec[n]["status"] in FAILURES for r in reports for rec in r["records"]
              for n, _ in backends)
    bad |= any(rec[n].get("warmup_status", "ok") not in ("ok", "benign")
               for r in reports for rec in r["records"] for n, _ in backends)
    bad |= any(not rec.get("same", True) for r in reports for rec in r["records"])
    if args.budgets is not None:
        bad |= budget_summary(reports, [n for n, _ in backends], args.budgets)[1]
    if args.checkpoints:
        args.checkpoints.close()
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
