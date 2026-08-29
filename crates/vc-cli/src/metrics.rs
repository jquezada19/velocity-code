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
}

/// Append one metrics line for this invocation. Never fails outward: any
/// I/O error (unwritable `.vc/`, full disk, ...) is silently dropped —
/// see the module doc comment.
pub fn record(
    root: &Path,
    verb: &str,
    ms: u64,
    files: usize,
    edits: usize,
    refusal: Option<&str>,
    epoch8: &str,
) {
    let _ = try_record(root, verb, ms, files, edits, refusal, epoch8);
}

fn try_record(
    root: &Path,
    verb: &str,
    ms: u64,
    files: usize,
    edits: usize,
    refusal: Option<&str>,
    epoch8: &str,
) -> std::io::Result<()> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = metrics_dir(root);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.jsonl", date_string(ts)));
    let line = MetricLine {
        ts,
        verb,
        ms,
        files,
        edits,
        refusal,
        epoch8,
        version: env!("CARGO_PKG_VERSION"),
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
#[derive(serde::Deserialize)]
struct MetricRecord {
    verb: String,
    #[serde(default)]
    ms: u64,
    #[serde(default)]
    refusal: Option<String>,
}

#[derive(Debug, Default)]
pub struct VerbStats {
    pub count: usize,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub refusals: BTreeMap<String, usize>,
}

#[derive(Debug, Default)]
pub struct GainReport {
    pub verbs: BTreeMap<String, VerbStats>,
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
        history: with_history.then_some(history),
    }
}

/// Small fixed-width table, one row per verb, plus a refusal-kind
/// breakdown line for any verb that had refusals, the M1 "reads:
/// n/a until M2" counterfactual placeholder (§7.2's token/byte-savings
/// block arrives with the read verbs in M2), and — when `--history` was
/// passed — per-day totals.
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
    let _ = writeln!(out, "reads: n/a until M2");
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
        "reads": "n/a until M2",
    });
    if let Some(hist) = &report.history {
        obj["history"] = serde_json::json!(hist);
    }
    obj
}

#[cfg(test)]
mod tests {
    use super::*;

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
        record(r, "apply", 10, 2, 3, None, "abcd1234");
        record(r, "apply", 20, 1, 1, Some("stale"), "");
        record(r, "status", 5, 0, 0, None, "abcd1234");

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
        record(r, "status", 1, 0, 0, None, "");
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
}
