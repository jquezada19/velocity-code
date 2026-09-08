import json
import os
import tempfile
import unittest

import xmr


def rec(commit, wrong=0, refused=100, mism=0, top1=100.0, parsed=True, drop_mism=False):
    lex = {"parsed": parsed, "pass": True, "queries": 40, "mismatches": mism}
    if drop_mism:
        del lex["mismatches"]
    return {
        "commit": commit,
        "t9a": {"parsed": parsed, "pass": True, "n": 100,
                "vc": {"wrong_apply": wrong, "refused": refused, "partial_write": 0},
                "base": {"wrong_apply": 100}},
        "r1_lexical": lex,
        "r1_defs": {"parsed": parsed, "pass": True, "top1_pct": top1, "wrong_pct": 0.0},
    }


def seq(n):
    return list(range(n))


class Limits(unittest.TestCase):
    def test_constants_and_both_limits(self):
        centre, mr_bar, unclamped, clamped, mr_ul = xmr.limits([1, 3, 2, 4], seq(4))
        self.assertAlmostEqual(centre, 2.5)
        self.assertAlmostEqual(mr_bar, 5 / 3)  # |2|,|1|,|2| → 5/3
        self.assertAlmostEqual(unclamped[0], 2.5 - 2.66 * 5 / 3)
        self.assertAlmostEqual(unclamped[1], 2.5 + 2.66 * 5 / 3)
        self.assertAlmostEqual(mr_ul, 3.268 * 5 / 3)

    def test_clamp_bounds_display_not_detection(self):
        values = [0, 1, 0, 1]
        centre, mr_bar, unclamped, clamped, mr_ul = xmr.limits(values, seq(4), lower=0.0)
        self.assertLess(unclamped[0], 0)      # the unclamped lower limit is negative
        self.assertEqual(clamped[0], 0.0)     # the displayed one is floored at 0
        self.assertEqual(clamped[1], unclamped[1])
        # a valid value at the clamp is inside the limits either way
        self.assertEqual([r for _, r in xmr.signals(values, seq(4), centre, unclamped, clamped, mr_ul) if r == "rule1"], [])

    def test_constant_series_has_zero_width(self):
        centre, mr_bar, unclamped, clamped, _ = xmr.limits([100.0] * 8, seq(8))
        self.assertEqual(mr_bar, 0)
        self.assertEqual(clamped, (100.0, 100.0))

    def test_gap_breaks_moving_range(self):
        # positions 0,1 then 3,4: the 1→3 pair is not consecutive, so its range is skipped
        self.assertEqual(xmr.moving_ranges([0, 0, 10, 10], [0, 1, 3, 4]), [0, None, 0])
        centre, mr_bar, *_ = xmr.limits([0, 0, 10, 10], [0, 1, 3, 4])
        self.assertEqual(mr_bar, 0)           # the jump across the gap never enters mR̄

    def test_no_consecutive_runs_means_no_limits(self):
        self.assertIsNone(xmr.limits([1, 2, 3, 4], [0, 2, 4, 6]))


