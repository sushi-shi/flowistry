#!/usr/bin/env python3
"""Exercise the real publication protocol, index and cancellation against fresh analysis."""
import argparse
import json
from pathlib import Path
import subprocess
import time

from incremental_matrix import Matrix, assert_equivalent
from incremental_races import require_success
from smoke_checkpoint import atomic_json, build_metadata, file_digest

PROTOCOL = {'FLOWISTRY_RESULT_PROTOCOL': '1'}
LIB = 'project/src/lib.rs'


def control(matrix, case, mode, operation):
    args = matrix.command(mode)[:-2]
    args[args.index('file-focus')] = operation
    result = subprocess.run(args, cwd=matrix.project, env=matrix.env(case, 'on'),
                            capture_output=True, timeout=30)
    if result.returncode:
        raise AssertionError(result.stderr.decode())
    if b'audit compiler' in result.stderr:
        raise AssertionError('index/control command invoked the compiler')
    return json.loads(result.stdout)


def current(result):
    require_success(result)
    publication = result.get('publication', {})
    if publication.get('status') != 'current' or not publication.get('revision') or not publication.get('generation'):
        raise AssertionError('response lacks validated publication provenance: ' + str(publication))


def selected(index):
    return next(body for body in index['bodies'] if body['name'] == 'selected')


def external_race(matrix, mode):
    matrix.reset()
    case = mode + '-new-external-input'
    matrix.write('outside.txt', 'first external input')
    matrix.replace(LIB, 'include_str!("included.txt")',
                   'include_str!(' + json.dumps(str(matrix.root / 'outside.txt')) + ')')
    gate = matrix.compiler_gate('after')
    for path in gate.iterdir():
        path.unlink()
    observations, request = {}, None
    try:
        (gate / 'armed').touch()
        request = matrix.begin(case, mode, extra=PROTOCOL)
        matrix.wait_gate(request, gate)
        matrix.write('outside.txt', 'external input changed after compiler consumed it')
        (gate / 'armed').unlink()
        (gate / 'release').touch()
        old = observations['old'] = matrix.finish(request)
        if old.get('publication', {}).get('status') == 'current':
            raise AssertionError('new external input was published with the wrong compiled revision')
        if control(matrix, case, mode, 'result-index')['status'] != 'miss':
            raise AssertionError('unvalidated external input produced a readable result index')
        result = observations['validated'] = matrix.run(case, mode, extra=PROTOCOL)
        current(result)
        fresh = observations['fresh'] = matrix.run(case, mode, 'off')
        require_success(fresh)
        assert_equivalent(result, fresh)
        warm = observations['warm'] = matrix.run(case, mode, extra=PROTOCOL)
        current(warm)
        if warm['compiler_invocations'] or warm['solved_bodies']:
            raise AssertionError('validated external input did not become reusable')
        ok, error = True, None
    except Exception as problem:
        ok, error = False, str(problem)
        atomic_json(matrix.root / 'failures' / (case + '.json'), observations)
    finally:
        if request and request[0].poll() is None:
            matrix.cancel(request)
        (gate / 'armed').unlink(missing_ok=True)
        (gate / 'release').touch()
    print(f'{case}: {"PASS" if ok else "FAIL: " + error}', flush=True)
    return {'case': 'new-external-input', 'mode': mode, 'passed': ok, 'error': error,
            'observations': {label: {k: v for k, v in value.items() if k not in ('response', 'stderr')}
                             for label, value in observations.items()}}


