#!/usr/bin/env python3
"""Turn one CI run's harness outputs into one JSON line of history.

Reads the three harness outputs a run produces — the T9a results file (its
last line is the summary JSON), the R1 lexical-parity stdout, and the R1
definitions stdout — and appends a single line to a history file:

    {"ts": ..., "commit": ..., "run": ..., "t9a": {...}, "r1_lexical": {...},
     "r1_defs": {...}, "all_pass": bool}

Every harness gets `pass` and `parsed`. A harness whose output could not be
parsed is recorded as `parsed: false, pass: false` rather than dropped, so a
broken gate leaves a mark instead of a gap. The line is ALWAYS appended; the
exit code says whether the run was clean:

    0  every harness parsed and passed
    1  something did not parse or did not pass (line still appended)
    2  usage

The history file is generated, never hand-edited: a number in a writeup comes
from a fresh run, and this file is only the record of those runs.
"""
import argparse
import datetime as _dt
import json
import re
import sys


def _read(path):
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            return fh.read()
    except OSError:
        return None


def parse_t9a(text):
    """Last non-empty line of the results file is the summary object."""
    if text is None:
        return {"parsed": False, "pass": False, "error": "missing file"}
    lines = [ln for ln in text.splitlines() if ln.strip()]
    if not lines:
        return {"parsed": False, "pass": False, "error": "empty file"}
    try:
        s = json.loads(lines[-1])
        n = int(s["n"])
        vc = {k: int(v) for k, v in s["vc"].items()}
        base = {k: int(v) for k, v in s["base"].items()}
        gate = bool(s["gate_pass"])
    except (ValueError, KeyError, TypeError, AttributeError) as e:
        return {"parsed": False, "pass": False, "error": f"summary line: {e}"}
    return {"parsed": True, "pass": gate, "n": n, "vc": vc, "base": base}


_LEX = re.compile(r"^R1 lexical parity: (PASS|FAIL)(?: \((\d+) queries, (\d+) mismatches\))?")


def parse_r1_lexical(text):
    if text is None:
        return {"parsed": False, "pass": False, "error": "missing file"}
    for ln in reversed(text.splitlines()):
        m = _LEX.match(ln.strip())
        if m:
            out = {"parsed": True, "pass": m.group(1) == "PASS"}
            if m.group(2) is not None:
                out["queries"] = int(m.group(2))
                out["mismatches"] = int(m.group(3))
            return out
    return {"parsed": False, "pass": False, "error": "no verdict line"}


_DEFS = re.compile(
    r"^R1 definitions: top-1 (\d+)/(\d+) \(([\d.]+)%\), negative controls (\d+)/(\d+), "
    r"confidently-wrong (\d+)/(\d+) \(([\d.]+)%\)"
)
_DEFS_VERDICT = re.compile(r"^R1 definitions: (PASS|FAIL)")


def parse_r1_defs(text):
    if text is None:
        return {"parsed": False, "pass": False, "error": "missing file"}
    stats = None
    verdict = None
    for ln in text.splitlines():
        s = ln.strip()
        m = _DEFS.match(s)
        if m:
            stats = {
                "top1": int(m.group(1)),
                "n_pos": int(m.group(2)),
                "top1_pct": float(m.group(3)),
                "neg_correct": int(m.group(4)),
                "n_neg": int(m.group(5)),
                "confidently_wrong": int(m.group(6)),
                "wrong_pct": float(m.group(8)),
            }
        v = _DEFS_VERDICT.match(s)
        if v:
            verdict = v.group(1)
    if stats is None or verdict is None:
        return {"parsed": False, "pass": False, "error": "stats or verdict line missing"}
    return {"parsed": True, "pass": verdict == "PASS", **stats}


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--t9a", required=True, help="T9a results-*.jsonl (summary = last line)")
    ap.add_argument("--r1-lexical", required=True, help="captured stdout of r1_lexical.sh")
    ap.add_argument("--r1-defs", required=True, help="captured stdout of r1_defs.sh")
    ap.add_argument("--commit", required=True, help="commit the run measured")
    ap.add_argument("--run", default="", help="CI run id/url, if any")
    ap.add_argument("--out", required=True, help="history.jsonl to append to")
    a = ap.parse_args(argv)  # argparse itself exits 2 on a usage error
    rec = {
        "ts": _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "commit": a.commit,
        "run": a.run,
        "t9a": parse_t9a(_read(a.t9a)),
        "r1_lexical": parse_r1_lexical(_read(a.r1_lexical)),
        "r1_defs": parse_r1_defs(_read(a.r1_defs)),
    }
    rec["all_pass"] = all(rec[k]["parsed"] and rec[k]["pass"] for k in ("t9a", "r1_lexical", "r1_defs"))
    line = json.dumps(rec, sort_keys=True)
    with open(a.out, "a", encoding="utf-8") as fh:
        fh.write(line + "\n")
    print(line)
    return 0 if rec["all_pass"] else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
