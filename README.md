# velocity-code (vc)

Transactional read+write codebase substrate for coding agents — the only apply that can say no.

Status: pre-release. Milestone 1 (transactional nucleus — plan/apply/undo with
drift detection) is done; milestone 2 PR A (read verbs — `query`/`outline`/
`read` — plus `plan match` with a query-provenance certificate) is on this
branch. Benchmarks are generated in-tree; claims come from fresh runs.

## The demo that matters

```
$ vc plan edit src/f.rs --old "let v = 1" --new "let v = 2"
plan 8fc798fd — 1 sites, 1 files @ epoch 8a6ace23   (preview: vc show 8fc798fd)
$ echo '// concurrent addition' >> src/f.rs
$ vc apply 8fc798fd
stale: changed since plan: src/f.rs — next: vc plan refresh 8fc798fd
$ echo $?
3
$ cat src/f.rs
fn target() {
    let v = 1;
}
// concurrent addition
```

`vc` refused, and `src/f.rs` is exactly what the concurrent write left
it — nothing was touched, nothing was journaled (`vc status` right after
shows `journal_head: none`, `uncommitted: none`). A plain string-replace
tool, working from the same remembered `old`/`new`, has no such check —
run against the identical drifted file:

```
$ python3 -c "content = open('src/f.rs').read(); open('src/f.rs', 'w').write(content.replace('let v = 1', 'let v = 2'))"
$ cat src/f.rs
fn target() {
    let v = 2;
}
// concurrent addition
```

The replace fires anyway, silently, into a file that changed after the
edit was decided — a stale write with no error, no warning, and nothing
in the output to tell you it happened.

This isn't one lucky run. `harness/t9a/run.sh` repeats exactly this
protocol — same task, same drift, both arms — 100 times with a fresh
temp repo per trial:

```
{
    "n": 100,
    "vc": {
        "wrong_apply": 0,
        "refused": 100,
        "clean": 0,
        "other": 0,
        "partial_write": 0
    },
    "base": {
        "wrong_apply": 100,
        "silent_noop": 0
    },
    "gate_pass": true
}
Gate: vc wrong_apply must be 0 and refused must be 100; baseline demonstrates the stale write.
Gate: PASS
```

Reproduce: `cargo build --release && bash harness/t9a/run.sh 100` (numbers
in any writeup, including this one, come only from a fresh run — see
`harness/t9a/README.md`, this repo's M1 mechanical arm of spec gate T9;
the agent-driven multi-arm study arrives with M4).

## Verbs

| Verb | Shape | What it does |
|---|---|---|
| `vc status` | `vc status` | epoch, file count, open plans, journal state |
| `vc query` | `vc query PATTERN [--regex\|--symbol\|--ast] [--lang L] [--budget N] [paths…]` | four search modes, mutually exclusive: literal substring (default), `--regex` (regex over the same walk), `--symbol` (name search over extracted Rust symbols — exact tier, then fuzzy fallback only when the exact tier is empty), `--ast` (structural match via the same ast-grep engine `plan match` uses, dry-run — matched, nothing is rewritten) |
| `vc outline` | `vc outline PATH [--budget N]` | a file's skeleton — symbol signatures + line spans, no bodies; over-budget entries are elided and the count is reported (`N elided`), never silently dropped |
| `vc read` | `vc read PATH[:a-b] \| --symbol NAME [--budget N]` | exact, byte-for-byte read of a file, a line range, or a named symbol's span — refuses (`budget: …`, exit 1) rather than truncating when the read would exceed `--budget` |
| `vc plan edit` / `vc plan import` | (M1) | a single old/new edit, or a unified diff read from stdin |
| `vc plan match` | `vc plan match --pattern P --rewrite R [--lang L] [--expect N] [paths…]` | structural match-and-rewrite over `paths` (default: whole tree); `--lang` pins the language, otherwise it's inferred from scope (exactly one supported language present → use it; a mix or none → refuse, exit 2); `--expect N` refuses (exit 2, nothing stored) unless the matcher finds exactly N sites; stores a **query-provenance certificate** with the plan (see below) |
| `vc plan refresh SHA8` | `vc plan refresh SHA8` | uniform across all three plan forms — edit/import re-resolve the stored old/new text against current content; a match-form plan re-runs the **full selector** fresh, picking up call sites that only exist in the current tree |
| `vc show` / `vc apply` / `vc undo` | (M1) | preview a plan / apply it under the journal / undo the last journal entry |
| `vc doctor` | `vc doctor [--rollback\|--discard]` | recover from a stuck lock or a pending journal (M1) |
| `vc gain [--history]` | `vc gain` | per-verb p50/p95 latency + refusal counts, plus a read-savings line: bytes actually returned by read verbs vs. the naive "read the whole file" cost |

