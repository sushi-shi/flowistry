#!/usr/bin/env python3
"""Cross-process callee reuse and invalidation against independent fresh results."""
import argparse
import json
from pathlib import Path

from incremental_matrix import Matrix, assert_equivalent
from incremental_races import require_success
from smoke_checkpoint import atomic_json, build_metadata, file_digest

LIB = 'project/src/lib.rs'
LEAF = 'project/src/leaf.rs'
FILES = {
    'project/Cargo.toml': '[package]\nname="matrix_fixture"\nversion="0.0.0"\nedition="2021"\n[workspace]\n',
    LEAF: 'pub fn select(pair: (i32, i32)) -> i32 { pair.0 }\n',
    LIB: '''mod leaf;
fn middle(pair: (i32, i32)) -> i32 { leaf::select(pair) }
fn independent(pair: (i32, i32)) -> i32 { pair.1 }
fn cycle_a(n: u32) -> u32 { if n == 0 { 0 } else { cycle_b(n - 1) } }
fn cycle_b(n: u32) -> u32 { if n == 0 { 0 } else { cycle_a(n - 1) } }
fn opaque(x: &mut i32) { unsafe { *(x as *mut i32) = 17; } }
pub fn selected() -> i32 {
    let mut values = (1, 2);
    let untouched = middle(values);
    let other = independent(values);
    opaque(&mut values.0);
    untouched + other + cycle_a(values.0 as u32) as i32
}
''',
}


def graph_evidence(cache, mode):
    graphs = [json.loads(path.read_text()) for path in (cache / 'dependencies-v1').glob('*.json')]
    selected = [graph for graph in graphs if graph['nodes'][graph['root']]['name'] == 'selected']
    if not selected:
        raise AssertionError('missing selected-body dependency snapshot')
    for graph in selected:
        nodes = graph['nodes']
        expected = {key: [] for key in nodes}
        for caller, node in nodes.items():
            for callee in node['callees']:
                expected[callee].append(caller)
        expected = {key: sorted(set(values)) for key, values in expected.items()}
        if graph['callers'] != expected:
            raise AssertionError('persisted reverse edges disagree with resolved forward edges')
        if mode == 'SigOnly':
            if len(nodes) != 1 or any(node['callees'] for node in nodes.values()):
                raise AssertionError('SigOnly acquired callee-body dependencies')
        else:
            identities = {node['name']: key for key, node in nodes.items()}
            for caller, callee in [('selected', 'middle'), ('middle', 'leaf::select'),
                                   ('cycle_a', 'cycle_b'), ('cycle_b', 'cycle_a')]:
                if identities[callee] not in nodes[identities[caller]]['callees']:
                    raise AssertionError(f'missing resolved dependency: {caller} -> {callee}')
    return {'snapshots': len(graphs), 'selected_snapshots': len(selected),
            'selected_node_counts': [len(graph['nodes']) for graph in selected]}


