//! Budget-aware rendering of [`QueryHit`]s and [`crate::SymbolHit`]s for
//! CLI/LLM consumption.

use crate::{QueryHit, SymbolHit};
use velocity_code_lang::SymbolKind;

/// Rough token estimate for a byte count: `(bytes + 3) / 4`.
pub fn tokens_est(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

/// Rendered hit text plus the count of hits dropped to stay within budget.
pub struct Budgeted {
    pub text: String,
    pub elided: usize,
}

/// Render `hits` one per line (`{path}:{line}:{col}: {line_text}`), joined
/// by `\n`. With `budget` set, emits whole hits only, stopping before the
/// running `tokens_est` of the rendered text would exceed it; hits dropped
/// this way are counted in `elided`. `budget: None` renders every hit.
pub fn render_hits(hits: &[QueryHit], budget: Option<usize>) -> Budgeted {
    render_budgeted(hits, budget, render_hit_line)
}

/// Same budgeting contract as [`render_hits`], one line per
/// [`SymbolHit`]: `{path}:{start_line}: [{kind}] {signature}` (kind
/// lowercase, e.g. `[method]` — see [`symbol_kind_label`]).
pub fn render_symbol_hits(hits: &[SymbolHit], budget: Option<usize>) -> Budgeted {
    render_budgeted(hits, budget, render_symbol_hit_line)
}

/// Shared budgeting loop behind [`render_hits`] and [`render_symbol_hits`]:
/// greedily includes whole rendered lines (via `line_of`) while the
/// running `tokens_est` stays within `budget`, counting the rest as
/// `elided`. `budget: None` renders every item.
fn render_budgeted<T>(
    items: &[T],
    budget: Option<usize>,
    line_of: impl Fn(&T) -> String,
) -> Budgeted {
    let Some(budget) = budget else {
        let text = items.iter().map(line_of).collect::<Vec<_>>().join("\n");
        return Budgeted { text, elided: 0 };
    };

    let mut text = String::new();
    let mut included = 0usize;
    for item in items {
        let line = line_of(item);
        let candidate_len = if text.is_empty() {
            line.len()
        } else {
            text.len() + 1 + line.len()
        };
        if tokens_est(candidate_len) > budget {
            break;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&line);
        included += 1;
    }
    let elided = items.len() - included;
    Budgeted { text, elided }
}

fn render_hit_line(hit: &QueryHit) -> String {
    format!(
        "{}:{}:{}: {}",
        hit.path.display(),
        hit.line,
        hit.col,
        hit.line_text
    )
}

fn render_symbol_hit_line(hit: &SymbolHit) -> String {
    format!(
        "{}:{}: [{}] {}",
        hit.path.display(),
        hit.symbol.start_line,
        symbol_kind_label(&hit.symbol.kind),
        hit.symbol.signature
    )
}

/// Lowercase label for a [`SymbolKind`], used in both the human hit line
/// (`[method]`) and the `--json` `kind` field.
///
/// An explicit match, not `format!("{kind:?}").to_lowercase()`. The
/// `Debug` derivation gave every variant a label for free, but it made
/// the CLI's public `kind` strings a silent function of internal type
/// names: renaming a variant, or adding one whose lowercased name is not
/// the label we want, would change or invent output with nothing to catch
/// it. Exhaustively matching turns both into compile errors at the exact
/// place the label is decided.
///
/// `TypeAlias` -> `"typealias"` is kept verbatim from the derived
/// behaviour — the labels are already a published contract, and this
/// change is about how they are produced, not what they are.
pub fn symbol_kind_label(kind: &SymbolKind) -> String {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Enum => "enum",
        SymbolKind::Trait => "trait",
        SymbolKind::Impl => "impl",
        SymbolKind::Const => "const",
        SymbolKind::Static => "static",
        SymbolKind::Module => "module",
        SymbolKind::TypeAlias => "typealias",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every label, pinned as the published `--json` `kind` contract — and
    /// as proof the hand-written match reproduces exactly what the
    /// `Debug`-derived version produced, `typealias` (no separator)
    /// included.
    #[test]
    fn symbol_kind_labels_are_stable_lowercase_names() {
        for (kind, label) in [
            (SymbolKind::Function, "function"),
            (SymbolKind::Method, "method"),
            (SymbolKind::Struct, "struct"),
            (SymbolKind::Enum, "enum"),
            (SymbolKind::Trait, "trait"),
            (SymbolKind::Impl, "impl"),
            (SymbolKind::Const, "const"),
            (SymbolKind::Static, "static"),
            (SymbolKind::Module, "module"),
            (SymbolKind::TypeAlias, "typealias"),
        ] {
            assert_eq!(symbol_kind_label(&kind), label, "for {kind:?}");
        }
    }
}
