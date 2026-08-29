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
/// (`[method]`) and the `--json` `kind` field. A straight
/// `Debug`-then-lowercase of the enum name (`Function` -> `function`,
/// `TypeAlias` -> `typealias`) — deterministic and exhaustive-by-construction
/// (a new `SymbolKind` variant picks up a label for free, no match arm to
/// forget), at the cost of no separator in the multi-word variant.
pub fn symbol_kind_label(kind: &SymbolKind) -> String {
    format!("{kind:?}").to_lowercase()
}
