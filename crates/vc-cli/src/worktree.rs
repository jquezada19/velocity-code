//! `vc worktree doctor` — a report-only lifecycle classifier over the
//! linked worktrees of a git repository.
//!
//! Every fact comes from a read-only git query (`worktree list`, `status`,
//! `rev-parse`, `merge-base --is-ancestor`, `rev-list --count`, `log -1`,
//! `merge-tree --write-tree`), each spawned with the inherited `GIT_DIR`/
//! `GIT_WORK_TREE`-style overrides stripped (so a doctor run from inside a
//! hook still targets the repo it was pointed at) and with
//! `GIT_OPTIONAL_LOCKS=0` (so `git status` does not opportunistically
//! rewrite the index it is only meant to read) and `GIT_NO_LAZY_FETCH=1`
//! (so a partial clone's `merge-tree` reports a missing promised object
//! as a failure — rendered `unknown` — instead of quietly fetching it;
//! honoured by git 2.45+, ignored by older gits). The doctor never runs a
//! git command that changes a ref, an index or a working tree; the one
//! network command, `fetch`, runs only under an explicit `--fetch`.
//!
//! Paths travel as bytes from `git worktree list --porcelain -z` into
//! `PathBuf`s; a non-UTF-8 path is rendered lossily in both the human
//! and the JSON output (this is a report, and the `path` field is for
//! reading, not for feeding back to git).
//!
//! Classification is a fixed priority order per linked worktree — the
//! first rule that holds names the state, so a worktree is never
//! double-counted:
//!
//! 1. `prunable`   — git itself says the worktree's directory is gone.
//! 2. `stale`      — the branch named by the worktree no longer exists
//!    (`branch refs/heads/x` in the porcelain but no such ref). Its HEAD
//!    is unborn, so git would report every file as newly added: neither
//!    dirtiness nor ahead/behind can be measured, and the reason says
//!    so rather than letting a meaningless `dirty` win.
//! 3. `dirty`      — uncommitted changes or untracked files. Reported
//!    before every merge-related state because nothing below is safe
//!    to act on while work is unsaved; the doctor never touches a
//!    worktree in any state anyway.
//! 4. `detached`   — no branch checked out; base comparisons are made
//!    against the bare commit, but the state stays `detached` so a
//!    later "remove merged worktrees" pass has to look twice.
//! 5. `base`       — the worktree has the base branch itself checked
//!    out; comparing base to base is trivially "merged", which is not
//!    what a reader means by that word.
//! 6. `merged`     — HEAD is an ancestor of base (`git merge-base
//!    --is-ancestor`): everything here is already in base.
//! 7. `conflict`   — `git merge-tree --write-tree base HEAD` exits 1.
//! 8. `unknown`    — `merge-tree` failed for any other reason, including
//!    a git too old to know `--write-tree` (pre-2.38).
//! 9. `stale`      — merges cleanly but the last commit is older than
//!    `--stale-days`: unmerged work that has gone quiet.
//! 10. `merge-ready` — merges cleanly and was touched recently. (If the
//!     last-commit date cannot be read, neither 9 nor 10 can be told
//!     apart and the worktree is `unknown`.)
//!
//! `locked` is not a state: a locked worktree can be dirty or merge-ready
//! like any other, so the lock is carried as a flag (`locked: true` in
//! JSON, `[locked]` in the reason column) rather than allowed to mask the
//! classification. The main worktree is never classified — it is the
//! thing the others are compared against — and is named in the summary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use velocity_code_kernel::{ErrorKind, VcError, VcResult};

/// Git environment variables that, when inherited (a doctor run from a
/// hook, a shell with `GIT_DIR` exported), would redirect every spawned
/// `git` away from the repo the doctor was pointed at.
const STRIPPED_GIT_ENV: [&str; 10] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_SHALLOW_FILE",
    "GIT_CEILING_DIRECTORIES",
];

pub const DEFAULT_STALE_DAYS: u64 = 14;

/// Git terminates a single-value answer with exactly one `\n`; strip
/// that and nothing else. `str::trim` would also eat Unicode whitespace
/// (U+00A0, U+3000, ...) that is legal inside a ref name, silently
/// turning one ref into another.
fn strip_nl(s: &str) -> &str {
    let s = s.strip_suffix('\n').unwrap_or(s);
    s.strip_suffix('\r').unwrap_or(s)
}

/// Result of one git invocation. `status` is `None` when the process was
/// killed by a signal (no exit code to report).
struct GitOut {
    status: Option<i32>,
    stdout: String,
    stdout_raw: Vec<u8>,
    stderr: String,
}

impl GitOut {
    fn ok(&self) -> bool {
        self.status == Some(0)
    }
}

/// Spawn `git -C <dir> <args>` read-only. A missing `git` binary (or any
/// spawn failure) is an `Io` error — the only error this module raises
/// besides "not a repo" and "no base branch found".
fn git(dir: &Path, args: &[&str]) -> VcResult<GitOut> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    cmd.args(args);
    for k in STRIPPED_GIT_ENV {
        cmd.env_remove(k);
    }
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd.env("GIT_NO_LAZY_FETCH", "1");
    // No pager, no prompts: this is a machine caller.
    cmd.env("GIT_PAGER", "cat");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.stdin(std::process::Stdio::null());
    let out = cmd.output().map_err(|e| {
        VcError::new(
            ErrorKind::Io,
            format!("git: failed to run `git {}`: {e}", args.join(" ")),
        )
    })?;
    Ok(GitOut {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stdout_raw: out.stdout,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// One record from `git worktree list --porcelain`, exactly as git
/// describes it — no interpretation yet.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: Option<String>,
    /// `refs/heads/...` stripped to the short branch name.
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: Option<String>,
    pub prunable: Option<String>,
}

/// Parse `git worktree list --porcelain -z` output. Every attribute
/// line is NUL-terminated and a record ends with one extra NUL, so a
/// path or lock reason containing a newline, a tab or a trailing space
/// arrives verbatim (the non-`-z` form C-quotes such values). The first
/// record is the main worktree (or the bare repository). Attributes are
/// matched on their exact prefix and the remainder is taken as-is. A
/// `locked`/`prunable` line with no reason yields `Some("")` so the flag
/// is still visible.
pub fn parse_porcelain(raw: &[u8]) -> Vec<WorktreeEntry> {
    let mut out = Vec::new();
    let mut cur: Option<WorktreeEntry> = None;
    for field in raw.split(|b| *b == 0) {
        let line = String::from_utf8_lossy(field);
        let line: &str = &line;
        if line.is_empty() {
            if let Some(e) = cur.take() {
                out.push(e);
            }
            continue;
        }
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(e) = cur.take() {
                out.push(e);
            }
            debug_assert!(!p.is_empty() || field.len() == "worktree ".len());
            cur = Some(WorktreeEntry {
                path: path_from_bytes(&field["worktree ".len()..]),
                ..Default::default()
            });
            continue;
        }
        let Some(e) = cur.as_mut() else {
            // Attribute line before any `worktree` line: malformed
            // output; ignore rather than invent a record.
            continue;
        };
        if let Some(h) = line.strip_prefix("HEAD ") {
            e.head = Some(h.to_string());
        } else if let Some(b) = line.strip_prefix("branch ") {
            e.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        } else if line == "detached" {
            e.detached = true;
        } else if line == "bare" {
            e.bare = true;
        } else if line == "locked" {
            e.locked = Some(String::new());
        } else if let Some(r) = line.strip_prefix("locked ") {
            e.locked = Some(r.to_string());
        } else if line == "prunable" {
            e.prunable = Some(String::new());
        } else if let Some(r) = line.strip_prefix("prunable ") {
            e.prunable = Some(r.to_string());
        }
        // Any other attribute a newer git adds is ignored, not an error.
    }
    if let Some(e) = cur.take() {
        out.push(e);
    }
    out
}

