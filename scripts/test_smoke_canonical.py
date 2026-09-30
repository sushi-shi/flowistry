"""Protocol-level regression tests for semantic smoke-output comparisons."""
import base64
import copy
import gzip
import importlib.util
import json
from pathlib import Path
import unittest

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
            other["place_info"][0][field] *= 3
            other["place_info"][0][field].reverse()
        other["containers"].reverse()
        self.assertEquivalent(other)

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


if __name__ == "__main__":
    unittest.main()
