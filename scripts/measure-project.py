#!/usr/bin/env python3
"""Audit every body of an explicit Cargo target and retain project resource curves.

Run after other acceptance jobs in an isolated/prepared source tree. This command
does not certify host quietness, cold builds, hardware counters or editor latency.
Raw outcomes survive failure; an existing output directory is never overwritten.
"""
import argparse
import base64
import gzip
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import subprocess
import time
import traceback
import uuid

from smoke_checkpoint import atomic_json, digest, file_digest, tree_digest

spec = importlib.util.spec_from_file_location('smoke', Path(__file__).with_name('smoke-real-crates.py'))
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)
LIMIT = 128 * 1024 * 1024


def decode(encoded):
    with gzip.GzipFile(fileobj=io.BytesIO(base64.b64decode(encoded, validate=True))) as source:
        raw = source.read(LIMIT + 1)
    if len(raw) > LIMIT:
        raise ValueError('decoded response exceeds measurement safety limit')
    value = json.loads(raw)
    if not isinstance(value, dict) or 'Ok' not in value:
        raise ValueError('analysis did not return an Ok response')
    return value['Ok']


def selected_digest(encoded, identity, expected_range=None, project=None):
    value = decode(encoded.strip())
    selected = [b for b in value['bodies'] if b.get('focus') is not None]
    if len(selected) != 1 or 'Ok' not in selected[0]['focus']:
        raise ValueError('body-focus must contain exactly one successful selection')
    # Compare the full file response too: includes filename identities, selected
    # range, source-selection metadata and maybe-slices, not just place counts.
    if value.get('cache', {}).get('selected_identity', identity) != identity:
        raise ValueError('wrong selected identity')
    if expected_range is not None:
        resolved = smoke.file_focus_source_ids(value)
        selected_range = next(b['range'] for b in resolved['bodies'] if b.get('focus') is not None)
        if project is not None:
            selected_range = dict(selected_range, filename=str((project / selected_range['filename']).resolve()))
            expected_range = dict(expected_range, filename=str((project / expected_range['filename']).resolve()))
        if selected_range != expected_range:
            raise ValueError('selected body range differs from the independent inventory')
    return digest(smoke.canonical(value))


def audit_stream(path, inventory, project=None):
    expected = {b['identity']: b for b in inventory}
    if len(expected) != len(inventory):
        raise ValueError('duplicate inventory identities')
    outcomes, started, run, final, total = {}, set(), None, None, None
    with path.open() as lines:
        for sequence, line in enumerate(lines):
            if final is not None:
                raise ValueError('event after terminal outcome')
            event = json.loads(line)
            if event['schema'] != 1 or event['sequence'] != sequence:
                raise ValueError('invalid stream schema/sequence')
            if sequence == 0:
                run = event['run']
                if event['event'] != 'started':
                    raise ValueError('missing started event')
            if event['run'] != run:
                raise ValueError('mixed stream run IDs')
            kind = event['event']
            if kind == 'inventory':
                if total is not None or event['total'] != len(expected):
                    raise ValueError('inventory coverage changed')
                total = event['total']
            elif kind in ('body-started', 'body'):
                identity = event['body']['identity']
                if total is None or identity not in expected:
                    raise ValueError('unknown body or missing inventory')
                if kind == 'body-started':
                    if identity in started or event['ordinal'] != len(outcomes):
                        raise ValueError('duplicate/out-of-order body start')
                    started.add(identity)
                    continue
                if identity not in started or identity in outcomes:
                    raise ValueError('missing start or duplicate body outcome')
                item = {k: v for k, v in event.items() if k not in ('output', 'inputs', 'diagnostics')}
                diagnostics = event.get('diagnostics', '')
                item['compiler_invocations'] = len(re.findall(r'audit compiler\b', diagnostics))
                item['solved_bodies'] = re.findall(r'audit solve (focus|shared|summary) (.+)', diagnostics)
                if event['status'] in ('current', 'uncached'):
                    if event.get('current') != (event['status'] == 'current'):
                        raise ValueError('inconsistent publication currentness')
                    item['semantic_digest'] = selected_digest(event['output'], identity, expected[identity].get('range'), project)
                    item['wire_bytes'] = len(event['output'].encode())
                    # Short-lived scopes can disappear before the supervisor's
                    # first sample. Keep unknown distinct from a zero-byte peak.
                    item['observed_peak_memory_bytes'] = event.get('observed_peak_memory_bytes')
                outcomes[identity] = item
            elif kind == 'finished':
                final = event
            elif kind not in ('started', 'diagnostic'):
                raise ValueError('unknown stream event')
    if not final or total is None or set(outcomes) != set(expected) or started != set(expected):
        raise ValueError('incomplete project body coverage')
    successes = sum('semantic_digest' in b for b in outcomes.values())
    required = {'total': len(expected), 'completed': len(expected), 'succeeded': successes,
                'failed': len(expected) - successes, 'pending': 0, 'coverage': 'initial-inventory',
                'project_current': False, 'status': 'complete' if successes == len(expected) else 'partial'}
    if any(final.get(k) != v for k, v in required.items()):
        raise ValueError('terminal counts/status contradict body outcomes')
    return {'final': final, 'bodies': outcomes,
            'missing_memory_observations': [identity for identity, item in outcomes.items()
                                            if item.get('observed_peak_memory_bytes') is None]}


