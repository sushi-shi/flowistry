#!/usr/bin/env python3
"""Exercise the public stream and real systemd-bounded compiler workers (Linux)."""
import argparse
import array
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import queue
import signal
import subprocess
import sys
import threading
import termios
import time
import traceback

from incremental_matrix import Matrix, decode, smoke
from smoke_checkpoint import atomic_json

spec = importlib.util.spec_from_file_location('workers', Path(__file__).with_name('test-project-workers.py'))
workers = importlib.util.module_from_spec(spec)
spec.loader.exec_module(workers)


def resolved(item, files):
    if isinstance(item, dict):
        return {key: files[str(value)] if key == 'filename' else resolved(value, files)
                for key, value in item.items()}
    if isinstance(item, list):
        return [resolved(value, files) for value in item]
    return item


class Stream:
    def __init__(self, command, matrix, case, extra=None):
        self.process = subprocess.Popen(command, cwd=matrix.project,
                                       env=matrix.env(case, 'on', extra),
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.events = []
        self.lines = queue.Queue()
        self.errors = bytearray()

        def stdout():
            for line in self.process.stdout:
                self.lines.put(line)
            self.lines.put(None)

        def stderr():
            self.errors.extend(self.process.stderr.read())

        self.readers = [threading.Thread(target=stdout), threading.Thread(target=stderr)]
        for reader in self.readers:
            reader.start()

    def next(self, timeout=120):
        line = self.lines.get(timeout=timeout)
        if line is None:
            return None
        event = json.loads(line)
        assert event['schema'] == 1 and event['sequence'] == len(self.events), event
        if self.events:
            assert event['run'] == self.events[0]['run'], event
        self.events.append(event)
        return event

    def finish(self):
        while self.next() is not None:
            pass
        self.process.wait(timeout=15)
        for reader in self.readers:
            reader.join(timeout=15)
            assert not reader.is_alive(), 'output reader did not finish'
        return self.events

    def stop(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        for reader in self.readers:
            reader.join(timeout=15)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--backend-dir', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--json', type=Path, required=True)
    parser.add_argument('--case', action='append', help='Run only these named cases')
    args = parser.parse_args()
    matrix = Matrix(args.backend_dir, args.work_dir, files=workers.FILES)
    report = {'schema': 1, 'records': []}
    active = []

    def command(kind='lib', name='custom_library', mode='SigOnly', features=None):
        result = [str(matrix.backend / 'cargo-flowistry'), 'flowistry', '--context-mode', mode,
                  '--package', 'matrix_fixture', '--target-kind', kind, '--target-name', name]
        if features:
            result += ['--features', features]
        return result

    def start(case, options=(), extra=None, memory=1024, timeout=60, **selection):
        stream = Stream(command(**selection) + ['project', '--memory-mib', str(memory),
                                              '--timeout-seconds', str(timeout), *map(str, options)],
                        matrix, case, extra)
        active.append(stream)
        return stream

    def check(case, function):
        if args.case and case not in args.case:
            return
        try:
            detail = function()
            record = {'case': case, 'passed': True, 'detail': detail}
        except Exception:
            record = {'case': case, 'passed': False, 'error': traceback.format_exc()}
        finally:
            for stream in active:
                stream.stop()
            active.clear()
        report['records'].append(record)
        atomic_json(args.json, report)
        print(case + ': ' + ('PASS' if record['passed'] else 'FAIL ' + record['error']), flush=True)

    for mode in ('SigOnly', 'Recurse'):
        for kind, name in [('lib', 'custom_library'), ('bin', 'custom-binary')]:
            case = mode + '-' + kind

            def equality():
                stream = start(case, kind=kind, name=name, mode=mode)
                while True:
                    event = stream.next()
                    assert event is not None, stream.errors.decode()
                    if event['event'] == 'body':
                        assert stream.process.poll() is None, 'first body arrived only after completion'
                        break
                events = stream.finish()
                assert stream.process.returncode == 0, (events, stream.errors.decode())
                final = events[-1]
                assert final['status'] == 'complete' and final['pending'] == 0, final
                assert final['coverage'] == 'initial-inventory' and not final['project_current'], final
                bodies = [e for e in events if e['event'] == 'body']
                for event in bodies:
                    assert event['status'] == 'current' and event['current'], event
                    actual = decode(event['output'].encode())['Ok']
                    body = event['body']
                    oracle = subprocess.run(command(kind, name, mode) + [
                        'body-focus', body['range']['filename'], body['identity']],
                        cwd=matrix.project, env=matrix.env(case, 'off'), capture_output=True, timeout=60)
                    assert oracle.returncode == 0, oracle.stderr.decode()
                    expected = decode(oracle.stdout)['Ok']
                    def selected(value):
                        item = next(b['focus']['Ok'] for b in value['bodies'] if b['focus'])
                        return smoke.canonical(resolved(item, value['files']))
                    assert selected(actual) == selected(expected), body
                warm = start(case, kind=kind, name=name, mode=mode)
                warm_events = warm.finish()
                assert warm.process.returncode == 0, (warm_events, warm.errors.decode())
                warm_bodies = [e for e in warm_events if e['event'] == 'body']
                assert len(warm_bodies) == len(bodies)
                assert all('audit compiler' not in e['diagnostics'] for e in warm_bodies), warm_bodies
                return {'equal_bodies': len(bodies), 'warm_body_compilers': 0,
                        'first_result_seconds': final['first_result_seconds']}
            check(case, equality)

    def priorities():
        stream = start('priorities', options=['--cursor-file', matrix.source, '--cursor-line', 3,
                                            '--cursor-column', 30, '--priority-file', matrix.root / 'outside.rs'])
        events = stream.finish()
        bodies = [e['body'] for e in events if e['event'] == 'body']
        assert stream.process.returncode == 0, events
        assert '{closure#' in bodies[0]['name'], bodies
        assert bodies[1]['name'] == 'outside::external', bodies
        preferred = bodies[-1]
        stream = start('priorities', options=['--priority-body', preferred['identity']])
        events = stream.finish()
        assert next(e['body'] for e in events if e['event'] == 'body')['identity'] == preferred['identity']
        return {'cursor_first': bodies[0]['name'], 'priority_file_second': bodies[1]['name']}
    check('priorities', priorities)

    def partial():
        stream = start('type-error', features='broken')
        events = stream.finish()
        final = events[-1]
        assert stream.process.returncode == 1 and final['status'] == 'partial', events
        assert final['failed'] == 1 and final['succeeded'] >= 4 and final['pending'] == 0, final
        failed = next(e for e in events if e['event'] == 'body' and e['body']['name'] == 'broken')
        assert failed['status'] in ('worker_error', 'analysis_error'), failed
        return final
    check('type-error-isolation', partial)

    for method in ('sigterm', 'cancel-file'):
        def cancel():
            path = matrix.root / ('cancel-' + method)
            stream = start('cancel-' + method, options=['--cancel-file', path])
            while True:
                event = stream.next()
                assert event is not None, stream.errors.decode()
                if event['event'] == 'body-started':
                    if method == 'sigterm':
                        stream.process.send_signal(signal.SIGTERM)
                    else:
                        path.touch()
                    break
            events = stream.finish()
            assert stream.process.returncode == 130 and events[-1]['status'] == 'canceled', events
            return events[-1]
        check(method, cancel)

    def install_gate(case, action):
        gate = matrix.root / ('gate-' + case)
        gate.mkdir()
        wrapper = gate / 'wrapper.py'
        wrapper.write_text('#!' + sys.executable + '\n' + '''
import json, os, pathlib, subprocess, sys, time
gate = pathlib.Path(__file__).parent
args = sys.argv[1:]
fixture = any(args[i:i+2] == ['--crate-name', 'custom_library'] for i in range(len(args)))
if fixture and 'BodyFocus' in os.environ.get('PLUGIN_ARGS', ''):
    count_file = gate / 'count'
    count = int(count_file.read_text()) + 1 if count_file.exists() else 1
    count_file.write_text(str(count))
    if count == 2:
        child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(120)'], start_new_session=True)
        def identity(pid):
            fields = pathlib.Path('/proc', str(pid), 'stat').read_text().rsplit(')', 1)[1].split()
            return {'pid':pid, 'start':fields[19]}
        ready = gate / 'ready.tmp'
        ready.write_text(json.dumps({'processes':[identity(os.getpid()), identity(child.pid)],
                                    'cgroup':pathlib.Path('/proc/self/cgroup').read_text()}))
        ready.rename(gate / 'ready')
        action = (gate / 'action').read_text()
        if action == 'oom':
            hog = subprocess.Popen([sys.executable, '-c', 'import time; data=bytearray(768*1024*1024); time.sleep(120)'])
            hog.wait()
            time.sleep(.2)
            sys.exit(1)
        deadline = time.monotonic() + 120
        while not (gate / 'release').exists():
            if time.monotonic() > deadline:
                sys.exit('gate timed out')
            time.sleep(.01)
        child.terminate()
        child.wait()
os.execvp(args[0], args)
''')
        wrapper.chmod(0o755)
        (gate / 'action').write_text(action)
        return gate, {'RUSTC_WRAPPER': str(wrapper), 'FLOWISTRY_NO_REPLAY': '1'}

    def wait_ready(gate, stream):
        deadline = time.monotonic() + 40
        while not (gate / 'ready').exists():
            assert time.monotonic() < deadline and stream.process.poll() is None, stream.errors.decode()
            time.sleep(.02)
        return json.loads((gate / 'ready').read_text())

    def assert_stopped(record):
        deadline = time.monotonic() + 12
        while True:
            alive = []
            for process in record['processes']:
                try:
                    fields = Path('/proc', str(process['pid']), 'stat').read_text().rsplit(')', 1)[1].split()
                except FileNotFoundError:
                    continue
                if fields[19] == process['start'] and fields[0] not in ('Z', 'X'):
                    alive.append(process)
            if not alive:
                return
            assert time.monotonic() < deadline, ('worker descendants survived', alive, record)
            time.sleep(.05)

    for action in ('timeout', 'oom', 'sigterm-tree', 'sigkill-tree', 'edit-during-run', 'closed-output'):
        def bounded():
            gate, extra = install_gate(action, action)
            if action == 'closed-output':
                process = subprocess.Popen(command() + ['project', '--memory-mib', '1024'],
                                           cwd=matrix.project, env=matrix.env(action, 'on', extra),
                                           stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                try:
                    deadline = time.monotonic() + 30
                    while not (gate / 'ready').exists():
                        assert time.monotonic() < deadline and process.poll() is None
                        time.sleep(.02)
                    record = json.loads((gate / 'ready').read_text())
                    process.stdout.close()
                    process.wait(timeout=15)
                    assert_stopped(record)
                    assert process.returncode != 0
                    return {'descendants_stopped': True, 'exit_code': process.returncode}
                finally:
                    if process.poll() is None:
                        process.kill()
                        process.wait(timeout=10)
                    process.stderr.close()
            stream = start(action, extra=extra, timeout=5 if action == 'timeout' else 60,
                           memory=384 if action == 'oom' else 1024)
            record = wait_ready(gate, stream)
            assert 'flowistry-body-' in record['cgroup'], record
            if action == 'sigterm-tree':
                stream.process.terminate()
            elif action == 'sigkill-tree':
                stream.process.kill()
            elif action == 'edit-during-run':
                matrix.source.write_text('\n\n' + matrix.source.read_text())
                (gate / 'release').touch()
            events = stream.finish()
            assert_stopped(record)
            if action == 'sigkill-tree':
                assert stream.process.returncode == -signal.SIGKILL
            elif action == 'sigterm-tree':
                assert events[-1]['status'] == 'canceled' and stream.process.returncode == 130, events
            elif action == 'edit-during-run':
                selected = next(e for e in events if e['event'] == 'body' and e['body']['name'] == 'selected')
                assert selected['status'] == 'superseded' or selected['body']['range']['start']['line'] == 4, selected
                assert events[-1]['coverage'] == 'initial-inventory' and not events[-1]['project_current'], events[-1]
                matrix.source.write_text(workers.FILES['project/src/lib.rs'])
            else:
                bodies = [e for e in events if e['event'] == 'body']
                assert bodies[1]['status'] == action, bodies
                assert any(e['status'] == 'current' for e in bodies[2:]), bodies
                assert events[-1]['status'] == 'partial' and events[-1]['pending'] == 0, events[-1]
                if action == 'oom':
                    assert bodies[1]['oom_kills'] > 0, bodies[1]
            return {'descendants_stopped': True, 'events': [{k:v for k,v in e.items() if k not in ('output', 'diagnostics')} for e in events]}
        check(action, bounded)

    def backpressure():
        original = matrix.source.read_text()
        matrix.source.write_text(original + '\n'.join(
            'pub fn many_' + str(i) + '(x: i32) -> i32 { x + 1 }' for i in range(80)))
        process = subprocess.Popen(command() + ['project', '--memory-mib', '1024'],
                                   cwd=matrix.project, env=matrix.env('backpressure', 'on'),
                                   stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        try:
            fcntl.fcntl(process.stdout, fcntl.F_SETPIPE_SZ, 4096)
            capacity = fcntl.fcntl(process.stdout, fcntl.F_GETPIPE_SZ)
            deadline = time.monotonic() + 45
            while True:
                pending = array.array('i', [0])
                fcntl.ioctl(process.stdout, termios.FIONREAD, pending, True)
                if pending[0] >= capacity // 2:
                    break
                assert time.monotonic() < deadline and process.poll() is None
                time.sleep(.05)
            # Leave the pipe unread and full while canceling the coordinator.
            time.sleep(1)
            process.terminate()
            process.wait(timeout=15)
            assert process.returncode == 130
            return {'pipe_capacity': capacity, 'unread_bytes': pending[0], 'exit_code': process.returncode}
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=10)
            process.stdout.close()
            matrix.source.write_text(original)
    check('backpressure-cancel', backpressure)

    def unavailable():
        stream = start('no-user-systemd', extra={
            'DBUS_SESSION_BUS_ADDRESS': 'unix:path=' + str(matrix.root / 'absent-bus'),
            'XDG_RUNTIME_DIR': str(matrix.root / 'absent-runtime')})
        events = stream.finish()
        assert stream.process.returncode == 1 and events[-1]['status'] == 'failed', events
        assert any(e['event'] == 'diagnostic' and e.get('phase') == 'inventory' for e in events), events
        assert not any(e['event'] == 'body' for e in events), events
        return {'bounded_failure': True, 'events': events}
    check('no-user-systemd', unavailable)

    raise SystemExit(0 if all(r['passed'] for r in report['records']) else 1)


if __name__ == '__main__':
    main()