class Signals(unittest.TestCase):
    def sig(self, values, positions=None, lower=None, upper=None):
        positions = positions or seq(len(values))
        centre, _, unc, cl, mr_ul = xmr.limits(values, positions, lower, upper)
        return xmr.signals(values, positions, centre, unc, cl, mr_ul)

    def test_rule1_on_the_first_change_after_a_flat_history(self):
        values = [0.0] * 9 + [1.0]
        self.assertIn((9, "rule1"), self.sig(values, lower=0.0))

    def test_rule1_both_sides_with_exact_index(self):
        values = [10.0, 11.0, 10.0, 11.0, 10.0, 11.0, 10.0, 11.0, 30.0, 10.0, -20.0]
        rules = self.sig(values)
        self.assertIn((8, "rule1"), rules)
        self.assertIn((10, "rule1"), rules)
        self.assertNotIn((9, "rule1"), rules)

    def test_rule2_two_of_three_beyond_two_sigma_high_side_exact_completion(self):
        base = [10.0, 11.0] * 5
        values = base + [14.5, 10.5, 14.5]      # with the tail, mR̄ ≈ 1.71, centre ≈ 11.1, 2σ ≈ 14.1, 3σ ≈ 15.7
        centre, _, unc, cl, mr_ul = xmr.limits(values, seq(13))
        hi2 = centre + 2 * (unc[1] - centre) / 3
        self.assertLess(hi2, 14.5)               # the fixture really is beyond 2σ
        self.assertLess(14.5, unc[1])            # and inside 3σ, so this is rule2, not rule1
        rules = xmr.signals(values, seq(13), centre, unc, cl, mr_ul)
        self.assertIn((12, "rule2"), rules)
        self.assertNotIn((11, "rule2"), rules)   # only one high point in that window
        self.assertNotIn((12, "rule1"), rules)

    def test_rule2_low_side(self):
        base = [10.0, 11.0] * 5
        values = base + [6.5, 10.5, 6.5]
        centre, _, unc, cl, mr_ul = xmr.limits(values, seq(13))
        lo2 = centre - 2 * (unc[1] - centre) / 3
        self.assertGreater(lo2, 6.5)
        self.assertGreater(6.5, unc[0])
        self.assertIn((12, "rule2"), xmr.signals(values, seq(13), centre, unc, cl, mr_ul))

    def test_rule2_needs_two_not_one(self):
        base = [10.0, 11.0] * 5
        values = base + [14.5, 10.5, 10.0]
        self.assertEqual([r for _, r in self.sig(values) if r == "rule2"], [])

    def test_rule2_zone_uses_unclamped_limits(self):
        # a lower clamp at 0 must not narrow the two-sigma zone: with the clamped
        # width the zone would shrink and two ordinary lows would read as rule2
        values = [0.0, 2.0] * 4 + [0.0, 2.0, 0.0]
        centre, _, unc, cl, mr_ul = xmr.limits(values, seq(len(values)), lower=0.0)
        self.assertLess(centre - 2 * (unc[1] - centre) / 3, 0)
        self.assertEqual([r for _, r in xmr.signals(values, seq(len(values)), centre, unc, cl, mr_ul) if r == "rule2"], [])

    def test_run8_exact_completion_and_run7_is_not_enough(self):
        low = [10.0, 11.0, 10.0, 11.0]
        high = [14.0, 14.5, 14.2, 14.8, 14.1, 14.6, 14.3, 14.7]
        self.assertIn((11, "run8"), self.sig(low + high))
        self.assertEqual([r for _, r in self.sig(low + high[:7]) if r == "run8"], [])

    def test_run8_low_side(self):
        high = [14.0, 14.5, 14.2, 14.8]
        low = [10.0, 10.4, 10.2, 10.6, 10.1, 10.5, 10.3, 10.7]
        self.assertIn((11, "run8"), self.sig(high + low))

    def test_windows_never_span_a_gap(self):
        # eight zeros, four tens, a gap, four tens: run8 must not complete across the gap
        values = [0.0] * 8 + [10.0] * 8
        positions = seq(12) + [13, 14, 15, 16]  # position 12 is the missing run
        rules = self.sig(values, positions)
        self.assertNotIn((15, "run8"), rules)
        # and the same stream without the gap does complete
        self.assertIn((15, "run8"), self.sig(values))

    def test_mr_signal_on_a_jump_between_neighbours(self):
        values = [10.0, 10.5, 10.0, 10.5, 10.0, 10.5, 10.0, 10.5, 10.0, 10.5, 13.0, 12.6]
        self.assertIn((10, "mr"), self.sig(values))

    def test_mr_signal_not_across_a_gap(self):
        values = [10.0, 10.5, 10.0, 10.5, 10.0, 10.5, 10.0, 10.5, 10.0, 10.5, 13.0, 12.6]
        positions = seq(10) + [11, 12]         # the jump 10.5→13.0 straddles a gap
        self.assertNotIn((10, "mr"), self.sig(values, positions))

    def test_no_signal_inside_limits(self):
        self.assertEqual(self.sig([10.0, 11.0, 10.5, 11.5, 10.0, 11.0, 10.5, 11.0]), [])


class Series(unittest.TestCase):
    def test_unparsed_harness_is_a_gap_not_a_value(self):
        records = [rec("aaaaaaa1"), rec("bbbbbbb2", parsed=False), rec("ccccccc3")]
        points, gaps = xmr.series_for(records, "t9a", ("vc", "wrong_apply"))
        self.assertEqual([(p, c) for p, c, _ in points], [(0, "aaaaaaa"), (2, "ccccccc")])
        self.assertEqual(gaps, ["bbbbbbb"])

    def test_parsed_but_metric_absent_is_a_gap(self):
        records = [rec("a1"), rec("b2", drop_mism=True)]
        points, gaps = xmr.series_for(records, "r1_lexical", ("mismatches",))
        self.assertEqual(len(points), 1)
        self.assertEqual(gaps, ["b2"])

    def test_nan_infinity_bool_and_string_are_malformed(self):
        for bad in (float("nan"), float("inf"), True, "3"):
            r = rec("a1")
            r["r1_lexical"]["mismatches"] = bad
            with self.assertRaises(ValueError):
                xmr.series_for([r], "r1_lexical", ("mismatches",))

    def test_out_of_domain_is_malformed(self):
        r = rec("a1", mism=-1)
        with self.assertRaises(ValueError):
            xmr.series_for([r], "r1_lexical", ("mismatches",), lower=0.0)
        r = rec("a1", top1=101.0)
        with self.assertRaises(ValueError):
            xmr.series_for([r], "r1_defs", ("top1_pct",), lower=0.0, upper=100.0)
        # the boundary itself is valid
        xmr.series_for([rec("a1", top1=100.0), rec("a2", mism=0)], "r1_defs", ("top1_pct",), lower=0.0, upper=100.0)


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
        self.assertIn("mR̄ is 0 so far", out)

    def test_short_flat_history_does_not_flag_the_first_change(self):
        # [0,0,0,1]: the 1 enters mR̄ and widens the limits — documented, and pinned
        out = xmr.render(xmr.load_history(self._write([rec("c0"), rec("c1"), rec("c2"), rec("c3", mism=1)])))
        self.assertNotIn("signal rule1", out.split("### r1_lexical mismatches")[1].split("###")[0])

    def test_malformed_line_is_an_error_not_a_value(self):
        path = self._write([rec("a1")], extra_line="{not json")
        with self.assertRaises(ValueError):
            xmr.load_history(path)
        self.assertEqual(xmr.main([path]), 1)

    def test_non_finite_value_fails_the_report(self):
        r = rec("a1")
        r["r1_lexical"]["mismatches"] = float("nan")
        path = self._write([r] + [rec(f"c{i}") for i in range(7)])
        self.assertEqual(xmr.main([path]), 1)


if __name__ == "__main__":
    unittest.main()