def check(matrix, mode, name):
    matrix.reset()
    case = mode + '-' + name
    cache = matrix.root / 'caches' / case
    observations, graph, extra = {}, None, {}
    try:
        if name == 'disk-budget':
            matrix.environment['FLOWISTRY_CACHE_MAX_BYTES'] = str(128 * 1024)
        cold = observations['cold'] = matrix.run(case, mode)
        require_success(cold)
        graph = graph_evidence(cache, mode)
        if mode == 'Recurse':
            if not {'middle', 'leaf::select', 'independent', 'opaque', 'cycle_a'} <= set(cold['summary_computations']):
                raise AssertionError('cold request did not compute the expected summaries')
            entries = [json.loads(path.read_text()) for path in (cache / 'summaries-v1').glob('*.json')]
            if not any('Fallback' in entry['payload']['outcome'] for entry in entries):
                raise AssertionError('unsupported callee fallback was not persisted')
        elif cold['summary_computations'] or list((cache / 'summaries-v1').glob('*.json')):
            raise AssertionError('SigOnly unexpectedly computed/persisted callee summaries')

        if name in ('caller-edit', 'corrupt-payload', 'wrong-key', 'refresh', 'disk-budget'):
            matrix.replace(LIB, '(1, 2)', '(3, 4)')
        elif name == 'callee-edit':
            matrix.replace(LEAF, 'pair.0', 'pair.1')
        elif name == 'cycle-edit':
            matrix.replace(LIB, 'cycle_a(n - 1)', 'cycle_a(n.saturating_sub(2))')
        elif name == 'declaration-edit':
            matrix.replace(LEAF, 'pub fn', '#[inline]\npub fn')
        elif name == 'verify':
            extra['FLOWISTRY_VERIFY_SUMMARIES'] = '1'

        paths = sorted((cache / 'summaries-v1').glob('*.json'))
        if name == 'corrupt-payload':
            for path in paths:
                value = json.loads(path.read_text())
                value['integrity'] = 'corrupt'
                path.write_text(json.dumps(value))
        elif name == 'wrong-key' and paths:
            copied = paths[0].read_bytes()
            for path in paths[1:]:
                path.write_bytes(copied)

        reused = observations['reused'] = matrix.run(case, mode, 'refresh' if name == 'refresh' else 'on', extra=extra)
        fresh = observations['fresh'] = matrix.run(case, mode, 'off', extra=extra)
        require_success(reused)
        require_success(fresh)
        assert_equivalent(reused, fresh)
        if reused['compiler_invocations'] != 1:
            raise AssertionError('summary test was bypassed by snapshot replay')
        computations, hits = set(reused['summary_computations']), set(reused['summary_hits'])
        if mode == 'Recurse':
            if name == 'caller-edit':
                if computations or not {'middle', 'independent', 'opaque', 'cycle_a'} <= hits:
                    raise AssertionError('unchanged callees were not reused after caller edit')
            elif name == 'callee-edit':
                if not {'middle', 'leaf::select'} <= computations or 'independent' not in hits:
                    raise AssertionError('callee edit did not selectively invalidate Recurse callers')
            elif name == 'cycle-edit':
                if 'cycle_a' not in computations or 'independent' not in hits or 'middle' not in hits:
                    raise AssertionError('cycle edit did not invalidate its component selectively')
            elif name in ('declaration-edit', 'corrupt-payload', 'refresh'):
                if hits or not computations:
                    raise AssertionError('conservative invalidation/refresh did not recompute summaries')
            elif name == 'wrong-key':
                if not computations:
                    raise AssertionError('wrong-key records were accepted')
            elif name == 'verify':
                if not hits or set(reused['summary_verified']) != hits or not hits <= computations:
                    raise AssertionError('persisted logical summaries were not independently verified')
        elif computations or hits:
            raise AssertionError('SigOnly consulted callee summaries')
        if mode == 'SigOnly' and name in ('callee-edit', 'cycle-edit') and reused['solved_bodies']:
            raise AssertionError('ordinary callee-body edit reran SigOnly solver')

        if name in ('corrupt-payload', 'wrong-key'):
            matrix.replace(LIB, '(3, 4)', '(5, 6)')
            repaired = observations['repaired'] = matrix.run(case, mode)
            oracle = observations['repair_fresh'] = matrix.run(case, mode, 'off')
            require_success(repaired)
            require_success(oracle)
            assert_equivalent(repaired, oracle)
            if repaired['summary_computations']:
                raise AssertionError('repaired summaries did not become reusable')
        if name == 'disk-budget':
            maximum = 0
            for i in range(4, 24):
                matrix.replace(LIB, f'({i - 1}, {i})', f'({i}, {i + 1})')
                result = matrix.run(case, mode)
                oracle = matrix.run(case, mode, 'off')
                require_success(result)
                require_success(oracle)
                assert_equivalent(result, oracle)
                size = sum(path.stat().st_size for path in cache.rglob('*') if path.is_file())
                maximum = max(maximum, size)
                if size > 128 * 1024:
                    raise AssertionError('shared cache namespaces exceeded the disk budget')
            observations['disk'] = {'maximum_bytes': maximum,
                                    'namespaces': sorted(path.name for path in cache.iterdir() if path.is_dir())}
        ok, error = True, None
    except Exception as problem:
        ok, error = False, str(problem)
        atomic_json(matrix.root / 'failures' / (case + '.json'), observations)
    print(f'{case}: {"PASS" if ok else "FAIL: " + error}', flush=True)
    return {'case': name, 'mode': mode, 'passed': ok, 'error': error, 'graph': graph,
            'observations': {label: {key: value for key, value in record.items() if key not in ('response', 'stderr')}
                             for label, record in observations.items()}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--backend-dir', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--json', type=Path, required=True)
    args = parser.parse_args()
    matrix = Matrix(args.backend_dir, args.work_dir, files=FILES)
    report = {'schema': 1, 'build': build_metadata(args.backend_dir),
              'binaries': {name: file_digest(args.backend_dir / name) for name in ('cargo-flowistry', 'flowistry-driver')},
              'harness': {name: file_digest(Path(__file__).with_name(name)) for name in ('test-summary-cache.py', 'incremental_matrix.py')},
              'records': []}
    for mode in ('SigOnly', 'Recurse'):
        for name in ('caller-edit', 'callee-edit', 'cycle-edit', 'declaration-edit', 'corrupt-payload',
                     'wrong-key', 'verify', 'refresh', 'disk-budget'):
            report['records'].append(check(matrix, mode, name))
            atomic_json(args.json, report)
    report['passed'] = all(record['passed'] for record in report['records'])
    atomic_json(args.json, report)
    raise SystemExit(0 if report['passed'] else 1)


if __name__ == '__main__':
    main()
