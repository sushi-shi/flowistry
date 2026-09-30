import base64
import gzip
import json
import subprocess
import unittest

from incremental_matrix import assert_equivalent, observation


class OracleTests(unittest.TestCase):
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
