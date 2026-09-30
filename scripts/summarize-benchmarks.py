#!/usr/bin/env python3
"""Summarize all recorded samples; never treat resumed correctness evidence as timing."""
import argparse
import json
import math
from pathlib import Path
import statistics

from smoke_checkpoint import atomic_json


def distribution(values):
    if not values:
        return None
    values = sorted(values)
    return {'n': len(values), 'min': values[0], 'median': statistics.median(values),
            'p95': values[max(0, math.ceil(len(values) * .95) - 1)], 'max': values[-1]}


def summarize(report):
    rows, problems = [], []
    names = list(report['backends'])
    counters_required = bool(report.get('measurement', {}).get('perf'))
    for crate in report['crates']:
        problems.extend({'crate': crate.get('name', crate.get('crate')), 'skipped': skipped}
                        for skipped in crate['skipped'])
        for record in crate['records']:
            position = [record['file'], record['line'], record['column'], record['mode']]
            row = {'crate': crate['crate'], 'position': position,
                   'source_sha256': crate.get('source_sha256'), 'backends': {}}
            if record.get('checkpoint_reused'):
                raise ValueError('resumed correctness records cannot be used as new timing evidence')
            if len(names) == 2 and not record.get('same', False):
                problems.append({'crate': crate['crate'], 'position': position, 'issue': 'backends disagree'})
            for name in names:
                result = record[name]
                samples = result.get('samples', [result])
                valid = (result['status'] == 'ok' and not result.get('measurement_error')
                         and result.get('warmup_status', 'ok') == 'ok'
                         and len(samples) == report.get('measurement', {}).get('repeat', len(samples))
                         and all(sample['status'] == 'ok' for sample in samples))
                if counters_required and any(not sample.get('counters') or
                        set(sample['counters']) != {'instructions', 'cycles'} for sample in samples):
                    valid = False
                if not valid:
                    problems.append({'crate': crate['crate'], 'position': position, 'backend': name,
                                     'issue': 'failed/incomplete measurement', 'status': result['status']})
                metrics = {'valid': valid, 'sample_count': len(samples),
                           'seconds': distribution([sample['seconds'] for sample in samples]),
                           'peak_rss_mb': max((sample.get('max_rss_mb', 0) for sample in samples), default=0),
                           'instructions': distribution([s['counters']['instructions'] for s in samples
                                                          if s.get('counters') and 'instructions' in s['counters']]),
                           'cycles': distribution([s['counters']['cycles'] for s in samples
                                                   if s.get('counters') and 'cycles' in s['counters']]),
                           'wire_bytes': distribution([s['wire_bytes'] for s in samples if 'wire_bytes' in s]),
                           'cache_samples': [s.get('cache') for s in samples]}
                for field in ('phases', 'stats'):
                    keys = sorted({key for sample in samples for key in (sample.get(field) or {})})
                    metrics[field] = {key: distribution([s[field][key] for s in samples
                                                        if key in (s.get(field) or {})]) for key in keys}
                row['backends'][name] = metrics
            if len(names) == 2 and all(row['backends'][name]['valid'] for name in names) and record.get('same'):
                a, b = (row['backends'][name] for name in names)
                row['compare_over_base'] = {metric: b[metric]['median'] / a[metric]['median']
                                            for metric in ('seconds', 'instructions', 'cycles')
                                            if a[metric] and b[metric] and a[metric]['median'] > 0}
            rows.append(row)
    return {'schema': 1, 'measurement': report.get('measurement'),
            'validation_manifest': report.get('validation_manifest'),
            'command': report.get('command', 'focus'), 'cache_modes': report.get('cache_modes'),
            'positions': rows, 'problems': problems,
            'measurement_valid': bool(rows) and not problems,
            'note': 'Nearest-rank p95; small samples are exploratory. Wall time includes the harness memory-scope launcher. '
                    'Validity does not imply a quiet host, representative coverage, or a performance improvement.'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('report', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    result = summarize(json.loads(args.report.read_text()))
    atomic_json(args.output, result)
    print(json.dumps({'measurement_valid': result['measurement_valid'],
                      'positions': len(result['positions']), 'problems': len(result['problems'])}))
    raise SystemExit(0 if result['measurement_valid'] else 1)


if __name__ == '__main__':
    main()
