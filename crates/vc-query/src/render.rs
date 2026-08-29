//! Budget-aware rendering of [`QueryHit`]s for CLI/LLM consumption.

use crate::QueryHit;

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
    let Some(budget) = budget else {
        let text = hits
            .iter()
            .map(render_hit_line)
            .collect::<Vec<_>>()
            .join("\n");
        return Budgeted { text, elided: 0 };
    };

    let mut text = String::new();
    let mut included = 0usize;
    for hit in hits {
        let line = render_hit_line(hit);
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
    let elided = hits.len() - included;
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
