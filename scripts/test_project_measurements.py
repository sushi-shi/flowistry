import base64
import copy
import gzip
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('project_measurements', Path(__file__).with_name('measure-project.py'))
measure = importlib.util.module_from_spec(spec)
spec.loader.exec_module(measure)


class ProjectMeasurementTests(unittest.TestCase):
    def setUp(self):
        self.range = {'filename': '/fixture/src/lib.rs', 'start': {'line': 0, 'column': 0},
                      'end': {'line': 2, 'column': 1}}
        self.body = {'identity': 'body-one', 'name': 'selected', 'range': self.range}
        self.response = {'files': {'0': '/fixture/src/lib.rs'}, 'cache': {'hits': 0, 'misses': 1},
                         'bodies': [{'range': dict(self.range, filename=0), 'cached': False,
                                     'focus': {'Ok': {'place_info': [], 'ranges': []}}}]}
        self.events = [
            {'event': 'started'}, {'event': 'inventory', 'total': 1},
            {'event': 'body-started', 'body': self.body, 'ordinal': 0, 'total': 1},
            {'event': 'body', 'body': self.body, 'status': 'current', 'current': True,
             'output': self.encoded(), 'observed_peak_memory_bytes': 1024,
             'diagnostics': 'audit compiler fixture\naudit solve focus selected\n'},
            {'event': 'finished', 'status': 'complete', 'total': 1, 'completed': 1,
             'succeeded': 1, 'failed': 0, 'pending': 0, 'coverage': 'initial-inventory',
             'project_current': False}]

    def encoded(self):
        return base64.b64encode(gzip.compress(json.dumps({'Ok': self.response}).encode())).decode()

    def audit(self, events=None):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root, 'stream.jsonl')
            path.write_text(''.join(json.dumps(dict(event, schema=1, run='one', sequence=i)) + '\n'
                                    for i, event in enumerate(self.events if events is None else events)))
            return measure.audit_stream(path, [self.body])

    def test_full_response_equality_ignores_only_cache_bookkeeping(self):
        before = measure.selected_digest(self.encoded(), 'body-one', self.range)
        self.response['cache'] = {'hits': 1, 'misses': 0}
        self.response['bodies'][0]['cached'] = True
        self.assertEqual(before, measure.selected_digest(self.encoded(), 'body-one', self.range))
        self.response['bodies'][0]['focus']['Ok']['comment_ranges'] = [{'start': 1, 'end': 2}]
        self.assertNotEqual(before, measure.selected_digest(self.encoded(), 'body-one', self.range))
        with self.assertRaisesRegex(ValueError, 'range differs'):
            measure.selected_digest(self.encoded(), 'body-one', dict(self.range, filename='/wrong.rs'))

    def test_retains_body_metrics_and_checks_exact_coverage(self):
        body = self.audit()['bodies']['body-one']
        self.assertEqual(body['compiler_invocations'], 1)
        self.assertEqual(body['solved_bodies'], [('focus', 'selected')])
        self.assertEqual(body['observed_peak_memory_bytes'], 1024)
        for events in (self.events[:-1], self.events[:3] + self.events[4:],
                       self.events[:4] + [self.events[3]] + self.events[4:],
                       self.events + [self.events[3]]):
            with self.assertRaises(ValueError):
                self.audit(events)

    def test_relative_wire_paths_resolve_against_the_explicit_project(self):
        self.response['files']['0'] = 'src/lib.rs'
        measure.selected_digest(self.encoded(), 'body-one', self.range, Path('/fixture'))
        self.response['files']['0'] = '../other/src/lib.rs'
        with self.assertRaisesRegex(ValueError, 'range differs'):
            measure.selected_digest(self.encoded(), 'body-one', self.range, Path('/fixture'))

    def test_unknown_body_and_terminal_lies_cannot_pass(self):
        events = copy.deepcopy(self.events)
        events[3]['body']['identity'] = 'other'
        with self.assertRaises(ValueError):
            self.audit(events)
        for key, value in [('failed', 1), ('pending', 1), ('project_current', True), ('status', 'partial')]:
            events = copy.deepcopy(self.events)
            events[-1][key] = value
            with self.assertRaisesRegex(ValueError, 'terminal counts'):
                self.audit(events)

    def test_failed_body_is_retained_without_semantic_equality(self):
        events = copy.deepcopy(self.events)
        events[3] = {'event': 'body', 'body': self.body, 'status': 'oom', 'oom_kills': 1}
        events[-1].update(status='partial', succeeded=0, failed=1)
        body = self.audit(events)['bodies']['body-one']
        self.assertEqual(body['status'], 'oom')
        self.assertNotIn('semantic_digest', body)

    def test_missing_memory_stays_unknown_and_selection_must_be_unique(self):
        events = copy.deepcopy(self.events)
        events[3]['observed_peak_memory_bytes'] = None
        result = self.audit(events)
        self.assertEqual(result['missing_memory_observations'], ['body-one'])
        self.assertIsNone(result['bodies']['body-one']['observed_peak_memory_bytes'])
        self.response['bodies'].append(copy.deepcopy(self.response['bodies'][0]))
        with self.assertRaisesRegex(ValueError, 'exactly one'):
            measure.selected_digest(self.encoded(), 'body-one')


if __name__ == '__main__':
    unittest.main()
