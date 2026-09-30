#!/usr/bin/env python3
"""Probe an isolated incremental experiment build; never enable it in production."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
import time
import tomllib

from incremental_matrix import Matrix, decode, smoke
from smoke_checkpoint import atomic_json, build_metadata, file_digest

FUNCTION = '''pub fn selected(input: i32) -> i32 {
    let mut state = (1, 2);
    child::apply(&mut state, input);
    let closure = |value: i32| value + 1;
    closure(state.1) + state.0
}
'''
FILES = {
    'project/Cargo.toml': '''[package]
name="matrix_fixture"
version="0.0.0"
edition="2021"
[features]
broken=[]
[workspace]
''',
    'project/src/child.rs': 'pub fn apply(state: &mut (i32, i32), input: i32) { state.1 = input; }\n',
}
for name in ('lib', 'main'):
    FILES[f'project/src/{name}.rs'] = 'mod child;\n' + FUNCTION + ''.join(
        f'fn unused_{i}(value: (u32, u32)) -> u32 {{ value.0 + value.1 }}\n' for i in range(128)) + (
        'fn main() { println!("{}", selected(3)); }\n' if name == 'main' else '') + (
        '#[cfg(feature="broken")] fn broken() -> i32 { "unrelated type error" }\n')


def canonical(value):
    return hashlib.sha256(json.dumps(smoke.canonical(value), sort_keys=True).encode()).hexdigest()


def snapshot(directory):
    files = [p for p in directory.rglob('*') if p.is_file()] if directory.exists() else []
    return {'bytes': sum(p.stat().st_size for p in files),
            'files': {str(p.relative_to(directory)): p.stat().st_size for p in files},
            'finalized_sessions': sorted(str(p.parent.relative_to(directory)) for p in files
                                        if p.name == 'query-cache.bin' and not p.parent.name.endswith('-working'))}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--backend-dir', type=Path, required=True)
    parser.add_argument('--reference-dir', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--json', type=Path, required=True)
    parser.add_argument('--perf', type=Path)
    parser.add_argument('--repeat', type=int, default=6)
    parser.add_argument('--project-source', type=Path, help='prepared crate to copy without build artifacts')
    parser.add_argument('--source', help='relative Rust source in a real project')
    parser.add_argument('--line', type=int, default=3)
    parser.add_argument('--column', type=int, default=4)
    parser.add_argument('--kind', choices=['lib', 'bin'], action='append')
    args = parser.parse_args()
    if args.repeat < 1:
        parser.error('--repeat must be positive')
    files, provenance = FILES, None
    package, target = 'matrix_fixture', 'matrix_fixture'
    if args.project_source:
        if not args.source:
            parser.error('--project-source requires --source')
        files = {}
        for directory, dirs, names in os.walk(args.project_source):
            dirs[:] = [name for name in dirs if name not in ('target', '.git')]
            if any((Path(directory) / name).is_symlink() for name in dirs):
                parser.error('real probe requires a source tree without directory symlinks')
            for name in names:
                path = Path(directory) / name
                if path.is_symlink():
                    parser.error('real probe requires a source tree without symlinks')
                if name == '.flowistry-launch.lock':
                    continue
                files['project/' + str(path.relative_to(args.project_source))] = path.read_bytes()
        manifest = tomllib.loads(files['project/Cargo.toml'].decode())
        package = manifest['package']['name']
        target = manifest.get('lib', {}).get('name', package.replace('-', '_'))
        provenance = {'source':str(args.project_source.resolve()), 'package':package,
                      'files_sha256':{name:hashlib.sha256(value).hexdigest() for name,value in files.items()}}
    kinds = args.kind or (['lib'] if args.project_source else ['lib', 'bin'])
    matrix = Matrix(args.backend_dir, args.work_dir, files=files)
    report = {'schema': 1, 'experiment_build': build_metadata(args.backend_dir),
              'reference_build': build_metadata(args.reference_dir),
              'harness_sha256': file_digest(Path(__file__)), 'records': [],
              'load_start': os.getloadavg(), 'scope': 'isolated lib/bin fixtures, both modes; compiler reuse only',
              'source_provenance':provenance, 'position':[args.source, args.line, args.column],
              'command':[sys.executable, *sys.argv], 'repeat':args.repeat,
              'perf':{'path':str(args.perf), 'sha256':file_digest(args.perf)} if args.perf else None,
              'host':{'cpu_count':os.cpu_count(), 'uname':list(os.uname())},
              'controls':{'FLOWISTRY_CACHE':'off', 'FLOWISTRY_NO_REPLAY':'1', 'CARGO_INCREMENTAL':'0',
                          'incremental_statistics':'cold/warm diagnostics only; disabled during measurement'},
              'performance_caveat': 'Loaded host: instruction counts are measured; wall time is not a quiet latency claim.'}

    def run(kind, mode, variant, stage, expected=None, feature=None):
        number = len(report['records'])
        backend = args.reference_dir if variant == 'reference' else args.backend_dir
        inc = matrix.root / 'incremental' / f'{kind}-{mode}-{variant}'
        source = matrix.project / (args.source or ('src/lib.rs' if kind == 'lib' else 'src/main.rs'))
        command = [str(backend.resolve() / 'cargo-flowistry'), 'flowistry', '--context-mode', mode,
                   '--package', package, '--target-kind', kind, '--target-name', target]
        if feature:
            command += ['--features', feature]
        command += ['file-focus', str(source), str(args.line), str(args.column)]
        environment = matrix.env(f'{kind}-{mode}-{variant}', 'off', {
            'FLOWISTRY_NO_REPLAY': '1', 'CARGO_INCREMENTAL': '0',
            'FLOWISTRY_EXPERIMENT_INCREMENTAL': '' if variant == 'reference' else variant,
            'FLOWISTRY_EXPERIMENT_INCREMENTAL_DIR': str(inc),
            'FLOWISTRY_EXPERIMENT_INCREMENTAL_STATS': '1' if stage in ('cold', 'warm') else '0',
        })
        environment['PATH'] = str(backend.resolve()) + os.pathsep + os.environ['PATH']
        prefix = matrix.root / 'raw' / f'{number:04}-{kind}-{mode}-{variant}-{stage}'
        prefix.parent.mkdir(exist_ok=True)
        perf_path = prefix.with_suffix('.perf')
        if args.perf:
            command = [str(args.perf), 'stat', '-x', ';', '-e', 'instructions:u,cycles:u', '-o', str(perf_path), '--', *command]
        start = time.monotonic()
        result = subprocess.run(command, cwd=matrix.project, env=environment, capture_output=True, timeout=120)
        seconds = time.monotonic() - start
        prefix.with_suffix('.stderr').write_bytes(result.stderr)
        value = decode(result.stdout)
        digest = canonical(value['Ok']) if value and 'Ok' in value else None
        counters = {}
        if args.perf:
            for line in perf_path.read_text().splitlines():
                fields = line.split(';')
                if len(fields) >= 3 and fields[2] in ('instructions:u', 'cycles:u'):
                    try:
                        counters[fields[2]] = int(fields[0])
                    except ValueError:
                        pass
        record = {'kind': kind, 'mode': mode, 'variant': variant, 'stage': stage,
                  'exit_code': result.returncode, 'semantic_digest': digest,
                  'equal': digest is not None and (expected is None or digest == expected),
                  'seconds': seconds, 'counters': counters,
                  'compiler_invocations': result.stderr.count(b'audit compiler'),
                  'solver_invocations': result.stderr.count(b'audit solve '),
                  'analyzed_bodies':sum(bool(b.get('focus')) for b in value['Ok']['bodies']) if value and 'Ok' in value else 0,
                  'stderr_path': str(prefix.with_suffix('.stderr')),
                  'stderr_sha256': hashlib.sha256(result.stderr).hexdigest(), 'incremental': snapshot(inc)}
        report['records'].append(record)
        atomic_json(args.json, report)
        print(kind, mode, variant, stage, result.returncode, record['equal'], record['incremental']['bytes'], flush=True)
        return record

    for kind in kinds:
        for mode in ('SigOnly', 'Recurse'):
            baseline = run(kind, mode, 'reference', 'reference')
            assert baseline['exit_code'] == 0 and baseline['equal'] and baseline['compiler_invocations'] == 1 and baseline['analyzed_bodies'] == 1, baseline
            expected = baseline['semantic_digest']
            eligible = []
            for variant in ('off', 'retain', 'stop', 'finish'):
                cold = run(kind, mode, variant, 'cold', expected)
                warm = run(kind, mode, variant, 'warm', expected)
                if all(r['exit_code'] == 0 and r['equal'] and r['compiler_invocations'] == 1 for r in (cold, warm)):
                    eligible.append(variant)
                    run(kind, mode, variant, 'quiet-diagnostics-warmup', expected)
            for repeat in range(args.repeat):
                order = eligible if repeat % 2 == 0 else list(reversed(eligible))
                for variant in order:
                    run(kind, mode, variant, f'repeat-{repeat}', expected)
            if args.project_source:
                continue
            for stage, contents in [('callee-edit', FILES['project/src/child.rs'].replace('state.1 = input', 'state.0 = input')),
                                    ('undo', FILES['project/src/child.rs'])]:
                matrix.write('project/src/child.rs', contents)
                reference = run(kind, mode, 'reference', stage)
                assert reference['exit_code'] == 0 and reference['equal'], reference
                for variant in eligible:
                    run(kind, mode, variant, stage, reference['semantic_digest'])
            # Enabling a feature also changes the file's body inventory. Compare
            # against a fresh compiler under that feature, not the old inventory.
            broken = run(kind, mode, 'reference', 'unrelated-error', feature='broken')
            assert broken['exit_code'] == 0 and broken['equal'], broken
            for variant in eligible:
                run(kind, mode, variant, 'unrelated-error', broken['semantic_digest'], feature='broken')
    report['load_end'] = os.getloadavg()
    report['completed'] = True
    report['summary'] = []
    for kind in kinds:
        for mode in ('SigOnly', 'Recurse'):
            for variant in ('off', 'retain', 'stop', 'finish'):
                records = [r for r in report['records'] if (r['kind'], r['mode'], r['variant']) == (kind, mode, variant)]
                samples = [r for r in records if r['stage'].startswith('repeat-')]
                instructions = [r['counters']['instructions:u'] for r in samples if 'instructions:u' in r['counters']]
                report['summary'].append({'kind':kind, 'mode':mode, 'variant':variant,
                    'all_equal':all(r['exit_code'] == 0 and r['equal'] for r in records),
                    'samples':len(samples), 'instruction_samples':len(instructions),
                    'median_instructions':statistics.median(instructions) if instructions else None,
                    'median_loaded_seconds':statistics.median([r['seconds'] for r in samples]) if samples else None,
                    'final_incremental_bytes':records[-1]['incremental']['bytes'],
                    'finalized_sessions':len(records[-1]['incremental']['finalized_sessions'])})
    atomic_json(args.json, report)


if __name__ == '__main__':
    main()
