#!/usr/bin/env python3
"""Process-behaviour (XmR) report over harness history.

Reads `history.jsonl` (one line per metrics run, written by history.py) and,
for each tracked metric, prints a Markdown section with the individual values
in run order, the centre line and the natural process limits, and any
signals. The report is the reading aid for the stream; the stream itself is
the record.

Construction (Wheeler / Graban):

- centre line = mean of the individual values
- mR̄ = mean of successive absolute differences |x[i] - x[i-1]|
- natural process limits = centre ± 2.66 · mR̄   (never the global SD:
  shifts and outliers are the signals wanted and would widen it until an
  unstable stream certified itself stable)
- moving-range upper limit = 3.268 · mR̄

Clamps state what a metric CAN be, not how much it varies: a count cannot go
below 0 and a percentage cannot exceed 100. A clamped limit bounds the
display and Rule 1 (a point beyond a limit) reads the clamped value — a
point can never cross a bound it cannot reach. The two-sigma zone used by
Rule 2 is computed from the UNCLAMPED limits, because only mR̄ may set the
width of a limit.

Signals, each named on the point that completes it:

- rule1  a point beyond a natural process limit
- rule2  two of three successive points beyond two sigma on the same side
- run8   eight successive points on one side of the centre line

Sample size: below MIN_POINTS no limits are computed (the values are listed
as-is); from MIN_POINTS up to PROVISIONAL_BELOW the limits are printed but
labelled provisional; from PROVISIONAL_BELOW they are read as the process
voice. A stream with mR̄ = 0 (a metric that has never moved) has limits
equal to its centre, so its first change is a rule1 signal by construction —
that is the intended reading for gates that are expected to stay at 0 or N.

Lines whose harness did not parse contribute no value to that harness's
metrics (a gap, listed in the section) — a malformed observation must never
move the centre line for every other run's verdict.
"""
import argparse
import json
import sys

MIN_POINTS = 4
PROVISIONAL_BELOW = 8
XMR_LIMIT = 2.66
MR_LIMIT = 3.268

# (label, harness key, path inside the harness object, lower clamp, upper clamp)
METRICS = [
    ("t9a vc.wrong_apply", "t9a", ("vc", "wrong_apply"), 0.0, None),
    ("t9a vc.refused", "t9a", ("vc", "refused"), 0.0, None),
    ("t9a vc.partial_write", "t9a", ("vc", "partial_write"), 0.0, None),
    ("t9a base.wrong_apply", "t9a", ("base", "wrong_apply"), 0.0, None),
    ("r1_lexical mismatches", "r1_lexical", ("mismatches",), 0.0, None),
    ("r1_defs top1_pct", "r1_defs", ("top1_pct",), 0.0, 100.0),
    ("r1_defs wrong_pct", "r1_defs", ("wrong_pct",), 0.0, 100.0),
]


def load_history(path):
    """Return the list of records; raise ValueError on a malformed line."""
    records = []
    with open(path, encoding="utf-8") as fh:
        for lineno, raw in enumerate(fh, 1):
            raw = raw.strip()
            if not raw:
                continue
            try:
                rec = json.loads(raw)
            except json.JSONDecodeError as e:
                raise ValueError(f"{path}:{lineno}: not JSON ({e})") from e
            if not isinstance(rec, dict) or "commit" not in rec:
                raise ValueError(f"{path}:{lineno}: not a history record")
            records.append(rec)
    return records


def series_for(records, harness, path):
    """(points, gaps): points = [(short_commit, value)], gaps = short commits skipped."""
    points, gaps = [], []
    for rec in records:
        short = str(rec.get("commit", ""))[:7]
        h = rec.get(harness)
        if not isinstance(h, dict) or not h.get("parsed"):
            gaps.append(short)
            continue
        node = h
        try:
            for key in path:
                node = node[key]
            value = float(node)
        except (KeyError, TypeError, ValueError):
            gaps.append(short)
            continue
        points.append((short, value))
    return points, gaps


