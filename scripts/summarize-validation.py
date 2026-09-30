#!/usr/bin/env python3
"""Audit locked-corpus coverage and retain every difference/status for human triage."""
import argparse
from collections import Counter
import importlib.util
import json
from pathlib import Path

from smoke_checkpoint import atomic_json

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('smoke', ROOT / 'scripts/smoke-real-crates.py')
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


def summarize(report, corpus_dir):
    corpus = json.loads((corpus_dir / 'corpus.json').read_text())
    entries = corpus['crates']
    names = list(report['backends'])
    if len(names) not in (1, 2):
        raise ValueError('expected one reference-check backend or two comparison backends')
    by_name = {}
    for crate in report['crates']:
        name = crate.get('name')
        if name is None and crate.get('skipped'):
            skipped_name = crate['skipped'][0][0]
            if skipped_name in {e['name'] for e in entries}:
                name = skipped_name
        if name is None:
            name = next((e['name'] for e in entries if crate.get('spec') in
                         (e.get('git'), f"{e['name']}@{e.get('version')}")), None)
        if name is None or name in by_name:
            raise ValueError(f'unidentified or duplicate crate report: {name}')
        by_name[name] = crate
    summary = {'schema': 1, 'checkpoint_id': report.get('checkpoint_id'),
               'validation_manifest': report.get('validation_manifest'),
               'crates': [], 'missing_entries': [], 'status_counts': {},
               'output_differences': [], 'status_changes': [], 'failures': []}
    counts = {name: Counter() for name in names}
    for entry in entries:
        name = entry['name']
        if name not in by_name:
            summary['missing_entries'].append(name)
            continue
        crate = by_name[name]
        directory = name if 'git' in entry else f"{name}-{entry['version']}"
        expected = {(file, line, col, mode)
                    for file, line, col in smoke.read_positions(corpus_dir / directory / 'positions.tsv')
                    for mode in smoke.MODES}
        actual = []
        for rec in crate['records']:
            position = (rec['file'], rec['line'], rec['column'], rec['mode'])
            actual.append(position)
            where = {'crate': name, 'position': list(position)}
            for backend in names:
                result = rec[backend]
                counts[backend][result['status']] += 1
                if result['status'] not in ('ok', 'benign'):
                    summary['failures'].append(dict(where, backend=backend, result=result))
            if len(names) == 2 and not rec.get('same', False):
                results = {backend: rec[backend] for backend in names}
                key = 'output_differences' if all(r['status'] == 'ok' for r in results.values()) else 'status_changes'
                summary[key].append(dict(where, results=results))
        observed = set(actual)
        summary['crates'].append({'name': name, 'expected': len(expected), 'recorded': len(actual),
                                  'missing': sorted(expected - observed), 'extra': sorted(observed - expected),
                                  'duplicates': len(actual) - len(observed), 'skipped': crate['skipped'],
                                  'reused': sum(bool(r.get('checkpoint_reused')) for r in crate['records'])})
    summary['status_counts'] = {name: dict(value) for name, value in counts.items()}
    summary['coverage_complete'] = not summary['missing_entries'] and all(
        not any(c[k] for k in ('missing', 'extra', 'duplicates', 'skipped')) for c in summary['crates'])
    summary['needs_triage'] = bool(summary['output_differences'] or summary['status_changes'] or summary['failures'])
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('report', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--corpus-dir', type=Path, default=ROOT / 'scripts/smoke-corpus')
    args = parser.parse_args()
    result = summarize(json.loads(args.report.read_text()), args.corpus_dir)
    atomic_json(args.output, result)
    print(json.dumps({k: result[k] for k in ('coverage_complete', 'needs_triage', 'status_counts', 'missing_entries')}, indent=2))
    raise SystemExit(0 if result['coverage_complete'] and not result['needs_triage'] else 1)


if __name__ == '__main__':
    main()
