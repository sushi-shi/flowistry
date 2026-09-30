#!/usr/bin/env python3
"""Prove response replay avoids compiler invocation and still invalidates."""
import argparse
import base64
import gzip
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--backend', required=True)
    backend = str(Path(parser.parse_args().backend).resolve())
    with tempfile.TemporaryDirectory(prefix='flowistry fast cache ') as tmp:
        outside = Path(tmp)
        root = outside / 'project'
        (root / 'src').mkdir(parents=True)
        counter = outside / 'compiler-count'
        wrapper = outside / 'rustc-wrapper'
        wrapper.write_text('#!/bin/sh\necho x >> ' + shlex.quote(str(counter)) + '\nexec "$@"\n')
        wrapper.chmod(0o755)
        (root / 'Cargo.toml').write_text('[package]\nname="fast_cache_test"\nversion="0.1.0"\nedition="2021"\n')
        external = outside / 'included.txt'
        external.write_text('hello')
        build_input = outside / 'build-input.txt'
        build_input.write_text('one')
        dep = outside / 'dependency'
        (dep / 'src').mkdir(parents=True)
        (dep / 'Cargo.toml').write_text('[package]\nname="fast_dep"\nversion="0.1.0"\nedition="2021"\n')
        dep_input = outside / 'dependency included.txt'
        dep_input.write_text('first')
        (dep / 'src/lib.rs').write_text(f'pub fn value() -> usize {{ include_str!({json.dumps(str(dep_input))}).len() }}\n')
        manifest = root / 'Cargo.toml'
        manifest.write_text(manifest.read_text() + '\n[dependencies]\nfast_dep={path="../dependency"}\n')
        (root / 'build.rs').write_text('fn main() {\n'
            f' println!("cargo:rerun-if-changed={{}}", {json.dumps(str(build_input))});\n'
            f' let s = std::fs::read_to_string({json.dumps(str(build_input))}).unwrap();\n'
            ' println!("cargo:rustc-env=BUILD_VALUE={s}");\n}\n')
        source = root / 'src/main.rs'
        source.write_text('fn helper() -> usize { 1 }\n'
            'fn main() {\n'
            f' let value = include_str!({json.dumps(str(external))}).len() + helper();\n'
            ' let env = option_env!("FLOWISTRY_TEST_ENV");\n'
            ' let build = env!("BUILD_VALUE");\n'
            ' let output = (value, env, build, fast_dep::value());\n}\n')
        cache = outside / 'cache'
        env = dict(os.environ, FLOWISTRY_CACHE_DIR=str(cache), RUSTC_WRAPPER=str(wrapper), FLOWISTRY_TEST_ENV='one')
        count = 0
        times = []

        def calls():
            return counter.read_text().count('\n') if counter.exists() else 0

        def run(fast, line=5, column=6, mode='', success=True):
            nonlocal count
            before = calls()
            start = time.monotonic()
            p = subprocess.run([backend, '--context-mode', 'Recurse', 'file-focus', str(source), str(line), str(column)],
                               cwd=root, env=dict(env, FLOWISTRY_CACHE=mode), capture_output=True, timeout=120)
            elapsed = time.monotonic() - start
            if not success:
                assert p.returncode != 0, p.stdout
                assert calls() > before, 'compile failure must reach compiler'
                count += 1
                return
            assert p.returncode == 0, p.stderr.decode()
            value = json.loads(gzip.decompress(base64.b64decode(p.stdout)))['Ok']
            assert (value['cache'].get('validation') == 'snapshot') == fast, (count + 1, value['cache'], p.stderr.decode())
            assert (calls() == before) == fast, 'compiler must only run on misses'
            if fast:
                times.append(elapsed)
            count += 1
            return value

        run(False)
        # First discovery includes external build/dependency inputs that rustc's
        # selected SourceMap cannot attest. Revalidate the recorded watch list
        # once before allowing a compiler-free response.
        run(False)
        run(True)
        source.write_text(source.read_text())
        run(True)  # Unchanged save changes stamps, not analysis inputs.
        # Nix changes scratch directories each launch; irrelevant launcher
        # variables must not force a compiler restart.
        env['NIX_BUILD_TOP'] = str(outside / 'new-nix-build-top')
        env['SHLVL'] = '42'
        run(True)
        run(True, line=2, column=6)  # Different cursor, same saved function.
        run(False, line=0, column=24)  # Another body must be analyzed first.
        run(True, line=0, column=24)
        run(True)  # Adding another result retains the first body.
        external.write_text('a different included string')
        run(False)
        run(True)
        dep_input.write_text('changed dependency include')
        run(False)
        run(True)
        build_input.write_text('two')
        run(False)
        run(True)
        env['FLOWISTRY_TEST_ENV'] = 'two'
        run(False)
        run(True)
        # Same length and restored mtime still invalidate via ctime/inode.
        old = source.stat()
        source.write_text(source.read_text().replace('usize { 1 }', 'usize { 2 }'))
        os.utime(source, ns=(old.st_atime_ns, old.st_mtime_ns))
        run(False)
        run(True)
        config = root / '.cargo/config.toml'
        config.parent.mkdir()
        config.write_text('[build]\nrustflags=["--cfg", "fast_cache_test"]\n')
        run(False)
        run(True)
        run(False, mode='refresh')
        run(True)
        run(False, mode='off')
        run(True)
        for path in (cache / 'responses-v1').glob('*.json'):
            path.write_text('{broken')
        run(False)
        run(True)
        good = source.read_text()
        source.write_text(good.replace('let output =', 'let broken: i32 = true; let output ='))
        run(False, success=False)
        source.write_text(good)
        run(True)  # Undo restores the previous successful snapshot.
        run(True)
        source.write_text(good.replace('option_env!("FLOWISTRY_TEST_ENV")', 'option_env!("NIX_BUILD_TOP")'))
        run(False)
        run(False)  # Explicit semantic use of a transient variable disables replay.
        print(f'Passed {count} fast-cache cases; {len(times)} hits invoked no compiler; warm request min={min(times):.3f}s max={max(times):.3f}s')


if __name__ == '__main__':
    main()
