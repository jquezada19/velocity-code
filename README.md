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
| `vc query` | `vc query PATTERN [--regex\|--symbol\|--ast [--lang L]] [--budget N] [paths…]` | four search modes, mutually exclusive: literal substring (default), `--regex` (regex over the same walk), `--symbol` (name search over extracted Rust symbols — exact tier, then fuzzy fallback only when the exact tier is empty), `--ast` (structural match via the same ast-grep engine `plan match` uses, dry-run — matched, nothing is rewritten). `--lang` is **`--ast`-only** — it names the grammar the structural matcher parses with, and passing it to any other mode refuses (`usage`, exit 2) rather than being silently ignored |
| `vc outline` | `vc outline PATH [--budget N]` | a file's skeleton — symbol signatures + line spans, no bodies; over-budget entries are elided and the count is reported (`N elided`), never silently dropped |
| `vc read` | `vc read PATH[:a-b] \| --symbol NAME [--budget N]` | line-oriented read of a file, a line range, or a named symbol's span — every line returned with a 1-based `{line}: ` prefix. Refuses (`budget: …`, exit 1) rather than truncating when the read would exceed `--budget`, and refuses (`not-found:`) rather than serving a near-miss when `--symbol` matches nothing exactly |
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

A call site the plan never saw — here a second one, in `b.rs`, a file the
plan did not even name — refuses the apply before it can be silently missed
(`scope-drift`, exit 4, distinct from the M1 `stale`, exit 3). `vc plan
refresh` re-runs the full selector against current content and picks it up
(1 site becomes 2); the refreshed plan then applies clean.

The drift check is **best-effort in time, not a lock**: it runs before the
journal lock is taken, so a write landing in the window between the check
and the apply is not caught here. What catches that is the kernel's own
hash gate, which re-verifies every named file's content under the lock
immediately before writing and is the authoritative check. Drift detection
adds refusals for files the kernel cannot see (those outside the plan's
named set); it never authorizes a write the kernel would otherwise refuse.

## Honest labels

- Rust symbol extraction is always grammar-driven (tree-sitter), never a
  heuristic guess. Every extracted symbol carries a `syntax_inferred` flag
  recording that — always `false` for Rust — so a future language that has
  to fall back on heuristics can say so truthfully rather than inherit a
  false claim of certainty. The flag is internal in M2: no verb emits it
  yet, in human output or `--json`. It becomes a per-hit `--json` field
  when there is a language whose answer differs.
- A budget-constrained `outline` or `query` never silently drops results —
  the exact elided count is reported, for every one of `query`, `query
  --symbol`, `query --ast`, and `outline`. Human output ends with a
  `… elided N hits (budget)` line for the query modes and reports `N
  elided` in `outline`'s header; `--json` carries `"elided": N` in all
  four, and its `hits` array is sliced to match, so `hits.length + elided`
  is always the true total.
- A hit's `text` on a very long line is a **window, not the line**: it is
  clamped to 500 bytes centered on the match and marked with a trailing
  `…`. `col` is unaffected — it is the 1-based byte column in the
  **original** line, not an index into `text` — so do not slice `text` by
  `col`.
