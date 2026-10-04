#!/usr/bin/env python3
"""Exercise inferred Cargo target names and files excluded by cfg."""
import argparse
import base64
import gzip
import json
import os
from pathlib import Path
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--backend', required=True)
    args = parser.parse_args()
    backend = str(Path(args.backend).resolve())
    with tempfile.TemporaryDirectory(prefix='flowistry target selection ') as tmp:
        root = Path(tmp)
        (root / 'src').mkdir()
        (root / 'Cargo.toml').write_text('''[package]
name="different-package-name"
version="0.1.0"
edition="2021"
[lib]
name="custom_library"
[[bin]]
name="CustomBinary"
path="src/main.rs"
[features]
optional=[]
[workspace]
''')
        (root / 'src/lib.rs').write_text('pub fn helper(value: i32) -> i32 { value + 1 }\n')
        (root / 'src/main.rs').write_text('''#[cfg(feature="optional")] mod optional;
fn main() { let value = custom_library::helper(1); println!("{value}"); }
''')
        (root / 'src/optional.rs').write_text('pub fn optional(value: i32) -> i32 { value * 2 }\n')
        env = dict(os.environ, CARGO_TARGET_DIR=str(root / 'target'),
                   FLOWISTRY_CACHE_DIR=str(root / 'cache'), FLOWISTRY_CACHE='off',
                   FLOWISTRY_RESULT_PROTOCOL='1')

        def run(source, flags=()):
            result = subprocess.run([backend, *flags, 'file-focus', str(root / 'src' / source)],
                                    cwd=root, env=env, capture_output=True, timeout=120)
            assert result.returncode == 0, result.stderr.decode()
            publication = json.loads(result.stdout)
            assert publication['output'], 'successful command returned no analysis'
            return json.loads(gzip.decompress(base64.b64decode(publication['output'])))

        for source in ('lib.rs', 'main.rs'):
            for repeat in range(2):
                value = run(source)
                assert value['Ok']['bodies'], value
                assert all('Err' not in (b.get('focus') or {}) for b in value['Ok']['bodies']), value
        # The binary compiled the library as a dependency. Its metadata must
        # not make Cargo skip the subsequent library analysis.
        assert run('lib.rs')['Ok']['bodies']
        print('custom library and binary targets: PASS')

        # Use explicit selection because this shared source directory contains
        # both a library and a binary; optional.rs belongs only to the binary.
        selection = ('--package', 'different-package-name', '--target-kind', 'bin',
                     '--target-name', 'CustomBinary')
        value = run('optional.rs', selection)
        assert value['Err']['type'] == 'AnalysisError', value
        assert 'not compiled by the selected Cargo target' in value['Err']['error'], value
        assert '#[cfg]' in value['Err']['error'], value
        assert run('optional.rs', (*selection, '--features', 'optional'))['Ok']['bodies']
        print('cfg-excluded file and enabled feature: PASS')


if __name__ == '__main__':
    main()
