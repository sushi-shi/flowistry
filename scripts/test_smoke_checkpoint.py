"""Resume must preserve failures and reject changed/corrupt evidence."""
import contextlib
import io
import importlib.util
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from smoke_checkpoint import Checkpoints, tree_digest, configuration_inputs

spec = importlib.util.spec_from_file_location('smoke', Path(__file__).with_name('smoke-real-crates.py'))
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


class CheckpointTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def store(self, manifest=None):
        store = Checkpoints(self.root / 'checkpoints', manifest or {'backend': 'one'})
        self.addCleanup(store.close)
        return store

    def test_manifest_changes_do_not_reuse_records(self):
        first = self.store()
        first.save({'position': 1}, {'status': 'crash', 'message': 'ICE'})
        first.close()
        second = self.store({'backend': 'two'})
        self.assertIsNone(second.load({'position': 1}))
        second.close()
        third = self.store()
        self.assertEqual(third.load({'position': 1})['status'], 'crash')

    def test_corrupted_record_is_not_evidence(self):
        store = self.store()
        store.save({'position': 1}, {'status': 'crash'})
        record = next(p for p in store.root.glob('*.json') if p.name != 'manifest.json')
        data = json.loads(record.read_text())
        data['record']['status'] = 'ok'
        record.write_text(json.dumps(data))
        self.assertIsNone(store.load({'position': 1}))
        record.write_text('{partial')
        self.assertIsNone(store.load({'position': 1}))

    def test_concurrent_run_is_rejected(self):
        self.store()
        with self.assertRaisesRegex(RuntimeError, 'in use'):
            Checkpoints(self.root / 'checkpoints', {'backend': 'one'})

    def test_source_changes_invalidate_but_build_outputs_do_not(self):
        source = self.root / 'source'
        source.mkdir()
        (source / 'main.rs').write_text('fn main() {}')
        before = tree_digest(source)
        (source / 'target').mkdir()
        (source / 'target' / 'binary').write_text('compiled')
        self.assertEqual(before, tree_digest(source))
        (source / 'main.rs').write_text('fn main() { panic!() }')
        self.assertNotEqual(before, tree_digest(source))

    def test_paired_runs_resume_without_losing_differences_or_digests(self):
        source = self.root / 'source'
        source.mkdir()
        (source / 'main.rs').write_text('fn main() {}')
        positions = self.root / 'positions'
        positions.mkdir()
        (positions / 'positions.tsv').write_text('main.rs\t0\t3\n')
        store = self.store()
        args = SimpleNamespace(update_corpus=False, budgets=None, modes=['SigOnly'],
                               prepare_only=False, checkpoints=store, keep_outputs=False)
        def report():
            return {'crate': 'test', 'files': ['main.rs'], 'records': []}
        results = [{'status': 'ok', 'output': {'digest': value, 'places': 1}}
                   for value in ['a', 'b']]
        with patch.object(smoke, 'focus_repeated', side_effect=results) as run:
            first = smoke.run_positions(report(), positions, source, args,
                                        [('base', self.root), ('compare', self.root)])
            self.assertEqual(run.call_count, 2)
        self.assertFalse(first['records'][0]['same'])
        self.assertEqual(first['records'][0]['base']['output_digest'], 'a')
        with patch.object(smoke, 'focus_repeated', side_effect=AssertionError('unexpected rerun')):
            second = smoke.run_positions(report(), positions, source, args,
                                         [('base', self.root), ('compare', self.root)])
        self.assertFalse(second['records'][0]['same'])
        self.assertTrue(second['records'][0]['checkpoint_reused'])
        (source / 'main.rs').write_text('fn main() { }')
        with patch.object(smoke, 'focus_repeated', return_value={'status': 'timeout', 'message': 'timeout'}) as run:
            third = smoke.run_positions(report(), positions, source, args,
                                        [('base', self.root), ('compare', self.root)])
            self.assertEqual(run.call_count, 2)
            self.assertEqual(third['records'][0]['base']['status'], 'timeout')

    def test_configuration_content_participates_in_identity(self):
        project = self.root / 'project'
        project.mkdir()
        with patch.dict('os.environ', {'CARGO_HOME': str(self.root / 'cargo-home')}):
            before = configuration_inputs(project)
            config = self.root / '.cargo' / 'config.toml'
            config.parent.mkdir()
            config.write_text('[build]\nrustflags = ["--cfg", "changed"]\n')
            self.assertNotEqual(before, configuration_inputs(project))

    def test_skipped_corpus_entry_fails_the_run(self):
        for binary in ('cargo-flowistry', 'flowistry-driver'):
            (self.root / binary).write_text('unused')
        report = {'crate': None, 'skipped': [('either', 'missing dependency')], 'records': [], 'files': []}
        argv = ['smoke', str(self.root), '--crate', 'either', '--work-dir', str(self.root / 'work')]
        with patch('sys.argv', argv), patch.object(smoke.shutil, 'which', return_value='/cargo'), \
             patch.object(smoke, 'registry_dirs', return_value=[]), \
             patch.object(smoke, 'smoke_crate', return_value=report), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(SystemExit) as raised:
                smoke.main()
        self.assertEqual(raised.exception.code, 1)

    def test_mid_run_source_change_is_not_checkpointed(self):
        source = self.root / 'source'
        source.mkdir()
        file = source / 'main.rs'
        file.write_text('fn main() {}')
        positions = self.root / 'positions'
        positions.mkdir()
        (positions / 'positions.tsv').write_text('main.rs\t0\t3\n')
        store = self.store()
        args = SimpleNamespace(update_corpus=False, budgets=None, modes=['SigOnly'],
                               prepare_only=False, checkpoints=store, keep_outputs=False)
        def mutate(*args):
            file.write_text('fn main() { panic!() }')
            return {'status': 'ok', 'output': {'digest': 'stale', 'places': 1}}
        with patch.object(smoke, 'focus_repeated', side_effect=mutate):
            with self.assertRaisesRegex(RuntimeError, 'changed during validation'):
                smoke.run_positions({'crate': 'test', 'files': ['main.rs'], 'records': []},
                                    positions, source, args, [('base', self.root)])
        self.assertEqual([p.name for p in store.root.glob('*.json')], ['manifest.json'])


if __name__ == '__main__':
    unittest.main()