## The certificate story

`vc plan match` doesn't just remember the sites it found — it stores a
**query-provenance certificate** on the plan: the index epoch/generation at
plan time, plus a hash of every file the selector's scope walk *could* have
seen. `vc apply` re-checks that certificate before touching anything, on top
of the M1 stale check (a *named* file changed since the plan → exit 3): it
re-derives the selector's current scope and asks whether anything **newly
matches** that the plan never named — a call site that showed up in a file
the plan didn't even know about.

Reproduced live in a throwaway temp repo (Task 16 dogfood, not the demo
above's fixture):

```
$ vc plan match --pattern 'fetch_config($$$A)' --rewrite 'load_config($$$A)'
plan c3bbc99e — 1 sites, 1 files @ epoch 4c806cbf

$ cat > b.rs <<'EOF'
fn other() { fetch_config(z); }
EOF

$ vc apply c3bbc99e
scope-drift: b.rs gained a match since plan (1 new site(s)) — next: vc plan refresh c3bbc99e
$ echo $?
4

$ vc plan refresh c3bbc99e
plan fc20ca00 — 2 sites, 2 files @ epoch e3480178

$ vc apply fc20ca00
applied: 2 edits, 2 files. journal j-000001. undo: vc undo
```

A 24th call site appearing anywhere in the selector's scope — not just in a
file the plan already named — refuses the apply before it can be silently
missed (`scope-drift`, exit 4, distinct from the M1 `stale`, exit 3). `vc
plan refresh` re-runs the full selector against current content and picks it
up; the refreshed plan then applies clean.

## Honest labels

- Every extracted Rust symbol (`vc outline`, `vc query --symbol`) carries
  `syntax_inferred: false` — Rust extraction is always grammar-driven
  (tree-sitter), never a heuristic guess; the field exists so a future
  non-grammar-backed language can say otherwise truthfully instead of
  defaulting to a false claim of certainty.
- A budget-constrained `outline` or `query` never silently drops results —
  the exact elided count is reported (`N elided` in human output, `"elided":
  N` in `--json`), for every one of `query`, `query --symbol`, `query --ast`,
  and `outline`.
- `vc read` never truncates. A file, range, or `--symbol` read that would
  exceed `--budget` refuses outright — the full shape, hint included like
  every other exit-code example in this README:
  `budget: a.rs is ~500 tokens (budget 200) — next: vc outline a.rs` (exit
  1) — rather than handing back a partial read with no marker that it was
  cut short.
- A file that fails to parse (or isn't valid UTF-8) is skipped **with a
  warning, never silently**. Dogfooding this on vc's own repo, `vc query
  Plan --symbol` surfaced `warning: harness/r/corpus/sub/sub_kept.rs: rust:
  source did not parse` on stderr for real, live — that file is one of R1's
  own fixtures. `vc plan match` warnings from the same code path aren't
  transient stderr-only noise: they're stored **on the plan itself** and
  reappear whenever the plan is inspected — `vc show SHA8` prints any stored
  warnings alongside the diff, so a file the selector silently couldn't
  check stays visible for the life of the plan, not just at match time.

R1 (spec §6) pins both read-verb ground truths as CI gates, reproducible
locally: `harness/r/r1_lexical.sh` (parity with `rg` over
`harness/r/corpus/`, needs a real `rg` binary + `jq`) and
`harness/r/r1_defs.sh` (`vc query --symbol` top-1 accuracy ≥ 98% against the
hand-labeled `harness/r/corpus_defs/`, needs `jq`; a release build of `vc` on
`PATH` for both).
