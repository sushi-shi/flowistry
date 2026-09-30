#!/usr/bin/env python3
"""Validate real macro-source filename identities through cold and cached output."""
import argparse
from pathlib import Path

from incremental_matrix import Matrix, assert_equivalent
from incremental_races import require_success
from smoke_checkpoint import atomic_json, build_metadata, file_digest

FILES = {
    'project/Cargo.toml': '[package]\nname="matrix_fixture"\nversion="0.0.0"\nedition="2021"\n[workspace]\n',
    'project/src/lib.rs': '''pub fn selected(line: &str) -> bool {
    line.chars().all(|c| matches!(c, ' ' | '\\t' | '\\r' | '\\n'))
}
''',
}


class ClosureMatrix(Matrix):
    def command(self, mode, source=None):
        command = super().command(mode, source)
        line = (source or self.source).read_text().splitlines()[int(command[-2])]
        command[-1] = str(line.index('|c|') + 1)
        return command


def filenames(value):
    if isinstance(value, dict):
        for key, item in value.items():
            if key == 'filename':
                yield item
            else:
                yield from filenames(item)
    elif isinstance(value, list):
        for item in value:
            yield from filenames(item)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--backend-dir', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--json', type=Path, required=True)
    args = parser.parse_args()
    matrix = ClosureMatrix(args.backend_dir, args.work_dir, files=FILES, anchor='line.chars().all')
    report = {'schema': 1, 'build': build_metadata(args.backend_dir),
              'harness': {name: file_digest(Path(__file__).with_name(name)) for name in
                          ('test-file-identities.py', 'incremental_matrix.py', 'smoke-real-crates.py')},
              'records': []}
    for mode in ('SigOnly', 'Recurse'):
        fresh = None
        for name, policy in [('cache-off', 'off'), ('fresh-cache', 'on'), ('snapshot', 'on')]:
            result, foreign = None, []
            try:
                result = matrix.run(mode, mode, policy)
                require_success(result)
                output = result['response']['Ok']
                files = output['files']
                ids = set(filenames(output['bodies']))
                if any(type(value) is not int for value in ids):
                    raise AssertionError('existing numeric filename fields changed protocol')
                if set(files) != {str(value) for value in ids}:
                    raise AssertionError('filename table does not exactly cover emitted ranges')
                primary = str(output['bodies'][0]['range']['filename'])
                foreign = [files[str(value)] for value in ids if files[str(value)] != files[primary]]
                if not any('/library/core/src/macros/mod.rs' in path for path in foreign):
                    raise AssertionError('macro definition source identity was lost')
                if fresh is None:
                    fresh = result
                else:
                    assert_equivalent(result, fresh)
                if name == 'snapshot' and result['compiler_invocations']:
                    raise AssertionError('filename table prevented unchanged-input snapshot replay')
                passed, error = True, None
            except Exception as problem:
                passed, error = False, str(problem)
            record = {'mode': mode, 'case': name, 'passed': passed, 'error': error,
                      'foreign_files': foreign, 'observation': result}
            report['records'].append(record)
            atomic_json(args.json, report)
            print(f'{mode}-{name}: {"PASS" if passed else "FAIL: " + error}', flush=True)
    report['passed'] = all(record['passed'] for record in report['records'])
    atomic_json(args.json, report)
    raise SystemExit(0 if report['passed'] else 1)


if __name__ == '__main__':
    main()
