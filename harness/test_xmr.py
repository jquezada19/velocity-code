import json
import os
import tempfile
import unittest

import xmr


def rec(commit, wrong=0, refused=100, mism=0, top1=100.0, parsed=True):
    return {
        "commit": commit,
        "t9a": {"parsed": parsed, "pass": True, "n": 100,
                "vc": {"wrong_apply": wrong, "refused": refused, "partial_write": 0},
                "base": {"wrong_apply": 100}},
        "r1_lexical": {"parsed": parsed, "pass": True, "queries": 40, "mismatches": mism},
        "r1_defs": {"parsed": parsed, "pass": True, "top1_pct": top1, "wrong_pct": 0.0},
    }


class Limits(unittest.TestCase):
    def test_constants_and_construction(self):
        centre, mr_bar, unclamped, clamped, mr_ul = xmr.limits([1, 3, 2, 4])
        self.assertAlmostEqual(centre, 2.5)
        self.assertAlmostEqual(mr_bar, 5 / 3)  # |2|,|1|,|2| → 5/3
        self.assertAlmostEqual(unclamped[1], 2.5 + 2.66 * 5 / 3)
        self.assertAlmostEqual(mr_ul, 3.268 * 5 / 3)

    def test_clamp_bounds_display_not_detection(self):
        centre, mr_bar, unclamped, clamped, _ = xmr.limits([0, 1, 0, 1], lower=0.0)
        self.assertLess(unclamped[0], 0)      # the unclamped lower limit is negative
        self.assertEqual(clamped[0], 0.0)     # the displayed one is floored at 0
        self.assertEqual(clamped[1], unclamped[1])

    def test_constant_series_has_zero_width(self):
        centre, mr_bar, unclamped, clamped, _ = xmr.limits([100.0] * 8)
        self.assertEqual(mr_bar, 0)
        self.assertEqual(clamped, (100.0, 100.0))


class Signals(unittest.TestCase):
    def test_rule1_on_first_change_of_a_constant_series(self):
        values = [0.0] * 9 + [1.0]
        centre, _, unc, cl, _ = xmr.limits(values, lower=0.0)
        rules = [r for i, r in xmr.signals(values, centre, unc, cl) if i == 9]
        self.assertIn("rule1", rules)

    def test_run8_on_a_sustained_shift(self):
        values = [10.0, 11.0, 10.0, 11.0] + [14.0, 14.5, 14.2, 14.8, 14.1, 14.6, 14.3, 14.7]
        centre, _, unc, cl, _ = xmr.limits(values)
        rules = {(i, r) for i, r in xmr.signals(values, centre, unc, cl)}
        self.assertIn((11, "run8"), rules)

    def test_mr_signal_on_a_jump_between_neighbours(self):
        # steady small wobble, then one large step: the X chart may stay inside
        # its limits but the moving-range chart flags the step itself
        values = [10.0, 10.5, 10.0, 10.5, 10.0, 10.5, 10.0, 10.5, 10.0, 10.5, 13.0, 12.6]
        centre, _, unc, cl, mr_ul = xmr.limits(values)
        rules = {(i, r) for i, r in xmr.signals(values, centre, unc, cl, mr_ul)}
        self.assertIn((10, "mr"), rules)

    def test_no_signal_inside_limits(self):
        values = [10.0, 11.0, 10.5, 11.5, 10.0, 11.0, 10.5, 11.0]
        centre, _, unc, cl, mr_ul = xmr.limits(values)
        self.assertEqual(xmr.signals(values, centre, unc, cl, mr_ul), [])

    def test_rule2_zone_uses_unclamped_limits(self):
        # a lower clamp at 0 must not narrow the two-sigma zone
        values = [0.0, 2.0, 0.0, 2.0, 0.0, 2.0, 0.0, 2.0]
        centre, _, unc, cl, _ = xmr.limits(values, lower=0.0)
        sigma = (unc[1] - centre) / 3
        self.assertLess(centre - 2 * sigma, 0)  # unclamped zone extends below the clamp
        self.assertEqual([r for _, r in xmr.signals(values, centre, unc, cl) if r == "rule2"], [])


class Series(unittest.TestCase):
    def test_unparsed_harness_is_a_gap_not_a_value(self):
        records = [rec("aaaaaaa1"), rec("bbbbbbb2", parsed=False), rec("ccccccc3")]
        points, gaps = xmr.series_for(records, "t9a", ("vc", "wrong_apply"))
        self.assertEqual([p[0] for p in points], ["aaaaaaa", "ccccccc"])
        self.assertEqual(gaps, ["bbbbbbb"])


class Render(unittest.TestCase):
    def _write(self, records, extra_line=None):
        fd, path = tempfile.mkstemp(suffix=".jsonl")
        with os.fdopen(fd, "w") as fh:
            for r in records:
                fh.write(json.dumps(r) + "\n")
            if extra_line is not None:
                fh.write(extra_line + "\n")
        return path

    def test_below_min_points_lists_values_without_limits(self):
        out = xmr.render(xmr.load_history(self._write([rec("a1"), rec("a2")])))
        self.assertIn("limits: not computed (n < 4)", out)
        self.assertNotIn("centre =", out)

    def test_provisional_between_min_and_eight(self):
        out = xmr.render(xmr.load_history(self._write([rec(f"c{i}") for i in range(5)])))
        self.assertIn("(provisional)", out)

    def test_eight_points_are_not_provisional(self):
        out = xmr.render(xmr.load_history(self._write([rec(f"c{i}") for i in range(8)])))
        self.assertNotIn("(provisional)", out)
        self.assertIn("mR̄ is 0", out)

    def test_malformed_line_is_an_error_not_a_value(self):
        path = self._write([rec("a1")], extra_line="{not json")
        with self.assertRaises(ValueError):
            xmr.load_history(path)
        self.assertEqual(xmr.main([path]), 1)


if __name__ == "__main__":
    unittest.main()
