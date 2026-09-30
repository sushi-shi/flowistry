"""Curated edits in isolated copies of two prepared locked-corpus crates."""
import json
import hashlib
import os
from pathlib import Path
import tomllib

from incremental_matrix import Matrix
from smoke_checkpoint import digest, file_digest

LIB = 'project/src/lib.rs'
CASES = {'either': ('layout', 'body'), 'smallvec': ('layout', 'body', 'callee')}
NAMES = {f'{name}-{case}' for name, cases in CASES.items() for case in cases}


def run(backend, work, prepared, modes, selected, on_record):
    corpus_dir = Path(__file__).parent / 'smoke-corpus'
    corpus = json.loads((corpus_dir / 'corpus.json').read_text())
    records, provenance = [], {}
    for name, cases in CASES.items():
        if selected and not any(f'{name}-{case}' in selected for case in cases):
            continue
        entry = next(e for e in corpus['crates'] if e['name'] == name)
        label = f'{name}-{entry["version"]}'
        source = prepared / label
        package = tomllib.loads((source / 'Cargo.toml').read_text())['package']
        if package['name'] != name or package['version'] != entry['version']:
            raise ValueError('prepared crate does not match the locked corpus: ' + label)
        if file_digest(source / 'Cargo.lock') != file_digest(corpus_dir / label / 'Cargo.lock'):
            raise ValueError('prepared lockfile does not match the locked corpus: ' + label)
        files = {}
        for directory, dirs, names in os.walk(source):
            dirs[:] = [d for d in dirs if d not in ('target', '.git')]
            for child in [*(Path(directory) / d for d in dirs), *(Path(directory) / n for n in names)]:
                if child.is_symlink():
                    raise ValueError('unexpected symlink in curated fixture: ' + str(child))
            for filename in names:
                path = Path(directory) / filename
                files['project/' + str(path.relative_to(source))] = path.read_bytes()
        hashes = {path: hashlib.sha256(value).hexdigest() for path, value in files.items()}
        provenance[name] = {'corpus_entry': entry, 'source': str(source), 'files_sha256': hashes,
                            'prepared_source_digest': digest(hashes)}
        anchor = 'pub fn is_left(&self)' if name == 'either' else 'pub fn is_empty(&self)'
        matrix = Matrix(backend, work / ('real-' + name), files=files, anchor=anchor, offset=1)
        for mode in modes:
            for case in cases:
                label = f'{name}-{case}'
                if selected and label not in selected:
                    continue
                if case == 'layout':
                    edit = lambda m: m.write(LIB, '\n// é🦀\n' + m.source.read_text())
                    expectation = 'hit'
                elif case == 'body' and name == 'either':
                    edit = lambda m: m.replace(LIB, 'Left(_) => true,', 'Left(_) => false,')
                    expectation = 'miss'
                elif case == 'body':
                    edit = lambda m: m.replace(LIB, 'self.len() == 0', 'self.len() == 1')
                    expectation = 'miss'
                else:
                    edit = lambda m: m.replace(LIB, 'self.triple().1', 'self.capacity()')
                    expectation = 'miss' if mode == 'Recurse' else 'hit'
                record = matrix.pair(label, mode, edit, expectation)
                records.append(record)
                on_record(record)
    return records, provenance
