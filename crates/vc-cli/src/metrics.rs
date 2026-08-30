//! Local, telemetry-free JSONL metrics (spec §7.1/§7.2): one line per CLI
//! invocation under `.vc/metrics/<YYYY-MM-DD>.jsonl`, and `vc gain`'s
//! aggregation over that stream. Nothing here is ever transmitted; nothing
//! here is allowed to make a command fail — [`record`] swallows every I/O
//! error, because this is observability, not correctness (the same
//! posture `apply`'s own post-commit index-refresh warning takes).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;

fn metrics_dir(root: &Path) -> std::path::PathBuf {
    root.join(".vc/metrics")
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch
/// (1970-01-01) -> (year, month, day). Public-domain algorithm
/// (http://howardhinnant.github.io/date_algorithms.html), ported directly
/// so metrics filenames don't need a chrono/time dependency for one date
/// computation per write.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

fn date_string(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

#[derive(serde::Serialize)]
struct MetricLine<'a> {
    ts: u64,
    verb: &'a str,
    ms: u64,
    files: usize,
    edits: usize,
    refusal: Option<&'a str>,
    epoch8: &'a str,
    version: &'a str,
    /// Read-side gain accounting (Task 15, spec §7.2), populated only for
    /// the read verbs (`query`/`outline`/`read`) — every other verb
    /// passes `None`, and `skip_serializing_if` omits the key entirely so
    /// an old-format reader (or a human skimming the JSONL) never sees a
    /// meaningless `null`/`0` on a write/status/plan line.
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes_out: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    naive_bytes: Option<u64>,
}

/// One invocation's worth of metrics, as a named struct rather than a
/// nine-argument positional list.
///
/// Every field but `verb` is a scalar, and four of them are numbers of
/// the same two types — so a transposed pair (`files`/`edits`,
/// `bytes_out`/`naive_bytes`) type-checks perfectly and silently corrupts
/// the metric it lands in. Naming them at the call site is what makes
/// that a visible mistake instead of an invisible one, and it retires the
/// three `#[allow(clippy::too_many_arguments)]` suppressions that were
/// standing in for this.
pub struct MetricEvent<'a> {
    pub verb: &'a str,
    pub ms: u64,
    pub files: usize,
    pub edits: usize,
    pub refusal: Option<&'a str>,
    pub epoch8: &'a str,
    /// Read-side gain accounting (Task 15) — `Some` only for the read
    /// verbs (`query`/`outline`/`read`), from `CmdOutcome::bytes_out`.
    pub bytes_out: Option<u64>,
    /// The counterfactual half of the same accounting, from
    /// `CmdOutcome::naive_bytes`. `Some` only alongside `bytes_out`.
    pub naive_bytes: Option<u64>,
}

/// Append one metrics line for this invocation. Never fails outward: any
/// I/O error (unwritable `.vc/`, full disk, ...) is silently dropped —
/// see the module doc comment.
pub fn record(root: &Path, event: &MetricEvent<'_>) {
    let _ = try_record(root, event);
}

fn try_record(root: &Path, event: &MetricEvent<'_>) -> std::io::Result<()> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = metrics_dir(root);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.jsonl", date_string(ts)));
    let line = MetricLine {
        ts,
        verb: event.verb,
        ms: event.ms,
        files: event.files,
        edits: event.edits,
        refusal: event.refusal,
        epoch8: event.epoch8,
        version: env!("CARGO_PKG_VERSION"),
        bytes_out: event.bytes_out,
        naive_bytes: event.naive_bytes,
    };
    let json = serde_json::to_string(&line).map_err(std::io::Error::other)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{json}")
}

/// Only the fields `gain` actually aggregates — unknown JSON keys (`ts`,
/// `files`, `edits`, `epoch8`, `version`) are ignored by serde rather than
/// modeled and left unread, so nothing here can go dead-code-stale.
/// `bytes_out`/`naive_bytes` are `#[serde(default)]` (Task 15): an
/// OLD-format line written before this task simply lacks the keys, and
/// must still parse — not error out, not degrade the rest of the line —
/// same posture `ms`/`refusal` already take for a line missing THOSE
/// keys.
#[derive(serde::Deserialize)]
struct MetricRecord {
    verb: String,
    #[serde(default)]
    ms: u64,
    #[serde(default)]
    refusal: Option<String>,
    #[serde(default)]
    bytes_out: Option<u64>,
    #[serde(default)]
    naive_bytes: Option<u64>,
}

#[derive(Debug, Default)]
pub struct VerbStats {
    pub count: usize,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub refusals: BTreeMap<String, usize>,
}

