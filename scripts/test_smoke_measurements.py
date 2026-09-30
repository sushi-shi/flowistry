import importlib.util
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('smoke', Path(__file__).with_name('smoke-real-crates.py'))
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


class MeasurementTests(unittest.TestCase):
    def args(self):
        return SimpleNamespace(repeat=3, timeout=20, phases=False, memory_limit='6G', keep_outputs=False)

    def test_memory_scope_preserves_the_callers_invocation_environment(self):
        self.assertEqual(smoke.memory_scope('6G', {'INVOCATION_ID': 'caller'})[-2:],
                         ['env', 'INVOCATION_ID=caller'])
        self.assertEqual(smoke.memory_scope('6G', {})[-3:], ['env', '-u', 'INVOCATION_ID'])
        self.assertEqual(smoke.memory_scope(None, {}), [])

    def test_comparisons_alternate_backend_order(self):
        order = []
        def run(dest, env, *args):
            order.append(env['backend'])
            return {'status': 'ok', 'seconds': .1, 'output': {'digest': env['backend'], 'places': 1}}
        with patch.object(smoke, 'focus_repeated', side_effect=run):
            results = smoke.focus_interleaved([('base', {'backend': 'A'}), ('compare', {'backend': 'B'})],
                                              '.', 'lib.rs', 1, 2, 'SigOnly', self.args(), None)
        self.assertEqual(order, ['A', 'B', 'B', 'A', 'A', 'B'])
        self.assertEqual(len(results['base']['samples']), 3)
        self.assertEqual(len(results['compare']['samples']), 3)

    def test_counter_parser_ignores_diagnostics(self):
        self.assertEqual(smoke.parse_counters('error;unrelated;diagnostic\n12345;;instructions:u;20;100.00;;\n67890;;cycles:u;20;100.00;;'),
                         {'instructions': 12345, 'cycles': 67890})

    def test_missing_or_unavailable_counters_fail_explicitly(self):
        for stderr in ['', '<not counted>;;instructions:u;0;0;;\n12;;cycles:u;1;100;;', '123;;instructions:u;1;100;;']:
            with self.assertRaises(ValueError):
                smoke.parse_counters(stderr)

    def test_keeps_all_samples_including_slowest_and_largest(self):
        runs = [{'status': 'ok', 'seconds': seconds, 'max_rss_mb': rss,
                 'output': {'digest': 'equal', 'places': 1}, 'counters': {'instructions': count}}
                for seconds, rss, count in [(0.1, 30, 1000), (0.4, 70, 1200), (0.2, 50, 1100)]]
        with patch.object(smoke, 'flowistry_focus', side_effect=runs):
            result = smoke.focus_repeated('.', {}, 'lib.rs', 1, 2, 'SigOnly', self.args())
        self.assertEqual(result['seconds'], 0.1)
        self.assertEqual(result['all_seconds'], [0.1, 0.4, 0.2])
        self.assertEqual([s['max_rss_mb'] for s in result['samples']], [30, 70, 50])
        self.assertEqual(result['samples'][1]['counters']['instructions'], 1200)
        self.assertNotIn('output', result['samples'][0])

    def test_flaky_semantics_cannot_be_reported_as_a_speedup(self):
        runs = [{'status': 'ok', 'seconds': 0.1, 'output': {'digest': digest, 'places': 1}}
                for digest in ['first', 'second', 'first']]
        with patch.object(smoke, 'flowistry_focus', side_effect=runs):
            result = smoke.focus_repeated('.', {}, 'lib.rs', 1, 2, 'SigOnly', self.args())
        self.assertEqual(result['status'], 'error')
        self.assertTrue(result['measurement_error'])

    def test_failed_sample_does_not_disappear_behind_fast_success(self):
        runs = [{'status': 'ok', 'seconds': 0.1, 'output': {'digest': 'same', 'places': 1}},
                {'status': 'timeout', 'seconds': 20, 'message': 'timeout'},
                {'status': 'ok', 'seconds': 0.2, 'output': {'digest': 'same', 'places': 1}}]
        with patch.object(smoke, 'flowistry_focus', side_effect=runs):
            result = smoke.focus_repeated('.', {}, 'lib.rs', 1, 2, 'SigOnly', self.args())
        self.assertEqual(result['status'], 'timeout')
        self.assertEqual(len(result['samples']), 3)


if __name__ == '__main__':
    unittest.main()
