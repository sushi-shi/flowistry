#!/usr/bin/env python3
"""Real compiler checks for source-selection metadata across cache replay/relocation."""
import argparse
import base64
import gzip
import json
import os
from pathlib import Path
import runpy
import subprocess
import tempfile

helpers = runpy.run_path(str(Path(__file__).with_name('test-focus-cache.py')))
expanded, canonical, coverage = (helpers[name] for name in ('expanded', 'canonical', 'coverage'))
SOURCE = '''struct State { health: i32, timer: i32 }
fn restore<'a>(map: &'a State, other: i32) -> State {
    // travel comment
    /* nested /* comment */ café */
    let text = r#"// literal /* literal */"#;
    let state = State { health: map.health, timer: other };
    state
}
fn main() {}
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--backend', required=True)
    args = parser.parse_args()
    backend = str(Path(args.backend).resolve())
    count = 0
    with tempfile.TemporaryDirectory(prefix='flowistry-source-selection-') as directory:
        root = Path(directory)
        (root / 'Cargo.toml').write_text('[package]\nname="selection_cache"\nversion="0.1.0"\nedition="2021"\n')
        (root / 'src').mkdir()
        file = root / 'src/main.rs'

        def run(text, hit, refresh=False, snapshot=False, first_comment='// travel comment'):
            nonlocal count
            file.write_text(text)
            row = next(i for i, line in enumerate(text.splitlines()) if 'fn restore' in line)
            result = subprocess.run([backend, 'file-focus', str(file), str(row), '3'], cwd=root,
                                    env=dict(os.environ, FLOWISTRY_CACHE_DIR=str(root / 'cache'),
                                             FLOWISTRY_CACHE='refresh' if refresh else ''),
                                    capture_output=True, check=True, timeout=120)
            response = json.loads(gzip.decompress(base64.b64decode(result.stdout)))['Ok']
            assert response['cache']['hits'] == int(hit), response['cache']
            if snapshot:
                assert response['cache'].get('validation') == 'snapshot', response['cache']
            focus = next(body['focus']['Ok'] for body in response['bodies'] if body['focus'])
            assert len(focus['comments']) == 2, focus
            assert len(focus['parameter_aliases']) == 2, focus
            resolved = expanded(focus)
            lines = text.splitlines()

            def snippet(r):
                a, b = r['start'], r['end']
                if a['line'] == b['line']:
                    return lines[a['line']][a['column']:b['column']]
                return '\n'.join([lines[a['line']][a['column']:],
                                  *lines[a['line'] + 1:b['line']], lines[b['line']][:b['column']]])

            assert [snippet(r) for r in resolved['comments']] == [
                first_comment, '/* nested /* comment */ café */']
            aliases = [(snippet(a['range']), snippet(a['target'])) for a in resolved['parameter_aliases']]
            assert aliases == [("&'a State", 'map'), ('i32', 'other')], aliases
            count += 1
            return resolved

        cold = run(SOURCE, False)
        assert canonical(run(SOURCE, True, snapshot=True)) == canonical(cold)
        relocated = '\n\n' + SOURCE.replace('    ', '        ').replace('map:', '\n    map:')
        moved = run(relocated, True)
        assert coverage(moved, relocated) == coverage(run(relocated, False, refresh=True), relocated)
        assert canonical(run(relocated, True, snapshot=True)) == canonical(moved)
        changed_comment = relocated.replace('travel comment', 'different travel comment')
        run(changed_comment, False, first_comment='// different travel comment')
        entries = list((root / 'cache' / 'focus-v1').glob('*.json'))
        assert entries, 'no body-cache entries to corrupt'
        for path in entries:
            value = json.loads(path.read_text())
            value['comments'] = [2**32 - 1]
            path.write_text(json.dumps(value))
        # Force compiler validation without reusing the completed-response snapshot.
        restored = run('\n' + relocated, False)
        assert coverage(restored, '\n' + relocated) == coverage(run('\n' + relocated, False, refresh=True), '\n' + relocated)
    print(f'Passed {count} source-selection cache requests (fresh, snapshot, relocation, corrupt metadata)')


if __name__ == '__main__':
    main()