def storage_case(matrix, mode, interrupted):
    matrix.reset()
    name = 'interrupted-publication' if interrupted else 'configured-disk-budget'
    case = mode + '-' + name
    request, gate, observations = None, None, {}
    cache = matrix.root / 'caches' / case
    try:
        if interrupted:
            gate = matrix.compiler_gate('after')
            for path in gate.iterdir():
                path.unlink()
            (gate / 'armed').touch()
            request = matrix.begin(case, mode, extra=PROTOCOL)
            matrix.wait_gate(request, gate)
            observations['killed'] = matrix.cancel(request)
            if observations['killed']['exit_code'] == 0:
                raise AssertionError('interrupted process succeeded')
            (gate / 'armed').unlink()
            if list(cache.glob('inputs-*.tmp')):
                raise AssertionError('killed publisher leaked an input sidecar')
            if control(matrix, case, mode, 'result-index')['status'] != 'miss':
                raise AssertionError('killed publisher exposed a completed index')
        else:
            limit = 128 * 1024
            matrix.environment['FLOWISTRY_CACHE_MAX_BYTES'] = str(limit)
            peaks = []
            for value in range(1, 21):
                matrix.replace(LIB, f'a: {value}, b: 2', f'a: {value + 1}, b: 2')
                result = matrix.run(case, mode, extra=PROTOCOL)
                current(result)
                size = sum(path.stat().st_size for path in cache.rglob('*') if path.is_file())
                peaks.append(size)
                if size > limit:
                    raise AssertionError('combined result store exceeded its configured budget')
            retained = len(list((cache / 'focus-v1').glob('*.json')))
            if retained >= 20:
                raise AssertionError('fixture did not exercise eviction')
            observations['budget'] = {'limit': limit, 'maximum_bytes': max(peaks), 'retained_focus_entries': retained}
        result = observations['recovered'] = matrix.run(case, mode, extra=PROTOCOL)
        current(result)
        fresh = observations['fresh'] = matrix.run(case, mode, 'off')
        require_success(fresh)
        assert_equivalent(result, fresh)
        if interrupted and result['solved_bodies']:
            raise AssertionError('restart discarded the completed, valid semantic body result')
        ok, error = True, None
    except Exception as problem:
        ok, error = False, str(problem)
        atomic_json(matrix.root / 'failures' / (case + '.json'), observations)
    finally:
        if request and request[0].poll() is None:
            matrix.cancel(request)
        if gate:
            (gate / 'armed').unlink(missing_ok=True)
            (gate / 'release').touch()
    print(f'{case}: {"PASS" if ok else "FAIL: " + error}', flush=True)
    return {'case': name, 'mode': mode, 'passed': ok, 'error': error,
            'observations': {label: {k: v for k, v in value.items() if k not in ('response', 'stderr')}
                             for label, value in observations.items()}}


