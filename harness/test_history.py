import contextlib
import io
import json
import os
import tempfile
import unittest

import history


def tmp(text):
    fd, path = tempfile.mkstemp()
    with os.fdopen(fd, "w") as fh:
        fh.write(text)
    return path


class T9a(unittest.TestCase):
    def test_summary_line_parses(self):
        p = tmp('{"trial": 1}\n{"n": 100, "vc": {"wrong_apply": 0, "refused": 100, "clean": 0, "other": 0, "partial_write": 0}, "base": {"wrong_apply": 100, "silent_noop": 0}, "gate_pass": true}\n')
        r = history.parse_t9a(history._read(p))
        self.assertTrue(r["parsed"] and r["pass"])
        self.assertEqual(r["vc"]["refused"], 100)

    def test_string_gate_pass_is_malformed(self):
        p = tmp('{"n": 100, "vc": {"wrong_apply": 0, "refused": 100}, "base": {"wrong_apply": 100}, "gate_pass": "false"}\n')
        r = history.parse_t9a(history._read(p))
        self.assertFalse(r["parsed"])

    def test_empty_counters_are_malformed(self):
        p = tmp('{"n": 100, "vc": {}, "base": {}, "gate_pass": true}\n')
        self.assertFalse(history.parse_t9a(history._read(p))["parsed"])

    def test_non_json_last_line_is_malformed(self):
        p = tmp('{"n": 1}\nGate: PASS\n')
        self.assertFalse(history.parse_t9a(history._read(p))["parsed"])

    def test_fractional_bool_string_and_negative_counters_are_malformed(self):
        for bad in ("0.5", "true", '"0"', "-1"):
            p = tmp('{"n": 100, "vc": {"wrong_apply": %s, "refused": 100}, "base": {"wrong_apply": 100}, "gate_pass": true}\n' % bad)
            self.assertFalse(history.parse_t9a(history._read(p))["parsed"], bad)

    def test_missing_file(self):
        self.assertFalse(history.parse_t9a(history._read("/nonexistent/x"))["parsed"])


class R1Lexical(unittest.TestCase):
    def test_pass_line_with_counts(self):
        r = history.parse_r1_lexical("R1 lexical parity: PASS (40 queries, 0 mismatches)\n")
        self.assertEqual((r["parsed"], r["pass"], r["queries"], r["mismatches"]), (True, True, 40, 0))

    def test_fail_line_without_counts(self):
        r = history.parse_r1_lexical("R1 MISMATCH [literal] foo\nR1 lexical parity: FAIL — see mismatches above\n")
        self.assertEqual((r["parsed"], r["pass"]), (True, False))

    def test_pass_without_counts_or_with_garbage_is_not_parsed(self):
        self.assertFalse(history.parse_r1_lexical("R1 lexical parity: PASS\n")["parsed"])
        self.assertFalse(history.parse_r1_lexical("R1 lexical parity: PASS (garbage)\n")["parsed"])
        self.assertFalse(history.parse_r1_lexical("R1 lexical parity: PASS (40 queries, 0 mismatches) extra\n")["parsed"])

    def test_pass_with_nonzero_mismatches_is_not_the_producer_shape(self):
        self.assertFalse(history.parse_r1_lexical("R1 lexical parity: PASS (40 queries, 1 mismatches)\n")["parsed"])

    def test_no_verdict(self):
        self.assertFalse(history.parse_r1_lexical("this gate compares vc against ripgrep; install ripgrep and re-run\n")["parsed"])


class R1Defs(unittest.TestCase):
    STATS = "R1 definitions: top-1 67/67 (100.00%), negative controls 5/5, confidently-wrong 0/67 (0.00%)\n"

    def test_pass(self):
        r = history.parse_r1_defs(self.STATS + "R1 definitions: PASS\n")
        self.assertTrue(r["parsed"] and r["pass"])
        self.assertEqual((r["top1"], r["n_pos"], r["top1_pct"], r["n_neg"]), (67, 67, 100.0, 5))

    def test_fail_variant(self):
        r = history.parse_r1_defs("R1 definitions: top-1 4/5 (80.00%), negative controls 2/2, confidently-wrong 0/5 (0.00%)\nR1 definitions: FAIL — top-1 80.00% < 98% threshold\n")
        self.assertEqual((r["parsed"], r["pass"], r["top1_pct"]), (True, False, 80.0))

    def test_malformed_percentage(self):
        r = history.parse_r1_defs("R1 definitions: top-1 5/5 (..%), negative controls 2/2, confidently-wrong 0/5 (0.00%)\nR1 definitions: PASS\n")
        self.assertFalse(r["parsed"])

    def test_percentage_outside_domain_is_malformed(self):
        r = history.parse_r1_defs("R1 definitions: top-1 5/5 (101.00%), negative controls 2/2, confidently-wrong 0/5 (0.00%)\nR1 definitions: PASS\n")
        self.assertFalse(r["parsed"])

    def test_trailing_garbage_and_passing_are_not_parsed(self):
        self.assertFalse(history.parse_r1_defs(self.STATS.rstrip("\n") + " extra\nR1 definitions: PASS\n")["parsed"])
        self.assertFalse(history.parse_r1_defs(self.STATS + "R1 definitions: PASSING\n")["parsed"])
        self.assertFalse(history.parse_r1_defs(self.STATS + "R1 definitions: FAIL\n")["parsed"])  # FAIL always carries a reason

    def test_stats_without_verdict(self):
        self.assertFalse(history.parse_r1_defs(self.STATS)["parsed"])


