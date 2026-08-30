# Provenance of `ground_truth.tsv`

How the R1 definitions ground truth was produced, stated precisely enough
that a reader can judge what the gate does and does not prove. Source: the
Task 10 build report (2026-08-29), plus the collision count re-measured
against the current corpus for this note.

## What the corpus is

`harness/r/corpus_defs/` — four Rust files (`plan.rs`, `query.rs`,
`shapes.rs`, `util.rs`), **written for this gate**, not sampled from real
code. They were authored to cover all ten `SymbolKind` labels (function,
method, struct, enum, trait, impl, const, static, module, typealias) and
to include the shapes that are hard for a name-only symbol search:

- the same method name on two different types (`describe`, `area`,
  `scale`),
- a trait method colliding with its implementations (`rank`),
- an inherent impl whose name equals its struct's (`Circle`, `Square`,
  `Widget`) and a generic one whose name does not (`impl<V> Cache<V>`
  extracts as `Cache<V>`, not bare `Cache`),
- a trait impl (`fmt::Display for Plan`) whose extracted name is
  source-literal, carrying whatever scoping the source wrote,
- a macro-invocation-generated function (`make_getter!(label, …)`), which
  tree-sitter parses as `macro_invocation` and which therefore produces no
  symbol — deliberately given **no** ground-truth row,
- five names that appear only inside a doc comment (`PhantomHelper`,
  `ghost_fn`, `LEGACY_TOKEN`, `retired_widget`, `NotARealTrait`), used as
  negative controls.

## What the rows are

72 rows: **67 positive** (`name<TAB>kind<TAB>path:line`) and **5 negative**
(`name<TAB>NONE`).

Each positive row's expected `path:line` was derived two ways and the two
were required to agree:

1. **Hand-derivation** from `crates/vc-lang/src/rust_symbols.rs`'s actual
   extraction rules — function-vs-method depends on `in_impl` ancestry,
   set by `impl_item` and not by `trait_item`; an `impl_item`'s name is the
   type text, or `"<trait text> for <type>"` for a trait impl; the sort key
   is `(path, start_line)`.
2. **Cross-check against `vc query NAME --symbol --json`** on the built
   corpus.

All 72 agreed on the first run. No `vc` source was changed by that task.

## The honest caveat

Because step 2 compares against `vc`'s own output, these labels are **not
an independent oracle**. What the gate proves is that the extractor's
behaviour matches a hand-derivation of its documented rules and stays that
way — a regression detector, not a validation against an outside source of
truth. Anywhere the hand-derivation and `vc` had disagreed, that
disagreement would have been investigated as a possible extractor bug;
none arose.

## Collision rows: resolved to vc's documented ordering

A bare name cannot disambiguate two same-named symbols — `Symbol.name`
never carries a type or module qualifier. **14 of the 67 positive rows
name a symbol that shares its name with at least one other definition in
the corpus** (re-measured 2026-08-30 by counting exact hits per labeled
name: `Plan`, `new` (5 definitions), `fmt`, `summary`, `QueryHit`,
`SymbolHit`, `rank` (3), `describe`, `Circle`, `Square`, `area` (3),
`scale`, `should_evict`, `Widget`).

For each, the label is the definition `search_symbol`'s documented
`(path, start_line)` ordering puts first — the only answer that can ever
be top-1 for that bare name. The loser is deliberately **not** given a row
of its own: asserting it would be self-contradictory, since a bare-name
query can never return it first.

So for those rows the tie-break is **resolved to `vc`'s documented
ordering**, not derived independently. What they pin is that the ordering
stays deterministic and stays as documented. A change to the sort key
would fail them, which is the point; they are not evidence that this
particular definition is the one a human would have wanted.

## Reproducing

```
cargo build --release && PATH="$PWD/target/release:$PATH" bash harness/r/r1_defs.sh
```

Needs `jq` and a release `vc` on `PATH`. Thresholds (over the positive rows
only): top-1 ≥ 98%, confidently-wrong ≤ 1%; the negative controls are
all-or-nothing.
