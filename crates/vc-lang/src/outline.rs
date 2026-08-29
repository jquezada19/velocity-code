//! Skeleton rendering for `vc outline`.
//!
//! Per top-level [`crate::Symbol`], one line: `{start_line}: {signature} {
//! … {n} lines }` (n = the item's own line span; bodyless items — const,
//! static, type alias, a bodyless trait `fn`; anything whose `signature`
//! ends in `;` — render bare, no brace suffix). Nested items (`impl`/
//! `trait`/`mod` children) indent two spaces per nesting level under their
//! parent.
//!
//! `vc_lang::symbols` returns a *flat* pre-order list (parent pushed before
//! its children are walked — see `rust_symbols::walk`), so this module
//! reconstructs the containment tree itself from each symbol's
//! `start_line`/`end_line` span rather than needing a parent pointer on
//! `Symbol`.

use velocity_code_kernel::{ErrorKind, VcError, VcResult};

use crate::{Symbol, symbols};

/// Languages `outline` knows how to render. Mirrors `vc-query`'s
/// `SYMBOL_LANGS` (Rust only in M2 — Python's `symbols()` stub returns
/// `Ok(vec![])`, which is "nothing found," not "outline this," so it must
/// not be treated as supported here).
const OUTLINE_LANGS: &[&str] = &["rust"];

/// Rough token estimate for a byte count — the same `(bytes + 3) / 4`
/// formula as `vc-query::render::tokens_est`, duplicated here rather than
/// imported: `vc-query` depends on this crate, not the other way around,
/// so importing it would be a cycle.
fn tokens_est(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

/// Render `src`'s skeleton and return `(rendered, elided_count)`. `budget`
/// is a `tokens_est` estimate: when the full render would exceed it, the
/// deepest-nested lines are dropped first, one whole line at a time, never
/// truncating a line's own text, until it fits (or everything nested and
/// top-level alike has been dropped).
pub fn outline(src: &str, lang: &str, budget: Option<usize>) -> VcResult<(String, usize)> {
    if !OUTLINE_LANGS.contains(&lang) {
        return Err(VcError::new(
            ErrorKind::Usage,
            "outline: unsupported language — vc read the file instead",
        ));
    }

    let syms = symbols(src, lang)?;
    let forest = build_forest(&syms);
    let mut lines = Vec::new();
    for node in &forest {
        render_node(node, 0, &mut lines);
    }
    Ok(render_budgeted(&lines, budget))
}

/// One rendered skeleton line plus its nesting depth (0 = top-level),
/// carried alongside the text so budget elision can prioritize by depth
/// without re-parsing the rendered string.
struct Line {
    depth: usize,
    text: String,
}

/// A [`Symbol`] plus the (already-nested) children found inside its line
/// span.
struct Node<'a> {
    symbol: &'a Symbol,
    children: Vec<Node<'a>>,
}

/// Reconstruct the containment tree from `syms`' flat pre-order list: a
/// symbol is a child of the nearest still-open symbol whose line span
/// contains its `start_line`. Pre-order + non-overlapping spans (true for
/// any well-formed AST — a sibling never starts before its predecessor's
/// span ends) makes one linear pass sufficient: `build_siblings` peeks the
/// next symbol and, as long as it still starts within `enclosing_end`,
/// consumes it as a child and recurses to collect *its* children before
/// moving to the next sibling.
fn build_forest(syms: &[Symbol]) -> Vec<Node<'_>> {
    let mut iter = syms.iter().peekable();
    build_siblings(&mut iter, usize::MAX)
}

fn build_siblings<'a>(
    iter: &mut std::iter::Peekable<std::slice::Iter<'a, Symbol>>,
    enclosing_end: usize,
) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    while let Some(&sym) = iter.peek() {
        if sym.start_line > enclosing_end {
            break;
        }
        iter.next();
        let children = build_siblings(iter, sym.end_line);
        out.push(Node {
            symbol: sym,
            children,
        });
    }
    out
}

fn render_node(node: &Node, depth: usize, lines: &mut Vec<Line>) {
    lines.push(Line {
        depth,
        text: render_line(node.symbol, depth),
    });
    for child in &node.children {
        render_node(child, depth + 1, lines);
    }
}

/// `{start_line}: {signature} { … {n} lines }`, `"  "` (two spaces) per
/// nesting `depth` prefixed. `n` is the symbol's own line span
/// (`end_line - start_line + 1`) — the only body-size signal derivable
/// from `Symbol`'s fields, since `signature` is already whitespace-
/// collapsed to one line and carries no body-start-line of its own.
/// Bodyless items — `signature` ending in `;`, per `rust_symbols`'s
/// documented convention for const/static/type-alias and a bodyless trait
/// `fn` — render bare, with no brace suffix, since there is no body to
/// summarize.
fn render_line(sym: &Symbol, depth: usize) -> String {
    let indent = "  ".repeat(depth);
    if sym.signature.ends_with(';') {
        format!("{indent}{}: {}", sym.start_line, sym.signature)
    } else {
        let n = sym.end_line - sym.start_line + 1;
        format!(
            "{indent}{}: {} {{ … {n} lines }}",
            sym.start_line, sym.signature
        )
    }
}

