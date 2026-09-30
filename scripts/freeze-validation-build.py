#!/usr/bin/env python3
"""Build a clean revision in a shared Cargo target and archive only the two executables."""
import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import shutil
import subprocess

from smoke_checkpoint import atomic_json, file_digest


def git(source, *args):
    return subprocess.check_output(['git', '-C', str(source), *args], text=True).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--target-dir', type=Path, required=True, help='shared, reusable Cargo build directory')
    parser.add_argument('--output', type=Path, required=True, help='new immutable executable archive')
    parser.add_argument('--features', default='')
    args = parser.parse_args()
    args.source, args.target_dir, args.output = (p.resolve() for p in (args.source, args.target_dir, args.output))
    if args.output.exists():
        parser.error('output exists; choose a new archive, never overwrite evidence')
    if git(args.source, 'status', '--porcelain', '--untracked-files=all'):
        parser.error('source has modifications or untracked files; commit the candidate first')
    revision = git(args.source, 'rev-parse', 'HEAD')
    command = ['cargo', 'build', '--locked', '--release', '-p', 'flowistry_ide', '-j', '4']
    if args.features:
        command += ['--features', args.features]
    subprocess.run(command, cwd=args.source,
                   env=dict(os.environ, CARGO_TARGET_DIR=str(args.target_dir)), check=True)
    if git(args.source, 'rev-parse', 'HEAD') != revision or git(args.source, 'status', '--porcelain', '--untracked-files=all'):
        raise RuntimeError('source changed during build; refusing to attest the output')
    args.output.mkdir(parents=True)
    binaries = {}
    for binary in ('cargo-flowistry', 'flowistry-driver'):
        shutil.copy2(args.target_dir / 'release' / binary, args.output / binary)
        binaries[binary] = file_digest(args.output / binary)
    atomic_json(args.output / 'build.json', {
        'revision': revision, 'tree': git(args.source, 'rev-parse', 'HEAD^{tree}'),
        'compiler': subprocess.check_output(['rustc', '-vV'], text=True).strip(),
        'cargo': subprocess.check_output(['cargo', '-V'], text=True).strip(),
        'command': command, 'features': args.features,
        'rustflags': os.environ.get('RUSTFLAGS', ''),
        'cargo_encoded_rustflags': os.environ.get('CARGO_ENCODED_RUSTFLAGS', ''),
        'profile': {k: v for k, v in os.environ.items() if k.startswith('CARGO_PROFILE_')},
        'binaries': binaries, 'recorded_at': datetime.now(timezone.utc).isoformat(),
    })
    print(json.dumps({'archive': str(args.output), 'revision': revision}))


if __name__ == '__main__':
    main()
