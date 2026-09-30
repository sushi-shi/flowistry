"""Fixture and independent-oracle support for edit/concurrency regression runs."""
import base64
import gzip
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time

from smoke_checkpoint import atomic_json, digest, file_digest

spec = importlib.util.spec_from_file_location('matrix_smoke', Path(__file__).with_name('smoke-real-crates.py'))
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)

SOURCE = '''mod child;
use std::{cell::RefCell, rc::Rc};
pub struct State { pub a: i32, pub b: i32 }
impl State {
 fn update(&mut self, input: i32) { child::apply(self, input); }
}
trait Choice { fn choice(&self) -> i32; }
impl Choice for State { fn choice(&self) -> i32 { self.a } }
fn unrelated() -> i32 { 2 }
fn cycle_a(n: i32) -> i32 { if n > 0 { cycle_b(n - 1) } else { n } }
fn cycle_b(n: i32) -> i32 { if n > 0 { cycle_a(n - 1) } else { n } }
#[cfg(feature="alternate")] fn configured() -> i32 { 2 }
#[cfg(not(feature="alternate"))] fn configured() -> i32 { 1 }
pub fn selected(input: i32) -> i32 {
 let mut state = State { a: 1, b: 2 };
 let café = input + matrix_dep::value() + matrix_macros::number!() + configured();
 let text = include_str!("included.txt");
 let env = option_env!("FLOWISTRY_MATRIX_ENV").unwrap_or("");
 let build = env!("MATRIX_BUILD_VALUE");
 state.update(café + text.len() as i32 + env.len() as i32 + build.len() as i32);
 let handle = Rc::new(RefCell::new(input));
 let alias = handle.clone();
 *handle.borrow_mut() = café;
 let mapper = |x: i32| x + 1;
 let untouched = state.a;
 let affected = state.b + *alias.borrow() + state.choice();
 mapper(untouched + affected)
}
'''

FILES = {
    'project/Cargo.toml': '''[package]
name="matrix_fixture"
version="0.0.0"
edition="2021"
[features]
default=[]
alternate=[]
[dependencies]
matrix_dep={path="../dependency"}
matrix_macros={path="../macros"}
[workspace]
''',
    'project/src/lib.rs': SOURCE,
    'project/src/child.rs': 'pub fn apply(state: &mut super::State, input: i32) { state.b = input; }\n',
    'project/src/included.txt': 'included input\n',
    'project/build-input.txt': 'build one',
    'project/build.rs': '''fn main() {
 println!("cargo:rerun-if-changed=build-input.txt");
 let value = std::fs::read_to_string("build-input.txt").unwrap();
 println!("cargo:rustc-env=MATRIX_BUILD_VALUE={value}");
}
''',
    'dependency/Cargo.toml': '[package]\nname="matrix_dep"\nversion="0.0.0"\nedition="2021"\n',
    'dependency/src/lib.rs': 'pub fn value() -> i32 { 7 }\n',
    'macros/Cargo.toml': '[package]\nname="matrix_macros"\nversion="0.0.0"\nedition="2021"\n[lib]\nproc-macro=true\n',
    'macros/src/lib.rs': '''extern crate proc_macro;
#[proc_macro] pub fn number(_: proc_macro::TokenStream) -> proc_macro::TokenStream {
 "7".parse().unwrap()
}
''',
}


def decode(stdout):
    lines = stdout.strip().splitlines()
    if not lines:
        return None
    try:
        return json.loads(gzip.decompress(base64.b64decode(lines[-1])))
    except (ValueError, OSError):
        return None


def observation(completed, seconds):
    value = decode(completed.stdout)
    stderr = completed.stderr.decode(errors='replace')
    result = {'exit_code': completed.returncode, 'seconds': seconds,
              'compiler_invocations': len(re.findall(r'audit compiler\b', stderr)),
              'solved_bodies': re.findall(r'audit solve (focus|shared|summary) (.+)', stderr),
              'cache_hits': re.findall(r'Focus cache hit: (.+)', stderr),
              'cache_misses': re.findall(r'Focus cache miss: (.+)', stderr),
              'response': value, 'stderr': stderr}
    if value and isinstance(value.get('Ok'), dict):
        result['semantic_digest'] = digest(smoke.canonical(value['Ok']))
        result['cache'] = value['Ok'].get('cache', {})
    return result


def assert_equivalent(reused, fresh):
    """An exit/error match alone is insufficient to prove successful equivalence."""
    if fresh['exit_code'] != 0:
        if reused['exit_code'] == 0:
            raise AssertionError('reuse accepted a revision rejected by the fresh compiler')
        return
    if fresh.get('semantic_digest') is None or reused['exit_code'] != 0:
        raise AssertionError('a successful fresh response must have a successful reuse response')
    if reused.get('semantic_digest') != fresh['semantic_digest']:
        raise AssertionError('reused output differs from independent cache-off analysis')