/// `budget: None` renders every line. Otherwise: keep all lines while the
/// joined text's `tokens_est` is within `budget`; past that, drop lines
/// one at a time in priority order — deepest nesting first, and within a
/// depth, the later (document-order) line before the earlier one, so the
/// earliest top-level context survives longest — until it fits (or
/// everything has been dropped). Whole lines only, matching the outline
/// contract's "never truncate mid-line."
fn render_budgeted(lines: &[Line], budget: Option<usize>) -> (String, usize) {
    let join = |kept: &[bool]| -> String {
        lines
            .iter()
            .zip(kept)
            .filter_map(|(l, &k)| k.then_some(l.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let Some(budget) = budget else {
        return (join(&vec![true; lines.len()]), 0);
    };

    let mut kept = vec![true; lines.len()];
    if tokens_est(join(&kept).len()) <= budget {
        return (join(&kept), 0);
    }

    let mut order: Vec<usize> = (0..lines.len()).collect();
    order.sort_by(|&a, &b| lines[b].depth.cmp(&lines[a].depth).then(b.cmp(&a)));

    let mut elided = 0usize;
    for idx in order {
        kept[idx] = false;
        elided += 1;
        if tokens_est(join(&kept).len()) <= budget {
            break;
        }
    }
    (join(&kept), elided)
}

#[cfg(test)]
mod tests {
    use super::*;
    use velocity_code_kernel::ErrorKind;

    /// Task 7's fixture (struct + impl + method + free fn), reused
    /// verbatim so the extracted `Symbol`s' line numbers are pinned facts
    /// already, not re-derived here.
    const SAMPLE: &str = "\n/// doc\npub struct Plan { pub version: u32 }\n\nimpl Plan {\n    pub fn id(&self) -> String { String::new() }\n}\n\nfn free() {}\n";

    #[test]
    fn skeleton_shape_preserves_line_numbers_and_nests_impl_methods() {
        let (text, elided) = outline(SAMPLE, "rust", None).unwrap();
        assert_eq!(elided, 0);
        assert_eq!(
            text,
            "3: pub struct Plan { … 1 lines }\n\
             5: impl Plan { … 3 lines }\n\
             \x20 6: pub fn id(&self) -> String { … 1 lines }\n\
             9: fn free() { … 1 lines }"
        );
    }

    /// Bodyless items (a trait's `;`-terminated method declaration) render
    /// bare — no `{ … N lines }` suffix — while a default-bodied sibling
    /// still gets one; both nest two spaces under the trait.
    #[test]
    fn bodyless_trait_method_renders_bare_default_bodied_gets_brace_suffix() {
        let src = "\ntrait Greeter {\n    fn name(&self) -> String;\n    fn greet(&self) -> String { format!(\"hi {}\", self.name()) }\n}\n";
        let (text, elided) = outline(src, "rust", None).unwrap();
        assert_eq!(elided, 0);
        assert_eq!(
            text,
            "2: trait Greeter { … 4 lines }\n\
             \x20 3: fn name(&self) -> String;\n\
             \x20 4: fn greet(&self) -> String { … 1 lines }"
        );
    }

    /// Budget elision is deepest-first: a budget that fits everything
    /// except the nested method line drops exactly that line (never a
    /// top-level struct/impl/fn line) and reports `elided == 1`.
    #[test]
    fn budget_elides_the_nested_method_before_any_top_level_symbol() {
        let (full, elided_full) = outline(SAMPLE, "rust", None).unwrap();
        assert_eq!(elided_full, 0);

        let without_method = "3: pub struct Plan { … 1 lines }\n\
             5: impl Plan { … 3 lines }\n\
             9: fn free() { … 1 lines }";
        assert!(
            full.contains("pub fn id"),
            "sanity: the full render must contain the method line"
        );

        let budget = without_method.len().div_ceil(4);
        let (budgeted, elided) = outline(SAMPLE, "rust", Some(budget)).unwrap();
        assert_eq!(elided, 1, "exactly the nested method line must be dropped");
        assert_eq!(budgeted, without_method);
        assert!(
            !budgeted.contains("pub fn id"),
            "the nested method must be the one elided, not a top-level symbol"
        );
    }

    #[test]
    fn unsupported_language_is_a_usage_refusal_pointing_at_read() {
        let err = outline("x = 1", "python", None).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Usage);
        assert_eq!(
            err.message,
            "outline: unsupported language — vc read the file instead"
        );
    }
}
