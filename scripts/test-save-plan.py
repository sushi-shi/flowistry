#!/usr/bin/env python3
"""Real compiler save hints, mode isolation, invalidation and project ordering."""
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess
import traceback

from incremental_matrix import Matrix, decode, smoke
from smoke_checkpoint import atomic_json

spec = importlib.util.spec_from_file_location('coordinator', Path(__file__).with_name('test-project-coordinator.py'))
coordinator = importlib.util.module_from_spec(spec)
spec.loader.exec_module(coordinator)

FILES = {
    'project/Cargo.toml': '[package]\nname="matrix_fixture"\nversion="0.0.0"\nedition="2021"\n[workspace]\n',
    'project/src/lib.rs': '''mod helper;
mod open;
pub fn selected(input: i32) -> i32 { helper::callee(input) }
pub fn cursor(input: i32) -> i32 { input + 7 }
''',
    'project/src/helper.rs': 'pub fn callee(input: i32) -> i32 { input * 2 }\n',
    'project/src/open.rs': 'pub fn unrelated(input: i32) -> i32 { input + 5 }\n',
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--backend-dir', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--json', type=Path, required=True)
    args = parser.parse_args()
    matrix = Matrix(args.backend_dir, args.work_dir, files=FILES)
    report = {'schema': 1, 'records': []}

    for mode in ('SigOnly', 'Recurse'):
        matrix.reset()
        prefix = [str(args.backend_dir.resolve() / 'cargo-flowistry'), 'flowistry',
                  '--package', 'matrix_fixture', '--target-kind', 'lib', '--target-name', 'matrix_fixture',
                  '--context-mode', mode, '--mutability-mode', 'DistinguishMut', '--pointer-mode', 'Precise']

        def run(operation, *arguments, policy='on'):
            result = subprocess.run(prefix + [operation, *map(str, arguments)], cwd=matrix.project,
                                    env=matrix.env(mode, policy), capture_output=True, timeout=120)
            value = decode(result.stdout)
            assert result.returncode == 0 and value and 'Ok' in value, (result.returncode, result.stderr.decode(), value)
            return result, value['Ok']

        def inventory():
            _, value = run('project-bodies', '.')
            return {body['name']: body for body in value['bodies']}

        def warm(bodies, policy='on'):
            for body in bodies.values():
                run('body-focus', body['range']['filename'], body['identity'], policy=policy)

        def check(case, function):
            try:
                detail = function()
                record = {'case': mode + '-' + case, 'passed': True, 'detail': detail}
            except Exception:
                record = {'case': mode + '-' + case, 'passed': False, 'error': traceback.format_exc()}
            report['records'].append(record)
            atomic_json(args.json, report)
            print(record['case'] + ': ' + ('PASS' if record['passed'] else record['error']), flush=True)
            return record['passed']

        def initial():
            bodies = inventory()
            assert len(bodies) == 4 and all(b['save_plan']['status'] == 'unknown' for b in bodies.values()), bodies
            warm(bodies)
            bodies = inventory()
            assert all(b['save_plan']['status'] == 'unchanged-input-hint' for b in bodies.values()), bodies
            assert all(not b['save_plan']['validated'] for b in bodies.values())
            return {name: body['save_plan'] for name, body in bodies.items()}
        if not check('observations-across-workers', initial):
            continue

        def edit():
            matrix.replace('project/src/helper.rs', 'input * 2', 'input * 3')
            bodies = inventory()
            assert bodies['helper::callee']['save_plan']['status'] == 'changed', bodies
            assert bodies['selected']['save_plan']['status'] == ('affected' if mode == 'Recurse' else 'unchanged-input-hint'), bodies
            assert bodies['open::unrelated']['save_plan']['status'] == 'unchanged-input-hint', bodies
            source = matrix.source
            command = prefix + ['project', '--memory-mib', '1024', '--timeout-seconds', '60',
                                '--cursor-file', str(source), '--cursor-line', '3', '--cursor-column', '35',
                                '--saved-file', str(matrix.project / 'src/helper.rs'),
                                '--priority-file', str(matrix.project / 'src/open.rs')]
            stream = coordinator.Stream(command, matrix, mode)
            try:
                stream.finish()
            finally:
                stream.stop()
            assert stream.process.returncode == 0, bytes(stream.errors).decode()
            starts = [e['body']['name'] for e in stream.events if e['event'] == 'body-started']
            assert starts[:2] == ['cursor', 'helper::callee'], starts
            if mode == 'Recurse':
                assert starts == ['cursor', 'helper::callee', 'selected', 'open::unrelated'], starts
            else:
                assert starts == ['cursor', 'helper::callee', 'open::unrelated', 'selected'], starts
            comparisons = []
            work = {}
            for event in stream.events:
                if event['event'] != 'body':
                    continue
                assert event['status'] == 'current', event
                actual = decode(event['output'].encode())['Ok']
                body = event['body']
                diagnostics = event['diagnostics']
                work[body['name']] = {'compiler_invocations': diagnostics.count('audit compiler'),
                                      'solver_invocations': diagnostics.count('audit solve '),
                                      'summary_hits': diagnostics.count('audit summary-hit ')}
                _, expected = run('body-focus', body['range']['filename'], body['identity'], policy='off')
                def selected(value):
                    focus = next(b['focus']['Ok'] for b in value['bodies'] if b['focus'] is not None)
                    return smoke.canonical(coordinator.resolved(focus, value['files']))
                assert selected(actual) == selected(expected), body['name']
                comparisons.append(body['name'])
            for name in ['cursor', 'open::unrelated'] + (['selected'] if mode == 'SigOnly' else []):
                assert work[name]['solver_invocations'] == 0, work
            assert work['helper::callee']['solver_invocations'] > 0, work
            if mode == 'Recurse':
                assert work['selected']['solver_invocations'] > 0, work
            fresh = inventory()
            assert all(b['save_plan']['status'] == 'unchanged-input-hint' for b in fresh.values()), fresh
            return {'order': starts, 'cache_off_equal': comparisons, 'work': work,
                    'hints': {n: b['save_plan'] for n, b in bodies.items()}}
        if not check('callee-edit-and-public-order', edit):
            continue

        def corruption():
            entries = list((matrix.root / 'caches' / mode / 'dependencies-v1').glob('plan-*.json'))
            assert len(entries) >= 4, entries
            for path in entries:
                path.write_text('{"corrupt":true}')
            bodies = inventory()
            assert all(b['save_plan']['status'] == 'unknown' for b in bodies.values()), bodies
            # Snapshot hits need not reconstruct optional scheduling records.
            # They remain unknown until compiler validation writes observations.
            warm(bodies, policy='refresh')
            assert all(b['save_plan']['status'] == 'unchanged-input-hint' for b in inventory().values())
            return {'corrupt_records': len(entries), 'recovered': True}
        check('corrupt-observation-recovery', corruption)

        def declaration():
            matrix.replace('project/src/helper.rs', 'input: i32', 'input: i64')
            # Inventory remains available despite a caller type error. Context
            # changes have no old observations and must schedule all validation.
            bodies = inventory()
            assert all(b['save_plan']['status'] == 'unknown' for b in bodies.values()), bodies
            matrix.replace('project/src/helper.rs', 'input: i64', 'input: i32')
            warm(inventory())
            assert all(b['save_plan']['status'] == 'unchanged-input-hint' for b in inventory().values())
            return {'declaration_fallback': len(bodies), 'revert_recovered': True}
        check('declaration-and-revert', declaration)

    raise SystemExit(0 if len(report['records']) == 8 and all(r['passed'] for r in report['records']) else 1)


if __name__ == '__main__':
    main()
