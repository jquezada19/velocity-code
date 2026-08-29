# T9a — stale-write injection micro-harness

`run.sh` is the M1 **mechanical arm** of spec gate T9 (design doc §7.3,
§8): a scripted, two-arm comparison of `vc` against a naive str-replace
baseline under an injected concurrent-write race. It answers one narrow
question — *given a file that changed between "decide the edit" and
"apply the edit," does the tool notice?* — with no agent, no model, and
no judgment call anywhere in the loop, so the result is a deterministic
property of the kernel, not a sample from an agent's behavior.

## What it measures

For `N` trials (default 100), both arms run the identical protocol:

1. Create the same one-line Rust file.
2. Record the same intended edit (`let v = 1` → `let v = 2`).
3. Inject the same concurrent-mutation **drift**: append a marker line
   (`// concurrent addition`) to the file. The file changes, but the
   text the edit is looking for (`let v = 1`) is untouched and still
   present — the drift is deliberately *undetectable by re-matching the
   needle*, only by noticing the file as a whole changed.
4. Attempt the edit against the now-drifted file.
5. Score the outcome.

The two arms differ only in *when* they last looked at the file relative
to the drift:

- **baseline** (plain Python `str.replace`) has no notion of "since" — it
  reads the file, replaces the substring, writes it back, all in one
  shot, *after* the drift has already landed. The replace fires. The
  edit lands inside content that changed since anyone decided to make
  it. That is a **stale write**, and it happens silently — nothing about
  the baseline's output distinguishes this from a normal, safe edit.
- **vc** plans *before* the drift (`vc plan edit` snapshots the file's
  whole-file blake3 hash at that moment) and applies *after* it. `apply`
  re-hashes every file the plan touches against fresh on-disk state —
  never a stat-cache, never the bytes it remembered — and refuses on any
  mismatch, whole-file, not just the touched region. So even though the
  drift never touches the matched text, the file's overall hash no
  longer matches what the plan recorded, and `apply` exits 3 (`stale`)
  without writing anything.

Why an append, specifically: an earlier draft of this script had the
drift *replace* the needle (`let v = 1` → `let v = 10`). Whether the
*baseline's* replace then fires or silently no-ops turns out to hinge on
a numeric coincidence, not on anything the harness means to test:
`"let v = 1"` is a literal prefix of `"let v = 10"`, so the replace still
fires — just mangled, into `let v = 20` — while a drifted value like `99`
shares no such prefix and would genuinely no-op. Either way, the
demonstration's outcome would be at the mercy of which numbers happened
to be picked, not a property of the mechanism under test. An append-only
drift removes that coincidence entirely: the needle survives
*unconditionally*, so detection can only happen by noticing the file as a
whole changed (a content hash) — the same property `vc`'s staleness check
relies on, and the only honest way to provoke the naive tool's real
failure mode.

## The gate

```
vc.wrong_apply  == 0   (never silently applies an edit computed against stale content)
vc.refused      == N   (every trial correctly detected the drift and refused, exit 3, kind "stale")
vc.other        == 0   (no refusal of any other kind — a non-zero value here would mean a
                         different bug, not the one this harness is designed to provoke)
vc.partial_write == 0  (a refusal must leave the file byte-identical to what the drift left —
                         checked directly, not inferred: script snapshots the file right after
                         the drift and `cmp`s it against the post-refusal file)
base.wrong_apply == N  (the baseline demonstrates the failure this harness exists to contrast against)
```

`run.sh` asserts this itself: it prints a `Gate: PASS` / `Gate: FAIL` line
and exits 0/1 accordingly, so it can be wired into CI as a real gate, not
just a number generator. **A `Gate: FAIL` is an M1 kill-criterion
investigation (design doc §9: "any T9 gate fails → stop, the bet was
wrong"), not a script problem to paper over** — the drift, the two arms,
and the scoring are intentionally the simplest thing that could work, so
a failure here almost certainly means a real regression in `vc plan`/
`vc apply`'s staleness detection, not a harness bug.

This also instantiates the pre-registered T9 gate from design doc §8
(`wrong-apply ≤ 1% for vc vs ≥ 10% best baseline`) at its two extremes —
0% and 100% — which is what a *mechanical, non-adversarial* injection is
expected to produce. It is not, by itself, the full statistically-powered
T9 study.

## This is the M1 slice, not the whole of T9

The design doc (§7.3) describes `harness/t9/` as a five-arm study —
`{str-replace, checked-write (git apply --3way), Serena, ast-grep-MCP,
vc}` — run by real coding agents against real tasks. That is a
milestone-4 deliverable (§9: "M4 — MCP + stage-2 agent study"): it needs
the MCP server, frozen agent configs, and task manifests that don't exist
yet in M1.

`harness/t9a/` (this directory) is the **mechanical precursor**: it
proves the kernel mechanism the agent-driven study will depend on —
hash-gated staleness detection — deterministically and without any of
the variance an agent introduces, so M1 can gate on it today. When the
M4 agent study lands, it supersedes this harness as the source of the
headline T9 number; this harness keeps its value as a fast, deterministic
regression check on the one property it isolates.

## Running it

```bash
cargo build --release
bash harness/t9a/run.sh [N]   # N defaults to 100
```

Requires the release `vc` binary (built above) and `python3` on `PATH`;
the script checks both up front and fails fast with a clear message if
either is missing.

## Output

- `harness/t9a/results-<UTC-ish local timestamp>.jsonl` — one JSON line
  per trial-arm attempt (`{"trial", "arm", "outcome", ...}` — auditable
  without re-running), then one final summary line
  (`{"n", "vc": {...}, "base": {...}, "gate_pass"}`). Generated, not
  committed — `.gitignore` excludes `harness/t9a/results-*.jsonl`.
- The same summary object, pretty-printed, on stdout, followed by the
  gate line.

## Numbers in any writeup come only from a fresh run

Per design doc §7.4's reporting rule, any T9a number quoted in this
repo's README, a PR description, or elsewhere is only ever the output of
running `bash harness/t9a/run.sh 100` fresh, after the change being
described. Nothing here is precomputed or hand-edited into prose —
reproduce it yourself with the command above.