def check(matrix, mode, name):
    matrix.reset()
    case = mode + '-' + name
    observations, requests = {}, []
    gate = matrix.compiler_gate() if name in ('superseded', 'canceled', 'generation-corruption') else None
    if gate:
        for path in gate.iterdir():
            path.unlink()
    try:
        if control(matrix, case, mode, 'result-index')['status'] != 'miss':
            raise AssertionError('new isolated index must miss')
        cold = observations['cold'] = matrix.run(case, mode, extra=PROTOCOL)
        current(cold)
        warm = observations['warm'] = matrix.run(case, mode, extra=PROTOCOL)
        current(warm)
        assert_equivalent(warm, cold)
        if warm['compiler_invocations'] or warm['solved_bodies']:
            raise AssertionError('protocol snapshot hit unexpectedly analyzed')
        index = observations['initial_index'] = control(matrix, case, mode, 'result-index')
        if not selected(index)['available']:
            raise AssertionError('selected body missing from index')
        if selected(index)['range']['filename'] != str(matrix.source):
            raise AssertionError('persistent body index lacks the requested file identity')
        if any(body['available'] for body in index['bodies'] if body['name'] == 'unrelated'):
            raise AssertionError('unanalyzed body was advertised as available')
        if index['revision'] != cold['publication']['revision']:
            raise AssertionError('index and response disagree about the input revision')
        if name == 'layout':
            matrix.write(LIB, '// é🦀\n\n' + matrix.source.read_text())
            if control(matrix, case, mode, 'result-index')['status'] != 'miss':
                raise AssertionError('index accepted old ranges after source movement')
            result = observations['edited'] = matrix.run(case, mode, extra=PROTOCOL)
            current(result)
            if result['solved_bodies']:
                raise AssertionError('compiler-validated layout edit reran the solver')
            moved = observations['moved_index'] = control(matrix, case, mode, 'result-index')
            if moved['revision'] == index['revision'] or selected(moved)['range'] == selected(index)['range']:
                raise AssertionError('source revision/ranges did not move')
        elif name == 'concurrent-bodies':
            requests.append(matrix.begin(case, mode, 'refresh', extra=PROTOCOL))
            matrix.anchor = 'fn unrelated'
            requests.append(matrix.begin(case, mode, 'refresh', extra=PROTOCOL))
            matrix.anchor = 'let untouched'
            for i, request in enumerate(requests):
                result = observations[f'writer{i}'] = matrix.finish(request)
                current(result)
            index = observations['merged_index'] = control(matrix, case, mode, 'result-index')
            available = {body['name'] for body in index['bodies'] if body['available']}
            if not {'selected', 'unrelated'} <= available:
                raise AssertionError('concurrent publication lost a completed body')
            matrix.anchor = 'fn unrelated'
            unrelated = observations['unrelated'] = matrix.run(case, mode, extra=PROTOCOL)
            oracle = observations['unrelated_fresh'] = matrix.run(case, mode, 'off')
            assert_equivalent(unrelated, oracle)
            if unrelated['compiler_invocations']:
                raise AssertionError('published second body was not reusable after restart')
            matrix.anchor = 'let untouched'
        elif gate:
            matrix.replace(LIB, 'a: 1, b: 2', 'a: 9, b: 2')
            (gate / 'armed').touch()
            request = matrix.begin(case, mode, extra=PROTOCOL)
            requests.append(request)
            matrix.wait_gate(request, gate)
            if name == 'superseded':
                matrix.replace(LIB, 'a: 9, b: 2', 'a: 8, b: 2')
                state_path = matrix.root / 'caches' / case / '.generations'
                previous_state = state_path.read_bytes()
                newer = matrix.begin(case, mode, extra=PROTOCOL)
                requests.append(newer)
                # Cargo may serialize compiler phases. Wait for the newer
                # preflight registration, then release the older compiler.
                deadline = time.monotonic() + 20
                while state_path.read_bytes() == previous_state:
                    if newer[0].poll() is not None or time.monotonic() > deadline:
                        raise AssertionError('newer revision was not registered')
                    time.sleep(.02)
            elif name == 'canceled':
                observations['cancellation'] = control(matrix, case, mode, 'cancel-results')
            else:
                (matrix.root / 'caches' / case / '.generations').write_text('{interrupted')
            (gate / 'armed').unlink()
            (gate / 'release').touch()
            stale = observations['obsolete'] = matrix.finish(request)
            if stale['exit_code'] != 75 or stale.get('publication', {}).get('status') != 'superseded' or stale['response'] is not None:
                raise AssertionError('obsolete completion delivered analysis as current')
            if not stale['publication'].get('generation'):
                raise AssertionError('obsolete completion lost its generation')
            if name == 'superseded':
                winner = observations['winner'] = matrix.finish(newer)
                current(winner)
            if name != 'superseded' and control(matrix, case, mode, 'result-index')['status'] != 'miss':
                raise AssertionError('canceled/interrupted request published an index')
        result = observations['current'] = matrix.run(case, mode, extra=PROTOCOL)
        current(result)
        fresh = observations['fresh'] = matrix.run(case, mode, 'off')
        require_success(fresh)
        assert_equivalent(result, fresh)
        before = control(matrix, case, mode, 'result-index')
        off = observations['cache_off_protocol'] = matrix.run(case, mode, 'off', extra=PROTOCOL)
        assert_equivalent(off, fresh)
        if off.get('publication', {}).get('status') != 'uncached':
            raise AssertionError('cache-off request claimed snapshot publication')
        if control(matrix, case, mode, 'result-index')['generation'] != before['generation']:
            raise AssertionError('cache-off changed the publication generation')
        ok, error = True, None
    except Exception as problem:
        ok, error = False, str(problem)
        atomic_json(matrix.root / 'failures' / (case + '.json'), observations)
    finally:
        matrix.anchor = 'let untouched'
        for request in requests:
            if request[0].poll() is None:
                matrix.cancel(request)
        if gate:
            (gate / 'armed').unlink(missing_ok=True)
            (gate / 'release').touch()
    compact = {label: {k: v for k, v in value.items() if k not in ('response', 'stderr')}
               for label, value in observations.items()}
    print(f'{case}: {"PASS" if ok else "FAIL: " + error}', flush=True)
    return {'case': name, 'mode': mode, 'passed': ok, 'error': error, 'observations': compact}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--backend-dir', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--json', type=Path, required=True)
    args = parser.parse_args()
    matrix = Matrix(args.backend_dir, args.work_dir)
    report = {'schema': 1, 'build': build_metadata(args.backend_dir),
              'binaries': {name: file_digest(args.backend_dir / name) for name in ('cargo-flowistry', 'flowistry-driver')},
              'harness': {name: file_digest(Path(__file__).with_name(name)) for name in ('test-result-index.py', 'incremental_matrix.py')},
              'records': []}
    for mode in ('SigOnly', 'Recurse'):
        for name in ('layout', 'concurrent-bodies', 'superseded', 'canceled', 'generation-corruption'):
            report['records'].append(check(matrix, mode, name))
            atomic_json(args.json, report)
        report['records'].append(external_race(matrix, mode))
        atomic_json(args.json, report)
        for interrupted in (False, True):
            report['records'].append(storage_case(matrix, mode, interrupted))
            atomic_json(args.json, report)
    report['passed'] = all(record['passed'] for record in report['records'])
    atomic_json(args.json, report)
    raise SystemExit(0 if report['passed'] else 1)


if __name__ == '__main__':
    main()
