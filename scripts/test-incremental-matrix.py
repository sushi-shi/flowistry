#!/usr/bin/env python3
"""Compare edited cache reuse with a fresh oracle; preserve per-body work and failures."""
import argparse
import json
import os
from pathlib import Path
import subprocess

from incremental_matrix import FILES, Matrix
from smoke_checkpoint import atomic_json, build_metadata, file_digest

LIB = 'project/src/lib.rs'
CHILD = 'project/src/child.rs'


def replace(path, old, new):
    return lambda matrix: matrix.replace(path, old, new)


def move_module(matrix):
    matrix.write('project/src/renamed.rs', (matrix.root / CHILD).read_text())
    (matrix.root / CHILD).unlink()
    matrix.replace(LIB, 'mod child;', 'mod renamed;')
    matrix.replace(LIB, 'child::apply', 'renamed::apply')


def add_module(matrix):
    matrix.write('project/src/extra.rs', 'pub fn value() -> i32 { 17 }\n')
    matrix.write(LIB, 'mod extra;\n' + matrix.source.read_text())


def remove_module(matrix):
    matrix.replace(LIB, 'mod extra;\n', '')
    (matrix.root / 'project/src/extra.rs').unlink()


def rustfmt(matrix):
    subprocess.run([matrix.rustfmt, '--edition', '2021', str(matrix.source)], check=True)


def restore_mtime(matrix):
    stat = matrix.source.stat()
    matrix.replace(LIB, 'a: 1, b: 2', 'a: 9, b: 2')
    os.utime(matrix.source, ns=(stat.st_atime_ns, stat.st_mtime_ns))


def corrupt(matrix):
    # Both caches must tolerate truncated public entries and abandoned temp files.
    for path in (matrix.root / 'caches' / matrix.active_case).rglob('*.json'):
        path.write_text('{interrupted')
        path.with_suffix('.tmp-abandoned').write_text('{partial')


def scenarios(mode):
    return [
        ('unchanged-save', lambda m: m.source.write_text(m.source.read_text()), 'snapshot', None),
        ('leading-layout', lambda m: m.write(LIB, '\n\n' + m.source.read_text()), 'hit', None),
        ('interior-layout', replace(LIB, ' let café', '\n    let café'), 'hit', None),
        ('unicode-comment', lambda m: m.write(LIB, '// é🦀\n' + m.source.read_text()), 'hit', None),
        ('body-comment', replace(LIB, ' let café', ' // body comment\n let café'), 'miss', None),
        ('rustfmt', rustfmt, 'hit', None),
        ('literal', replace(LIB, 'a: 1, b: 2', 'a: 9, b: 2'), 'miss', None),
        ('restored-mtime', restore_mtime, 'miss', None),
        ('control-flow', replace(LIB, 'mapper(untouched + affected)', 'if input > 0 { untouched } else { affected }'), 'miss', None),
        ('callee', replace(CHILD, 'state.b = input', 'state.a = input'), 'miss' if mode == 'Recurse' else 'hit', None),
        ('unrelated-body', replace(LIB, 'fn unrelated() -> i32 { 2 }', 'fn unrelated() -> i32 { 9 }'), 'hit', None),
        ('recursive-cycle', replace(LIB, 'cycle_a(n - 1)', 'cycle_a(n - 2)'), 'miss' if mode == 'Recurse' else 'hit',
         replace(LIB, 'let café = input +', 'let café = cycle_a(input) +')),
        ('signature', replace(LIB, 'pub fn selected(input: i32)', 'pub fn selected(mut input: i32)'), 'miss', None),
        ('field-type', replace(LIB, 'pub a: i32', 'pub a: i64'), 'error', None),
        ('trait-callee', replace(LIB, 'fn choice(&self) -> i32 { self.a }', 'fn choice(&self) -> i32 { self.b }'),
         'miss' if mode == 'Recurse' else 'hit', None),
        ('trait-resolution', replace(LIB, 'state.choice()', 'Choice::choice(&state)'), 'miss', None),
        ('file-move', move_module, 'miss', None),
        ('module-add', add_module, 'miss', None),
        ('module-remove', remove_module, 'miss', add_module),
        ('feature', replace('project/Cargo.toml', 'default=[]', 'default=["alternate"]'), 'miss', None),
        ('cfg', lambda m: m.environment.update(RUSTFLAGS='--cfg matrix_extra'), 'miss', None),
        ('cargo-config', lambda m: m.write('project/.cargo/config.toml', '[build]\nrustflags=["--cfg", "matrix_extra"]\n'), 'miss', None),
        ('dependency', replace('dependency/src/lib.rs', '7', '9'), 'miss', None),
        ('build-input', lambda m: m.write('project/build-input.txt', 'build input changed'), 'miss', None),
        ('build-script', replace('project/build.rs', 'MATRIX_BUILD_VALUE={value}', 'MATRIX_BUILD_VALUE=changed-{value}'), 'miss', None),
        ('include', lambda m: m.write('project/src/included.txt', 'changed included source'), 'miss', None),
        ('environment', lambda m: m.environment.update(FLOWISTRY_MATRIX_ENV='changed environment'), 'miss', None),
        ('proc-macro', replace('macros/src/lib.rs', '"7"', '"9"'), 'miss', None),
        ('compile-error', replace(LIB, 'let café =', 'let invalid: i32 = true; let café ='), 'error', None),
        ('closure', replace(LIB, '|x: i32| x + 1', '|x: i32| x * 2'), 'miss', None),
        ('corrupt-and-partial-writes', corrupt, 'miss', None),
    ]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--backend-dir', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True, help='new isolated directory; never overwritten')
    parser.add_argument('--json', type=Path, required=True)
    parser.add_argument('--modes', default='SigOnly,Recurse')
    parser.add_argument('--case', action='append', default=[])
    parser.add_argument('--rustfmt', default='rustfmt')
    args = parser.parse_args()
    backend = args.backend_dir.resolve()
    build = build_metadata(backend)
    if build and build.get('features'):
        parser.error('work counters require a normal build without reference instrumentation')
    matrix = Matrix(backend, args.work_dir)
    matrix.rustfmt = args.rustfmt
    report = {'schema': 1, 'build': build,
              'binaries': {name: file_digest(backend / name) for name in ('cargo-flowistry', 'flowistry-driver')},
              'harness': {name: file_digest(Path(__file__).with_name(name)) for name in
                          ('incremental_matrix.py', 'test-incremental-matrix.py', 'smoke-real-crates.py')},
              'records': matrix.records, 'harness_failures': [],
              'pending': ['real-project edit matrix', 'concurrent saves and cancellation',
                          'versioned publication / background-worker scenarios added with those features']}
    names = {name for name, *_ in scenarios('Recurse')}
    if set(args.case) - names:
        parser.error('unknown case: ' + ', '.join(sorted(set(args.case) - names)))
    for mode in args.modes.split(','):
        if mode not in ('SigOnly', 'Recurse'):
            parser.error('invalid mode')
        for name, edit, expectation, setup in scenarios(mode):
            if args.case and name not in args.case:
                continue
            try:
                matrix.pair(name, mode, edit, expectation, setup)
            except Exception as error:
                report['harness_failures'].append({'case': name, 'mode': mode, 'error': str(error)})
                print(f'{mode}-{name}: HARNESS FAILURE: {error}', flush=True)
            atomic_json(args.json, report)
    report['passed'] = bool(matrix.records) and not report['harness_failures'] and all(r['passed'] for r in matrix.records)
    atomic_json(args.json, report)
    raise SystemExit(0 if report['passed'] else 1)


if __name__ == '__main__':
    main()
