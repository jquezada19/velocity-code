# harness — the numbers, and where they go

Three mechanical harnesses run in CI on every pull request and every push to
`main`: the T9a stale-write gate (`t9a/run.sh`), R1 lexical parity against
ripgrep (`r/r1_lexical.sh`) and R1 definition lookup (`r/r1_defs.sh`). Each
is a real gate — a non-zero exit fails the run.

## Per-run artifact (every CI run)

The `ci` workflow captures each harness's stdout under `harness/out/` and
uploads whatever is there together with any T9a `results-*.jsonl` as the run
artifact `harness-outputs`, on failure as well as success. It holds the
outputs of the harnesses that actually ran: `ci` stops at the first failing
step, so a T9a failure leaves no R1 output, and a build or test failure
leaves no harness output at all. It is the evidence for a single run and
expires with the artifact retention window. The `metrics` workflow uploads
its own captured outputs the same way (`metrics-harness-outputs`).

## Per-run history (pushes to `main`)

The `metrics` workflow re-runs the three harnesses on each push to `main`
and appends **one JSON line per completed run**, measuring that push's tip
commit, to `history.jsonl` on the dedicated `metrics` branch. Two honest
limits: a push that lands while another is already queued is superseded and
not measured (GitHub keeps one pending run per concurrency group), and a
manual re-run appends a second line for the same commit. The branch is an
orphan that holds nothing else, so `main`'s history stays free of bot
commits and no `[skip ci]` tricks are needed. Read it with:

```bash
git fetch origin metrics
git show origin/metrics:history.jsonl | tail -n 20
```

Line shape (`harness/history.py` writes it; keys sorted):

```json
{"all_pass": true, "commit": "<sha>", "run": "<actions run url>", "ts": "…Z",
 "t9a": {"parsed": true, "pass": true, "n": 100, "vc": {...}, "base": {...}},
 "r1_lexical": {"parsed": true, "pass": true, "queries": N, "mismatches": 0},
 "r1_defs": {"parsed": true, "pass": true, "top1": c, "n_pos": t, "top1_pct": p, "neg_correct": a, "n_neg": b, "confidently_wrong": w, "wrong_pct": q}}
```

A harness whose output could not be parsed is recorded as
`"parsed": false, "pass": false` — a broken gate leaves a mark, never a gap —
and the `metrics` run goes red after the line is appended.

## Reading the stream: `REPORT.md`

`harness/xmr.py` renders `history.jsonl` as a process-behaviour report,
regenerated on the `metrics` branch by every run (`git show
origin/metrics:REPORT.md`). Per metric: the values in run order, the centre
line, the natural process limits (centre ± 2.66·mR̄, never the global SD),
and any signals — `rule1` (a point beyond a limit), `rule2` (two of three
beyond two sigma on one side), `run8` (eight on one side of centre), `mr` (a moving range beyond 3.268·mR̄). Limits
are not computed below 4 points and are labelled provisional below 8. A
count is floored at 0 and a percentage capped at 100 for display only; the
two-sigma zone comes from the unclamped limits, so a clamp never tightens
detection. A metric that has never moved has zero-width limits, so its first
change is a `rule1` signal by construction — the intended reading for gates
expected to sit at 0 or N.

Both tools have unit tests (`harness/test_*.py`), run by `ci` before the
build.

## Rules

- `history.jsonl` is generated. Never hand-edit it; a bad line is fixed by
  a later run, not by editing history.
- Any number quoted anywhere comes from a fresh run of the harness in
  question, after the change being described. The history is the record of
  those runs, not a substitute for running one.
- Read the stream for drift over time, not for one-off deltas between two
  neighbouring lines.