#[cfg(unix)]
fn path_from_bytes(b: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(b))
}

#[cfg(not(unix))]
fn path_from_bytes(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).into_owned())
}

/// Human output is tab-separated, one record per line, so a tab or a
/// newline inside a path or reason is rendered escaped rather than
/// allowed to forge a column or a record. JSON needs no escaping — the
/// serializer does it.
fn escape_cell(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    Prunable,
    Dirty,
    Detached,
    Base,
    Stale,
    Merged,
    Conflict,
    Unknown,
    MergeReady,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Prunable => "prunable",
            State::Dirty => "dirty",
            State::Detached => "detached",
            State::Base => "base",
            State::Stale => "stale",
            State::Merged => "merged",
            State::Conflict => "conflict",
            State::Unknown => "unknown",
            State::MergeReady => "merge-ready",
        }
    }
}

/// What `git merge-tree --write-tree <base> <head>` said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeTree {
    Clean,
    Conflict,
    /// The command failed for a reason other than a conflict. The string
    /// is the short reason rendered into the report.
    Unsupported(String),
}

/// The read-only facts about one linked worktree that classification
/// needs. Gathered by [`gather`], classified by [`classify`] — split so
/// the ordering rules can be unit-tested without a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub prunable: Option<String>,
    /// `Some(summary)` when the worktree has uncommitted changes or
    /// untracked files; the summary names the first entry and a count.
    pub dirty: Option<String>,
    pub detached: bool,
    /// The worktree's branch is the base branch itself.
    pub is_base: bool,
    /// `branch` named in the porcelain but git confirms no such ref.
    pub branch_gone: bool,
    /// A ref lookup needed for classification failed outright (not
    /// "missing" — "could not be answered"). Reported as `unknown` after
    /// the dirty check, which does not depend on it.
    pub query_failed: Option<String>,
    /// HEAD is an ancestor of base; `None` when the query itself failed.
    pub is_ancestor: Option<bool>,
    pub merge_tree: Option<MergeTree>,
    /// Seconds since the last commit on HEAD; `None` if unreadable.
    pub age_secs: Option<u64>,
}

/// The fixed priority order from the module doc. Returns the state and
/// the one-line reason the report prints.
pub fn classify(f: &Facts, stale_secs: u64) -> (State, String) {
    if let Some(r) = &f.prunable {
        let r = if r.is_empty() { "gone" } else { r.as_str() };
        return (State::Prunable, format!("git reports prunable: {r}"));
    }
    if f.branch_gone {
        return (
            State::Stale,
            "branch no longer exists (HEAD unborn; uncommitted work not checkable)".to_string(),
        );
    }
    if let Some(d) = &f.dirty {
        return (State::Dirty, format!("uncommitted changes: {d}"));
    }
    if let Some(why) = &f.query_failed {
        return (State::Unknown, format!("ref lookup failed: {why}"));
    }
    if f.detached {
        return (State::Detached, "no branch checked out".to_string());
    }
    if f.is_base {
        return (State::Base, "checked out on the base branch".to_string());
    }
    match f.is_ancestor {
        Some(true) => return (State::Merged, "HEAD is an ancestor of base".to_string()),
        Some(false) => {}
        None => {
            return (
                State::Unknown,
                "merge-base --is-ancestor failed; merged/unmerged not determinable".to_string(),
            );
        }
    }
    match &f.merge_tree {
        Some(MergeTree::Conflict) => {
            return (State::Conflict, "merge-tree reports conflicts".to_string());
        }
        Some(MergeTree::Unsupported(why)) => {
            return (State::Unknown, format!("merge-tree unsupported: {why}"));
        }
        Some(MergeTree::Clean) => {}
        None => {
            return (
                State::Unknown,
                "merge-tree not run (no HEAD to compare)".to_string(),
            );
        }
    }
    match f.age_secs {
        Some(age) if age > stale_secs => (
            State::Stale,
            format!(
                "merges cleanly but last commit is {} days old",
                age / 86_400
            ),
        ),
        Some(_) => (State::MergeReady, "merges cleanly".to_string()),
        None => (
            State::Unknown,
            "merges cleanly but last commit date unreadable; stale/merge-ready not determinable"
                .to_string(),
        ),
    }
}

/// One classified worktree, ready to render.
#[derive(Debug, Clone)]
pub struct Report {
    pub state: State,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub ahead: Option<u64>,
    pub behind: Option<u64>,
    pub locked: Option<String>,
    pub last_commit_unix: Option<u64>,
    pub reason: String,
}

pub struct DoctorArgs<'a> {
    pub repo: Option<&'a Path>,
    pub base: Option<&'a str>,
    pub stale_days: u64,
    pub fetch: bool,
}

pub struct DoctorReport {
    pub repo_root: PathBuf,
    pub base: String,
    /// The canonical ref (or expression) actually compared against.
    pub base_full: String,
    pub base_commit: String,
    pub base_source: &'static str,
    pub main_path: Option<PathBuf>,
    pub main_branch: Option<String>,
    pub worktrees: Vec<Report>,
}

/// The resolved base: the name to display, the canonical full ref name
/// every git query uses (so a tag that happens to share the short name
/// cannot redirect a comparison), and the commit it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub display: String,
    /// The branch identity a linked worktree's branch is compared to,
    /// when the base is a branch at all (a tag or a bare sha has none).
    pub branch: Option<BaseBranch>,
    /// `refs/heads/main`, `refs/remotes/origin/main`, or — for a
    /// `--base` that is not a ref at all (a bare sha, `HEAD~3`,
    /// `release~1`) — the expression with its leading ref operand spelled
    /// canonically (`refs/heads/release~1`), so git resolves it without
    /// consulting its name precedence.
    pub full: String,
    pub commit: String,
    pub source: &'static str,
    /// `true` when `full` is an exact ref git can fetch into (a branch,
    /// a tag, a remote-tracking ref); `false` for a revision expression
    /// or a bare object id.
    pub is_ref: bool,
}

/// How a base ref names a branch: a local branch (`refs/heads/x` ->
/// `x`) or a remote-tracking one (`refs/remotes/<remote>/x` -> `x`, the
/// remote resolved against `git remote` so a remote whose own name
/// contains a slash splits correctly).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseBranch {
    Local(String),
    /// `(remote, branch)` — the remote is kept so `--fetch` refreshes
    /// the one the base actually lives on.
    Remote(String, String),
}