class Main(unittest.TestCase):
    def test_line_always_appended_and_exit_code_reflects_cleanliness(self):
        t9a = tmp('{"n": 100, "vc": {"wrong_apply": 0, "refused": 100}, "base": {"wrong_apply": 100}, "gate_pass": true}\n')
        lex_ok = tmp("R1 lexical parity: PASS (40 queries, 0 mismatches)\n")
        lex_bad = tmp("no verdict here\n")
        defs = tmp(R1Defs.STATS + "R1 definitions: PASS\n")
        out = tmp("")
        with contextlib.redirect_stdout(io.StringIO()):  # main prints the line; keep the test output clean
            self.assertEqual(history.main(["--t9a", t9a, "--r1-lexical", lex_ok, "--r1-defs", defs, "--commit", "abc", "--out", out]), 0)
            self.assertEqual(history.main(["--t9a", t9a, "--r1-lexical", lex_bad, "--r1-defs", defs, "--commit", "abc", "--out", out]), 1)
        lines = [json.loads(l) for l in open(out)]
        self.assertEqual([l["all_pass"] for l in lines], [True, False])
        self.assertFalse(lines[1]["r1_lexical"]["parsed"])

    def test_each_parsed_but_failed_verdict_exits_one_through_main(self):
        t9a_ok = tmp('{"n": 100, "vc": {"wrong_apply": 0, "refused": 100}, "base": {"wrong_apply": 100}, "gate_pass": true}\n')
        t9a_fail = tmp('{"n": 100, "vc": {"wrong_apply": 3, "refused": 97}, "base": {"wrong_apply": 100}, "gate_pass": false}\n')
        lex_ok = tmp("R1 lexical parity: PASS (40 queries, 0 mismatches)\n")
        lex_fail = tmp("R1 MISMATCH [literal] q\nR1 lexical parity: FAIL — see mismatches above\n")
        defs_ok = tmp(R1Defs.STATS + "R1 definitions: PASS\n")
        defs_fail = tmp("R1 definitions: top-1 4/5 (80.00%), negative controls 2/2, confidently-wrong 0/5 (0.00%)\nR1 definitions: FAIL — top-1 80.00% < 98% threshold\n")
        for name, args in (("t9a", [t9a_fail, lex_ok, defs_ok]), ("r1_lexical", [t9a_ok, lex_fail, defs_ok]), ("r1_defs", [t9a_ok, lex_ok, defs_fail])):
            out = tmp("")
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(history.main(["--t9a", args[0], "--r1-lexical", args[1], "--r1-defs", args[2], "--commit", "abc", "--out", out]), 1, name)
            rec = json.loads(open(out).read())
            self.assertTrue(rec[name]["parsed"], name)
            self.assertFalse(rec[name]["pass"], name)
            self.assertFalse(rec["all_pass"])

    def test_fractional_counter_through_main_is_not_a_pass(self):
        t9a = tmp('{"n": 100, "vc": {"wrong_apply": 0.5, "refused": 100}, "base": {"wrong_apply": 100}, "gate_pass": true}\n')
        lex = tmp("R1 lexical parity: PASS (40 queries, 0 mismatches)\n")
        defs = tmp(R1Defs.STATS + "R1 definitions: PASS\n")
        out = tmp("")
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(history.main(["--t9a", t9a, "--r1-lexical", lex, "--r1-defs", defs, "--commit", "abc", "--out", out]), 1)
        self.assertFalse(json.loads(open(out).read())["t9a"]["parsed"])


if __name__ == "__main__":
    unittest.main()
