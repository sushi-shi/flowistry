"""Protocol-level regression tests for semantic smoke-output comparisons."""
import base64
import copy
import gzip
import importlib.util
import json
from pathlib import Path
import unittest
from types import SimpleNamespace
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "smoke", Path(__file__).with_name("smoke-real-crates.py"))
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


def digest(output, pretty=False):
    encoded = base64.b64encode(gzip.compress(json.dumps(
        {"Ok": output}, indent=2 if pretty else None,
        separators=None if pretty else (",", ":")).encode()))
    return smoke.decode_response(encoded, False)


class CanonicalOutputTests(unittest.TestCase):
    def setUp(self):
        self.a = {"filename": "λ.rs", "start": [1, 0], "end": [1, 2]}
        self.b = {"filename": "λ.rs", "start": [2, 0], "end": [2, 3]}
        self.output = {"place_info": [{
            "range": self.a, "ranges": [self.a], "slice": [self.a, self.b],
            "direct_influence": [self.b], "maybe_slice": [self.a],
        }], "containers": [self.a, self.b]}

    def assertEquivalent(self, other):
        self.assertEqual(smoke.canonical(self.output), smoke.canonical(other))
        for pretty in [False, True]:
            self.assertEqual(digest(self.output), digest(other, pretty))

    def test_duplicates_and_order_are_not_changes(self):
        other = copy.deepcopy(self.output)
        for field in smoke.RANGE_LIST_FIELDS:
            if field not in other["place_info"][0]:
                continue
            other["place_info"][0][field] *= 3
            other["place_info"][0][field].reverse()
        other["containers"].reverse()
        self.assertEquivalent(other)

    def test_directional_ranges_survive_index_expansion(self):
        for field in ('pre_slice', 'post_slice', 'maybe_pre_slice', 'maybe_post_slice'):
            plain = copy.deepcopy(self.output)
            plain['place_info'][0][field] = [self.b]
            entry = {'range': 0, 'ranges': [0], 'slice': [0, 1],
                     'direct_influence': [1], 'maybe_slice': [0], field: [1]}
            indexed = {'ranges': [self.a, self.b], 'place_info': [entry],
                       'containers': self.output['containers']}
            self.assertEqual(digest(plain), digest(indexed))
            entry[field] = [0]
            self.assertNotEqual(digest(plain), digest(indexed))

    def test_range_table_order_and_unused_entries_are_not_changes(self):
        for table, a, b in [([self.a, self.b], 0, 1), ([self.b, self.a, self.b], 1, 0)]:
            entry = {"range": a, "ranges": [a, a], "slice": [b, a, b],
                     "direct_influence": [b], "maybe_slice": [a]}
            # Exercise both the fallback and streaming decoder's object orders.
            self.assertEquivalent({"ranges": table, "place_info": [entry],
                                   "containers": self.output["containers"]})
            self.assertEquivalent({"place_info": [entry], "ranges": table,
                                   "containers": self.output["containers"]})

    def test_real_changes_remain_visible(self):
        for field in smoke.RANGE_LIST_FIELDS:
            other = copy.deepcopy(self.output)
            other["place_info"][0][field] = []
            self.assertNotEqual(smoke.canonical(self.output), smoke.canonical(other))
            self.assertNotEqual(digest(self.output), digest(other))
        for mutate in [lambda o: o["place_info"][0].update(range=self.b),
                       lambda o: o.update(containers=[]),
                       lambda o: o["place_info"].append(o["place_info"][0])]:
            other = copy.deepcopy(self.output)
            mutate(other)
            self.assertNotEqual(digest(self.output), digest(other))

    def test_invalid_table_references_are_rejected(self):
        for index in [-1, 2, True, "0"]:
            output = {"ranges": [self.a, self.b],
                      "place_info": [{"range": index, "slice": []}], "containers": []}
            with self.assertRaises(ValueError):
                digest(output)

    def test_empty_output_formats_match(self):
        self.assertEqual(digest({"place_info": [], "containers": []}),
                         digest({"ranges": [], "place_info": [], "containers": []}))

    def test_source_selection_metadata_resolves_indices(self):
        inline = dict(self.output, comments=[self.a], parameter_aliases=[{"range": self.b, "target": self.a}])
        for table, a, b in [([self.a, self.b], 0, 1), ([self.b, self.a], 1, 0)]:
            entry = {"range": a, "ranges": [a], "slice": [a, b], "direct_influence": [b], "maybe_slice": [a]}
            indexed = {"place_info": [entry], "ranges": table, "containers": self.output["containers"],
                       "comments": [a], "parameter_aliases": [{"range": b, "target": a}]}
            self.assertEqual(smoke.canonical(inline), smoke.canonical(indexed))
            for pretty in [False, True]:
                self.assertEqual(digest(inline), digest(indexed, pretty))
            indexed["parameter_aliases"][0]["target"] = b
            self.assertNotEqual(digest(inline), digest(indexed))
            indexed["comments"] = [-1]
            with self.assertRaises(ValueError):
                digest(indexed)