/// For `refs/remotes/...`, the branch part with the remote name — the
/// LONGEST configured remote name that prefixes the ref — removed.
/// `None` when no configured remote matches (a stray ref under
/// `refs/remotes/` that no remote owns cannot be "the base branch").
fn remote_branch_name(repo: &Path, full: &str) -> VcResult<Option<BaseBranch>> {
    let Some(rest) = full.strip_prefix("refs/remotes/") else {
        return Ok(None);
    };
    let remotes = git(repo, &["remote"])?;
    if !remotes.ok() {
        return Err(VcError::new(
            ErrorKind::Io,
            format!("git remote failed: {}", remotes.stderr.trim()),
        ));
    }
    let best = remotes
        .stdout
        .split('\n')
        .map(strip_nl)
        .filter(|r| !r.is_empty())
        .filter_map(|r| {
            rest.strip_prefix(r)?
                .strip_prefix('/')
                .map(|b| (r.len(), b))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(len, b)| BaseBranch::Remote(rest[..len].to_string(), b.to_string()));
    Ok(best)
}

/// The source ref that `remote.<remote>.fetch` maps onto `dst`, if any
/// configured refspec covers it. Handles the literal and the single-`*`
/// glob forms (`+refs/heads/*:refs/remotes/origin/*`); a negative
/// refspec (`^...`) is skipped. `git config` exit 1 = no such key.
fn fetch_source_for(repo: &Path, remote: &str, dst: &str, fallback: &str) -> VcResult<String> {
    let key = format!("remote.{remote}.fetch");
    let o = git(repo, &["config", "--get-all", "--", &key])?;
    match o.status {
        Some(0) => {}
        Some(1) => return Ok(fallback.to_string()),
        _ => {
            return Err(VcError::new(
                ErrorKind::Io,
                format!("git config --get-all {key} failed: {}", o.stderr.trim()),
            ));
        }
    }
    // Positive mappings whose destination covers `dst` yield candidate
    // sources; negative refspecs (`^pattern`) then exclude any source
    // they match. What is left must be one distinct source.
    let specs: Vec<&str> = o
        .stdout
        .split('\n')
        .map(strip_nl)
        .filter(|l| !l.is_empty())
        .collect();
    let mut sources: Vec<String> = Vec::new();
    for spec in &specs {
        let spec = spec.trim_start_matches('+');
        if spec.starts_with('^') {
            continue;
        }
        let Some((src, d)) = spec.split_once(':') else {
            continue;
        };
        if let Some(mid) = glob_match(d, dst) {
            let src = match src.split_once('*') {
                Some((pre, suf)) => format!("{pre}{mid}{suf}"),
                None => src.to_string(),
            };
            if !sources.contains(&src) {
                sources.push(src);
            }
        }
    }
    // Nothing mapped onto this destination: the same-named branch is
    // the source — but it is still subject to the negative refspecs
    // below, exactly as a mapped source would be.
    let mapped = sources.len();
    if mapped == 0 {
        sources.push(fallback.to_string());
    }
    for spec in &specs {
        let Some(neg) = spec.strip_prefix('^') else {
            continue;
        };
        sources.retain(|src| glob_match(neg, src).is_none());
    }
    match sources.len() {
        // Every candidate excluded: the configuration says NOT to fetch
        // this, and that is refused rather than turned into a guess.
        0 => Err(VcError::new(
            ErrorKind::Usage,
            format!(
                "{key}: {} onto {dst} is excluded by a negative refspec; nothing to fetch",
                if mapped == 0 {
                    format!("the only source ({fallback})")
                } else {
                    "every source mapped".to_string()
                }
            ),
        )),
        1 => Ok(sources.pop().unwrap_or_default()),
        _ => Err(VcError::new(
            ErrorKind::Usage,
            format!(
                "{key}: more than one source maps onto {dst} ({}); cannot choose what to fetch",
                sources.join(", ")
            ),
        )),
    }
}

/// Match `s` against a refspec pattern with at most one `*`; returns
/// the text the `*` stood for (empty for a literal match), or `None`.
fn glob_match(pattern: &str, s: &str) -> Option<String> {
    match pattern.split_once('*') {
        Some((pre, suf)) => s
            .strip_prefix(pre)
            .and_then(|r| r.strip_suffix(suf))
            .map(str::to_string),
        None => (pattern == s).then(String::new),
    }
}

/// Whether `name` is spelled as a full object id of this repository's
/// format — the only spelling that is a commit rather than a name.
fn is_full_oid(repo: &Path, name: &str) -> VcResult<bool> {
    Ok(name.len() == object_id_width(repo)? && name.bytes().all(|c| c.is_ascii_hexdigit()))
}

/// Every exact ref `name` could mean, in git's own precedence order
/// (pseudo-ref, `refs/<n>`, `refs/tags/<n>`, `refs/heads/<n>`,
/// `refs/remotes/<n>`, `refs/remotes/<n>/HEAD`); a `refs/`-prefixed name
/// means only itself. Each candidate is checked as an EXACT name; one
/// whose existence cannot be checked is an error (`label` is the typed
/// `--base`, for the message), never treated as absent.
fn name_hits(repo: &Path, name: &str, label: &str) -> VcResult<Vec<String>> {
    let mut candidates: Vec<String> = Vec::new();
    if name.starts_with("refs/") {
        candidates.push(name.to_string());
    } else {
        if !name.contains('/') && pseudo_ref_exists(repo, name)? {
            candidates.push(name.to_string());
        }
        for c in [
            format!("refs/{name}"),
            format!("refs/tags/{name}"),
            format!("refs/heads/{name}"),
            format!("refs/remotes/{name}"),
            format!("refs/remotes/{name}/HEAD"),
        ] {
            candidates.push(c);
        }
    }
    let mut hits: Vec<String> = Vec::new();
    for c in &candidates {
        if !c.starts_with("refs/") {
            // The pseudo-ref: its file was just seen to exist.
            hits.push(c.clone());
            continue;
        }
        match ref_exists(repo, c)? {
            Some(true) => hits.push(c.clone()),
            Some(false) => {}
            None => {
                return Err(VcError::new(
                    ErrorKind::Io,
                    format!("--base {label}: could not check whether {c} exists"),
                ));
            }
        }
    }
    Ok(hits)
}

/// A revision expression with its leading ref operand spelled
/// canonically: `release~1` -> `refs/heads/release~1` when `release`
/// names exactly one ref; refused when it names more than one; left as
/// typed when it names none (an object id) or when there is no operand
/// (`~1`, `:path`). Operators recognised: `~`, `^`, `@{`, `:`.
fn canonical_expr(repo: &Path, expr: &str) -> VcResult<String> {
    let cut = ["~", "^", "@{", ":"]
        .iter()
        .filter_map(|op| expr.find(op))
        .min();
    let Some(idx) = cut else {
        return Ok(expr.to_string());
    };
    if idx == 0 {
        return Ok(expr.to_string());
    }
    let (operand, rest) = expr.split_at(idx);
    if is_full_oid(repo, operand)? {
        return Ok(expr.to_string());
    }
    let hits = name_hits(repo, operand, expr)?;
    // A pseudo-ref operand (`HEAD~1`) is git's own, unambiguous spelling
    // for a revision expression — `$GIT_DIR/HEAD` outranks a stray
    // `refs/tags/HEAD` there — and is left as typed; only collisions
    // among real refs are refused.
    if hits.iter().any(|h| !h.starts_with("refs/")) {
        return Ok(expr.to_string());
    }
    match hits.len() {
        0 => Ok(expr.to_string()),
        // `@{upstream}`/`@{u}`/`@{push}` are keyed by the SHORT local
        // branch name (git looks up `branch.<name>.*` by it), so a branch
        // hit keeps the short spelling for THOSE selectors only — the
        // operand was just shown to be a unique short name. Numeric and
        // date reflog selectors (`@{0}`, `@{yesterday}`) accept the
        // canonical ref and keep it: shortening them would re-open the
        // branch/tag reflog ambiguity a qualified operand had closed.
        1 if is_branch_config_selector(rest) => match hits[0].strip_prefix("refs/heads/") {
            Some(short) => Ok(format!("{short}{rest}")),
            None => Ok(format!("{}{rest}", hits[0])),
        },
        1 => Ok(format!("{}{rest}", hits[0])),
        _ => Err(VcError::new(
            ErrorKind::Usage,
            format!(
                "--base {expr}: ambiguous operand {operand} ({})",
                hits.join(", ")
            ),
        )
        .with_next(format!("vc worktree doctor --base {}{rest}", hits[0]))),
    }
}

/// `@{upstream}` / `@{u}` / `@{push}` (case-insensitive, as git
/// accepts them), optionally followed by more suffix (`@{u}~1`).
fn is_branch_config_selector(rest: &str) -> bool {
    let Some(inner) = rest.strip_prefix("@{") else {
        return false;
    };
    let Some(end) = inner.find('}') else {
        return false;
    };
    matches!(
        inner[..end].to_ascii_lowercase().as_str(),
        "upstream" | "u" | "push"
    )
}

/// The hex width of a full object id in this repository (40 for sha1,
/// 64 for sha256), from git itself rather than assumed.
fn object_id_width(repo: &Path) -> VcResult<usize> {
    let o = git(repo, &["rev-parse", "--show-object-format"])?;
    if !o.ok() {
        return Err(VcError::new(
            ErrorKind::Io,
            format!(
                "git rev-parse --show-object-format failed: {}",
                o.stderr.trim()
            ),
        ));
    }
    match o.stdout.trim() {
        "sha1" => Ok(40),
        "sha256" => Ok(64),
        other => Err(VcError::new(
            ErrorKind::Io,
            format!("unknown object format {other:?}"),
        )),
    }
}

/// The branch identity of a resolved (already dereferenced) ref: a
/// local branch for `refs/heads/<x>`, a remote-tracking one for
/// `refs/remotes/<remote>/<x>`, none for anything else. The ONE place
/// every resolution path — explicit, `origin/HEAD`, fallback — derives
/// it, so a symbolic ref that lands on a local branch is identified the
/// same way whichever path found it.
fn branch_identity(repo: &Path, full: &str) -> VcResult<Option<BaseBranch>> {
    if let Some(l) = full.strip_prefix("refs/heads/") {
        return Ok(Some(BaseBranch::Local(l.to_string())));
    }
    remote_branch_name(repo, full)
}

/// Resolve the base: `--base` (must name a commit), else `origin/HEAD`'s
/// target, else `main`, else `master`. `source` names which rule won,
/// for the summary line.
fn resolve_base(repo: &Path, explicit: Option<&str>) -> VcResult<Base> {
    if let Some(b) = explicit {
        // A short name is expanded through git's own documented
        // precedence (`$GIT_DIR/<b>` for a pseudo-ref such as `HEAD`,
        // then `refs/<b>`, `refs/tags/<b>`, `refs/heads/<b>`,
        // `refs/remotes/<b>`, `refs/remotes/<b>/HEAD`), each checked as an
        // EXACT name — a pseudo-ref by the presence of the file git
        // itself would read, every other candidate by `show-ref
        // --verify` — never parsed as a revision expression, so `HEAD~1`
        // cannot be captured by a tag named `HEAD` and a branch named
        // `RELEASE` is a branch. Where git would silently pick the first
        // hit and warn, a base that matches more than one is refused:
        // every comparison in the report hangs off it. A candidate whose
        // existence cannot be checked aborts resolution rather than being
        // treated as absent. A name matching nothing is a revision
        // expression (a sha, `HEAD~3`) that must still peel to a commit.
        // A symbolic ref (`origin/HEAD`, or plain `origin`) is
        // dereferenced so the base's branch identity is the branch it
        // points at, not the name `HEAD`.
        // A full object id is a commit, never a name to expand — but a
        // ref that happens to be spelled like one is a collision the
        // doctor refuses rather than resolves either way.
        // "Full" means the width of THIS repository's object format: a
        // 64-hex tag name in a sha1 repository is a name, not an id.
        let is_oid = is_full_oid(repo, b)?;
        let hits = name_hits(repo, b, b)?;
        if is_oid && !hits.is_empty() {
            return Err(VcError::new(
                ErrorKind::Usage,
                format!(
                    "--base {b}: a full object id, but also the name of {}",
                    hits.join(", ")
                ),
            )
            .with_next(format!("vc worktree doctor --base {}^{{commit}}", b)));
        }
        // `exact` = the base is a ref git can fetch into and whose name
        // carries a branch identity; an expression (`HEAD~3`, a sha,
        // `refs/remotes/origin/main~1`) is neither.
        let (full, commit, exact) = match hits.len() {
            0 => {
                // A revision expression. Its leading ref operand
                // (`release` in `release~1`, `main@{upstream}`) goes
                // through the same exact-name expansion as a bare
                // name, so a branch and a tag both called `release` are
                // refused here too instead of git quietly picking the
                // tag (with a warning nobody reads); a single hit is
                // spelled canonically so the report hangs off
                // `refs/heads/release~1`, never off a precedence order.
                // An operand matching nothing (an object id) is left for
                // git to resolve.
                let expr = canonical_expr(repo, b)?;
                let Some(commit) = commit_of_expr(repo, &expr)? else {
                    return Err(VcError::new(
                        ErrorKind::NotFound,
                        format!("--base {b}: does not name a commit"),
                    ));
                };
                (expr, commit, false)
            }
            1 => {
                let full = deref_symbolic(repo, &hits[0])?;
                let commit = if full.starts_with("refs/") {
                    commit_of(repo, &full)?
                } else {
                    commit_of_expr(repo, &full)?
                };
                let Some(commit) = commit else {
                    return Err(VcError::new(
                        ErrorKind::NotFound,
                        format!("--base {b}: {full} does not peel to a commit"),
                    ));
                };
                let exact = full.starts_with("refs/");
                (full, commit, exact)
            }
            _ => {
                return Err(VcError::new(
                    ErrorKind::Usage,
                    format!("--base {b}: ambiguous ({})", hits.join(", ")),
                )
                .with_next(format!("vc worktree doctor --base {}", hits[0])));
            }
        };
        let branch = if exact {
            branch_identity(repo, &full)?
        } else {
            None
        };
        return Ok(Base {
            display: b.to_string(),
            branch,
            full,
            commit,
            source: "--base",
            is_ref: exact,
        });
    }
    // `symbolic-ref -q` exits 1 when origin/HEAD is simply unset — the
    // ordinary "fall through to main" case. Any other failure is an
    // error: a report against `main` when origin/HEAD pointed elsewhere
    // is a wrong report, not a degraded one.
    let o = git(repo, &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])?;
    if !matches!(o.status, Some(0) | Some(1)) {
        return Err(VcError::new(
            ErrorKind::Io,
            format!(
                "git symbolic-ref refs/remotes/origin/HEAD failed: {}",
                o.stderr.trim()
            ),
        ));
    }
    if o.ok() {
        let full = strip_nl(&o.stdout);
        if let Some(commit) = commit_of(repo, full)? {
            let display = full.strip_prefix("refs/remotes/").unwrap_or(full);
            return Ok(Base {
                display: display.to_string(),
                branch: branch_identity(repo, full)?,
                full: full.to_string(),
                commit,
                source: "origin/HEAD",
                is_ref: true,
            });
        }
    }
    for cand in ["main", "master"] {
        let named = format!("refs/heads/{cand}");
        match ref_exists(repo, &named)? {
            Some(true) => {}
            Some(false) => continue,
            None => {
                return Err(VcError::new(
                    ErrorKind::Io,
                    format!("base candidate {named}: show-ref could not verify it"),
                ));
            }
        }
        // A symbolic `main` (pointing at another branch) is compared and
        // identified as its TARGET, exactly as `--base main` would be.
        let full = deref_symbolic(repo, &named)?;
        let Some(commit) = commit_of(repo, &full)? else {
            continue;
        };
        let branch = branch_identity(repo, &full)?;
        return Ok(Base {
            display: cand.to_string(),
            branch,
            full,
            commit,
            source: "fallback",
            is_ref: true,
        });
    }
    Err(VcError::new(
        ErrorKind::Usage,
        "no base branch found (origin/HEAD unset, no main or master)",
    )
    .with_next("vc worktree doctor --base <ref>"))
}