- **Budget semantics**: `--budget N` is checked against the *content* —
  the rendered hits or the read's line-prefixed text — using a `(bytes +
  3) / 4` token estimate. The `bytes_out` figure `vc gain` accounts
  against is the *whole* rendered output including the `epoch …` header,
  so the two numbers are deliberately different things: one bounds what
  you asked for, the other measures what you were sent.
- `vc read` never truncates. A file, range, or `--symbol` read that would
  exceed `--budget` refuses outright — the full shape, hint included like
  every other exit-code example in this README:
  `budget: a.rs is ~500 tokens (budget 200) — next: vc outline a.rs` (exit
  1) — rather than handing back a partial read with no marker that it was
  cut short.
- `vc read` is **line-oriented, not `cat`**. Every returned line carries a
  1-based `{line}: ` prefix, and lines are split with Rust's `str::lines`,
  which strips a trailing `\r` — so a CRLF file comes back with `\n`
  endings. A final line with no trailing newline is returned intact and
  counted. What `read` guarantees is that the lines you get are exactly
  the lines that are there, not that the byte stream round-trips.
- `vc read --symbol` refuses rather than guessing. `vc query --symbol`
  falls back to a case-insensitive substring tier when nothing matches the
  name exactly, and marks it (`— N hits (fuzzy: no exact match)`, and
  `"fuzzy": true` in `--json`). `read` has no way to mark a body as
  "close enough", so a fuzzy-only result refuses (`not-found:`, exit 1),
  lists the near-misses as `path:line name`, and points at `vc query
  --symbol`.
- A file that fails to parse, isn't valid UTF-8, can't be read, or is
  larger than the 16 MiB content-search limit is skipped **with a warning,
  never silently** — in every search mode, `query` and `query --regex`
  included. The one deliberate exception is **binary files, which are
  skipped silently, exactly as ripgrep does**: a NUL byte in the first
  8 KiB marks a file binary, and a warning per PNG would bury the
  diagnostics that matter. Dogfooding this on vc's own repo, `vc query Plan --symbol`
  surfaced `warning: harness/r/corpus/sub/sub_kept.rs: rust: source did
  not parse` on stderr for real, live — that file is one of R1's own
  fixtures. `vc plan match` warnings from the same code path aren't
  transient stderr-only noise: they're stored **on the plan itself** and
  reappear whenever the plan is inspected — `vc show SHA8` prints any stored
  warnings alongside the diff, so a file the selector couldn't check stays
  visible for the life of the plan, not just at match time.
- An empty pattern is refused, not answered. `vc query ""` — and any
  `--regex` pattern that matches the empty string, like `a*` — has a match
  at every byte position in the tree, which is an out-of-memory shape
  rather than an answer. Both refuse `usage:` (exit 2). A search that
  would produce more than **100,000 hits** refuses the same way (`usage:
  too many hits (>100000)`, exit 2): the hit list is built in full before
  `--budget` can trim it, and silently returning the first N would look
  like a complete answer without being one.
- A hit's reported text is clamped to **500 bytes**, as a window centered
  on the match with a `…` marker — a minified bundle or a single-row CSV
  is one enormous line, and a copy of it per hit is quadratic. The
  reported `line` and `col` are always the true position in the true,
  unclamped line.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | success — including a search that found nothing. "Found nothing" and "the command failed" are different answers and an agent has to be able to tell them apart |
| 1 | a fact about the content: `not-found`, `ambiguous`, `overlap`, `malformed`, `toctou`, `io`, `budget` |
| 2 | `usage` — the invocation itself is wrong |
| 3 | `stale` — a file the plan named changed since the plan was made |
| 4 | `scope-drift` — a file the plan did *not* name now matches the selector |
| 5 | `journal-blocked` — a previous apply left a pending journal entry; the tree is not in a state where a new apply may start. Recover with `vc doctor --rollback` (undo the pending entry) or `vc doctor --discard` |

## .vcignore

`vc` walks the tree with the same `ignore` crate ripgrep uses, honoring
`.gitignore` and `.ignore` by default, and skipping `.git/` and `.vc/`
everywhere. On top of those it reads **`.vcignore`**, registered as a
per-directory ignore file exactly like `.gitignore`: a `.vcignore` in any
directory applies to that subtree, with the same pattern syntax. It is for
paths that belong in the repo but not in `vc`'s attention — vendored
sources, generated code, large fixtures — without touching how git sees
them. Per-machine global gitignore is deliberately NOT honored, so the
walk is identical on every machine.

R1 (spec §6) pins both read-verb ground truths as CI gates, reproducible
locally: `harness/r/r1_lexical.sh` (parity with `rg` over
`harness/r/corpus/`, needs a real `rg` binary + `jq`) and
`harness/r/r1_defs.sh` (`vc query --symbol` top-1 accuracy ≥ 98% against
`harness/r/corpus_defs/` — a hand-authored corpus whose labels were
derived from the extractor's documented rules and cross-checked against
`vc`'s own output, with tie-break rows resolved to `vc`'s documented
`(path, line)` ordering; needs `jq`; a release build of `vc` on `PATH` for
both). Exactly how those labels were produced, and what they therefore do
and do not prove, is `harness/r/r1_defs/PROVENANCE.md`.