class FileFocusCanonicalTests(unittest.TestCase):
    def setUp(self):
        self.span = {'filename': 'test.rs', 'start': [1, 2], 'end': [1, 3]}
        self.focus = {'place_info': [{'range': self.span, 'ranges': [self.span],
                                     'slice': [self.span], 'direct_influence': [],
                                     'maybe_slice': [self.span]}], 'containers': [self.span]}
        self.output = {'bodies': [{'range': self.span, 'focus': {'Ok': self.focus}, 'cached': False},
                                  {'range': dict(self.span, start=[5, 0]), 'focus': None, 'cached': None}],
                       'cache': {'hits': 0, 'misses': 1}}

    def test_cache_metadata_and_table_layout_do_not_change_semantics(self):
        other = copy.deepcopy(self.output)
        other['cache'] = {'hits': 1, 'misses': 0, 'validation': 'snapshot'}
        other['bodies'][0]['cached'] = True
        other['bodies'][0]['focus']['Ok'] = {
            'ranges': [self.span], 'place_info': [{'range': 0, 'ranges': [0, 0],
                                                'slice': [0], 'direct_influence': [], 'maybe_slice': [0]}],
            'containers': [self.span]}
        other['bodies'].reverse()
        self.assertEqual(smoke.canonical(self.output), smoke.canonical(other))
        a, b = digest(self.output), digest(other)
        self.assertEqual(a['Ok'], b['Ok'])
        self.assertEqual(a['cache']['misses'], 1)
        self.assertEqual(b['cache']['validation'], 'snapshot')
        self.assertEqual(a['Ok']['places'], 1)

    def test_body_errors_and_changed_maybe_slices_are_visible(self):
        changed = copy.deepcopy(self.output)
        changed['bodies'][0]['focus'] = {'Err': 'analysis failed'}
        self.assertNotEqual(digest(self.output)['Ok'], digest(changed)['Ok'])
        changed = copy.deepcopy(self.output)
        changed['bodies'][0]['focus']['Ok']['place_info'][0]['maybe_slice'] = []
        self.assertNotEqual(digest(self.output)['Ok'], digest(changed)['Ok'])

    def test_body_locations_are_not_discarded(self):
        changed = copy.deepcopy(self.output)
        changed['bodies'][0]['range']['filename'] = 'other.rs'
        self.assertNotEqual(digest(self.output)['Ok'], digest(changed)['Ok'])

    def numeric_files(self, source_id):
        value = json.dumps(self.output).replace('"test.rs"', str(source_id))
        return json.loads(value)

    def test_requested_file_interner_slot_is_response_local(self):
        first = self.numeric_files(0)
        second = self.numeric_files(7)
        second['bodies'].reverse()
        self.assertEqual(smoke.canonical(first), smoke.canonical(second))
        self.assertEqual(digest(first)['Ok'], digest(second, pretty=True)['Ok'])
        # Normalization must not mutate retained raw evidence.
        self.assertEqual(first['bodies'][0]['range']['filename'], 0)

    def test_foreign_numeric_filename_is_not_guessed(self):
        for field in ('range', 'maybe_slice'):
            output = self.numeric_files(0)
            entry = output['bodies'][0]['focus']['Ok']['place_info'][0]
            span = entry[field] if field == 'range' else entry[field][0]
            span['filename'] = 1
            with self.assertRaisesRegex(ValueError, 'foreign'):
                digest(output)
        output = self.numeric_files(0)
        output['bodies'][1]['range']['filename'] = 1
        with self.assertRaisesRegex(ValueError, 'foreign'):
            digest(output)

    def test_normalizing_ids_preserves_range_changes(self):
        first = self.numeric_files(0)
        second = self.numeric_files(7)
        second['bodies'][0]['focus']['Ok']['place_info'][0]['maybe_slice'][0]['end'] = [2, 0]
        self.assertNotEqual(digest(first)['Ok'], digest(second)['Ok'])

    def test_filename_table_resolves_foreign_files_and_aliases(self):
        first = self.numeric_files(0)
        first['files'] = {'0': 'test.rs', '1': '/rustc/core/macros.rs'}
        first['bodies'][0]['focus']['Ok']['containers'][0]['filename'] = 1
        second = self.numeric_files(7)
        second['files'] = {'7': 'test.rs', '9': '/rustc/core/macros.rs', '11': 'test.rs', '12': 'unused.rs'}
        second['bodies'][0]['focus']['Ok']['containers'][0]['filename'] = 9
        second['bodies'][1]['range']['filename'] = 11
        self.assertEqual(digest(first)['Ok'], digest(second)['Ok'])
        second['files']['9'] = '/rustc/other/macros.rs'
        self.assertNotEqual(digest(first)['Ok'], digest(second)['Ok'])
        del second['files']['9']
        with self.assertRaisesRegex(ValueError, 'missing'):
            digest(second)

    def test_decoder_rejection_is_a_protocol_failure_not_a_compiler_crash(self):
        output = self.numeric_files(0)
        output['bodies'][1]['range']['filename'] = 1
        wire = base64.b64encode(gzip.compress(json.dumps({'Ok': output}).encode())).decode()
        completed = SimpleNamespace(returncode=0, stdout=wire, stderr='', max_rss_kb=1024)
        with patch.object(smoke, 'run', return_value=completed):
            result = smoke.flowistry_focus('.', {}, 'lib.rs', 0, 0, 'SigOnly', 30,
                                          keep_output=False, command='file-focus')
        self.assertEqual(result['status'], 'error')
        self.assertTrue(result['protocol_error'])
        self.assertIn('foreign', result['message'])

if __name__ == "__main__":
    unittest.main()
