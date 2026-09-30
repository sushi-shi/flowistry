#!/usr/bin/env python3
"""Real compiler cache invalidation regressions; no third-party dependencies."""
import argparse
import base64
import gzip
import json
import os
from pathlib import Path
import subprocess
import tempfile

SOURCE = '''struct State { a: i32, b: i32 }
impl State {
 fn inner(&mut self, input: i32) { self.b = input; }
 fn update(&mut self, input: i32) { self.inner(input); }
}
fn unrelated() { let x = 2; }
const VALUE: i32 = 17;
fn main() {
 let mut state = State { a: 1, b: 2 };
 let input = VALUE;
 state.update(input);
 let untouched = state.a;
 let affected = state.b;
}
'''


def expanded(value):
    """Compare semantic ranges, independently of range-table insertion order."""
    if isinstance(value, dict):
        if "place_info" in value and "ranges" in value:
            table = value["ranges"]
            places = []
            for entry in value["place_info"]:
                item = dict(entry, range=table[entry["range"]])
                for field in ("ranges", "slice", "direct_influence", "maybe_slice"):
                    if field in item:
                        item[field] = [table[index] for index in item[field]]
                places.append(item)
            value = {k: v for k, v in value.items() if k != "ranges"}
            value["place_info"] = places
        return {k: expanded(v) for k, v in value.items()}
    if isinstance(value, list):
        return [expanded(v) for v in value]
    return value


def canonical(value):
    if isinstance(value, dict):
        return {k: canonical(v) for k, v in sorted(value.items()) if k not in ('filename', 'cache', 'cached')}
    if isinstance(value, list):
        return sorted((canonical(v) for v in value), key=lambda v: json.dumps(v, sort_keys=True))
    return value