/// Read-side gain accounting (Task 15, spec §7.2's counterfactual): what
/// the read verbs (`query`/`outline`/`read`) actually cost the caller
/// (`bytes_out`, summed) versus what reading the same files naively would
/// have cost (`naive`, summed) — over every metrics line carrying BOTH
/// fields, old-format lines and non-read-verb lines (which never set
/// either field) contributing nothing. `saved` is the sum of PER-LINE
/// `naive_bytes.saturating_sub(bytes_out)` — a line where `bytes_out`
/// exceeds `naive_bytes` (pathological, but not impossible) clamps to a
/// `0` delta for that line alone, never going negative and eating into
/// another line's real savings.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReadSavings {
    pub saved: u64,
    pub naive: u64,
    pub calls: usize,
}

impl ReadSavings {
    /// Percent of `naive` that `saved` recovers, floored to an integer —
    /// `0` (not NaN/a panic) when `naive` is `0` (no qualifying lines
    /// yet).
    pub fn pct(&self) -> u64 {
        self.saved
            .saturating_mul(100)
            .checked_div(self.naive)
            .unwrap_or(0)
    }
}

#[derive(Debug, Default)]
pub struct GainReport {
    pub verbs: BTreeMap<String, VerbStats>,
    pub read_savings: ReadSavings,
    /// Per-day invocation totals, across every verb. `Some` (possibly
    /// empty) only when `--history` was requested.
    pub history: Option<BTreeMap<String, usize>>,
}

/// Nearest-rank percentile over an ascending-sorted slice: rank =
/// ceil(p/100 * n), 1-indexed, clamped into range. `sorted` must already
/// be sorted ascending; empty input returns 0.
fn percentile_nearest_rank(sorted: &[u64], p: u64) -> u64 {
    let n = sorted.len() as u64;
    if n == 0 {
        return 0;
    }
    let rank = (p * n).div_ceil(100).max(1);
    sorted[(rank - 1).min(n - 1) as usize]
}

/// Read every `.vc/metrics/*.jsonl` file and aggregate per verb: count,
/// p50/p95 latency (nearest-rank), and refusal counts by kind. A missing
/// `.vc/metrics/` directory (nothing has ever run) or any unreadable/
/// malformed line degrades to "not counted," never an error — `gain` must
/// always have an answer.
pub fn aggregate(root: &Path, with_history: bool) -> GainReport {
    let dir = metrics_dir(root);
    let mut per_verb_ms: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut per_verb_refusals: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut history: BTreeMap<String, usize> = BTreeMap::new();
    let mut read_savings = ReadSavings::default();

    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let date = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(rec) = serde_json::from_str::<MetricRecord>(line) else {
                    continue;
                };
                // Read-side gain (Task 15): only a line carrying BOTH
                // fields contributes — an old-format line (`#[serde(default)]`
                // leaves both `None`) or a non-read-verb line (which never
                // sets either) is silently excluded, exactly the spec's "over
                // lines having both fields" rule. Each line's own delta is
                // clamped to 0 before summing (`saturating_sub`), so one
                // pathological line can't eat into another's real savings.
                if let (Some(bytes_out), Some(naive_bytes)) = (rec.bytes_out, rec.naive_bytes) {
                    read_savings.saved += naive_bytes.saturating_sub(bytes_out);
                    read_savings.naive += naive_bytes;
                    read_savings.calls += 1;
                }
                per_verb_ms
                    .entry(rec.verb.clone())
                    .or_default()
                    .push(rec.ms);
                if let Some(kind) = rec.refusal {
                    *per_verb_refusals
                        .entry(rec.verb)
                        .or_default()
                        .entry(kind)
                        .or_insert(0) += 1;
                }
                if with_history {
                    *history.entry(date.clone()).or_insert(0) += 1;
                }
            }
        }
    }

    let mut verbs = BTreeMap::new();
    for (verb, mut times) in per_verb_ms {
        times.sort_unstable();
        let refusals = per_verb_refusals.remove(&verb).unwrap_or_default();
        verbs.insert(
            verb,
            VerbStats {
                count: times.len(),
                p50_ms: percentile_nearest_rank(&times, 50),
                p95_ms: percentile_nearest_rank(&times, 95),
                refusals,
            },
        );
    }

    GainReport {
        verbs,
        read_savings,
        history: with_history.then_some(history),
    }
}

