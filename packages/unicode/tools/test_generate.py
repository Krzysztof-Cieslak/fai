"""Focused tests for the fixed-width table encoding and UCD record reader."""

import unittest

import generate


class GeneratorTests(unittest.TestCase):
    def test_packed_field_boundaries(self):
        values = [0, 63, 64, 4095, 4096, 0x10FFFF, 0xFFFFFF]
        text = generate.packed(values)
        decoded = [sum((ord(text[i + j]) - 32) << (6 * j) for j in range(4))
                   for i in range(0, len(text), 4)]
        self.assertEqual(decoded, values)

    def test_negative_field_rejected(self):
        with self.assertRaises(ValueError):
            generate.packed([-1])

    def test_oversized_field_rejected(self):
        with self.assertRaises(ValueError):
            generate.packed([0x1000000])

    def test_comments_and_directives_are_not_records(self):
        rows = list(generate.records({"fixture": "# heading\n@Part0\n0041 ; A # detail\n"}, "fixture"))
        self.assertEqual(rows, [["0041", "A"]])

    def test_singleton_range(self):
        self.assertEqual(generate.interval("0041"), (65, 65))

    def test_inclusive_range(self):
        self.assertEqual(generate.interval("0041..005A"), (65, 90))


if __name__ == "__main__":
    unittest.main()
