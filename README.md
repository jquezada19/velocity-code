# velocity-code (vc)

Transactional read+write codebase substrate for coding agents — the only apply that can say no.

Status: pre-release, milestone 1 (transactional nucleus) in progress. Benchmarks are generated in-tree; claims come from fresh runs.

## The demo that matters

```
$ vc plan edit src/f.rs --old "let v = 1" --new "let v = 2"
plan 2694379f — 1 sites, 1 files @ epoch 8a6ace23   (preview: vc show 2694379f)
$ echo '// concurrent addition' >> src/f.rs
$ vc apply 2694379f
stale: changed since plan: src/f.rs — next: vc plan --refresh 2694379f
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
$ python3 -c "open('src/f.rs','w').write(open('src/f.rs').read().replace('let v = 1','let v = 2'))"
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