/// Small fixed-width table, one row per verb, plus a refusal-kind
/// breakdown line for any verb that had refusals, the Task 15 read-side
/// gain counterfactual (§7.2) as a single `read savings: ...` line
/// (superseding the M1 "reads: n/a until M2" placeholder now that the
/// read verbs actually record `bytes_out`/`naive_bytes`), and — when
/// `--history` was passed — per-day totals.
pub fn format_human(report: &GainReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if report.verbs.is_empty() {
        let _ = writeln!(out, "(no metrics recorded yet)");
    } else {
        let _ = writeln!(out, "verb        count   p50ms   p95ms  refusals");
        for (verb, s) in &report.verbs {
            let refusal_total: usize = s.refusals.values().sum();
            let _ = writeln!(
                out,
                "{verb:<10}  {:>5}  {:>6}  {:>6}  {:>8}",
                s.count, s.p50_ms, s.p95_ms, refusal_total
            );
            if !s.refusals.is_empty() {
                let kinds: Vec<String> =
                    s.refusals.iter().map(|(k, v)| format!("{k}={v}")).collect();
                let _ = writeln!(out, "  refusal kinds: {}", kinds.join(", "));
            }
        }
    }
    let _ = writeln!(
        out,
        "read savings: {} bytes ({}%) across {} read-verb calls",
        report.read_savings.saved,
        report.read_savings.pct(),
        report.read_savings.calls
    );
    if let Some(hist) = &report.history {
        let _ = writeln!(out, "\nhistory (per-day totals):");
        if hist.is_empty() {
            let _ = writeln!(out, "  (none)");
        } else {
            for (date, count) in hist {
                let _ = writeln!(out, "  {date}: {count}");
            }
        }
    }
    out
}

