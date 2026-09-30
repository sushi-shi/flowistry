import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('acceptance', Path(__file__).with_name('run-acceptance-queue.py'))
queue = importlib.util.module_from_spec(spec)
spec.loader.exec_module(queue)


class AcceptanceQueueTests(unittest.TestCase):
    def test_pid_reuse_and_zombies_are_not_live_prerequisites(self):
        identity = {'pid': 10, 'start': 400}
        self.assertTrue(queue.active(identity, {10: dict(identity, state='S')}))
        self.assertTrue(queue.active(identity, {10: dict(identity, state='T')}))
        self.assertFalse(queue.active(identity, {10: dict(identity, start=401, state='S')}))
        self.assertFalse(queue.active(identity, {10: dict(identity, state='Z')}))
        self.assertFalse(queue.active(identity, {}))

    def test_proc_stat_parses_parentheses_in_names(self):
        fields = ['S', '7'] + ['0'] * 18
        fields[11], fields[12], fields[19] = '12', '3', '991'
        item = queue.process('8 (worker (test)) ' + ' '.join(fields))
        self.assertEqual((item['name'], item['parent'], item['ticks'], item['start']),
                         ('worker (test)', 7, 15, 991))

    def test_background_excludes_own_children_but_detects_unrelated_builds(self):
        before = {1: {'parent': 0, 'name': 'init', 'start': 1, 'ticks': 0, 'state': 'S'},
                  10: {'parent': 1, 'name': 'python3', 'start': 2, 'ticks': 10, 'state': 'R'},
                  11: {'parent': 10, 'name': 'rustc', 'start': 3, 'ticks': 20, 'state': 'R'},
                  12: {'parent': 1, 'name': 'rustc', 'start': 4, 'ticks': 30, 'state': 'R'}}
        after = copy.deepcopy(before)
        after[11]['ticks'] += 500
        after[12]['ticks'] += 100
        result = queue.background(before, after, 2, 10, 100)
        self.assertEqual(result['heavy'], [{'pid': 12, 'name': 'rustc'}])
        self.assertEqual(result['background_cores'], .5)

    def fixture(self):
        job = {'kind': 'performance', 'repeat': 2, 'expected_positions': [['src/lib.rs', 3, 4, 'SigOnly']]}
        sample = {'status': 'ok', 'counters': {'instructions': 100, 'cycles': 70},
                  'cargo_replay_observed': True, 'max_rss_mb': 20, 'wire_bytes': 50}
        result = dict(sample, samples=[dict(sample), dict(sample)])
        report = {'validation_manifest': {'schema': 1}, 'crates': [{'skipped': [], 'records': [
            {'file': 'src/lib.rs', 'line': 3, 'column': 4, 'mode': 'SigOnly', 'same': True,
             'base': copy.deepcopy(result), 'compare': copy.deepcopy(result)}]}]}
        return job, report

    def test_counters_replay_and_exact_coverage_are_mandatory(self):
        job, report = self.fixture()
        self.assertEqual(queue.verify_job(job, report), 1)
        row = report['crates'][0]['records'][0]
        for edit in (lambda r: r.update(same=False), lambda r: r.update(checkpoint_reused=True),
                     lambda r: r['base']['samples'][0].update(counters=None),
                     lambda r: r['base']['samples'][0].update(cargo_replay_observed=False),
                     lambda r: r['base']['samples'].pop(), lambda r: r.update(line=4)):
            damaged = copy.deepcopy(report)
            edit(damaged['crates'][0]['records'][0])
            with self.assertRaises(ValueError):
                queue.verify_job(job, damaged)
        report['crates'][0]['records'].append(copy.deepcopy(row))
        with self.assertRaises(ValueError):
            queue.verify_job(job, report)

    def test_diagnostics_require_storage_instead_of_treating_missing_values_as_zero(self):
        job, report = self.fixture()
        job['kind'] = 'diagnostics'
        with self.assertRaises(ValueError):
            queue.verify_job(job, report)
        for result in ('base', 'compare'):
            for sample in report['crates'][0]['records'][0][result]['samples']:
                sample.update(phases={'seed rows': .1}, storage=[{'stored_rows': 10}])
        self.assertEqual(queue.verify_job(job, report), 1)


if __name__ == '__main__':
    unittest.main()
