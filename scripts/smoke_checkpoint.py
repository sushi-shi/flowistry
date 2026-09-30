"""Content-addressed checkpoints for correctness runs, never benchmark resumption."""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def file_digest(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def tree_digest(root):
    """Hash prepared source contents, ignoring compiler outputs and git metadata."""
    root = Path(root)
    files = {}
    for directory, dirs, names in os.walk(root):
        dirs[:] = sorted(d for d in dirs if d not in ('target', '.git'))
        if any((Path(directory) / name).is_symlink() for name in dirs):
            raise ValueError('checkpoint source trees cannot contain directory symlinks')
        for name in sorted(names):
            path = Path(directory) / name
            if path.is_file():
                files[str(path.relative_to(root))] = file_digest(path)
    return digest(files)


def atomic_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode='w', dir=path.parent, delete=False) as stream:
        temporary = Path(stream.name)
        try:
            json.dump(value, stream, sort_keys=True, indent=2)
            stream.write('\n')
            stream.flush()
            os.fsync(stream.fileno())
        except BaseException:
            temporary.unlink(missing_ok=True)
            raise
    os.replace(temporary, path)


class Checkpoints:
    def __init__(self, directory, manifest):
        self.directory = Path(directory)
        self.directory.mkdir(parents=True, exist_ok=True)
        self.lock = (self.directory / '.lock').open('a')
        try:
            fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            self.lock.close()
            raise RuntimeError('checkpoint directory is in use by another validation run') from None
        self.manifest = manifest
        self.identity = digest(manifest)
        self.root = self.directory / self.identity
        atomic_json(self.root / 'manifest.json', manifest)

    def close(self):
        self.lock.close()

    def load(self, key):
        try:
            value = json.loads((self.root / f'{digest(key)}.json').read_text())
            if value['key'] == key and value['checksum'] == digest(value['record']):
                return value['record']
        except (OSError, ValueError, KeyError, TypeError):
            pass
        return None

    def save(self, key, record):
        atomic_json(self.root / f'{digest(key)}.json', {
            'key': key, 'record': record, 'checksum': digest(record),
        })


def manifest(args, backends, corpus_dir, harness):
    """Keep environment values private; a conservative digest prevents unsafe reuse."""
    env = dict(os.environ)
    return {
        'schema': 1,
        'harness': file_digest(harness),
        'checkpoint_implementation': file_digest(__file__),
        'corpus': tree_digest(corpus_dir),
        'environment_sha256': digest(env),
        'configuration_inputs': configuration_inputs(args.work_dir),
        'compiler': subprocess.check_output(['rustc', '-vV'], text=True).strip(),
        'backends': {name: {binary: file_digest(directory / binary)
                           for binary in ('cargo-flowistry', 'flowistry-driver')}
                     for name, directory in backends},
        'builds': {name: build_metadata(directory) for name, directory in backends},
        'settings': {key: getattr(args, key) for key in
                     ('modes', 'timeout', 'memory_limit', 'phases', 'repeat', 'budgets',
                      'command', 'base_cache', 'compare_cache')},
        'work_dir': str(args.work_dir),
        'cache_dir': str(args.cache_dir) if args.cache_dir else None,
    }


def build_metadata(directory):
    path = directory / "build.json"
    if not path.exists():
        return None
    value = json.loads(path.read_text())
    for binary in ("cargo-flowistry", "flowistry-driver"):
        if value["binaries"][binary] != file_digest(directory / binary):
            raise ValueError(f"{directory}: archived binary no longer matches build.json")
    return value


def configuration_inputs(work_dir):
    root = Path(work_dir).resolve()
    paths = {base / name for base in (root, *root.parents)
             for name in (".cargo/config", ".cargo/config.toml", "rust-toolchain", "rust-toolchain.toml")}
    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    paths.update(cargo_home / name for name in ("config", "config.toml"))
    return {str(path): file_digest(path) if path.is_file() else None for path in sorted(paths)}
