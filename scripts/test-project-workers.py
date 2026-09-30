#!/usr/bin/env python3
"""Check explicit target inventories and portable body workers against file-focus."""
import argparse
import fcntl
import json
from pathlib import Path
import subprocess
import time
import traceback

from incremental_matrix import Matrix, decode, smoke
from smoke_checkpoint import atomic_json

FILES = {
    'project/Cargo.toml': '''[package]
name="matrix_fixture"
version="0.0.0"
edition="2021"
[lib]
name="custom_library"
[[bin]]
name="custom-binary"
path="src/main.rs"
[[bin]]
name="file-focus"
path="src/extra.rs"
[features]
alternate=[]
broken=[]
[workspace]
''',
    'project/src/lib.rs': '''mod shared;
#[path="../../outside.rs"] mod outside;
pub fn selected(input: i32) -> i32 {
 let closure = |x: i32| shared::helper(x);
 closure(input)
}
#[cfg(feature="alternate")] pub fn extra(input: i32) -> i32 { input + 1 }
#[cfg(feature="broken")] pub fn broken() -> i32 { "type error" }
''',
    'project/src/main.rs': 'mod shared;\nfn main() { println!("{}", shared::helper(3)); }\n',
    'project/src/extra.rs': 'mod shared;\nfn main() { println!("{}", shared::helper(4)); }\n',
    'project/src/shared.rs': 'pub fn helper(input: i32) -> i32 { input * 2 }\n',
    'outside.rs': 'pub fn external(input: i32) -> i32 { input + 4 }\n',
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--backend-dir', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--json', type=Path, required=True)
    args = parser.parse_args()
    matrix = Matrix(args.backend_dir, args.work_dir, files=FILES)
    report = {'schema': 1, 'records': []}

    def run(case, mode, kind, name, operation, *arguments, policy='on', features=None):
        command = [str(args.backend_dir.resolve() / 'cargo-flowistry'), 'flowistry',
                   '--context-mode', mode, '--package', 'matrix_fixture',
                   '--target-kind', kind, '--target-name', name]
        if features:
            command += ['--features', features]
        command += [operation, *map(str, arguments)]
        environment = matrix.env(case, policy)
        result = subprocess.run(command, cwd=matrix.project, env=environment,
                                capture_output=True, timeout=120)
        return result, decode(result.stdout)

    def check(case, function):
        try:
            detail = function()
            record = {'case': case, 'passed': True, 'detail': detail}
        except Exception as error:
            record = {'case': case, 'passed': False, 'error': traceback.format_exc()}
        report['records'].append(record)
        atomic_json(args.json, report)
        print(case + ': ' + ('PASS' if record['passed'] else 'FAIL ' + record['error']), flush=True)

    def target_inventory():
        result = subprocess.run([str(args.backend_dir.resolve() / 'cargo-flowistry'), 'flowistry', 'project-targets'],
                                cwd=matrix.project, env=matrix.env('targets', 'on'), capture_output=True, timeout=30)
        assert result.returncode == 0, result.stderr.decode()
        value = decode(result.stdout)['Ok']
        assert value['schema'] == 1 and Path(value['workspace_root']) == matrix.project, value
        assert {(t['target_kind'], t['target_name']) for t in value['targets']} == {
            ('lib', 'custom_library'), ('bin', 'custom-binary'), ('bin', 'file-focus')}, value
        assert all(t['package'] == 'matrix_fixture' and t['package_id'] and t['supported'] for t in value['targets'])
        assert all(Path(t['src_path']).is_file() and Path(t['manifest_path']).is_file() for t in value['targets'])
        assert b'audit compiler' not in result.stderr, result.stderr.decode()
        return value
    check('project-targets', target_inventory)

    for mode in ('SigOnly', 'Recurse'):
        for kind, name, source in [('lib', 'custom_library', 'lib.rs'), ('bin', 'custom-binary', 'main.rs'), ('bin', 'file-focus', 'extra.rs')]:
            case = mode + '-' + kind + '-' + name

            def workers():
                inventory_result, inventory = run(case, mode, kind, name, 'project-bodies', matrix.project / 'src' / source)
                assert inventory_result.returncode == 0, inventory_result.stderr.decode()
                bodies = inventory['Ok']['bodies']
                assert bodies and all('error' not in body for body in bodies), inventory
                assert any(body['name'] == 'shared::helper' for body in bodies), inventory
                assert len({body['identity'] for body in bodies}) == len(bodies), inventory
                assert all(isinstance(body['range']['filename'], str) for body in bodies)
                observations = []
                for body in bodies:
                    filename = Path(body['range']['filename'])
                    oracle_result, oracle = run(case, mode, kind, name, 'file-focus', filename, policy='off')
                    assert oracle_result.returncode == 0, oracle_result.stderr.decode()
                    wanted = next(b for b in oracle['Ok']['bodies']
                                  if b['range']['start'] == body['range']['start'] and b['range']['end'] == body['range']['end'])
                    assert wanted['focus'] and 'Ok' in wanted['focus'], wanted
                    for pass_name in ('fresh', 'warm'):
                        result, value = run(case, mode, kind, name, 'body-focus', filename, body['identity'])
                        assert result.returncode == 0, result.stderr.decode()
                        selected = [b for b in value['Ok']['bodies'] if b['focus'] is not None]
                        assert len(selected) == 1, value
                        # Resolve response-local source IDs before comparing all focus fields.
                        def resolved(item, files):
                            if isinstance(item, dict):
                                return {key: files[str(child)] if key == 'filename' else resolved(child, files)
                                        for key, child in item.items()}
                            if isinstance(item, list):
                                return [resolved(child, files) for child in item]
                            return item
                        actual = smoke.canonical(resolved(selected[0]['focus']['Ok'], value['Ok']['files']))
                        expected = smoke.canonical(resolved(wanted['focus']['Ok'], oracle['Ok']['files']))
                        if actual != expected:
                            atomic_json(matrix.root / 'failures' / (case + '-raw.json'),
                                        {'body': body, 'pass': pass_name, 'actual': actual, 'expected': expected})
                        assert actual == expected, f'{body["name"]} {pass_name}: focus differs; see failures/{case}-raw.json'
                        if pass_name == 'warm':
                            assert b'audit compiler' not in result.stderr, result.stderr.decode()
                        observations.append({'body': body['name'], 'pass': pass_name,
                                             'compilers': result.stderr.count(b'audit compiler')})
                    indexed, _ = run(case, mode, kind, name, 'result-index', filename)
                    index = json.loads(indexed.stdout)
                    assert index['status'] == 'current', index
                    assert next(b for b in index['bodies'] if b['identity'] == body['identity'])['available'], index
                    assert 'matrix_fixture' in index['package'], index
                return observations
            check(case, workers)

        def features():
            case = mode + '-features'
            _, plain = run(case, mode, 'lib', 'custom_library', 'project-bodies', matrix.source)
            _, extra = run(case, mode, 'lib', 'custom_library', 'project-bodies', matrix.source, features='alternate')
            assert 'extra' not in {b['name'] for b in plain['Ok']['bodies']}
            assert 'extra' in {b['name'] for b in extra['Ok']['bodies']}
            return {'default': len(plain['Ok']['bodies']), 'alternate': len(extra['Ok']['bodies'])}
        check(mode + '-features', features)

        def errors():
            case = mode + '-errors'
            result, inventory = run(case, mode, 'lib', 'custom_library', 'project-bodies', matrix.source, features='broken')
            assert result.returncode == 0, result.stderr.decode()
            broken = next(b for b in inventory['Ok']['bodies'] if b['name'] == 'broken')
            result, value = run(case, mode, 'lib', 'custom_library', 'body-focus', matrix.source, broken['identity'], features='broken')
            assert result.returncode != 0 or 'Err' in value, value
            good = next(b for b in inventory['Ok']['bodies'] if b['name'] == 'selected')
            result, value = run(case, mode, 'lib', 'custom_library', 'body-focus', matrix.source, good['identity'], features='broken')
            assert result.returncode == 0 and 'Ok' in value, result.stderr.decode()
            result, value = run(case, mode, 'lib', 'custom_library', 'body-focus', matrix.source, 'nonexistent')
            assert result.returncode != 0 or 'Err' in value, value
            result, value = run(case, mode, 'lib', 'custom_library', 'body-focus', matrix.project / 'src/shared.rs', good['identity'])
            assert result.returncode != 0 or 'Err' in value, value
            return {'type_error_isolated': True, 'wrong_identity_rejected': True, 'wrong_file_rejected': True}
        check(mode + '-errors', errors)

    def invalid_target():
        result, _ = run('invalid-target', 'SigOnly', 'bin', 'missing', 'project-bodies', matrix.source)
        assert result.returncode != 0, result.stdout.decode()
        assert b'requested target does not exist' in result.stderr, result.stderr.decode()
        return {'exit_code': result.returncode}
    check('invalid-target', invalid_target)

    def missing_selection():
        result = subprocess.run([str(args.backend_dir.resolve() / 'cargo-flowistry'),
                                 'flowistry', 'project-bodies', str(matrix.source)],
                                cwd=matrix.project, env=matrix.env('missing-selection', 'off'),
                                capture_output=True, timeout=30)
        assert result.returncode != 0, result.stdout.decode()
        assert b'require explicit package and target selection' in result.stderr, result.stderr.decode()
        return {'exit_code': result.returncode}
    check('missing-selection', missing_selection)

    def launcher_lock():
        locks = list((matrix.root / 'target').glob('plugin-*/.flowistry-launch.lock'))
        assert len(locks) == 1, locks
        sentinel = locks[0].parent / 'debug/deps/libcustom_library-launch-lock-test.rmeta'
        sentinel.write_bytes(b'fixture-owned marker: must survive until the launcher acquires its lock')
        process = None
        try:
            with locks[0].open('rb+') as lock:
                fcntl.flock(lock, fcntl.LOCK_EX)
                command = [str(args.backend_dir.resolve() / 'cargo-flowistry'), 'flowistry',
                           '--package', 'matrix_fixture', '--target-kind', 'lib',
                           '--target-name', 'custom_library', 'project-bodies', str(matrix.source)]
                process = subprocess.Popen(command, cwd=matrix.project, env=matrix.env('lock', 'off'),
                                           stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                time.sleep(.3)
                assert process.poll() is None, 'launcher did not wait for the target lock'
                assert sentinel.exists(), 'launcher removed library artifacts before acquiring the lock'
                fcntl.flock(lock, fcntl.LOCK_UN)
            stdout, stderr = process.communicate(timeout=120)
            assert process.returncode == 0 and decode(stdout), stderr.decode()
            assert not sentinel.exists(), 'explicit library preparation did not run after the lock was released'
        finally:
            sentinel.unlink(missing_ok=True)
            if process and process.poll() is None:
                process.kill()
                process.communicate()
        return {'waited_for_lock': True, 'artifact_preparation_serialized': True}
    check('launcher-lock', launcher_lock)
    raise SystemExit(0 if all(record['passed'] for record in report['records']) else 1)


if __name__ == '__main__':
    main()