/// Outcome of looking up an exact ref name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RefLookup {
    /// The ref exists and peels to this commit.
    Found(String),
    /// `show-ref --verify` says so with exit 1: no such ref.
    Missing,
    /// The ref exists but does not peel to a commit, or a query died:
    /// the question could not be answered.
    Failed(String),
}

/// `git show-ref --verify -q <full>`: exact-name existence, no
/// revision-expression parsing. `Ok(Some(true))` exists, `Ok(Some(false))`
/// exit 1 = missing, `Ok(None)` any other exit.
fn ref_exists(repo: &Path, full: &str) -> VcResult<Option<bool>> {
    let o = git(repo, &["show-ref", "--verify", "-q", "--", full])?;
    Ok(match o.status {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    })
}

/// Existence first (`show-ref --verify`), then peel (`rev-parse
/// --verify <full>^{commit}`). A ref that exists but does not peel — a
/// tag of a tree, a corrupt object — is `Failed`, never `Missing`:
/// "missing" is what makes a worktree's branch count as deleted.
fn lookup_ref(repo: &Path, full: &str) -> VcResult<RefLookup> {
    match ref_exists(repo, full)? {
        Some(false) => return Ok(RefLookup::Missing),
        None => {
            return Ok(RefLookup::Failed(format!(
                "show-ref could not verify {full}"
            )));
        }
        Some(true) => {}
    }
    let o = git(
        repo,
        &["rev-parse", "--verify", "-q", &format!("{full}^{{commit}}")],
    )?;
    Ok(match o.status {
        Some(0) => RefLookup::Found(strip_nl(&o.stdout).to_string()),
        Some(code) => RefLookup::Failed(format!(
            "{full} exists but does not peel to a commit (rev-parse exit {code}: {})",
            o.stderr.lines().next().unwrap_or("")
        )),
        None => RefLookup::Failed("rev-parse killed by signal".to_string()),
    })
}

