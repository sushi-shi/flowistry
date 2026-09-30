#!/usr/bin/env python3
"""Run a pinned serial measurement plan after prerequisite jobs and quiet checks.

Never merge old timing samples, restart an existing job, or kill other work.
SIGTERM/SIGINT request a stop after the bounded active child has finished.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

from smoke_checkpoint import atomic_json, file_digest

STOP = False
HEAVY = {'cargo', 'rustc', 'flowistry-drive', 'flowistry-driver', 'cc1', 'cc1plus', 'nix', 'ld', 'ld.lld'}


def process(stat):
    end = stat.rindex(')')
    fields = stat[end + 2:].split()
    return {'pid': int(stat[:stat.index('(')].strip()), 'name': stat[stat.index('(') + 1:end],
            'state': fields[0], 'parent': int(fields[1]), 'ticks': int(fields[11]) + int(fields[12]),
            'start': int(fields[19])}


def processes():
    result = {}
    for path in Path('/proc').glob('[0-9]*/stat'):
        try:
            item = process(path.read_text())
            result[item['pid']] = item
        except (FileNotFoundError, ProcessLookupError):
            continue
    return result


def active(identity, observed):
    item = observed.get(identity['pid'])
    return bool(item and item['start'] == identity['start'] and item['state'] not in ('Z', 'X'))


def owned(observed, root):
    result = {root}
    while True:
        more = {pid for pid, item in observed.items() if item['parent'] in result}
        if more <= result:
            return result
        result |= more


def background(before, after, elapsed, root, ticks_per_second):
    allowed = owned(before, root) | owned(after, root)
    # The launcher's ancestors are not its children, but are not workload noise.
    parent = root
    while parent in after and after[parent]['parent'] != parent:
        parent = after[parent]['parent']
        allowed.add(parent)
    busy, heavy = [], []
    for pid, item in after.items():
        if pid in allowed or item['state'] in ('Z', 'X'):
            continue
        old = before.get(pid)
        if old and old['start'] == item['start']:
            cores = max(0, item['ticks'] - old['ticks']) / ticks_per_second / elapsed
        else:
            # Account for short newly-observed unrelated processes conservatively.
            cores = item['ticks'] / ticks_per_second / elapsed
        if cores:
            busy.append({'pid': pid, 'name': item['name'], 'cores': round(cores, 4)})
        if item['name'] in HEAVY:
            heavy.append({'pid': pid, 'name': item['name']})
    return {'background_cores': sum(p['cores'] for p in busy),
            'busy': sorted(busy, key=lambda p: p['cores'], reverse=True)[:10], 'heavy': heavy}


def cpu():
    fields = list(map(int, Path('/proc/stat').read_text().splitlines()[0].split()[1:9]))
    return sum(fields), fields[3] + fields[4]


def checked_inputs(plan):
    for filename, digest in plan['inputs'].items():
        if file_digest(filename) != digest:
            raise RuntimeError(f'pinned input changed: {filename}')


def verify_job(job, report):
    if not report.get('crates') or not report.get('validation_manifest'):
        raise ValueError('missing measurement records/provenance')
    rows = [row for crate in report['crates'] for row in crate['records']]
    if any(crate['skipped'] for crate in report['crates']):
        raise ValueError('measurement skipped a crate')
    expected = {tuple(position) for position in job['expected_positions']}
    actual = [(r['file'], r['line'], r['column'], r['mode']) for r in rows]
    if len(actual) != len(expected) or set(actual) != expected:
        raise ValueError('measurement coverage differs from the pinned plan')
    for row in rows:
        if not row.get('same') or row.get('checkpoint_reused'):
            raise ValueError('measurement differs or reused a correctness checkpoint')
        for name in ('base', 'compare'):
            result = row[name]
            samples = result.get('samples', [result])
            if result['status'] != 'ok' or result.get('warmup_status', 'ok') != 'ok' or len(samples) != job['repeat']:
                raise ValueError('unsuccessful or incomplete measurement')
            for sample in samples:
                if sample['status'] != 'ok' or sample.get('measurement_error') or sample.get('max_rss_mb', 0) <= 0 or sample.get('wire_bytes', 0) <= 0:
                    raise ValueError('failed sample')
                if job['kind'] == 'performance':
                    if not sample.get('cargo_replay_observed') or not sample.get('counters') or set(sample['counters']) != {'instructions', 'cycles'}:
                        raise ValueError('missing direct replay or instruction/cycle counters')
                    if any(value <= 0 for value in sample['counters'].values()):
                        raise ValueError('nonpositive hardware counter')
                elif not sample.get('phases') or not sample.get('storage'):
                    raise ValueError('missing diagnostic phases or per-analysis storage')
    return len(rows)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--plan', type=Path, required=True)
    parser.add_argument('--state', type=Path, required=True)
    args = parser.parse_args()
    if args.state.exists():
        parser.error('state already exists: inspect the previous run; never overwrite evidence')
    plan = json.loads(args.plan.read_text())
    checked_inputs(plan)
    for job in plan['jobs']:
        if Path(job['report']).exists() or Path(job['log']).exists():
            parser.error('a planned output already exists: create a new reviewed run plan')
    observed = processes()
    state = {'schema': 1, 'plan_sha256': file_digest(args.plan), 'runner_sha256': file_digest(Path(__file__)),
             'pid': os.getpid(), 'start': observed[os.getpid()]['start'], 'status': 'waiting-prerequisites',
             'jobs': [], 'quiet_policy': plan['quiet'], 'host': {'cpu_count': os.cpu_count(), 'uname': list(os.uname())},
             'monitor_caveat': 'Sampled process/CPU monitoring cannot exclude sub-interval noise; quiet means the recorded policy passed.'}
    def save():
        state['updated_at'] = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
        atomic_json(args.state, state)
    def request_stop(*_):
        global STOP
        STOP = True
    signal.signal(signal.SIGTERM, request_stop)
    signal.signal(signal.SIGINT, request_stop)
    save()
    try:
        while not STOP:
            observed = processes()
            state['live_prerequisites'] = [identity for identity in plan['prerequisites'] if active(identity, observed)]
            save()
            if not state['live_prerequisites']:
                break
            time.sleep(plan['poll_seconds'])
        if STOP:
            state['status'] = 'stopped'; save(); return
        # A dead/missing process is terminal, but is not proof its task succeeded.
        checked_inputs(plan)
        for identity in plan['prerequisites']:
            subprocess.run([sys.executable, 'scripts/summarize-validation.py', identity['report'],
                            '--output', identity['summary']], cwd=plan['cwd'], check=True)
        ticks = os.sysconf('SC_CLK_TCK')
        for job in plan['jobs']:
            if STOP:
                break
            state['status'], state['next_job'] = 'waiting-quiet', job['name']
            quiet = []
            while not STOP:
                before, before_cpu, start = processes(), cpu(), time.monotonic()
                time.sleep(plan['quiet']['interval_seconds'])
                after, after_cpu = processes(), cpu()
                sample = background(before, after, time.monotonic() - start, os.getpid(), ticks)
                total = after_cpu[0] - before_cpu[0]
                sample.update(load1=os.getloadavg()[0], busy_fraction=1 - (after_cpu[1] - before_cpu[1]) / max(1, total))
                good = (not sample['heavy'] and sample['background_cores'] <= plan['quiet']['max_background_cores']
                        and sample['busy_fraction'] <= plan['quiet']['max_busy_fraction']
                        and sample['load1'] <= plan['quiet']['max_load1'])
                quiet = (quiet + [sample])[-plan['quiet']['consecutive']:] if good else []
                state['last_quiet_sample'], state['quiet_streak'] = sample, len(quiet); save()
                if len(quiet) == plan['quiet']['consecutive']:
                    break
            if STOP:
                break
            checked_inputs(plan)
            if Path(job['report']).exists() or Path(job['log']).exists():
                raise RuntimeError('output appeared while waiting; refusing to replace it')
            record = {'name': job['name'], 'kind': job['kind'], 'quiet_preflight': quiet,
                      'command': job['command'], 'status': 'running', 'contamination': [],
                      'report': job['report'], 'log': job['log'], 'load_start': os.getloadavg()}
            state['jobs'].append(record);state['status'] = 'running';save()
            started = time.monotonic()
            with Path(job['log']).open('x') as log:
                child = subprocess.Popen(job['command'], cwd=plan['cwd'], env=dict(os.environ, **plan['environment']),
                                         stdout=log, stderr=subprocess.STDOUT)
                before, last = processes(), time.monotonic()
                while child.poll() is None:
                    time.sleep(plan['quiet']['monitor_seconds'])
                    after, now = processes(), time.monotonic()
                    sample = background(before, after, now - last, os.getpid(), ticks)
                    if sample['heavy'] or sample['background_cores'] > plan['quiet']['max_background_cores']:
                        record['contamination'].append(sample)
                    before, last = after, now
                    save()
            record.update(returncode=child.returncode, seconds=time.monotonic() - started, load_end=os.getloadavg(),
                          log_sha256=file_digest(job['log']))
            if Path(job['report']).exists():
                record['report_sha256'] = file_digest(job['report'])
            if child.returncode != 0:
                raise RuntimeError(f"job failed: {job['name']}; inspect retained report/log")
            record['positions'] = verify_job(job, json.loads(Path(job['report']).read_text()))
            record['quiet_accepted'] = not record['contamination']
            record['status'] = 'complete' if record['quiet_accepted'] else 'contaminated'
            save()
            if not record['quiet_accepted']:
                raise RuntimeError(f"background load contaminated {job['name']}; do not treat its latency as quiet")
        state['status'] = 'stopped' if STOP else 'complete'; save()
    except Exception as error:
        if state['jobs'] and state['jobs'][-1]['status'] == 'running':
            state['jobs'][-1]['status'] = 'failed'
        state['status'], state['error'] = 'needs-review', str(error); save()
        raise


if __name__ == '__main__':
    main()
