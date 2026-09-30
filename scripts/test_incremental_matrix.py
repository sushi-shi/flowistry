import base64
import copy
import gzip
import json
import subprocess
import unittest

from incremental_matrix import assert_equivalent, observation


class OracleTests(unittest.TestCase):
    def test_superseded_envelope_cannot_deliver_a_current_result(self):
        encoded = base64.b64encode(gzip.compress(json.dumps({'Ok': {'bodies': []}}).encode())).decode()
        envelope = {'schema': 1, 'status': 'current', 'generation': 'new', 'output': encoded}
        fresh = observation(subprocess.CompletedProcess([], 0, json.dumps(envelope).encode(), b''), .1)
        self.assertIsNotNone(fresh['semantic_digest'])
        envelope.update(status='superseded', generation='old', output=None)
        stale = observation(subprocess.CompletedProcess([], 75, json.dumps(envelope).encode(), b''), .1)
        self.assertIsNone(stale['response'])
        self.assertEqual(stale['publication']['generation'], 'old')
        with self.assertRaises(AssertionError):
            assert_equivalent(stale, fresh)

    def test_indexed_maybe_slice_changes_are_semantic(self):
        first = {'Ok': {'bodies': [{'range': {'filename': 'lib.rs'}, 'focus': {'Ok': {
            'ranges': [{'filename': 'lib.rs', 'start': [1, 0], 'end': [1, 8]},
                       {'filename': 'lib.rs', 'start': [2, 0], 'end': [2, 8]}],
            'place_info': [{'range': 0, 'ranges': [0], 'slice': [0],
                            'direct_influence': [], 'maybe_slice': [1]}],
            'containers': []}}}]}}
        changed = copy.deepcopy(first)
        changed['Ok']['bodies'][0]['focus']['Ok']['place_info'][0]['maybe_slice'] = []
        def observe(value):
            wire = base64.b64encode(gzip.compress(json.dumps(value).encode()))
            return observation(subprocess.CompletedProcess([], 0, wire, b''), .1)
        original, modified = observe(first), observe(changed)
        self.assertEqual(original['maybe_slice_indices'], 1)
        self.assertEqual(modified['maybe_slice_indices'], 0)
        with self.assertRaisesRegex(AssertionError, 'differs'):
            assert_equivalent(original, modified)

    def test_cache_hit_metadata_cannot_hide_stale_semantics(self):
        fresh = {'exit_code': 0, 'semantic_digest': 'new'}
        stale = {'exit_code': 0, 'semantic_digest': 'old', 'cache': {'hits': 1}}
        with self.assertRaisesRegex(AssertionError, 'differs'):
            assert_equivalent(stale, fresh)

    def test_cached_success_cannot_hide_current_compile_error(self):
        with self.assertRaisesRegex(AssertionError, 'rejected'):
            assert_equivalent({'exit_code': 0, 'semantic_digest': 'old'}, {'exit_code': 1})

    def test_empty_success_is_not_an_oracle(self):
        with self.assertRaises(AssertionError):
            assert_equivalent({'exit_code': 0}, {'exit_code': 0})

    def test_observes_actual_solver_work_even_with_a_cache_hit(self):
        wire = base64.b64encode(gzip.compress(json.dumps({'Ok': {'bodies': [], 'cache': {'hits': 1}}}).encode()))
        stderr = b'[INFO] audit compiler\n[INFO] audit solve summary child::apply\n[INFO] Focus cache hit: selected\n'
        result = observation(subprocess.CompletedProcess([], 0, wire, stderr), .1)
        self.assertEqual(result['compiler_invocations'], 1)
        self.assertEqual(result['solved_bodies'], [('summary', 'child::apply')])
        self.assertEqual(result['cache_hits'], ['selected'])


if __name__ == '__main__':
    unittest.main()