/// Whether `$GIT_DIR/<name>` is a pseudo-ref — the first place git's own
/// ref resolution looks for a short name (`HEAD`, `ORIG_HEAD`,
/// `FETCH_HEAD`, ...). Decided by the file's CONTENT: a symbolic
/// `ref: refs/...` line or a bare object id. An ordinary git-dir file
/// that merely shares a branch's name (`index`, `config`) is not one.
/// A failure to ask git where the file would be, or to read a file that
/// exists, is an error, not "absent".
fn pseudo_ref_exists(repo: &Path, name: &str) -> VcResult<bool> {
    let o = git(repo, &["rev-parse", "--git-path", name])?;
    if !o.ok() {
        return Err(VcError::new(
            ErrorKind::Io,
            format!(
                "git rev-parse --git-path {name} failed: {}",
                o.stderr.trim()
            ),
        ));
    }
    let raw = o.stdout_raw.strip_suffix(b"\n").unwrap_or(&o.stdout_raw);
    let p = path_from_bytes(raw);
    let p = if p.is_absolute() { p } else { repo.join(p) };
    match std::fs::read(&p) {
        Ok(bytes) => Ok(looks_like_ref_file(&bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        // A directory (`refs`, `objects`) is not a pseudo-ref either.
        Err(e) if p.is_dir() => {
            let _ = e;
            Ok(false)
        }
        Err(e) => Err(VcError::new(ErrorKind::Io, format!("{}: {e}", p.display()))),
    }
}

/// `ref: <target>`, or a 40/64-hex object id as the first line's first
/// whitespace-delimited token — `FETCH_HEAD` and `MERGE_HEAD` follow the
/// id with tab-separated metadata (`<id>\t\tbranch 'x' of <url>`).
fn looks_like_ref_file(bytes: &[u8]) -> bool {
    let first = bytes.split(|b| *b == b'\n').next().unwrap_or(&[]);
    let first = first.strip_suffix(b"\r").unwrap_or(first);
    if let Some(t) = first.strip_prefix(b"ref: ") {
        return t.starts_with(b"refs/");
    }
    let id = first
        .split(|b| *b == b'\t' || *b == b' ')
        .next()
        .unwrap_or(&[]);
    (id.len() == 40 || id.len() == 64) && id.iter().all(u8::is_ascii_hexdigit)
}

/// A revision EXPRESSION (`HEAD~3`, a sha, `main` when the caller has
/// already decided it is not an exact ref) peeled to a commit; `None`
/// when git cannot resolve it.
fn commit_of_expr(repo: &Path, expr: &str) -> VcResult<Option<String>> {
    let o = git(
        repo,
        &["rev-parse", "--verify", "-q", &format!("{expr}^{{commit}}")],
    )?;
    Ok(if o.ok() {
        Some(strip_nl(&o.stdout).to_string())
    } else {
        None
    })
}

/// The commit an exact ref names, `None` when the ref is confirmed
/// missing, and an error when the lookup itself failed — base
/// resolution must never step past a candidate it could not check.
fn commit_of(repo: &Path, full: &str) -> VcResult<Option<String>> {
    match lookup_ref(repo, full)? {
        RefLookup::Found(c) => Ok(Some(c)),
        RefLookup::Missing => Ok(None),
        RefLookup::Failed(why) => Err(VcError::new(
            ErrorKind::Io,
            format!("base candidate {full}: {why}"),
        )),
    }
}

/// If `full` is a symbolic ref (`refs/remotes/origin/HEAD`, `HEAD`), the
/// ref it points at; otherwise `full` unchanged. `symbolic-ref -q` exits
/// 1 for a plain (non-symbolic) ref; any other failure is an error, so a
/// base cannot silently keep the name `HEAD` as its identity.
fn deref_symbolic(repo: &Path, full: &str) -> VcResult<String> {
    let o = git(repo, &["symbolic-ref", "-q", "--", full])?;
    match o.status {
        Some(0) if !strip_nl(&o.stdout).is_empty() => Ok(strip_nl(&o.stdout).to_string()),
        Some(0) | Some(1) => Ok(full.to_string()),
        _ => Err(VcError::new(
            ErrorKind::Io,
            format!("git symbolic-ref {full} failed: {}", o.stderr.trim()),
        )),
    }
}

/// `git rev-list --left-right --count base...head` -> (behind, ahead).
fn ahead_behind(repo: &Path, base: &str, head: &str) -> VcResult<Option<(u64, u64)>> {
    let spec = format!("{base}...{head}");
    let o = git(repo, &["rev-list", "--left-right", "--count", &spec])?;
    if !o.ok() {
        return Ok(None);
    }
    let mut it = o.stdout.split_whitespace();
    let behind = it.next().and_then(|s| s.parse::<u64>().ok());
    let ahead = it.next().and_then(|s| s.parse::<u64>().ok());
    Ok(behind.zip(ahead))
}

fn last_commit_unix(repo: &Path, head: &str) -> VcResult<Option<u64>> {
    // `--no-show-signature`: with `log.showSignature=true` a signed commit
    // would print verification text around the one integer wanted here.
    let o = git(
        repo,
        &[
            "log",
            "-1",
            "--no-show-signature",
            "--format=%ct",
            head,
            "--",
        ],
    )?;
    if !o.ok() {
        return Ok(None);
    }
    Ok(o.stdout.trim().parse::<u64>().ok())
}

/// `git merge-base --is-ancestor`: `Ok(Some(true))` on exit 0,
/// `Ok(Some(false))` on exit 1, `Ok(None)` for any other exit — the
/// answer is then unknown, and the worktree is reported so rather than
/// allowed to fall through to a `merge-ready` it may not deserve.
fn is_ancestor(repo: &Path, head: &str, base: &str) -> VcResult<Option<bool>> {
    let o = git(repo, &["merge-base", "--is-ancestor", head, base])?;
    Ok(match o.status {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    })
}

fn merge_tree(repo: &Path, base: &str, head: &str) -> VcResult<MergeTree> {
    let o = git(
        repo,
        &["merge-tree", "--write-tree", "--messages", base, head],
    )?;
    Ok(match o.status {
        Some(0) => MergeTree::Clean,
        Some(1) => MergeTree::Conflict,
        Some(code) => {
            let err = o.stderr.trim();
            let why = if err.starts_with("usage:")
                || err.contains("unknown option")
                || err.contains("--write-tree")
            {
                "git too old for --write-tree (needs 2.38+)".to_string()
            } else {
                let first = err.lines().next().unwrap_or("").to_string();
                format!("exit {code}: {first}")
            };
            MergeTree::Unsupported(why)
        }
        None => MergeTree::Unsupported("killed by signal".to_string()),
    })
}

/// `None` when clean; `Some(summary)` otherwise. An unreadable worktree
/// is reported as dirty rather than as clean: "clean" is the state a
/// later cleanup pass would act on, so it must never be the default on
/// failure.
fn dirty_summary(wt: &Path) -> VcResult<Option<String>> {
    let o = git(
        wt,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=normal",
            "--ignore-submodules=none",
            "--no-renames",
        ],
    )?;
    if !o.ok() {
        let first = o.stderr.lines().next().unwrap_or("").to_string();
        return Ok(Some(format!("status failed ({first})")));
    }
    let lines: Vec<&str> = o.stdout.lines().filter(|l| !l.is_empty()).collect();
    Ok(match lines.as_slice() {
        [] => None,
        [one] => Some(one.trim().to_string()),
        [first, rest @ ..] => Some(format!("{} (+{} more)", first.trim(), rest.len())),
    })
}

/// Run the whole scan. Errors only for "not a git repo", "git not
/// runnable", or an unresolvable base; a per-worktree failure lands in
/// that worktree's `unknown` line instead.
pub fn doctor(cwd: &Path, args: &DoctorArgs<'_>) -> VcResult<DoctorReport> {
    let start = args.repo.unwrap_or(cwd);
    let top = git(start, &["rev-parse", "--show-toplevel"])?;
    if !top.ok() {
        return Err(VcError::new(
            ErrorKind::NotFound,
            format!("{}: not inside a git work tree", start.display()),
        )
        .with_next("vc worktree doctor --repo <path>"));
    }
    // Exactly the one `\n` terminator git prints — a directory name that
    // itself ends in a newline keeps it.
    let raw = top
        .stdout_raw
        .strip_suffix(b"\n")
        .unwrap_or(&top.stdout_raw);
    let repo_root = path_from_bytes(raw);

    let mut base = resolve_base(&repo_root, args.base)?;
    if args.fetch {
        // Fetch the remote the BASE lives on — git's default remote is
        // not necessarily it — with an explicit refspec for the base
        // branch, so the remote's configured refspecs (which may not
        // cover it) do not decide whether the base is refreshed; then
        // resolve the base again so the report compares against what
        // was just fetched. `+` because a remote-tracking ref is a
        // mirror, and a rewound remote branch should be mirrored too.
        let refspec;
        // `--no-prune --no-prune-tags`: a `fetch.prune`/`remote.<r>.prune`
        // setting would otherwise delete stale remote-tracking refs as a
        // side effect of a report that promises to delete nothing.
        // `--no-auto-maintenance`: fetch's automatic gc would otherwise
        // run `worktree prune` on expired registrations (a stale one is
        // exactly what the report exists to list) and could remove
        // objects under a dangling worktree's HEAD.
        let mut argv = vec![
            "fetch",
            "--quiet",
            "--no-prune",
            "--no-prune-tags",
            "--no-auto-maintenance",
        ];
        if let Some(BaseBranch::Remote(remote, branch)) = &base.branch {
            // The SOURCE is whatever the remote's configured fetch
            // mapping names for this destination (`+refs/heads/release:
            // refs/remotes/upstream/main` fetches `release` into
            // `upstream/main`); only when no mapping covers the base is
            // the same-named remote branch assumed.
            let src = fetch_source_for(
                &repo_root,
                remote,
                &base.full,
                &format!("refs/heads/{branch}"),
            )?;
            refspec = format!("+{src}:{}", base.full);
            // `--`: a remote is allowed to be named like an option
            // (`--all`), and git would otherwise read it as one.
            argv.push("--");
            argv.push(remote);
            argv.push(&refspec);
        } else if !base.is_ref {
            // An expression (`upstream/main~1`, `refs/remotes/x/y^2`, a
            // sha) may hang off any remote-tracking ref, and which ones —
            // and through which fetch mappings — is not derivable from
            // its text. `fetch --all` would only follow each remote's
            // configured refspecs and could leave the very ref the
            // expression reads stale while claiming to have refreshed
            // it. Refused: name the ref, or fetch first.
            return Err(VcError::new(
                ErrorKind::Usage,
                format!(
                    "--fetch: {} is a revision expression, not a ref; the remote-tracking refs it depends on cannot be determined, so nothing is fetched",
                    base.display
                ),
            )
            .with_next("run `git fetch <remote>` yourself, then re-run without --fetch, or pass --base <ref>"));
        }
        let f = git(&repo_root, &argv)?;
        if !f.ok() {
            return Err(VcError::new(
                ErrorKind::Io,
                format!("git {} failed: {}", argv.join(" "), f.stderr.trim()),
            ));
        }
        base = resolve_base(&repo_root, args.base)?;
    }
    let stale_secs = args.stale_days.saturating_mul(86_400);

    let list = git(&repo_root, &["worktree", "list", "--porcelain", "-z"])?;
    if !list.ok() {
        return Err(VcError::new(
            ErrorKind::Io,
            format!("git worktree list failed: {}", list.stderr.trim()),
        ));
    }
    let entries = parse_porcelain(&list.stdout_raw);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut worktrees = Vec::new();
    let mut main_path = None;
    let mut main_branch = None;
    for (i, e) in entries.iter().enumerate() {
        // The first porcelain record is always the main worktree (or the
        // bare repository) — the thing the others are measured against.
        if i == 0 {
            main_path = Some(e.path.clone());
            main_branch = e.branch.clone();
            continue;
        }
        worktrees.push(gather(&repo_root, e, &base, stale_secs, now)?);
    }

    Ok(DoctorReport {
        repo_root,
        base_source: base.source,
        base_full: base.full,
        base_commit: base.commit,
        base: base.display,
        main_path,
        main_branch,
        worktrees,
    })
}

fn gather(
    repo: &Path,
    e: &WorktreeEntry,
    base: &Base,
    stale_secs: u64,
    now: u64,
) -> VcResult<Report> {
    // git prints the all-zero id for an unborn HEAD (a worktree whose
    // branch ref was deleted); it names no commit, so treat it as absent.
    let head = e
        .head
        .as_deref()
        .filter(|h| !h.is_empty() && h.bytes().any(|b| b != b'0'));
    let mut facts = Facts {
        prunable: e.prunable.clone(),
        dirty: None,
        detached: e.detached || e.branch.is_none(),
        is_base: false,
        branch_gone: false,
        query_failed: None,
        is_ancestor: None,
        merge_tree: None,
        age_secs: None,
    };
    let mut ahead = None;
    let mut behind = None;
    let mut last_commit = None;

    // Short-circuit in the same order `classify` decides, so no worktree
    // pays for a query whose answer cannot change its state — and so a
    // prunable (missing) directory is never `cd`-ed into.
    if facts.prunable.is_none() {
        if let Some(b) = &e.branch {
            // Existence/peelability come from a fresh lookup; the commit
            // compared against the base is the one the porcelain listing
            // reported for THIS worktree, so a branch that moves between
            // the two queries cannot make `is_base` disagree with the
            // row's own head and ahead/behind.
            match lookup_ref(repo, &format!("refs/heads/{b}"))? {
                RefLookup::Found(_) => facts.is_base = branch_is_base(b, head.unwrap_or(""), base),
                RefLookup::Missing => facts.branch_gone = true,
                RefLookup::Failed(why) => facts.query_failed = Some(why),
            }
        }
        if !facts.branch_gone {
            facts.dirty = dirty_summary(&e.path)?;
        }
        if let Some(h) = head {
            if let Some((bh, ah)) = ahead_behind(repo, &base.commit, h)? {
                behind = Some(bh);
                ahead = Some(ah);
            }
            last_commit = last_commit_unix(repo, h)?;
            facts.age_secs = last_commit.map(|t| now.saturating_sub(t));
            if facts.dirty.is_none()
                && facts.query_failed.is_none()
                && !facts.detached
                && !facts.is_base
                && !facts.branch_gone
            {
                // Every comparison uses the commit captured at resolution,
                // never the ref name: a ref that moves mid-scan (a fetch,
                // a commit on another worktree) would otherwise make the
                // rows disagree with each other and with `base_commit`.
                facts.is_ancestor = is_ancestor(repo, h, &base.commit)?;
                if facts.is_ancestor == Some(false) {
                    facts.merge_tree = Some(merge_tree(repo, &base.commit, h)?);
                }
            }
        }
    }

    let (state, mut reason) = classify(&facts, stale_secs);
    if let Some(l) = &e.locked {
        if l.is_empty() {
            reason.push_str(" [locked]");
        } else {
            reason.push_str(&format!(" [locked: {l}]"));
        }
    }
    Ok(Report {
        state,
        path: e.path.clone(),
        branch: e.branch.clone(),
        head: e.head.clone(),
        ahead,
        behind,
        locked: e.locked.clone(),
        last_commit_unix: last_commit,
        reason,
    })
}

/// A worktree "is the base" when its local branch IS the base's branch:
/// the same local branch (`refs/heads/main` for a base of
/// `refs/heads/main`), or — for a remote-tracking base — the local
/// branch of the same name at the same commit (a local `main` that has
/// drifted from `origin/main` is genuinely ahead/behind and deserves a
/// real classification). Tags, bare shas and other local branches never
/// qualify, whatever commit they share.
fn branch_is_base(branch: &str, local_commit: &str, base: &Base) -> bool {
    match &base.branch {
        Some(BaseBranch::Local(b)) => b == branch,
        Some(BaseBranch::Remote(_, b)) => b == branch && local_commit == base.commit,
        None => false,
    }
}

/// The summary shows the canonical ref only when it says something the
/// display name does not — `main` for `refs/heads/main` is noise, `main`
/// for `refs/tags/main` is the point.
fn base_is_plainly(display: &str, full: &str) -> bool {
    display == full
        || full == format!("refs/heads/{display}")
        || full == format!("refs/remotes/{display}")
}

pub fn counts(r: &DoctorReport) -> BTreeMap<&'static str, usize> {
    let mut m = BTreeMap::new();
    for w in &r.worktrees {
        *m.entry(w.state.label()).or_insert(0) += 1;
    }
    m
}

