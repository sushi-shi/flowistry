import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('summary', Path(__file__).with_name('summarize-benchmarks.py'))
summary = importlib.util.module_from_spec(spec)
spec.loader.exec_module(summary)


class SummaryTests(unittest.TestCase):
    def report(self):
        return {'backends': {'base': '/bin'}, 'measurement': {'perf': '/perf'}, 'crates': [{
            'crate': 'test', 'skipped': [], 'records': [{
                'file': 'lib.rs', 'line': 1, 'column': 2, 'mode': 'SigOnly',
                'base': {'status': 'ok', 'seconds': .1, 'samples': [
                    {'status': 'ok', 'seconds': value, 'max_rss_mb': rss,
                     'counters': {'instructions': count, 'cycles': count * 2}}
                    for value, rss, count in [(.1, 10, 100), (.5, 30, 200), (.3, 20, 150)]]}}]}]}

    def test_distribution_keeps_tail_and_peak(self):
        result = summary.summarize(self.report())
        self.assertTrue(result['measurement_valid'])
        metrics = result['positions'][0]['backends']['base']
        self.assertEqual(metrics['seconds']['median'], .3)
        self.assertEqual(metrics['seconds']['p95'], .5)
        self.assertEqual(metrics['peak_rss_mb'], 30)
        self.assertEqual(metrics['instructions']['median'], 150)

    def test_resumed_record_is_rejected(self):
        report = self.report()
        report['crates'][0]['records'][0]['checkpoint_reused'] = True
        with self.assertRaises(ValueError):
            summary.summarize(report)

    def test_failed_sample_invalidates_measurement(self):
        report = self.report()
        report['crates'][0]['records'][0]['base']['samples'][1]['status'] = 'timeout'
        self.assertFalse(summary.summarize(report)['measurement_valid'])

    def test_absent_counters_are_not_a_zero_cost_sample(self):
        report = self.report()
        report['crates'][0]['records'][0]['base']['samples'][1]['counters'] = None
        self.assertFalse(summary.summarize(report)['measurement_valid'])

    def test_partial_counters_are_reported_as_incomplete(self):
        report = self.report()
        del report['crates'][0]['records'][0]['base']['samples'][1]['counters']['cycles']
        self.assertFalse(summary.summarize(report)['measurement_valid'])

    def test_missing_repetitions_are_not_accepted(self):
        report = self.report()
        report['measurement']['repeat'] = 4
        self.assertFalse(summary.summarize(report)['measurement_valid'])

    def test_missing_phases_are_not_imputed_as_zero(self):
        report = self.report()
        samples = report['crates'][0]['records'][0]['base']['samples']
        samples[0]['phases'] = {'solver': .05}
        samples[2]['phases'] = {'solver': .07}
        metrics = summary.summarize(report)['positions'][0]['backends']['base']
        self.assertEqual(metrics['phases']['solver']['n'], 2)
        self.assertEqual(metrics['phases']['solver']['min'], .05)


if __name__ == '__main__':
    unittest.main()
