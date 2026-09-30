"""Deterministic cache races against a real, separately launched compiler oracle."""
from pathlib import Path
import time

from incremental_matrix import assert_equivalent
from smoke_checkpoint import atomic_json

NAMES = ('save-during-compile', 'two-quick-saves', 'cancel-during-compile',
         'concurrent-refresh', 'error-recovery', 'undo')
LIB = 'project/src/lib.rs'


def require_success(value):
    if value['exit_code'] or not value.get('semantic_digest'):
        raise AssertionError('analysis did not succeed: ' + value['stderr'][-2000:])


def require_snapshot(value):
    require_success(value)
    if value['compiler_invocations'] or value['solved_bodies'] or value.get('cache', {}).get('validation') != 'snapshot':
        raise AssertionError('unchanged request did not use compiler-free snapshot replay')


def run(matrix, name, mode):
    matrix.reset()
    case = f'{mode}-{name}'
    gate = matrix.compiler_gate() if name in NAMES[:3] else None
    if gate:
        for path in gate.iterdir():
            path.unlink()
    observations = {}
    requests = []
    details = {}
    try:
        initial = observations['initial'] = matrix.run(case, mode)
        require_success(initial)
        if not initial['solved_bodies'] or not initial['compiler_invocations']:
            raise AssertionError('cold fixture did not exercise compiler and solver')
        warm = observations['warm'] = matrix.run(case, mode)
        require_snapshot(warm)
        assert_equivalent(warm, initial)
        if gate:
            matrix.replace(LIB, 'a: 1, b: 2', 'a: 9, b: 2')
            details['requested_revision'] = matrix.revision()
            (gate / 'armed').touch()
            request = matrix.begin(case, mode)
            requests.append(request)
            child = matrix.wait_gate(request, gate)
            details['gate'] = child
            if name == 'cancel-during-compile':
                killed = observations['canceled'] = matrix.cancel(request)
                if killed['exit_code'] == 0:
                    raise AssertionError('canceled process unexpectedly succeeded')
                # The held descendant belongs to our new process group. Zombies
                # have exited and cannot continue analysis or publish results.
                state_path = Path(f'/proc/{child["pid"]}/stat')
                deadline = time.monotonic() + 5
                while state_path.exists():
                    try:
                        state = state_path.read_text().rsplit(')', 1)[1].split()[0]
                    except FileNotFoundError:
                        break
                    if state == 'Z':
                        break
                    if time.monotonic() > deadline:
                        raise AssertionError('cancellation left a live compiler descendant')
                    time.sleep(.02)
                details['descendant_stopped'] = True
            else:
                matrix.replace(LIB, 'a: 9, b: 2', 'a: 8, b: 2')
                if name == 'two-quick-saves':
                    matrix.replace(LIB, 'a: 8, b: 2', 'a: 6, b: 2')
                (gate / 'release').touch()
                observations['overlapped'] = matrix.finish(request)
            (gate / 'armed').unlink()
            (gate / 'release').touch()
            current = observations['current'] = matrix.run(case, mode)
            if current.get('cache', {}).get('validation') == 'snapshot':
                raise AssertionError('interrupted or superseded preflight published a snapshot')
        elif name == 'concurrent-refresh':
            requests.append(matrix.begin(case, mode, 'refresh'))
            requests.append(matrix.begin(case, mode, 'refresh'))
            for index, request in enumerate(requests):
                observations[f'writer{index}'] = matrix.finish(request)
            a, b = observations['writer0'], observations['writer1']
            if any(not value['compiler_invocations'] or not value['solved_bodies'] for value in (a, b)):
                raise AssertionError('refresh writers did not both invoke compiler and solver')
            details['request_intervals_overlap'] = max(a['started'], b['started']) < min(a['completed'], b['completed'])
            if not details['request_intervals_overlap']:
                raise AssertionError('writer requests did not overlap')
            current = observations['current'] = matrix.run(case, mode)
            require_snapshot(current)
        else:
            if name == 'error-recovery':
                matrix.replace(LIB, 'let café =', 'let invalid: i32 = true; let café =')
                for label, policy in (('error_reused', 'on'), ('error_fresh', 'off')):
                    error = observations[label] = matrix.run(case, mode, policy)
                    if not error['exit_code'] or 'mismatched types' not in error['stderr']:
                        raise AssertionError('expected a compiler type error')
                matrix.replace(LIB, 'let invalid: i32 = true; let café =', 'let café =')
            else:
                matrix.replace(LIB, 'a: 1, b: 2', 'a: 9, b: 2')
                changed = observations['changed'] = matrix.run(case, mode)
                oracle = observations['changed_fresh'] = matrix.run(case, mode, 'off')
                require_success(oracle)
                assert_equivalent(changed, oracle)
                matrix.replace(LIB, 'a: 9, b: 2', 'a: 1, b: 2')
            current = observations['current'] = matrix.run(case, mode)
            if current['solved_bodies'] or not current.get('cache', {}).get('hits'):
                raise AssertionError('restoring an unchanged validated body reran its solver')
        revision = matrix.revision()
        fresh = observations['fresh'] = matrix.run(case, mode, 'off')
        require_success(fresh)
        if revision != matrix.revision():
            raise AssertionError('oracle inputs changed')
        if not fresh['compiler_invocations'] or not fresh['solved_bodies']:
            raise AssertionError('fresh oracle did not run compiler and solver')
        for label in ('current', 'overlapped', 'writer0', 'writer1'):
            if label in observations:
                assert_equivalent(observations[label], fresh)
        details['source_revision'] = revision
        ok, error = True, None
    except Exception as problem:
        ok, error = False, str(problem)
        atomic_json(matrix.root / 'failures' / f'{case}.json', observations)
    finally:
        for request in requests:
            if request[0].poll() is None:
                matrix.cancel(request)
        if gate:
            (gate / 'armed').unlink(missing_ok=True)
            (gate / 'release').touch()
    compact = {label: {k: v for k, v in value.items() if k not in ('response', 'stderr')}
               for label, value in observations.items()}
    record = dict(case=name, mode=mode, passed=ok, error=error, observations=compact, **details)
    matrix.records.append(record)
    print(f'{case}: {"PASS" if ok else "FAIL: " + error}', flush=True)
    return record