def disk_usage(root):
    apparent = allocated = files = 0
    for directory, _, names in os.walk(root):
        for name in names:
            path = Path(directory, name)
            try:
                stat = path.lstat()
            except FileNotFoundError:
                continue
            apparent += stat.st_size
            allocated += stat.st_blocks * 512
            files += 1
    return {'files': files, 'apparent_bytes': apparent, 'allocated_bytes': allocated}


def rss(pid):
    try:
        text = Path('/proc', str(pid), 'status').read_text()
    except (FileNotFoundError, ProcessLookupError):
        return None
    return {name: int(value) * 1024 for name, value in
            re.findall(r'^(VmRSS|VmHWM):\s+(\d+) kB', text, re.M)}


def capture(command, cwd, env, prefix, timeout, lifetime=False):
    """Persist raw streams; sample only the supervisor/coordinator's own RSS.

    Per-worker cgroup peaks come from the backend, not these process samples.
    Keep a supervisor's lifetime pipe open until it exits; communicate() would
    close it and cancel the very request being measured.
    """
    stdout, stderr, samples = (prefix.with_suffix(suffix) for suffix in ('.stdout', '.stderr', '.rss.jsonl'))
    started = time.monotonic()
    peak = 0
    with stdout.open('xb') as output, stderr.open('xb') as errors, samples.open('x') as memory:
        proc = subprocess.Popen(command, cwd=cwd, env=env, stdout=output, stderr=errors,
                                stdin=subprocess.PIPE if lifetime else subprocess.DEVNULL)
        try:
            while proc.poll() is None:
                elapsed = time.monotonic() - started
                observation = rss(proc.pid)
                if observation:
                    peak = max(peak, observation.get('VmHWM', 0), observation.get('VmRSS', 0))
                    memory.write(json.dumps({'seconds': elapsed, **observation}) + '\n')
                if elapsed > timeout:
                    raise TimeoutError('measurement deadline exceeded')
                if output.tell() > 8 * 1024**3:
                    raise ValueError('raw output exceeds 8 GiB safety limit')
                time.sleep(.05)
        finally:
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=10)
            if proc.stdin:
                proc.stdin.close()
    return {'command': command, 'returncode': proc.returncode, 'seconds': time.monotonic() - started,
            'supervisor_sampled_peak_bytes': peak,
            'artifacts': {str(p): file_digest(p) for p in (stdout, stderr, samples)}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('backend-dir', 'project', 'target-dir', 'output-dir'):
        parser.add_argument('--' + name, type=Path, required=True)
    for name in ('package', 'target-kind', 'target-name'):
        parser.add_argument('--' + name, required=True)
    parser.add_argument('--mode', choices=('SigOnly', 'Recurse'), required=True)
    parser.add_argument('--features')
    parser.add_argument('--no-default-features', action='store_true')
    parser.add_argument('--memory-mib', type=int, default=6144)
    parser.add_argument('--timeout-seconds', type=int, default=600)
    parser.add_argument('--warm-repeats', type=int, default=4)
    args = parser.parse_args()
    if min(args.memory_mib, args.timeout_seconds, args.warm_repeats) <= 0:
        parser.error('limits and warm repetitions must be positive')
    backend, project, target, output = (p.resolve() for p in
                                      (args.backend_dir, args.project, args.target_dir, args.output_dir))
    if output.is_relative_to(project) or target.is_relative_to(project):
        parser.error('measurement outputs and target must be outside the source tree')
    output.mkdir(parents=True, exist_ok=False)
    command = [str(backend / 'cargo-flowistry'), 'flowistry', '--context-mode', args.mode,
               '--package', args.package, '--target-kind', args.target_kind, '--target-name', args.target_name]
    if args.features:
        command += ['--features', args.features]
    if args.no_default_features:
        command += ['--no-default-features']
    env = dict(os.environ, PATH=str(backend) + os.pathsep + os.environ['PATH'],
               FLOWISTRY_CACHE_DIR=str(output / 'cache'), XDG_CACHE_HOME=str(output / 'xdg'),
               CARGO_TARGET_DIR=str(target), CARGO_BUILD_JOBS='2',
               RUST_LOG='flowistry::audit=info', FLOWISTRY_VERIFY_SUMMARIES='0')
    env.pop('FLOWISTRY_RESULT_PROTOCOL', None)
    report = {'schema': 1, 'status': 'running', 'arguments': {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
              'build': json.loads((backend / 'build.json').read_text()),
              'binary_sha256': {name: file_digest(backend / name) for name in ('cargo-flowistry', 'flowistry-driver')},
              'measurement_scope': 'Uncontrolled host; project observations, not quiet latency acceptance. No cold-build or instruction-count claim. Independent fresh body oracle uses the same frozen backend and the existing bounded worker supervisor.',
              'host': {'uname': list(os.uname()), 'cpu_count': os.cpu_count()},
              'environment_sha256': digest(env),
              'harness_sha256': file_digest(Path(__file__)),
              'project_runs': [], 'oracles': []}
    def save():
        atomic_json(output / 'report.json', report)
    def worker(name, operation, *arguments):
        request = {'unit': 'flowistry-body-measure-' + uuid.uuid4().hex + '.scope',
                   'args': command[1:] + [operation, *arguments], 'memory_mib': args.memory_mib,
                   'timeout_seconds': args.timeout_seconds, 'result_protocol': False}
        prefix = output / name
        observation = capture([command[0], '--flowistry-project-worker', json.dumps(request)], project,
                              dict(env, FLOWISTRY_CACHE='off'), prefix, args.timeout_seconds + 30, lifetime=True)
        if observation['returncode'] != 0:
            raise ValueError('oracle supervisor failed; inspect raw streams')
        result = json.loads(prefix.with_suffix('.stdout').read_text())
        observation['outcome'] = {k: v for k, v in result.items() if k not in ('stdout', 'stderr')}
        return observation, result
    save()
    try:
        if report['binary_sha256'] != report['build']['binaries']:
            raise ValueError('frozen binary hashes differ from the build manifest')
        # Cargo.lock is generated by dependency preparation if absent. Freeze the
        # source only after this independently bounded inventory has run.
        inventory_run, result = worker('inventory', 'project-bodies', '.')
        report['inventory_run'] = inventory_run
        if result['status'] != 'exited' or result['exit_code'] != 0:
            raise ValueError('standalone inventory failed')
        inventory = decode(result['stdout'].strip())['bodies']
        report['inventory'] = inventory
        source = tree_digest(project)
        report['source_before'] = source
        report['disk_before'] = {'cache': disk_usage(output / 'cache'), 'target': disk_usage(target)}
        for iteration in range(args.warm_repeats + 1):
            if tree_digest(project) != source:
                raise ValueError('source changed during project measurement')
            name = 'populate' if iteration == 0 else f'warm-{iteration}'
            prefix = output / name
            observed = capture(command + ['project', '--memory-mib', str(args.memory_mib),
                                          '--timeout-seconds', str(args.timeout_seconds)],
                               project, dict(env, FLOWISTRY_CACHE='on'), prefix,
                               (len(inventory) + 1) * (args.timeout_seconds + 30) + 60)
            audit = audit_stream(prefix.with_suffix('.stdout'), inventory, project)
            expected_exit = 0 if audit['final']['status'] == 'complete' else 1
            if observed['returncode'] != expected_exit:
                raise ValueError('process status contradicts terminal event')
            observed.update(name=name, **audit, cache_disk=disk_usage(output / 'cache'))
            report['project_runs'].append(observed)
            save()
        for ordinal, body in enumerate(inventory):
            if 'error' in body:
                report['oracles'].append({'identity': body['identity'], 'status': 'inventory_error'})
                save()
                continue
            observed, result = worker(f'oracle-{ordinal:05d}', 'body-focus', body['range']['filename'], body['identity'])
            observed['identity'] = body['identity']
            if result['status'] == 'exited' and result['exit_code'] == 0:
                try:
                    observed['semantic_digest'] = selected_digest(result['stdout'], body['identity'], body['range'], project)
                except (ValueError, KeyError, TypeError) as error:
                    observed['analysis_error'] = str(error)
            report['oracles'].append(observed)
            save()
        report['source_after'] = tree_digest(project)
        if report['source_after'] != source:
            raise ValueError('source changed during standalone validation')
        mismatches = []
        for oracle in report['oracles']:
            for run in report['project_runs']:
                body = run['bodies'][oracle['identity']]
                # A matching failure is still an unresolved body, never an equality pass.
                if not oracle.get('semantic_digest') or body.get('semantic_digest') != oracle['semantic_digest']:
                    mismatches.append({'identity': oracle['identity'], 'run': run['name'], 'status': body['status']})
        report['unresolved'] = mismatches
        report['correctness'] = 'passed' if not mismatches else 'needs-review'
        report['resource_observations_complete'] = all(
            not run['missing_memory_observations'] for run in report['project_runs'])
        report['resource_acceptance'] = 'not-established: no quiet-host policy or measured budget in this harness'
        report['disk_after'] = {'cache': disk_usage(output / 'cache'), 'target': disk_usage(target),
                                'artifacts': disk_usage(output)}
        report['status'] = 'passed' if not mismatches else 'needs-review'
    except Exception:
        report['status'] = 'needs-review'
        report['error'] = traceback.format_exc()
    save()
    print(json.dumps({'status': report['status'], 'report': str(output / 'report.json')}), flush=True)
    return 0 if report['status'] == 'passed' else 1


if __name__ == '__main__':
    raise SystemExit(main())
