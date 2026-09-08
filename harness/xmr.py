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
- mr     a moving range beyond its own upper limit (3.268 · mR̄) — the
         moving-range chart is the other half of XmR

Sample size: below MIN_POINTS no limits are computed (the values are listed
as-is); from MIN_POINTS up to PROVISIONAL_BELOW the limits are printed but
labelled provisional; from PROVISIONAL_BELOW they are read as the process
voice. Limits are recomputed over the whole stream on every run, so a signal
is a retrospective reading of the stream as it stands, not a frozen verdict:
a change after a long flat run is flagged (the flat history keeps mR̄ near
zero), a change after a short one may not be, and a later, larger move can
absorb an earlier signal. A fixed-baseline policy would freeze limits; it is
not implemented.

Rule 1 reads the clamped limits: a valid value can never lie beyond a clamp,
so on valid data the clamped and unclamped readings agree, and the clamp
only removes a limit the data could never test. Values outside the metric's
domain (a negative count, a percentage above 100), non-finite numbers,
booleans and non-numbers are malformed data, not observations — the report
refuses with the offending record named, and exits non-zero.

Runs whose harness did not parse, or whose record lacks the metric, are
gaps: they contribute no value and they break adjacency. Moving ranges are
taken only between runs that are consecutive in the stream, and the
windowed rules (rule2, run8) never span a gap — a missing run must neither
move the centre line nor manufacture a neighbour.
"""
import argparse
import json
import math
import sys

MIN_POINTS = 4
PROVISIONAL_BELOW = 8
XMR_LIMIT = 2.66
MR_LIMIT = 3.268

# (label, harness key, path inside the harness object, kind)
# kind "count": integral, floored at 0.  kind "pct": within [0, 100].
METRICS = [
    ("t9a vc.wrong_apply", "t9a", ("vc", "wrong_apply"), "count"),
    ("t9a vc.refused", "t9a", ("vc", "refused"), "count"),
    ("t9a vc.partial_write", "t9a", ("vc", "partial_write"), "count"),
    ("t9a base.wrong_apply", "t9a", ("base", "wrong_apply"), "count"),
    ("r1_lexical mismatches", "r1_lexical", ("mismatches",), "count"),
    ("r1_defs top1_pct", "r1_defs", ("top1_pct",), "pct"),
    ("r1_defs wrong_pct", "r1_defs", ("wrong_pct",), "pct"),
]
KINDS = {"count": (0.0, None, True), "pct": (0.0, 100.0, False)}  # lower, upper, integral


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


def series_for(records, harness, path, kind="count"):
    """(points, gaps): points = [(position, short_commit, value)] in stream order,
    gaps = short commits with no usable value. A gap is a harness whose
    `parsed` is False or a metric key that is absent. Anything else that is
    present but malformed — a non-dict harness or container, a non-boolean
    `parsed`, a value that is not a finite number of the metric's kind inside
    its domain — raises ValueError naming the record."""
    lower, upper, integral = KINDS[kind]
    points, gaps = [], []
    for pos, rec in enumerate(records):
        short = str(rec.get("commit", ""))[:7]
        where = f"record {pos + 1} ({short}): {harness}.{'.'.join(path)}"
        if harness not in rec:
            gaps.append(short)
            continue
        h = rec[harness]
        if not isinstance(h, dict) or not isinstance(h.get("parsed"), bool):
            raise ValueError(f"{where}: harness object malformed (need a dict with a boolean 'parsed')")
        if not h["parsed"]:
            gaps.append(short)
            continue
        node = h
        missing = False
        for key in path:
            if not isinstance(node, dict):
                raise ValueError(f"{where}: container is not an object: {node!r}")
            if key not in node:
                missing = True
                break
            node = node[key]
        if missing:
            gaps.append(short)
            continue
        if isinstance(node, bool) or not isinstance(node, (int, float)):
            raise ValueError(f"{where}: not a number: {node!r}")
        value = float(node)
        if math.isnan(value) or math.isinf(value):
            raise ValueError(f"{where}: not finite: {node!r}")
        if (lower is not None and value < lower) or (upper is not None and value > upper):
            raise ValueError(f"{where}: {node!r} is outside [{lower}, {upper}]")
        if integral and value != int(value):
            raise ValueError(f"{where}: a count must be integral: {node!r}")
        if integral and abs(node) > 2**53:
            raise ValueError(f"{where}: a count beyond 2**53 is not exact as a float: {node!r}")
        points.append((pos, short, value))
    return points, gaps


def moving_ranges(values, positions):
    """|x[i] - x[i-1]| for pairs that are consecutive in the stream; None where a gap sits between."""
    out = []
    for i in range(1, len(values)):
        out.append(abs(values[i] - values[i - 1]) if positions[i] == positions[i - 1] + 1 else None)
    return out


def limits(values, positions, lower=None, upper=None):
    """Centre, mR̄, unclamped (lnpl, unpl), clamped (lnpl, unpl), mR upper limit.
    Returns None when no two runs are consecutive (mR̄ undefined)."""
    n = len(values)
    centre = sum(values) / n
    mrs = [m for m in moving_ranges(values, positions) if m is not None]
    if not mrs:
        return None
    mr_bar = sum(mrs) / len(mrs)
    lnpl = centre - XMR_LIMIT * mr_bar
    unpl = centre + XMR_LIMIT * mr_bar
    mr_ul = MR_LIMIT * mr_bar
    if not all(math.isfinite(x) for x in (centre, mr_bar, lnpl, unpl, mr_ul)):
        raise ValueError("limits are not representable (arithmetic overflow on the values)")
    c_lnpl = max(lnpl, lower) if lower is not None else lnpl
    c_unpl = min(unpl, upper) if upper is not None else unpl
    return centre, mr_bar, (lnpl, unpl), (c_lnpl, c_unpl), mr_ul


def _contiguous(positions, i, width):
    """True when the `width` points ending at i are consecutive runs in the stream."""
    return i - width + 1 >= 0 and positions[i] - positions[i - width + 1] == width - 1


def signals(values, positions, centre, unclamped, clamped, mr_ul):
    """List of (index, rule) — index is the point that completes the signal."""
    out = []
    lnpl, unpl = unclamped
    c_lnpl, c_unpl = clamped
    sigma = (unpl - centre) / 3.0  # one sigma-equivalent from mR̄
    hi2, lo2 = centre + 2 * sigma, centre - 2 * sigma
    mrs = moving_ranges(values, positions)
    for i, v in enumerate(values):
        if v > c_unpl or v < c_lnpl:
            out.append((i, "rule1"))
    if mr_ul > 0:
        for i, m in enumerate(mrs, start=1):
            if m is not None and m > mr_ul:
                out.append((i, "mr"))
    for i in range(2, len(values)):  # strict comparisons: a constant series never fires
        if not _contiguous(positions, i, 3):
            continue
        window = values[i - 2 : i + 1]
        if sum(1 for v in window if v > hi2) >= 2 or sum(1 for v in window if v < lo2) >= 2:
            out.append((i, "rule2"))
    for i in range(7, len(values)):
        if not _contiguous(positions, i, 8):
            continue
        window = values[i - 7 : i + 1]
        if all(v > centre for v in window) or all(v < centre for v in window):
            out.append((i, "run8"))
    return out


def fmt(x):
    return f"{x:.2f}".rstrip("0").rstrip(".") if isinstance(x, float) else str(x)


def render_metric(label, points, gaps):
    lines = [f"### {label}", ""]
    n = len(points)
    if gaps:
        lines.append(f"- gaps (harness not parsed, or metric absent): {', '.join(gaps)}")
    if n == 0:
        lines += ["- no parsed values yet", ""]
        return lines
    positions = [p for p, _, _ in points]
    values = [v for _, _, v in points]
    lines.append(f"- n = {n}; last = {fmt(values[-1])} @ {points[-1][1]}")
    if n < MIN_POINTS:
        lines.append(f"- values: {', '.join(fmt(v) for v in values)}")
        lines.append(f"- limits: not computed (n < {MIN_POINTS})")
        lines.append("")
        return lines
    lower, upper, _ = KINDS[_kind_for(label)]
    lim = limits(values, positions, lower, upper)
    if lim is None:
        lines.append(f"- values: {', '.join(fmt(v) for v in values)}")
        lines.append("- limits: not computed (no two consecutive runs; mR̄ undefined)")
        lines.append("")
        return lines
    centre, mr_bar, unclamped, clamped, mr_ul = lim
    tag = " (provisional)" if n < PROVISIONAL_BELOW else ""
    lines.append(f"- centre = {fmt(centre)}; mR̄ = {fmt(mr_bar)}; limits{tag} = [{fmt(clamped[0])}, {fmt(clamped[1])}]; mR upper = {fmt(mr_ul)}")
    if mr_bar == 0:
        lines.append("- mR̄ is 0 so far: the limits have no width; a later change may read as rule1, depending on the limits recomputed with it")
    sig = signals(values, positions, centre, unclamped, clamped, mr_ul)
    if sig:
        for i, rule in sig:
            lines.append(f"- signal {rule} at {points[i][1]} (value {fmt(values[i])})")
    else:
        lines.append("- no signals")
    lines.append(f"- values: {', '.join(fmt(v) for v in values)}")
    lines.append("")
    return lines


def _kind_for(label):
    for lab, _h, _p, kind in METRICS:
        if lab == label:
            return kind
    raise KeyError(label)


def render(records):
    lines = ["# Harness history — process-behaviour report", "",
             f"{len(records)} run(s). Limits = centre ± {XMR_LIMIT}·mR̄; provisional below n = {PROVISIONAL_BELOW}, not computed below n = {MIN_POINTS}. React to signals, not to neighbouring deltas.", ""]
    for label, harness, path, kind in METRICS:
        points, gaps = series_for(records, harness, path, kind)
        lines += render_metric(label, points, gaps)
    return "\n".join(lines)


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("history", help="history.jsonl written by history.py")
    a = ap.parse_args(argv)
    try:
        records = load_history(a.history)
        report = render(records)
    except (OSError, ValueError) as e:
        print(f"xmr: {e}", file=sys.stderr)
        return 1
    print(report)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