fn fmt_opt(n: Option<u64>) -> String {
    n.map(|v| v.to_string()).unwrap_or_else(|| "?".to_string())
}

pub fn format_human(r: &DoctorReport) -> String {
    let mut s = String::new();
    for w in &r.worktrees {
        s.push_str(&format!(
            "{}\t{}\t{}\t{}/{}\t{}\n",
            w.state.label(),
            escape_cell(&w.path.display().to_string()),
            escape_cell(w.branch.as_deref().unwrap_or("(detached)")),
            fmt_opt(w.ahead),
            fmt_opt(w.behind),
            escape_cell(&w.reason)
        ));
    }
    let c = counts(r);
    let tally = if c.is_empty() {
        "none".to_string()
    } else {
        c.iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    s.push_str(&format!(
        "summary: {} linked worktree(s) — {tally}; base {}{} (from {}); main {} ({}); report-only, nothing was changed\n",
        r.worktrees.len(),
        r.base,
        if base_is_plainly(&r.base, &r.base_full) {
            String::new()
        } else {
            format!(" [{}]", r.base_full)
        },
        r.base_source,
        escape_cell(
            &r.main_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "?".to_string())
        ),
        escape_cell(r.main_branch.as_deref().unwrap_or("(detached)"))
    ));
    s
}