class Matrix:
    def __init__(self, backend, root, timeout=120):
        self.backend = Path(backend).resolve()
        self.root = Path(root).resolve()
        self.root.mkdir(parents=True, exist_ok=False)
        self.project = self.root / 'project'
        self.source = self.project / 'src/lib.rs'
        self.timeout = timeout
        self.changed = set()
        self.environment = {}
        self.rustfmt = 'rustfmt'
        self.records = []
        self.reset()

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        self.changed.add(name)

    def replace(self, name, old, new):
        text = (self.root / name).read_text()
        if old not in text:
            raise AssertionError(f'{name}: edit anchor is absent: {old}')
        self.write(name, text.replace(old, new))

    def reset(self):
        for name in self.changed - FILES.keys():
            (self.root / name).unlink(missing_ok=True)
        self.changed.clear()
        for name, text in FILES.items():
            self.write(name, text)
        self.environment = {}

    def revision(self):
        files = {name: file_digest(self.root / name) for name in sorted(self.changed)
                 if (self.root / name).is_file()}
        lock = self.project / 'Cargo.lock'
        if lock.exists():
            files['project/Cargo.lock'] = file_digest(lock)
        return digest({'files': files, 'environment': self.environment})

    def command(self, mode, source=None):
        source = source or self.source
        lines = source.read_text().splitlines()
        line = next(i for i, text in enumerate(lines) if 'let untouched' in text)
        return [str(self.backend / 'cargo-flowistry'), 'flowistry', '--context-mode', mode,
                'file-focus', str(source), str(line), str(lines[line].index('untouched'))]

    def env(self, case, cache_mode, extra=None):
        # Oracle and reused runs have identical compiler settings and source paths.
        # Only the permitted cache policy changes, using the same isolated store.
        return dict(os.environ, PATH=str(self.backend) + os.pathsep + os.environ['PATH'],
                    FLOWISTRY_CACHE_DIR=str(self.root / 'caches' / case),
                    XDG_CACHE_HOME=str(self.root / 'xdg'),
                    CARGO_TARGET_DIR=str(self.root / 'target'),
                    FLOWISTRY_CACHE=cache_mode, FLOWISTRY_MATRIX_ENV='one',
                    RUST_LOG='flowistry::audit=info,flowistry_ide::cache=info') | self.environment | (extra or {})

    def run(self, case, mode, cache_mode='on', source=None, extra=None):
        start = time.monotonic()
        proc = subprocess.Popen(self.command(mode, source), cwd=self.project,
                                env=self.env(case, cache_mode, extra), stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, start_new_session=True)
        try:
            stdout, stderr = proc.communicate(timeout=self.timeout)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.communicate()
            raise
        return observation(subprocess.CompletedProcess(proc.args, proc.returncode, stdout, stderr),
                           round(time.monotonic() - start, 6))

    def pair(self, name, mode, edit, expectation='any', setup=None):
        self.reset()
        if setup:
            setup(self)
        case = f'{mode}-{name}'
        self.active_case = case
        start = self.run(case, mode)
        if start['exit_code'] or not start.get('semantic_digest'):
            raise AssertionError(f'{case}: initial analysis failed: {start["stderr"][-2000:]}')
        if start['compiler_invocations'] < 1 or not start['solved_bodies']:
            raise AssertionError('backend lacks working compiler/solver audit instrumentation')
        warm = self.run(case, mode)
        assert_equivalent(warm, start)
        if warm['compiler_invocations'] or warm['solved_bodies'] or warm.get('cache', {}).get('validation') != 'snapshot':
            raise AssertionError(f'{case}: unchanged warm request did not replay without compiler work')
        edit(self)
        before = self.revision()
        reused = self.run(case, mode)
        analyzed = self.revision()
        fresh = self.run(case, mode, 'off')
        try:
            assert_equivalent(reused, fresh)
            if analyzed != self.revision():
                raise AssertionError('inputs changed across sequential oracle comparison')
            if expectation != 'error' and fresh['exit_code']:
                raise AssertionError('unexpected failure from the independent oracle')
            if fresh['compiler_invocations'] < 1:
                raise AssertionError('cache-off oracle did not invoke the compiler')
            if expectation == 'snapshot' and (reused['compiler_invocations'] or reused['solved_bodies']):
                raise AssertionError('unchanged input unexpectedly invoked the compiler or solver')
            if expectation == 'hit' and (not reused.get('cache', {}).get('hits') or reused['solved_bodies']):
                raise AssertionError('unchanged validated body was recomputed')
            if expectation == 'miss' and (not reused.get('cache', {}).get('misses') or not reused['solved_bodies']):
                raise AssertionError('semantic edit did not invoke the solver')
            if expectation == 'error' and not (reused['exit_code'] and fresh['exit_code']):
                raise AssertionError('compile-error fixture unexpectedly succeeded')
            if expectation == 'error' and any('mismatched types' not in r['stderr'] for r in (reused, fresh)):
                raise AssertionError('expected type error, not an unrelated process failure')
            ok, error = True, None
        except AssertionError as problem:
            ok, error = False, str(problem)
            atomic_json(self.root / 'failures' / f'{case}.json', {'reused': reused, 'fresh': fresh})
        compact = lambda r: {k: v for k, v in r.items() if k not in ('response', 'stderr')}
        record = {'case': name, 'mode': mode, 'requested_revision': before,
                  'source_revision': analyzed, 'expectation': expectation,
                  'passed': ok, 'error': error, 'warm': compact(warm),
                  'reused': compact(reused), 'fresh': compact(fresh)}
        self.records.append(record)
        print(f'{case}: {"PASS" if ok else "FAIL: " + error}', flush=True)
        return record