pub fn to_json(report: &GainReport) -> serde_json::Value {
    let verbs: serde_json::Map<String, serde_json::Value> = report
        .verbs
        .iter()
        .map(|(verb, s)| {
            (
                verb.clone(),
                serde_json::json!({
                    "count": s.count,
                    "p50_ms": s.p50_ms,
                    "p95_ms": s.p95_ms,
                    "refusals": s.refusals,
                }),
            )
        })
        .collect();
    let mut obj = serde_json::json!({
        "verbs": verbs,
        "read_savings": {
            "saved": report.read_savings.saved,
            "naive": report.read_savings.naive,
            "pct": report.read_savings.pct(),
            "calls": report.read_savings.calls,
        },
    });
    if let Some(hist) = &report.history {
        obj["history"] = serde_json::json!(hist);
    }
    obj
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Positional shorthand for the tests only: production call sites use
    /// the named `MetricEvent` fields, which is the point of the struct.
    #[allow(clippy::too_many_arguments)]
    fn ev<'a>(
        verb: &'a str,
        ms: u64,
        files: usize,
        edits: usize,
        refusal: Option<&'a str>,
        epoch8: &'a str,
        bytes_out: Option<u64>,
        naive_bytes: Option<u64>,
    ) -> MetricEvent<'a> {
        MetricEvent {
            verb,
            ms,
            files,
            edits,
            refusal,
            epoch8,
            bytes_out,
            naive_bytes,
        }
    }

    #[test]
    fn civil_from_days_matches_known_anchors() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-08-28: cross-checked against `date -u -j -f "%Y-%m-%d
        // %H:%M:%S" "2026-08-28 00:00:00" +%s` at authoring time.
        let ts_2026_08_28: u64 = 1_787_875_200;
        assert_eq!(date_string(ts_2026_08_28), "2026-08-28");
        // 2000-03-01 (a post-leap-day date, the algorithm's classic edge
        // case) is day 11017 since epoch.
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        // 1969-12-31, one day before epoch (negative-days branch).
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn percentile_nearest_rank_examples() {
        let v = [1u64, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        assert_eq!(percentile_nearest_rank(&v, 50), 5);
        assert_eq!(percentile_nearest_rank(&v, 95), 10);
        assert_eq!(percentile_nearest_rank(&[], 50), 0);
        assert_eq!(percentile_nearest_rank(&[42], 50), 42);
        assert_eq!(percentile_nearest_rank(&[42], 95), 42);
    }

    #[test]
    fn record_then_aggregate_round_trips_count_and_refusals() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        record(r, &ev("apply", 10, 2, 3, None, "abcd1234", None, None));
        record(r, &ev("apply", 20, 1, 1, Some("stale"), "", None, None));
        record(r, &ev("status", 5, 0, 0, None, "abcd1234", None, None));

        let report = aggregate(r, false);
        assert_eq!(report.verbs["apply"].count, 2);
        assert_eq!(report.verbs["apply"].refusals["stale"], 1);
        assert_eq!(report.verbs["status"].count, 1);
        assert!(report.history.is_none());
    }

    #[test]
    fn aggregate_on_missing_metrics_dir_is_empty_not_error() {
        let d = tempfile::tempdir().unwrap();
        let report = aggregate(d.path(), true);
        assert!(report.verbs.is_empty());
        assert_eq!(report.history, Some(BTreeMap::new()));
    }

    #[test]
    fn record_never_panics_when_vc_metrics_path_is_unwritable() {
        // record() must be a true no-op-on-failure: point it at a root
        // whose ".vc" segment is actually a file, so create_dir_all
        // cannot succeed, and confirm the call just... doesn't crash.
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::write(r.join(".vc"), "not a directory").unwrap();
        record(r, &ev("status", 1, 0, 0, None, "", None, None));
        // No panic reaching here is the assertion.
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let dir = metrics_dir(r);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("2026-08-28.jsonl"),
            "not json at all\n{\"verb\":\"status\",\"ms\":7}\n\n",
        )
        .unwrap();
        let report = aggregate(r, false);
        assert_eq!(report.verbs["status"].count, 1);
        assert_eq!(report.verbs["status"].p50_ms, 7);
    }

    /// Task 15: a query/outline/read invocation records `bytes_out` and
    /// `naive_bytes`, and `aggregate` sums them into `read_savings` —
    /// `saved` = Σ(naive_bytes - bytes_out) clamped per-line at 0, `naive`
    /// = Σ naive_bytes, `calls` = the count of qualifying lines. A verb
    /// that never sets either field (`status`) must not contribute.
    #[test]
    fn record_with_read_gain_fields_aggregates_into_read_savings() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        record(
            r,
            &ev("query", 5, 1, 3, None, "abcd1234", Some(40), Some(100)),
        );
        record(
            r,
            &ev("read", 3, 1, 0, None, "abcd1234", Some(20), Some(50)),
        );
        record(r, &ev("status", 1, 0, 0, None, "abcd1234", None, None));

        let report = aggregate(r, false);
        assert_eq!(report.read_savings.calls, 2);
        assert_eq!(report.read_savings.naive, 150);
        assert_eq!(report.read_savings.saved, 90); // (100-40) + (50-20)
        assert_eq!(report.read_savings.pct(), 60); // 90*100/150
    }

    /// A pathological line where `bytes_out` exceeds `naive_bytes` clamps
    /// ONLY that line's own delta to 0 — it must never subtract from
    /// another line's real, positive savings.
    #[test]
    fn negative_per_line_delta_clamps_to_zero_not_the_whole_sum() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        record(r, &ev("query", 1, 1, 1, None, "e", Some(120), Some(100)));
        record(r, &ev("query", 1, 1, 1, None, "e", Some(10), Some(50)));

        let report = aggregate(r, false);
        assert_eq!(report.read_savings.calls, 2);
        assert_eq!(report.read_savings.saved, 40); // 0 + 40, never -20 + 40
    }

    /// An OLD-format metrics line (written before Task 15, missing the
    /// `bytes_out`/`naive_bytes` keys entirely) must still parse and count
    /// toward its verb's ordinary stats — and must not contribute to
    /// `read_savings`, since it has neither field.
    #[test]
    fn old_format_line_without_read_gain_fields_still_parses() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let dir = metrics_dir(r);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("2026-08-01.jsonl"),
            "{\"ts\":1,\"verb\":\"query\",\"ms\":5,\"files\":1,\"edits\":1,\"refusal\":null,\"epoch8\":\"e\",\"version\":\"0.0.1\"}\n",
        )
        .unwrap();

        let report = aggregate(r, false);
        assert_eq!(
            report.verbs["query"].count, 1,
            "an old-format line must still count toward its verb's stats"
        );
        assert_eq!(
            report.read_savings.calls, 0,
            "an old-format line has neither field, so it must not contribute to read savings"
        );
    }

    #[test]
    fn format_human_includes_read_savings_line() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        record(r, &ev("query", 1, 1, 1, None, "e", Some(40), Some(100)));

        let report = aggregate(r, false);
        let human = format_human(&report);
        assert!(
            human.contains("read savings: 60 bytes (60%) across 1 read-verb calls"),
            "got: {human}"
        );
    }

    #[test]
    fn to_json_includes_read_savings_object() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        record(r, &ev("outline", 1, 1, 0, None, "e", Some(40), Some(100)));

        let report = aggregate(r, false);
        let json = to_json(&report);
        assert_eq!(json["read_savings"]["saved"], 60);
        assert_eq!(json["read_savings"]["naive"], 100);
        assert_eq!(json["read_savings"]["calls"], 1);
        assert_eq!(json["read_savings"]["pct"], 60);
    }
}