pub fn to_json(r: &DoctorReport) -> serde_json::Value {
    let wts: Vec<serde_json::Value> = r
        .worktrees
        .iter()
        .map(|w| {
            serde_json::json!({
                "state": w.state.label(),
                "path": w.path.display().to_string(),
                "branch": w.branch,
                "head": w.head,
                "ahead": w.ahead,
                "behind": w.behind,
                "locked": w.locked.is_some(),
                "lock_reason": w.locked,
                "last_commit_unix": w.last_commit_unix,
                "reason": w.reason,
            })
        })
        .collect();
    let counts: serde_json::Map<String, serde_json::Value> = counts(r)
        .into_iter()
        .map(|(k, v)| (k.to_string(), serde_json::Value::from(v)))
        .collect();
    serde_json::json!({
        "repo_root": r.repo_root.display().to_string(),
        "base": r.base,
        "base_full": r.base_full,
        "base_commit": r.base_commit,
        "base_source": r.base_source,
        "main": {
            "path": r.main_path.as_ref().map(|p| p.display().to_string()),
            "branch": r.main_branch,
        },
        "worktrees": wts,
        "counts": counts,
        "report_only": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean_facts() -> Facts {
        Facts {
            prunable: None,
            dirty: None,
            detached: false,
            is_base: false,
            branch_gone: false,
            query_failed: None,
            is_ancestor: Some(false),
            merge_tree: Some(MergeTree::Clean),
            age_secs: Some(0),
        }
    }

    #[test]
    fn porcelain_z_parses_every_attribute_and_keeps_paths_verbatim() {
        // `-z` framing: NUL after every attribute, an extra NUL per record.
        let text = "worktree /r/main repo\0HEAD aaaa\0branch refs/heads/main\0\0\
                    worktree /r/wt one \0HEAD bbbb\0branch refs/heads/feat/x\0locked busy\treason\0\0\
                    worktree /r/wt2\0HEAD cccc\0detached\0prunable gitdir file points to non-existent location\0\0\
                    worktree /r/new\nline\0HEAD dddd\0branch refs/heads/y\0locked\0\0";
        let v = parse_porcelain(text.as_bytes());
        assert_eq!(v.len(), 4);
        assert_eq!(v[0].path, PathBuf::from("/r/main repo"));
        assert_eq!(v[0].branch.as_deref(), Some("main"));
        assert_eq!(v[1].path, PathBuf::from("/r/wt one "));
        assert_eq!(v[1].branch.as_deref(), Some("feat/x"));
        assert_eq!(v[1].locked.as_deref(), Some("busy\treason"));
        assert!(v[2].detached);
        assert!(v[2].branch.is_none());
        assert_eq!(
            v[2].prunable.as_deref(),
            Some("gitdir file points to non-existent location")
        );
        assert_eq!(v[3].path, PathBuf::from("/r/new\nline"));
        assert_eq!(v[3].locked.as_deref(), Some(""));
        assert_eq!(v[3].head.as_deref(), Some("dddd"));
    }

    #[test]
    fn porcelain_without_trailing_record_nul_still_yields_last_record() {
        let v = parse_porcelain(b"worktree /a\0HEAD 1\0branch refs/heads/b");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].branch.as_deref(), Some("b"));
    }

    #[test]
    fn porcelain_empty_input_is_no_records() {
        assert!(parse_porcelain(b"").is_empty());
        assert!(parse_porcelain(b"\0\0").is_empty());
    }

    #[test]
    fn ref_file_detection_accepts_symbolic_and_oid_rejects_other_files() {
        assert!(looks_like_ref_file(b"ref: refs/heads/main\n"));
        assert!(looks_like_ref_file(b"ref: refs/remotes/origin/x"));
        assert!(looks_like_ref_file(
            b"0123456789abcdef0123456789abcdef01234567\n"
        ));
        assert!(looks_like_ref_file(
            b"0123456789abcdef0123456789abcdef01234567\t\tbranch 'x' of /r\n"
        ));
        assert!(!looks_like_ref_file(b"ref: notrefs/x\n"));
        assert!(!looks_like_ref_file(b"0123 not an id\n"));
        assert!(!looks_like_ref_file(b"DIRC\0\0\0\x02")); // an index file
        assert!(!looks_like_ref_file(
            b"[core]\n\trepositoryformatversion = 0\n"
        ));
        assert!(!looks_like_ref_file(b""));
    }

    #[test]
    fn refspec_glob_match_returns_the_wildcard_text() {
        assert_eq!(
            glob_match("refs/heads/*", "refs/heads/a/b"),
            Some("a/b".into())
        );
        assert_eq!(
            glob_match("refs/heads/rel*", "refs/heads/release"),
            Some("ease".into())
        );
        assert_eq!(
            glob_match("refs/heads/main", "refs/heads/main"),
            Some(String::new())
        );
        assert_eq!(glob_match("refs/heads/main", "refs/heads/maine"), None);
        assert_eq!(glob_match("refs/heads/*", "refs/tags/x"), None);
    }

    #[test]
    fn human_cells_escape_tabs_and_newlines() {
        assert_eq!(escape_cell("a\tb\nc\\d"), "a\\tb\\nc\\\\d");
        assert_eq!(escape_cell("plain"), "plain");
    }

    #[test]
    fn branch_is_base_needs_the_exact_branch_not_a_shared_commit() {
        let local = Base {
            display: "main".into(),
            branch: Some(BaseBranch::Local("main".into())),
            full: "refs/heads/main".into(),
            commit: "c1".into(),
            source: "fallback",
            is_ref: true,
        };
        assert!(branch_is_base("main", "c1", &local));
        assert!(branch_is_base("main", "c9", &local), "same ref, any commit");
        assert!(!branch_is_base("other/main", "c1", &local));
        // A tag base (no branch identity) never makes a branch "base".
        let tag = Base {
            branch: None,
            full: "refs/tags/main".into(),
            ..local.clone()
        };
        assert!(!branch_is_base("main", "c1", &tag));
        // Remote-tracking base: exact branch name AND same commit.
        let remote = Base {
            display: "origin/feat/x".into(),
            branch: Some(BaseBranch::Remote("origin".into(), "feat/x".into())),
            full: "refs/remotes/origin/feat/x".into(),
            commit: "c1".into(),
            source: "origin/HEAD",
            is_ref: true,
        };
        assert!(branch_is_base("feat/x", "c1", &remote));
        assert!(!branch_is_base("feat/x", "c2", &remote), "drifted local");
        assert!(!branch_is_base("x", "c1", &remote), "suffix is not enough");
    }

    #[test]
    fn classify_priority_order_is_fixed() {
        let stale = 14 * 86_400;
        let mut f = clean_facts();
        assert_eq!(classify(&f, stale).0, State::MergeReady);

        f.age_secs = Some(stale + 1);
        assert_eq!(classify(&f, stale).0, State::Stale);
        f.age_secs = Some(stale);
        assert_eq!(
            classify(&f, stale).0,
            State::MergeReady,
            "boundary is exclusive"
        );

        f.merge_tree = Some(MergeTree::Unsupported("x".into()));
        assert_eq!(classify(&f, stale).0, State::Unknown);
        f.merge_tree = Some(MergeTree::Conflict);
        assert_eq!(classify(&f, stale).0, State::Conflict);

        // a failed ancestry query is unknown, never merge-ready.
        f.is_ancestor = None;
        assert_eq!(classify(&f, stale).0, State::Unknown);
        // merged beats conflict: an ancestor cannot conflict.
        f.is_ancestor = Some(true);
        assert_eq!(classify(&f, stale).0, State::Merged);
        // base beats merged.
        f.is_base = true;
        assert_eq!(classify(&f, stale).0, State::Base);
        // detached beats base.
        f.detached = true;
        assert_eq!(classify(&f, stale).0, State::Detached);
        // a failed ref lookup beats detached, but not dirty.
        f.query_failed = Some("boom".into());
        assert_eq!(classify(&f, stale).0, State::Unknown);
        // dirty beats a failed lookup.
        f.dirty = Some("?? x".into());
        assert_eq!(classify(&f, stale).0, State::Dirty);
        // a gone branch beats dirty: its "dirt" is an unborn HEAD.
        f.branch_gone = true;
        let (s, r) = classify(&f, stale);
        assert_eq!(s, State::Stale);
        assert!(r.contains("branch no longer exists"), "{r}");
        // prunable beats everything.
        f.prunable = Some(String::new());
        let (s, r) = classify(&f, stale);
        assert_eq!(s, State::Prunable);
        assert_eq!(r, "git reports prunable: gone");
    }

    #[test]
    fn classify_without_merge_tree_result_is_unknown_not_merge_ready() {
        let mut f = clean_facts();
        f.merge_tree = None;
        assert_eq!(classify(&f, 1).0, State::Unknown);
    }

    #[test]
    fn classify_with_unknown_age_is_unknown_not_merge_ready() {
        let mut f = clean_facts();
        f.age_secs = None;
        let (s, r) = classify(&f, 1);
        assert_eq!(s, State::Unknown);
        assert!(r.contains("last commit date unreadable"), "{r}");
        // ...but the higher-priority states still win over a missing age.
        f.is_ancestor = Some(true);
        assert_eq!(classify(&f, 1).0, State::Merged);
        f.is_ancestor = Some(false);
        f.merge_tree = Some(MergeTree::Conflict);
        assert_eq!(classify(&f, 1).0, State::Conflict);
    }
}
