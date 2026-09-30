import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('summary', Path(__file__).with_name('summarize-validation.py'))
summary = importlib.util.module_from_spec(spec)
spec.loader.exec_module(summary)


class CoverageTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / 'corpus.json').write_text(json.dumps({'crates': [{'name': 'test', 'version': '1'}]}))
        (self.root / 'test-1').mkdir()
        (self.root / 'test-1/positions.tsv').write_text('lib.rs\t1\t2\n')
        self.report = {'backends': {'base': '/a', 'compare': '/b'}, 'crates': [{
            'name': 'test', 'skipped': [], 'records': [
                {'file': 'lib.rs', 'line': 1, 'column': 2, 'mode': mode, 'same': True,
                 'base': {'status': 'ok'}, 'compare': {'status': 'ok'}}
                for mode in ['SigOnly', 'Recurse']]}]}

    def test_exact_coverage(self):
        result = summary.summarize(self.report, self.root)
        self.assertTrue(result['coverage_complete'])
        self.assertFalse(result['needs_triage'])

    def test_missing_mode_cannot_be_hidden_by_duplicate(self):
        records = self.report['crates'][0]['records']
        records[1] = copy.deepcopy(records[0])
        result = summary.summarize(self.report, self.root)
        self.assertFalse(result['coverage_complete'])
        self.assertEqual(result['crates'][0]['duplicates'], 1)
        self.assertEqual(result['crates'][0]['missing'], [('lib.rs', 1, 2, 'Recurse')])

    def test_equal_failures_are_not_a_correctness_pass(self):
        for record in self.report['crates'][0]['records']:
            record['base']['status'] = record['compare']['status'] = 'crash'
        result = summary.summarize(self.report, self.root)
        self.assertTrue(result['coverage_complete'])
        self.assertTrue(result['needs_triage'])
        self.assertEqual(len(result['failures']), 4)

    def test_status_changes_and_output_changes_are_separate(self):
        records = self.report['crates'][0]['records']
        for record in records:
            record['same'] = False
        records[0]['base']['status'] = 'oom'
        result = summary.summarize(self.report, self.root)
        self.assertEqual(len(result['output_differences']), 1)
        self.assertEqual(len(result['status_changes']), 1)
        self.assertTrue(result['needs_triage'])

    def test_skipped_entry_is_incomplete(self):
        self.report['crates'][0]['skipped'] = [('test', 'missing prerequisite')]
        self.report['crates'][0]['records'] = []
        result = summary.summarize(self.report, self.root)
        self.assertFalse(result['coverage_complete'])


if __name__ == '__main__':
    unittest.main()
