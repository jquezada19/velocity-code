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

    def test_missing_file(self):
        self.assertFalse(history.parse_t9a(history._read("/nonexistent/x"))["parsed"])


class R1Lexical(unittest.TestCase):
    def test_pass_line_with_counts(self):
        r = history.parse_r1_lexical("R1 lexical parity: PASS (40 queries, 0 mismatches)\n")
        self.assertEqual((r["parsed"], r["pass"], r["queries"], r["mismatches"]), (True, True, 40, 0))

    def test_fail_line_without_counts(self):
        r = history.parse_r1_lexical("R1 MISMATCH [literal] foo\nR1 lexical parity: FAIL — see mismatches above\n")
        self.assertEqual((r["parsed"], r["pass"]), (True, False))

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

    def test_stats_without_verdict(self):
        self.assertFalse(history.parse_r1_defs(self.STATS)["parsed"])


class Main(unittest.TestCase):
    def test_line_always_appended_and_exit_code_reflects_cleanliness(self):
        t9a = tmp('{"n": 100, "vc": {"wrong_apply": 0, "refused": 100}, "base": {"wrong_apply": 100}, "gate_pass": true}\n')
        lex_ok = tmp("R1 lexical parity: PASS (40 queries, 0 mismatches)\n")
        lex_bad = tmp("no verdict here\n")
        defs = tmp(R1Defs.STATS + "R1 definitions: PASS\n")
        out = tmp("")
        self.assertEqual(history.main(["--t9a", t9a, "--r1-lexical", lex_ok, "--r1-defs", defs, "--commit", "abc", "--out", out]), 0)
        self.assertEqual(history.main(["--t9a", t9a, "--r1-lexical", lex_bad, "--r1-defs", defs, "--commit", "abc", "--out", out]), 1)
        lines = [json.loads(l) for l in open(out)]
        self.assertEqual([l["all_pass"] for l in lines], [True, False])
        self.assertFalse(lines[1]["r1_lexical"]["parsed"])


if __name__ == "__main__":
    unittest.main()
