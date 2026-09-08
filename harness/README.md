# harness — the numbers, and where they go

Three mechanical harnesses run in CI on every pull request and every push to
`main`: the T9a stale-write gate (`t9a/run.sh`), R1 lexical parity against
ripgrep (`r/r1_lexical.sh`) and R1 definition lookup (`r/r1_defs.sh`). Each
is a real gate — a non-zero exit fails the run.

## Per-run artifact (every CI run)

The `ci` workflow captures each harness's stdout under `harness/out/` and
uploads it together with the T9a `results-*.jsonl` as the run artifact
`harness-outputs`, on failure as well as success. That is the evidence for a
single run; it expires with the artifact retention window.

## Per-commit history (pushes to `main`)

The `metrics` workflow re-runs the three harnesses on every push to `main`
and appends **one JSON line per commit** to `history.jsonl` on the dedicated
`metrics` branch — an orphan branch that holds nothing else, so `main`'s
history stays free of bot commits and no `[skip ci]` tricks are needed. Read
it with:

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

## Rules

- `history.jsonl` is generated. Never hand-edit it; a bad line is fixed by
  a later run, not by editing history.
- Any number quoted anywhere comes from a fresh run of the harness in
  question, after the change being described. The history is the record of
  those runs, not a substitute for running one.
- Read the stream for drift over time, not for one-off deltas between two
  neighbouring lines.