def limits(values, lower=None, upper=None):
    """Centre, mR̄, unclamped (lnpl, unpl), clamped (lnpl, unpl), mR upper limit."""
    n = len(values)
    centre = sum(values) / n
    mrs = [abs(values[i] - values[i - 1]) for i in range(1, n)]
    mr_bar = sum(mrs) / len(mrs) if mrs else 0.0
    lnpl = centre - XMR_LIMIT * mr_bar
    unpl = centre + XMR_LIMIT * mr_bar
    c_lnpl = max(lnpl, lower) if lower is not None else lnpl
    c_unpl = min(unpl, upper) if upper is not None else unpl
    return centre, mr_bar, (lnpl, unpl), (c_lnpl, c_unpl), MR_LIMIT * mr_bar


def signals(values, centre, unclamped, clamped):
    """List of (index, rule) — index is the point that completes the signal."""
    out = []
    lnpl, unpl = unclamped
    c_lnpl, c_unpl = clamped
    sigma = (unpl - centre) / 3.0  # one sigma-equivalent from mR̄
    hi2, lo2 = centre + 2 * sigma, centre - 2 * sigma
    for i, v in enumerate(values):
        if v > c_unpl or v < c_lnpl:
            out.append((i, "rule1"))
    if sigma > 0:
        for i in range(2, len(values)):
            window = values[i - 2 : i + 1]
            if sum(1 for v in window if v > hi2) >= 2 or sum(1 for v in window if v < lo2) >= 2:
                out.append((i, "rule2"))
    for i in range(7, len(values)):
        window = values[i - 7 : i + 1]
        if all(v > centre for v in window) or all(v < centre for v in window):
            out.append((i, "run8"))
    return out


def fmt(x):
    return f"{x:.2f}".rstrip("0").rstrip(".") if isinstance(x, float) else str(x)


def render_metric(label, points, gaps, lower, upper):
    lines = [f"### {label}", ""]
    n = len(points)
    if gaps:
        lines.append(f"- gaps (harness not parsed): {', '.join(gaps)}")
    if n == 0:
        lines += ["- no parsed values yet", ""]
        return lines
    values = [v for _, v in points]
    lines.append(f"- n = {n}; last = {fmt(values[-1])} @ {points[-1][0]}")
    if n < MIN_POINTS:
        lines.append(f"- values: {', '.join(fmt(v) for v in values)}")
        lines.append(f"- limits: not computed (n < {MIN_POINTS})")
        lines.append("")
        return lines
    centre, mr_bar, unclamped, clamped, mr_ul = limits(values, lower, upper)
    tag = " (provisional)" if n < PROVISIONAL_BELOW else ""
    lines.append(f"- centre = {fmt(centre)}; mR̄ = {fmt(mr_bar)}; limits{tag} = [{fmt(clamped[0])}, {fmt(clamped[1])}]; mR upper = {fmt(mr_ul)}")
    if mr_bar == 0:
        lines.append("- mR̄ is 0: the metric has never moved, so its first change will be a rule1 signal")
    sig = signals(values, centre, unclamped, clamped)
    if sig:
        for i, rule in sig:
            lines.append(f"- signal {rule} at {points[i][0]} (value {fmt(values[i])})")
    else:
        lines.append("- no signals")
    lines.append(f"- values: {', '.join(fmt(v) for v in values)}")
    lines.append("")
    return lines


def render(records):
    lines = ["# Harness history — process-behaviour report", "",
             f"{len(records)} run(s). Limits = centre ± {XMR_LIMIT}·mR̄; provisional below n = {PROVISIONAL_BELOW}, not computed below n = {MIN_POINTS}. React to signals, not to neighbouring deltas.", ""]
    for label, harness, path, lower, upper in METRICS:
        points, gaps = series_for(records, harness, path)
        lines += render_metric(label, points, gaps, lower, upper)
    return "\n".join(lines)


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("history", help="history.jsonl written by history.py")
    a = ap.parse_args(argv)
    try:
        records = load_history(a.history)
    except (OSError, ValueError) as e:
        print(f"xmr: {e}", file=sys.stderr)
        return 1
    print(render(records))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