def coverage(value, text):
    """Compare all highlighted codepoints; line-split spans may have different shapes."""
    lines = text.splitlines()
    def is_range(v):
        return isinstance(v, dict) and 'start' in v and 'end' in v
    def points(r):
        a, b = r['start'], r['end']
        return {(row, col) for row in range(a['line'], b['line'] + 1)
                for col, char in enumerate(lines[row]) if not char.isspace()
                and (a['line'], a['column']) <= (row, col) < (b['line'], b['column'])}
    if is_range(value):
        return sorted(points(value))
    if isinstance(value, dict):
        return {k: coverage(v, text) for k, v in sorted(value.items()) if k not in ('filename', 'cache', 'cached')}
    if isinstance(value, list):
        if value and all(is_range(v) for v in value):
            return sorted(set().union(*(points(v) for v in value)))
        return sorted((coverage(v, text) for v in value), key=lambda v: json.dumps(v, sort_keys=True))
    return value


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--backend', required=True, help='Backend wrapper (takes Flowistry arguments directly)')
    args = parser.parse_args()
    backend = str(Path(args.backend).resolve())
    with tempfile.TemporaryDirectory(prefix='flowistry cache test ') as tmp:
        root = Path(tmp)
        (root / 'src').mkdir()
        (root / 'Cargo.toml').write_text('[package]\nname="focus_cache_test"\nversion="0.1.0"\nedition="2021"\n')
        source = root / 'src/main.rs'
        cache = root / 'cache'
        source.write_text(SOURCE)
        count = 0

        def run(expect_hit, mode='Recurse', cache_mode='', extra_env=None, fast=False):
            nonlocal count
            env = dict(os.environ, FLOWISTRY_CACHE_DIR=str(cache), FLOWISTRY_CACHE=cache_mode)
            env.update(extra_env or {})
            line = next(i for i, text in enumerate(source.read_text().splitlines()) if 'let untouched' in text)
            result = subprocess.run([backend, '--context-mode', mode, 'file-focus', str(source), str(line), '7'],
                                    cwd=root, env=env, capture_output=True, timeout=120)
            try:
                output = json.loads(gzip.decompress(base64.b64decode(result.stdout)))['Ok']
                assert result.returncode == 0, result.stderr.decode()
                assert output['cache']['hits'] == int(expect_hit), output['cache']
                assert output['cache']['misses'] == int(not expect_hit), output['cache']
                if fast:
                    assert output['cache'].get('validation') == 'snapshot', output['cache']
                assert any(body['focus'] and 'Ok' in body['focus'] for body in output['bodies'])
            except Exception as error:
                raise AssertionError(f'case {count + 1}: {error}\n{result.stderr.decode()}') from error
            count += 1
            return expanded(output)

        fresh = run(False)
        assert canonical(run(True, fast=True)) == canonical(fresh)
        # Whitespace and Unicode comments move unchanged tokens, including within the body.
        source.write_text('\n// é🦀\n' + SOURCE.replace(' let input', '\n    let input'))
        moved = run(True)
        assert canonical(moved) == canonical(run(False, cache_mode='refresh'))
        source.write_text(SOURCE.replace('let x = 2;', 'let x = 9; let more = x + 2;'))
        run(True)
        # Only transitive callees invalidate Recurse, including a changed call graph.
        source.write_text(SOURCE.replace('self.b = input', 'self.a = input'))
        run(False)
        run(True)
        source.write_text(SOURCE.replace('self.inner(input)', 'self.b = input'))
        run(False)
        source.write_text(SOURCE)
        run(True)  # Undo recovers an older content-addressed entry.
        run(False, mode='SigOnly')
        source.write_text(SOURCE.replace('self.b = input', 'self.a = input'))
        run(True, mode='SigOnly')
        source.write_text(SOURCE.replace('const VALUE: i32 = 17', 'const VALUE: i32 = 99'))
        run(False)
        source.write_text(SOURCE.replace('a: i32, b: i32', 'a: i64, b: i32'))
        run(False)
        source.write_text(SOURCE.replace('let input = VALUE', 'let input = VALUE + 1'))
        run(False)
        # Reject a type error in the selected body. Unrelated bodies are demand-checked.
        source.write_text(SOURCE.replace('let input = VALUE;', 'let input: i32 = true;'))
        result = subprocess.run([backend, '--context-mode', 'Recurse', 'file-focus', str(source), '11', '7'],
                                cwd=root, env=dict(os.environ, FLOWISTRY_CACHE_DIR=str(cache)), capture_output=True, timeout=120)
        assert b'mismatched types' in result.stderr
        source.write_text(SOURCE)
        run(True)
        run(False, cache_mode='off')
        run(True)
        for entry in cache.rglob('*.json'):
            entry.write_text('{broken')
        run(False)
        run(True)
        # Compiler settings and build-script cfg participate in the fingerprint.
        run(False, extra_env={'RUSTFLAGS': '--cfg cache_regression'})
        run(True, extra_env={'RUSTFLAGS': '--cfg cache_regression'})
        # Preserve token locations through interior formatting, not just line offsets.
        source.write_text(SOURCE)
        run(True)
        source.write_text(SOURCE.replace(';', ';\n').replace('let input =', 'let\n input ='))
        formatted = run(True)
        assert coverage(formatted, source.read_text()) == coverage(run(False, cache_mode='refresh'), source.read_text())
        source.write_text(SOURCE.replace(' = ', '=').replace('state.update', 'state . update'))
        compact = run(True)
        assert coverage(compact, source.read_text()) == coverage(run(False, cache_mode='refresh'), source.read_text())
        unicode = SOURCE.replace('input', 'café')
        source.write_text(unicode)
        run(False)
        source.write_text('\n' + unicode.replace('let café', '\n    let café'))
        relocated = run(True)
        assert canonical(relocated) == canonical(run(False, cache_mode='refresh'))
        # Recursive call cycles remain finite and participate in invalidation.
        cyclic = SOURCE.replace('self.b = input;', 'if input > 0 { self.update(input - 1); } self.b = input;')
        source.write_text(cyclic)
        run(False)
        run(True)
        source.write_text(cyclic.replace('input - 1', 'input - 2'))
        run(False)
        # Closures and captured inputs have no portable rustc identities in the cache.
        closure = SOURCE.replace('let input = VALUE;', 'let mapper = |x: i32| x + 1; let input = mapper(VALUE);')
        source.write_text(closure)
        run(False)
        run(True)
        # A callee in another source file is part of the dependency closure.
        child = root / 'src/child.rs'
        child.write_text('pub fn transform(x: i32) -> i32 { x + 1 }\n')
        source.write_text('mod child;\n' + SOURCE.replace('self.b = input;', 'self.b = child::transform(input);'))
        run(False)
        run(True)
        child.write_text('pub fn transform(x: i32) -> i32 { x + 2 }\n')
        run(False)
        # Changes to a macro and const function can affect code without direct call edges.
        macro = 'macro_rules! value { () => { 17 }; }\n' + SOURCE.replace('let input = VALUE;', 'let input = value!();')
        source.write_text(macro)
        run(False)
        run(True)
        source.write_text(macro.replace('=> { 17 }', '=> { 99 }'))
        run(False)
        const_fn = 'const fn value() -> i32 { 17 }\n' + SOURCE.replace('const VALUE: i32 = 17;', 'const VALUE: i32 = value();')
        source.write_text(const_fn)
        run(False)
        run(True)
        source.write_text(const_fn.replace('-> i32 { 17 }', '-> i32 { 99 }'))
        run(False)
        source.write_text(SOURCE)
        run(True)
        source.write_text(SOURCE.replace('a: i32, b: i32', 'pub a: i32, b: i32'))
        run(False)  # Visibility is stored separately from HIR field declarations.
        source.write_text(SOURCE)
        for entry in cache.rglob('*.json'):
            try:
                value = json.loads(entry.read_text())
            except json.JSONDecodeError:
                continue
            value['places'] = []  # Valid JSON with corrupted payload must not be replayed.
            entry.write_text(json.dumps(value))
        run(False)
        run(True)
        blocked = root / 'not-a-directory'
        blocked.write_text('cache IO must be optional')
        run(False, extra_env={'FLOWISTRY_CACHE_DIR': str(blocked)})
        # Metadata from an external path dependency must invalidate a cached root.
        dep = root / 'dependency'
        (dep / 'src').mkdir(parents=True)
        (dep / 'Cargo.toml').write_text('[package]\nname="dependency"\nversion="0.1.0"\nedition="2021"\n')
        library = dep / 'src/lib.rs'
        library.write_text('pub fn value() -> i32 { 1 }\n')
        manifest = root / 'Cargo.toml'
        manifest.write_text(manifest.read_text() + '\n[dependencies]\ndependency={path="dependency"}\n')
        source.write_text(SOURCE.replace('let input = VALUE;', 'let input = dependency::value();'))
        run(False)
        run(True)
        library.write_text('pub fn value() -> i32 { 42 }\n')
        run(False)
        print(f'Passed {count} cross-process cache cases, plus compilation-error rejection')


if __name__ == '__main__':
    main()
