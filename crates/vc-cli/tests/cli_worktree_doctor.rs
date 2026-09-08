//! `vc worktree doctor` end to end against a real temporary repository:
//! one linked worktree per lifecycle state, the classification of each,
//! the JSON shape, and — through a `git` shim placed first on PATH — the
//! proof that no mutating git command is ever spawned and that a git too
//! old for `merge-tree --write-tree` degrades to `unknown`, not a crash.

use assert_cmd::Command;
use std::path::{Path, PathBuf};

fn vc(dir: &Path) -> Command {
    let mut c = Command::cargo_bin("vc").unwrap();
    c.current_dir(dir);
    // The doctor must not inherit a GIT_DIR from the harness that runs
    // these tests (some CI runners export one).
    c.env_remove("GIT_DIR");
    c.env_remove("GIT_WORK_TREE");
    c
}

/// Run `git -C dir args` with a fixed identity and, when `date` is given,
/// a fixed author/committer date (`GIT_COMMITTER_DATE`, the one `%ct`
/// reads) so a "stale by age" worktree is stale by construction.
fn git(dir: &Path, args: &[&str], date: Option<&str>) -> String {
    let mut c = std::process::Command::new("git");
    c.arg("-C").arg(dir).args(args);
    // Every repository a fixture creates — the shared one, bare remotes,
    // clones, submodules — uses the same object format, whatever the
    // caller's GIT_DEFAULT_HASH says, so pushes between them cannot fail
    // on a format mismatch and the id-width tests mean what they say.
    c.env("GIT_DEFAULT_HASH", "sha1");
    c.env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t");
    // The same override list the doctor strips: a fixture `git add -A`
    // must never write to a caller's inherited index or object store.
    for k in [
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
    ] {
        c.env_remove(k);
    }
    if let Some(d) = date {
        c.env("GIT_AUTHOR_DATE", d).env("GIT_COMMITTER_DATE", d);
    }
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn commit_file(dir: &Path, file: &str, content: &str, msg: &str, date: Option<&str>) {
    std::fs::write(dir.join(file), content).unwrap();
    git(dir, &["add", "-A"], None);
    git(dir, &["commit", "-q", "-m", msg], date);
}

/// A repo with `main` as base and one linked worktree per state. Paths
/// contain a space on purpose. Returns (tempdir, repo root, map of
/// state name -> worktree path).
struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    wt: std::collections::BTreeMap<&'static str, PathBuf>,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    // git reports canonical paths (`/private/var/...` on macOS for a
    // `/var/...` tempdir), so every expected path is built from the
    // canonical tempdir.
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("repo with space");
    std::fs::create_dir_all(&root).unwrap();
    // `--object-format=sha1` explicitly: the tests that reason about id
    // widths must not depend on a `GIT_DEFAULT_HASH` in the environment.
    git(
        &root,
        &["init", "-q", "-b", "main", "--object-format=sha1"],
        None,
    );
    commit_file(&root, "a.txt", "one\n", "base 1", None);
    commit_file(&root, "b.txt", "b\n", "base 2", None);

    // Linked worktrees live beside the repo, not inside it: inside, the
    // fixture's own `git add -A` on the main worktree would stage them
    // as gitlinks and every state below would collapse into "dirty".
    let wts = base.join("wt dir");
    std::fs::create_dir_all(&wts).unwrap();
    let mut wt = std::collections::BTreeMap::new();

    // merge-ready: one commit ahead touching a new file, recent.
    let p = wts.join("ready wt");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat/ready",
            p.to_str().unwrap(),
        ],
        None,
    );
    commit_file(&p, "ready.txt", "r\n", "ready", None);
    wt.insert("merge-ready", p);

    // conflict: base and branch both rewrite a.txt line 1.
    let p = wts.join("conflict");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat/conflict",
            p.to_str().unwrap(),
        ],
        None,
    );
    commit_file(&p, "a.txt", "branch side\n", "branch edit", None);
    commit_file(&root, "a.txt", "main side\n", "main edit", None);
    wt.insert("conflict", p);

    // merged: branched off, no commits of its own -> HEAD ancestor of main.
    let p = wts.join("merged");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat/merged",
            p.to_str().unwrap(),
        ],
        None,
    );
    wt.insert("merged", p);

    // stale: one commit ahead, dated 40 days back.
    let p = wts.join("stale");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat/stale",
            p.to_str().unwrap(),
        ],
        None,
    );
    let old = format!(
        "{} +0000",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 40 * 86_400
    );
    commit_file(&p, "stale.txt", "s\n", "stale work", Some(&old));
    wt.insert("stale", p);

    // dirty: ahead AND has an untracked file — dirty must win.
    let p = wts.join("dirty");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat/dirty",
            p.to_str().unwrap(),
        ],
        None,
    );
    commit_file(&p, "d.txt", "d\n", "dirty work", None);
    std::fs::write(p.join("scratch.txt"), "wip\n").unwrap();
    wt.insert("dirty", p);

    // branch-gone: worktree on a branch whose ref is then deleted out
    // from under it (`branch -D` refuses while a worktree has it checked
    // out; `update-ref -d` is how it happens in practice — a remote
    // prune, a `git branch -D` from before the worktree existed, a
    // packed-refs edit). The linked worktree's HEAD file still names it.
    let p = wts.join("gone");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat/gone",
            p.to_str().unwrap(),
        ],
        None,
    );
    commit_file(&p, "g.txt", "g\n", "gone work", None);
    git(&root, &["update-ref", "-d", "refs/heads/feat/gone"], None);
    wt.insert("branch-gone", p);

    // detached: checked out at a commit, no branch.
    let p = wts.join("detached");
    let head = git(&root, &["rev-parse", "HEAD"], None);
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            p.to_str().unwrap(),
            head.trim(),
        ],
        None,
    );
    wt.insert("detached", p);

    // base: a linked worktree that has `main` itself checked out is not
    // possible (main is checked out in the main worktree), so exercise
    // `base` by pointing the doctor at a base that IS a linked worktree's
    // branch in a dedicated test instead. Here: nothing.

    // locked: merge-ready plus a lock; the lock is a flag, not a state.
    let p = wts.join("locked");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat/locked",
            p.to_str().unwrap(),
        ],
        None,
    );
    commit_file(&p, "l.txt", "l\n", "locked work", None);
    git(
        &root,
        &[
            "worktree",
            "lock",
            "--reason",
            "hands off",
            p.to_str().unwrap(),
        ],
        None,
    );
    wt.insert("locked", p);

    // prunable: a worktree whose directory was deleted out from under git.
    let p = wts.join("prunable");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat/prunable",
            p.to_str().unwrap(),
        ],
        None,
    );
    std::fs::remove_dir_all(&p).unwrap();
    wt.insert("prunable", p);

    Fixture {
        _tmp: tmp,
        root,
        wt,
    }
}

fn head_of(wt: &Path) -> String {
    git(wt, &["rev-parse", "HEAD"], None).trim().to_string()
}

fn doctor_json(dir: &Path, extra: &[&str]) -> serde_json::Value {
    let mut args = vec!["--json", "worktree", "doctor"];
    args.extend_from_slice(extra);
    let out = vc(dir)
        .args(&args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).unwrap()
}

/// One spelling for a path whichever side produced it: git prints
/// forward slashes and no `\\?\` prefix, `canonicalize` on Windows
/// produces both. Case is left alone — the fixture never relies on it.
fn norm(p: &str) -> String {
    p.strip_prefix(r"\\?\").unwrap_or(p).replace('\\', "/")
}

fn by_path<'a>(v: &'a serde_json::Value, p: &Path) -> &'a serde_json::Value {
    let want = norm(&p.display().to_string());
    v["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["path"].as_str().map(norm) == Some(want.clone()))
        .unwrap_or_else(|| panic!("no worktree entry for {want}\n{v:#}"))
}

#[test]
fn every_lifecycle_state_is_classified_exactly_once() {
    let f = fixture();
    // `log.showSignature=true` decorates `git log` output for signed
    // commits; the doctor's timestamp query must not be affected by the
    // setting (`--no-show-signature`). The fixture's commits are unsigned
    // — a signed-commit fixture needs a signing key, which CI has not.
    git(&f.root, &["config", "log.showSignature", "true"], None);
    let v = doctor_json(&f.root, &[]);

    assert_eq!(v["base"], "main");
    assert_eq!(v["base_source"], "fallback");
    assert_eq!(v["report_only"], true);
    assert_eq!(v["main"]["branch"], "main");
    assert_eq!(v["worktrees"].as_array().unwrap().len(), f.wt.len());

    let expect = [
        ("merge-ready", "merge-ready"),
        ("conflict", "conflict"),
        ("merged", "merged"),
        ("stale", "stale"),
        ("dirty", "dirty"),
        ("branch-gone", "stale"),
        ("detached", "detached"),
        ("locked", "merge-ready"),
        ("prunable", "prunable"),
    ];
    for (key, state) in expect {
        let w = by_path(&v, &f.wt[key]);
        assert_eq!(w["state"], state, "{key}: {w:#}");
    }

    // Ahead/behind and the reasons carry the discriminating facts.
    let ready = by_path(&v, &f.wt["merge-ready"]);
    assert_eq!(ready["ahead"], 1);
    assert_eq!(ready["behind"], 1, "main moved on by the conflict edit");
    assert_eq!(ready["branch"], "feat/ready");
    assert_eq!(ready["locked"], false);

    let merged = by_path(&v, &f.wt["merged"]);
    assert_eq!(merged["ahead"], 0);

    let stale = by_path(&v, &f.wt["stale"]);
    assert!(
        stale["reason"].as_str().unwrap().contains("40 days old"),
        "{stale:#}"
    );
    assert!(stale["last_commit_unix"].as_u64().unwrap() > 0);

    let dirty = by_path(&v, &f.wt["dirty"]);
    assert_eq!(dirty["ahead"], 1, "dirty still reports ahead/behind");

    let gone = by_path(&v, &f.wt["branch-gone"]);
    assert!(
        gone["reason"]
            .as_str()
            .unwrap()
            .starts_with("branch no longer exists"),
        "{gone:#}"
    );
    assert_eq!(gone["branch"], "feat/gone");
    assert!(gone["ahead"].is_null(), "unborn HEAD has no ahead/behind");

    let det = by_path(&v, &f.wt["detached"]);
    assert!(det["branch"].is_null());

    let locked = by_path(&v, &f.wt["locked"]);
    assert_eq!(locked["locked"], true);
    assert_eq!(locked["lock_reason"], "hands off");
    assert!(
        locked["reason"]
            .as_str()
            .unwrap()
            .ends_with("[locked: hands off]")
    );

    let prun = by_path(&v, &f.wt["prunable"]);
    assert!(
        prun["reason"]
            .as_str()
            .unwrap()
            .starts_with("git reports prunable:")
    );

    let c = &v["counts"];
    assert_eq!(c["stale"], 2);
    assert_eq!(c["merge-ready"], 2);
    assert_eq!(c["dirty"], 1);
    assert_eq!(c["conflict"], 1);
    assert_eq!(c["merged"], 1);
    assert_eq!(c["detached"], 1);
    assert_eq!(c["prunable"], 1);
}

#[test]
fn human_output_is_one_tab_line_per_worktree_plus_summary() {
    let f = fixture();
    let out = vc(&f.root)
        .args(["worktree", "doctor"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), f.wt.len() + 1);
    for l in &lines[..lines.len() - 1] {
        let cols: Vec<&str> = l.split('\t').collect();
        assert_eq!(cols.len(), 5, "five tab columns: {l:?}");
        assert!(cols[3].contains('/'), "ahead/behind column: {l:?}");
    }
    // A path with a space survives the tab format intact.
    let ready = norm(&f.wt["merge-ready"].display().to_string());
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("merge-ready\t") && norm(l).contains(&ready)),
        "{text}"
    );
    let summary = lines.last().unwrap();
    assert!(
        summary.starts_with("summary: 9 linked worktree(s)"),
        "{summary}"
    );
    assert!(summary.contains("base main (from fallback)"));
    assert!(summary.contains("report-only, nothing was changed"));
    // The main worktree is named in the summary, never as a row.
    assert!(
        !lines[..lines.len() - 1]
            .iter()
            .any(|l| l.split('\t').nth(2) == Some("main"))
    );
    assert!(summary.contains("main "));
}

#[test]
fn stale_days_moves_the_boundary_and_base_flag_names_its_source() {
    let f = fixture();
    // With a 60-day window the 40-day-old branch is merge-ready.
    let v = doctor_json(&f.root, &["--stale-days", "60"]);
    assert_eq!(by_path(&v, &f.wt["stale"])["state"], "merge-ready");
    // Explicit --base is honoured and reported.
    let v = doctor_json(&f.root, &["--base", "main"]);
    assert_eq!(v["base_source"], "--base");
    // A linked worktree whose branch IS the base is `base`, not `merged`.
    let v = doctor_json(&f.root, &["--base", "feat/ready"]);
    assert_eq!(by_path(&v, &f.wt["merge-ready"])["state"], "base");
    // Two clean linked branches at the SAME commit: only the one named
    // by --base is `base`; the other keeps a real classification
    // (`merged`, since it is an ancestor of its sibling).
    let twin = f.root.parent().unwrap().join("twin");
    git(
        &f.root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat/twin",
            twin.to_str().unwrap(),
            "feat/merged",
        ],
        None,
    );
    assert_eq!(head_of(&twin), head_of(&f.wt["merged"]));
    let v = doctor_json(&f.root, &["--base", "feat/merged"]);
    assert_eq!(by_path(&v, &f.wt["merged"])["state"], "base");
    assert_eq!(by_path(&v, &twin)["state"], "merged", "{v:#}");
}

#[test]
fn origin_head_wins_over_local_main_when_set() {
    let f = fixture();
    let remote = f.root.parent().unwrap().join("remote.git");
    git(
        &f.root,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            "--object-format=sha1",
            remote.to_str().unwrap(),
        ],
        None,
    );
    git(
        &f.root,
        &["remote", "add", "origin", remote.to_str().unwrap()],
        None,
    );
    git(&f.root, &["push", "-q", "origin", "main"], None);
    git(&f.root, &["remote", "set-head", "origin", "main"], None);
    let v = doctor_json(&f.root, &[]);
    assert_eq!(v["base"], "origin/main");
    assert_eq!(v["base_source"], "origin/HEAD");
    // origin/main == local main here, so classification is unchanged.
    assert_eq!(by_path(&v, &f.wt["conflict"])["state"], "conflict");
    assert_eq!(by_path(&v, &f.wt["merged"])["state"], "merged");
}

#[test]
fn repo_flag_scans_another_repository_from_an_unrelated_cwd() {
    let f = fixture();
    let elsewhere = tempfile::tempdir().unwrap();
    let v = doctor_json(elsewhere.path(), &["--repo", f.root.to_str().unwrap()]);
    assert_eq!(v["worktrees"].as_array().unwrap().len(), f.wt.len());
    // A cwd inside a linked worktree resolves to the same repository.
    let v2 = doctor_json(&f.wt["merged"], &[]);
    assert_eq!(v2["worktrees"].as_array().unwrap().len(), f.wt.len());
}

#[test]
fn not_a_git_repo_is_a_refusal_and_no_base_is_usage() {
    let d = tempfile::tempdir().unwrap();
    vc(d.path())
        .args(["worktree", "doctor"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicates::str::contains("not inside a git work tree"));

    let r = d.path().join("nobase");
    std::fs::create_dir_all(&r).unwrap();
    git(
        &r,
        &["init", "-q", "-b", "trunk", "--object-format=sha1"],
        None,
    );
    commit_file(&r, "x", "x\n", "c", None);
    vc(&r)
        .args(["worktree", "doctor"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("no base branch found"));
    // ...and --base fixes it (zero linked worktrees is a valid scan).
    let v = doctor_json(&r, &["--base", "trunk"]);
    assert_eq!(v["worktrees"].as_array().unwrap().len(), 0);
    vc(&r)
        .args(["worktree", "doctor", "--base", "trunk"])
        .assert()
        .success()
        .stdout(predicates::str::starts_with(
            "summary: 0 linked worktree(s) — none",
        ));
}

// ---- git shim tests (a POSIX `sh` shim on PATH; unix only) --------------

#[cfg(unix)]
mod shim {
    use super::*;

    // ---- git shim tests ------------------------------------------------------

    /// Write an executable `git` shim into `dir`. `body` is a POSIX sh
    /// fragment run with the original argv (never shifted, so the leading
    /// `-C <dir>` the doctor always passes still reaches the real git) and
    /// `REAL_GIT` set to the real binary; it may `exec "$REAL_GIT" "$@"`.
    /// `sub` is set to the git subcommand (the word after `-C <dir>`), and
    /// `arg2` to the word after that.
    fn write_git_shim(dir: &Path, body: &str) -> PathBuf {
        let real = which_git();
        let shim = dir.join("git");
        let script = format!(
            "#!/bin/sh\nREAL_GIT='{}'\nsub=\"$1\"; arg2=\"$2\"\nif [ \"$1\" = \"-C\" ]; then sub=\"$3\"; arg2=\"$4\"; fi\n{body}\n",
            real.display()
        );
        std::fs::write(&shim, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        shim
    }

    fn which_git() -> PathBuf {
        let out = std::process::Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap();
        PathBuf::from(String::from_utf8(out.stdout).unwrap().trim())
    }

    fn shimmed_path(shim_dir: &Path) -> std::ffi::OsString {
        let mut dirs = vec![shim_dir.to_path_buf()];
        dirs.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        std::env::join_paths(dirs).unwrap()
    }

    /// The one mutation-refusing shim body both shim tests use: every argv
    /// goes to `argv.log`; anything that is not one of the exact read-only
    /// query forms the doctor is known to issue is appended to
    /// `violations.log` and refused with exit 97 — an ALLOWLIST, so a new
    /// mutating verb (or `branch <new>`, which creates a ref with no flag
    /// at all) cannot slip past a denylist of spellings. `fetch` is not on
    /// the list: the doctor test never passes `--fetch`.
    fn mutation_shim_body(log: &Path, violations: &Path) -> String {
        format!(
            r#"printf '%s\n' "$*" >> '{log}'
printf 'env GIT_NO_LAZY_FETCH=%s GIT_OPTIONAL_LOCKS=%s\n' "${{GIT_NO_LAZY_FETCH:-unset}}" "${{GIT_OPTIONAL_LOCKS:-unset}}" >> '{log}'
refuse() {{ printf '%s\n' "$*" >> '{viol}'; echo "MUTATION: $*" >&2; exit 97; }}
ok=0
# Complete argument forms, argv positions counted WITH the leading `-C <dir>`
# ($1 $2) the doctor always passes: arity and every fixed operand pinned,
# so an extra option (`log --output=...`) is refused like any other verb.
# The doctor always passes `-C <dir>` first; an invocation without it is
# refused outright, so the positional checks below can rely on it.
{{ [ "$1" = "-C" ] && [ -n "$2" ]; }} || refuse "$@"
n=$#
# Operand validators: a full hex object id, `<id>...<id>`, a `refs/` name,
# a `<x>^{{commit}}` peel.
hex() {{ case "$1" in *[!0-9a-f]*|"") return 1 ;; esac; [ ${{#1}} -eq 40 ] || [ ${{#1}} -eq 64 ]; }}
range() {{ case "$1" in *...*) hex "${{1%%...*}}" && hex "${{1##*...}}" ;; *) return 1 ;; esac; }}
isref() {{ case "$1" in refs/*) return 0 ;; *) return 1 ;; esac; }}
peel() {{ case "$1" in *'^{{commit}}') return 0 ;; *) return 1 ;; esac; }}
case "$sub" in
  rev-parse)
    {{ [ $n -eq 4 ] && [ "$4" = "--show-toplevel" ]; }} && ok=1
    {{ [ $n -eq 4 ] && [ "$4" = "--show-object-format" ]; }} && ok=1
    {{ [ $n -eq 6 ] && [ "$4" = "--verify" ] && [ "$5" = "-q" ] && peel "$6"; }} && ok=1
    {{ [ $n -eq 5 ] && [ "$4" = "--git-path" ] && case "$5" in -*) false ;; *) true ;; esac; }} && ok=1 ;;
  show-ref)     {{ [ $n -eq 7 ] && [ "$4" = "--verify" ] && [ "$5" = "-q" ] && [ "$6" = "--" ] && isref "$7"; }} && ok=1 ;;
  symbolic-ref)
    {{ [ $n -eq 5 ] && [ "$4" = "-q" ] && case "$5" in -*) false ;; *) true ;; esac; }} && ok=1
    {{ [ $n -eq 6 ] && [ "$4" = "-q" ] && [ "$5" = "--" ]; }} && ok=1 ;;
  worktree)     {{ [ $n -eq 6 ] && [ "$4" = "list" ] && [ "$5" = "--porcelain" ] && [ "$6" = "-z" ]; }} && ok=1 ;;
  status)       {{ [ $n -eq 7 ] && [ "$4" = "--porcelain=v1" ] && [ "$5" = "--untracked-files=normal" ] && [ "$6" = "--ignore-submodules=none" ] && [ "$7" = "--no-renames" ]; }} && ok=1 ;;
  merge-base)   {{ [ $n -eq 6 ] && [ "$4" = "--is-ancestor" ] && hex "$5" && hex "$6"; }} && ok=1 ;;
  merge-tree)   {{ [ $n -eq 7 ] && [ "$4" = "--write-tree" ] && [ "$5" = "--messages" ] && hex "$6" && hex "$7"; }} && ok=1 ;;
  rev-list)     {{ [ $n -eq 6 ] && [ "$4" = "--left-right" ] && [ "$5" = "--count" ] && range "$6"; }} && ok=1 ;;
  log)          {{ [ $n -eq 8 ] && [ "$4" = "-1" ] && [ "$5" = "--no-show-signature" ] && [ "$6" = "--format=%ct" ] && hex "$7" && [ "$8" = "--" ]; }} && ok=1 ;;
  remote)       [ $n -eq 3 ] && ok=1 ;;
  config)       {{ [ $n -eq 6 ] && [ "$4" = "--get-all" ] && [ "$5" = "--" ] && case "$6" in remote.*.fetch) true ;; *) false ;; esac; }} && ok=1 ;;
esac
[ "$ok" = 1 ] || refuse "$@"
exec "$REAL_GIT" "$@"
"#,
            log = log.display(),
            viol = violations.display()
        )
    }

    #[test]
    fn doctor_never_spawns_a_mutating_git_command() {
        let f = fixture();
        let shim_dir = tempfile::tempdir().unwrap();
        let log = shim_dir.path().join("argv.log");
        let violations = shim_dir.path().join("violations.log");
        let shim = write_git_shim(shim_dir.path(), &mutation_shim_body(&log, &violations));
        let out = vc(&f.root)
            .env("PATH", shimmed_path(shim.parent().unwrap()))
            .args(["--json", "worktree", "doctor"])
            .assert()
            .success()
            .get_output()
            .clone();
        // The doctor captures git's stderr, so an attempted-and-ignored
        // mutation would not show on ITS stderr; the violation log is the
        // proof, and it must not exist at all.
        assert!(
            !violations.exists(),
            "mutating git commands were attempted:\n{}",
            std::fs::read_to_string(&violations).unwrap_or_default()
        );
        let recorded = std::fs::read_to_string(&log).unwrap();
        // Positive control: the shim was on the PATH the doctor used and
        // saw the read-only verbs expected.
        assert!(
            recorded.contains("worktree list --porcelain -z"),
            "{recorded}"
        );
        assert!(recorded.contains("merge-base --is-ancestor"), "{recorded}");
        assert!(recorded.contains("status --porcelain"), "{recorded}");
        assert!(
            !recorded.contains(" fetch"),
            "no fetch without --fetch: {recorded}"
        );
        // Every spawned git carried the no-lazy-fetch and no-optional-
        // locks environment: one `env` line per invocation, all exact.
        let argv_lines = recorded.lines().filter(|l| !l.starts_with("env ")).count();
        let env_lines: Vec<&str> = recorded.lines().filter(|l| l.starts_with("env ")).collect();
        assert_eq!(env_lines.len(), argv_lines, "one env line per git call");
        assert!(
            env_lines
                .iter()
                .all(|l| *l == "env GIT_NO_LAZY_FETCH=1 GIT_OPTIONAL_LOCKS=0"),
            "{env_lines:?}"
        );
        // The classification through the shim is the real one.
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(by_path(&v, &f.wt["conflict"])["state"], "conflict");
        assert_eq!(by_path(&v, &f.wt["dirty"])["state"], "dirty");

        // merge-tree ran exactly once for each worktree where it can change
        // the answer — ready, conflict, stale, locked — and for no other
        // (not dirty, detached, merged, branch-gone or prunable). Compared as
        // the exact multiset of HEAD arguments, not a bare count.
        let mut seen: Vec<String> = recorded
            .lines()
            .filter(|l| l.contains(" merge-tree --write-tree "))
            .map(|l| l.rsplit(' ').next().unwrap().to_string())
            .collect();
        seen.sort();
        let mut want: Vec<String> = ["merge-ready", "conflict", "stale", "locked"]
            .iter()
            .map(|k| head_of(&f.wt[k]))
            .collect();
        want.sort();
        assert_eq!(seen, want, "merge-tree HEAD arguments\n{recorded}");
        let dirty_head = head_of(&f.wt["dirty"]);
        assert!(
            !seen.contains(&dirty_head),
            "dirty worktree was merge-tree'd"
        );
    }

    #[test]
    fn mutating_shim_actually_refuses_so_the_previous_test_is_not_vacuous() {
        // The SAME shim body, invoked directly with mutating verbs: each must
        // be refused, logged, and never reach the real git.
        let shim_dir = tempfile::tempdir().unwrap();
        let log = shim_dir.path().join("argv.log");
        let violations = shim_dir.path().join("violations.log");
        let shim = write_git_shim(shim_dir.path(), &mutation_shim_body(&log, &violations));
        let cases: &[&[&str]] = &[
            &["-C", "/", "reset", "--hard"],
            &["-C", "/", "worktree", "remove", "x"],
            &["-C", "/", "worktree", "prune"],
            &["-C", "/", "branch", "-d", "x"],
            &["-C", "/", "checkout", "x"],
            &["-C", "/", "clean", "-fd"],
            &["-C", "/", "merge", "x"],
            &["-C", "/", "fetch"],
            &["-C", "/", "branch", "doctor-created"],
            &["-C", "/", "worktree", "add", "x"],
            &["-C", "/", "rev-parse", "--git-dir"],
            &[
                "-C",
                "/",
                "symbolic-ref",
                "-q",
                "refs/heads/doctor-created",
                "refs/heads/main",
            ],
            &["-C", "/", "log", "-1", "--output=/dev/null", "HEAD"],
            &[
                "-C",
                "/",
                "log",
                "-1",
                "--no-show-signature",
                "--format=%ct",
                "--output=/dev/null",
                "--",
            ],
            &[
                "-C",
                "/",
                "merge-base",
                "--is-ancestor",
                "HEAD",
                "--output=x",
            ],
            &["-C", "/", "rev-parse", "--verify", "-q", "--output=x"],
            // no `-C <dir>` prefix: positions shift, refused outright
            &[
                "log",
                "--output=/dev/null",
                "HEAD",
                "-1",
                "--no-show-signature",
                "--format=%ct",
                "0000000000000000000000000000000000000000",
                "--",
            ],
            &["remote"],
            &["-C", "/", "status", "--porcelain=v1"],
            &["-C", "/", "worktree", "list"],
        ];
        for c in cases {
            let st = std::process::Command::new(&shim).args(*c).status().unwrap();
            assert_eq!(st.code(), Some(97), "{c:?} was not refused");
        }
        let v = std::fs::read_to_string(&violations).unwrap();
        assert_eq!(v.lines().count(), cases.len(), "{v}");
        // ...and the refused branch creation really did not happen (the
        // shim refuses BEFORE exec): run it against a real repo.
        let f = fixture();
        let st = std::process::Command::new(&shim)
            .args(["-C", f.root.to_str().unwrap(), "branch", "doctor-created"])
            .status()
            .unwrap();
        assert_eq!(st.code(), Some(97));
        let refs = git(&f.root, &["branch", "--list", "doctor-created"], None);
        assert!(refs.trim().is_empty(), "branch was created: {refs}");
        let st = std::process::Command::new(&shim)
            .args([
                "-C",
                f.root.to_str().unwrap(),
                "symbolic-ref",
                "-q",
                "refs/heads/doctor-created",
                "refs/heads/main",
            ])
            .status()
            .unwrap();
        assert_eq!(st.code(), Some(97));
        assert!(!f.root.join(".git/refs/heads/doctor-created").exists());
        // `log --output=<file>` would overwrite the file: refused, and the
        // sentinel keeps its contents.
        let sentinel = f.root.join("sentinel.txt");
        std::fs::write(&sentinel, "keep\n").unwrap();
        for form in [
            vec![
                "log".to_string(),
                "-1".into(),
                format!("--output={}", sentinel.display()),
                "HEAD".into(),
            ],
            vec![
                "log".to_string(),
                "-1".into(),
                "--no-show-signature".into(),
                "--format=%ct".into(),
                format!("--output={}", sentinel.display()),
                "--".into(),
            ],
        ] {
            let st = std::process::Command::new(&shim)
                .arg("-C")
                .arg(&f.root)
                .args(&form)
                .status()
                .unwrap();
            assert_eq!(st.code(), Some(97), "{form:?}");
            assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep\n");
        }
        // The unprefixed spelling that would line up with the log rule's
        // positions is refused BEFORE any position is inspected, with a
        // real repo as its working directory.
        let st = std::process::Command::new(&shim)
            .current_dir(&f.root)
            .args([
                "log",
                &format!("--output={}", sentinel.display()),
                "HEAD",
                "-1",
                "--no-show-signature",
                "--format=%ct",
                &head_of(&f.root),
                "--",
            ])
            .status()
            .unwrap();
        assert_eq!(st.code(), Some(97));
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep\n");
        // The doctor's real log form, with a real id, passes through.
        let tip = head_of(&f.root);
        let st = std::process::Command::new(&shim)
            .args([
                "-C",
                f.root.to_str().unwrap(),
                "log",
                "-1",
                "--no-show-signature",
                "--format=%ct",
                &tip,
                "--",
            ])
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(st.code(), Some(0));
        // ...while both read forms pass through.
        for form in [
            vec!["-C", f.root.to_str().unwrap(), "symbolic-ref", "-q", "HEAD"],
            vec![
                "-C",
                f.root.to_str().unwrap(),
                "symbolic-ref",
                "-q",
                "--",
                "HEAD",
            ],
        ] {
            let st = std::process::Command::new(&shim)
                .args(&form)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap();
            assert_eq!(st.code(), Some(0), "{form:?}");
        }
        // ...and the doctor's exact merge-tree form passes through to the
        // real git (exit 0: HEAD merged with itself is clean).
        let st = std::process::Command::new(&shim)
            .args([
                "-C",
                f.root.to_str().unwrap(),
                "merge-tree",
                "--write-tree",
                "--messages",
                &tip,
                &tip,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(st.code(), Some(0), "merge-tree must not be refused");
    }

    #[test]
    fn old_git_without_write_tree_reports_unknown_not_a_crash() {
        let f = fixture();
        let shim_dir = tempfile::tempdir().unwrap();
        // Mimic git < 2.38: `merge-tree` knows no `--write-tree` and prints
        // its usage to stderr with exit 129.
        let shim = write_git_shim(
            shim_dir.path(),
            r#"if [ "$sub" = "merge-tree" ]; then
      echo "usage: git merge-tree <base-tree> <branch1> <branch2>" >&2
      exit 129
    fi
    exec "$REAL_GIT" "$@""#,
        );
        let v = {
            let out = vc(&f.root)
                .env("PATH", shimmed_path(shim.parent().unwrap()))
                .args(["--json", "worktree", "doctor"])
                .assert()
                .success()
                .get_output()
                .stdout
                .clone();
            serde_json::from_slice::<serde_json::Value>(&out).unwrap()
        };
        for key in ["merge-ready", "conflict", "stale", "locked"] {
            let w = by_path(&v, &f.wt[key]);
            assert_eq!(w["state"], "unknown", "{key}: {w:#}");
            assert!(
                w["reason"]
                    .as_str()
                    .unwrap()
                    .contains("merge-tree unsupported: git too old for --write-tree"),
                "{w:#}"
            );
        }
        // States that never needed merge-tree are unaffected.
        assert_eq!(by_path(&v, &f.wt["dirty"])["state"], "dirty");
        assert_eq!(by_path(&v, &f.wt["merged"])["state"], "merged");
        assert_eq!(by_path(&v, &f.wt["prunable"])["state"], "prunable");
        assert_eq!(v["counts"]["unknown"], 4);
    }

    #[test]
    fn failed_ancestry_query_is_unknown_even_when_merge_tree_would_be_clean() {
        let f = fixture();
        let shim_dir = tempfile::tempdir().unwrap();
        // `merge-base` dies with 128 (a corrupt object, say); `merge-tree`
        // would still say "clean" for the merged worktree — the doctor must
        // not turn that into `merge-ready`.
        let shim = write_git_shim(
            shim_dir.path(),
            r#"if [ "$sub" = "merge-base" ]; then
      echo "fatal: simulated failure" >&2
      exit 128
    fi
    exec "$REAL_GIT" "$@""#,
        );
        let out = vc(&f.root)
            .env("PATH", shimmed_path(shim.parent().unwrap()))
            .args(["--json", "worktree", "doctor"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        for key in ["merged", "merge-ready", "conflict", "stale", "locked"] {
            let w = by_path(&v, &f.wt[key]);
            assert_eq!(w["state"], "unknown", "{key}: {w:#}");
            assert!(
                w["reason"]
                    .as_str()
                    .unwrap()
                    .contains("merge-base --is-ancestor failed"),
                "{w:#}"
            );
        }
        assert_eq!(by_path(&v, &f.wt["dirty"])["state"], "dirty");
    }

    #[test]
    fn a_failed_branch_lookup_is_unknown_not_a_deleted_branch_and_dirty_still_wins() {
        let f = fixture();
        let shim_dir = tempfile::tempdir().unwrap();
        // `rev-parse --verify` dies with 128 for the ready and dirty branches
        // only: neither is "missing" (that would be exit 1), so neither may
        // be called stale/branch-gone — and the dirty one must stay dirty.
        let shim = write_git_shim(
            shim_dir.path(),
            r#"if [ "$sub" = "rev-parse" ]; then
      case " $* " in *" refs/heads/feat/ready^{commit} "*|*" refs/heads/feat/dirty^{commit} "*)
        echo "fatal: simulated object store failure" >&2; exit 128 ;;
      esac
    fi
    exec "$REAL_GIT" "$@""#,
        );
        let out = vc(&f.root)
            .env("PATH", shimmed_path(shim.parent().unwrap()))
            .args(["--json", "worktree", "doctor"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let ready = by_path(&v, &f.wt["merge-ready"]);
        assert_eq!(ready["state"], "unknown", "{ready:#}");
        assert!(
            ready["reason"]
                .as_str()
                .unwrap()
                .starts_with("ref lookup failed:")
                && ready["reason"]
                    .as_str()
                    .unwrap()
                    .contains("rev-parse exit 128"),
            "{ready:#}"
        );
        assert_eq!(by_path(&v, &f.wt["dirty"])["state"], "dirty");
        // The genuinely deleted branch is still recognised as such (exit 1).
        assert_eq!(by_path(&v, &f.wt["branch-gone"])["state"], "stale");
        assert_eq!(by_path(&v, &f.wt["conflict"])["state"], "conflict");
    }

    #[test]
    fn an_existing_branch_that_fails_to_peel_is_unknown_not_deleted() {
        let f = fixture();
        let shim_dir = tempfile::tempdir().unwrap();
        // `rev-parse --verify <ref>^{commit}` exits 1 for the ready
        // branch (the code a tag-of-a-tree produces) while `show-ref`
        // still confirms the ref exists: not a deleted branch.
        let shim = write_git_shim(
            shim_dir.path(),
            r#"if [ "$sub" = "rev-parse" ]; then
  case " $* " in *" refs/heads/feat/ready^{commit} "*)
    echo "error: expected commit type" >&2; exit 1 ;;
  esac
fi
exec "$REAL_GIT" "$@""#,
        );
        let out = vc(&f.root)
            .env("PATH", shimmed_path(shim.parent().unwrap()))
            .args(["--json", "worktree", "doctor"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let ready = by_path(&v, &f.wt["merge-ready"]);
        assert_eq!(ready["state"], "unknown", "{ready:#}");
        assert!(
            ready["reason"]
                .as_str()
                .unwrap()
                .contains("exists but does not peel to a commit"),
            "{ready:#}"
        );
        assert_eq!(by_path(&v, &f.wt["branch-gone"])["state"], "stale");
    }

    #[test]
    fn a_failed_base_candidate_check_aborts_instead_of_picking_the_other() {
        let f = fixture();
        git(&f.root, &["tag", "feat/merged", "main~1"], None);
        let shim_dir = tempfile::tempdir().unwrap();
        // show-ref dies for the TAG candidate; the branch candidate is
        // fine. Silently choosing the branch would hide a real ambiguity.
        let shim = write_git_shim(
            shim_dir.path(),
            r#"if [ "$sub" = "show-ref" ]; then
  case " $* " in *" refs/tags/feat/merged "*) echo "fatal: simulated" >&2; exit 128 ;; esac
fi
exec "$REAL_GIT" "$@""#,
        );
        vc(&f.root)
            .env("PATH", shimmed_path(shim.parent().unwrap()))
            .args(["worktree", "doctor", "--base", "feat/merged"])
            .assert()
            .failure()
            .code(1)
            .stderr(predicates::str::contains(
                "could not check whether refs/tags/feat/merged exists",
            ));
    }

    #[test]
    fn base_resolution_query_failures_are_errors_not_silent_choices() {
        let f = fixture();
        let remote = f.root.parent().unwrap().join("q.git");
        git(
            &f.root,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                "--object-format=sha1",
                remote.to_str().unwrap(),
            ],
            None,
        );
        git(
            &f.root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
            None,
        );
        git(&f.root, &["push", "-q", "origin", "main"], None);
        git(&f.root, &["remote", "set-head", "origin", "main"], None);
        let shim_dir = tempfile::tempdir().unwrap();
        // (a) symbolic-ref dies: `--base origin/HEAD` must not keep
        // `HEAD` as its identity.
        let shim = write_git_shim(
            shim_dir.path(),
            r#"if [ "$sub" = "symbolic-ref" ]; then echo "fatal: simulated" >&2; exit 128; fi
exec "$REAL_GIT" "$@""#,
        );
        vc(&f.root)
            .env("PATH", shimmed_path(shim.parent().unwrap()))
            .args(["worktree", "doctor", "--base", "origin/HEAD"])
            .assert()
            .failure()
            .code(1)
            .stderr(predicates::str::contains("symbolic-ref"));
        // (b) `git remote` dies: a remote-tracking base cannot derive its
        // branch identity, so the scan refuses rather than reporting the
        // matching worktree as merely `merged`.
        let shim = write_git_shim(
            shim_dir.path(),
            r#"if [ "$sub" = "remote" ]; then echo "fatal: simulated" >&2; exit 128; fi
exec "$REAL_GIT" "$@""#,
        );
        vc(&f.root)
            .env("PATH", shimmed_path(shim.parent().unwrap()))
            .args(["worktree", "doctor", "--base", "refs/remotes/origin/main"])
            .assert()
            .failure()
            .code(1)
            .stderr(predicates::str::contains("git remote failed"));
        // (c) `rev-parse --git-path` dies while a tag named HEAD exists:
        // the tag must not win by default.
        let first = git(&f.root, &["rev-list", "--max-parents=0", "HEAD"], None);
        git(
            &f.root,
            &["update-ref", "refs/tags/HEAD", first.trim()],
            None,
        );
        let shim = write_git_shim(
            shim_dir.path(),
            r#"if [ "$sub" = "rev-parse" ] && [ "$arg2" = "--git-path" ]; then echo "fatal: simulated" >&2; exit 128; fi
exec "$REAL_GIT" "$@""#,
        );
        vc(&f.root)
            .env("PATH", shimmed_path(shim.parent().unwrap()))
            .args(["worktree", "doctor", "--base", "HEAD"])
            .assert()
            .failure()
            .code(1)
            .stderr(predicates::str::contains("--git-path HEAD failed"));
    }

    #[test]
    fn automatic_base_selection_errors_when_a_preferred_candidate_cannot_be_checked() {
        let f = fixture();
        let shim_dir = tempfile::tempdir().unwrap();
        // (a) origin/HEAD query dies (not "unset", which is exit 1): the
        // doctor must not fall through to `main`.
        let shim = write_git_shim(
            shim_dir.path(),
            r#"if [ "$sub" = "symbolic-ref" ]; then echo "fatal: simulated" >&2; exit 128; fi
exec "$REAL_GIT" "$@""#,
        );
        vc(&f.root)
            .env("PATH", shimmed_path(shim.parent().unwrap()))
            .args(["worktree", "doctor"])
            .assert()
            .failure()
            .code(1)
            .stderr(predicates::str::contains(
                "symbolic-ref refs/remotes/origin/HEAD failed",
            ));
        // (b) `main` exists but its existence check dies while `master`
        // exists too: no silent fallback to `master`.
        git(&f.root, &["branch", "master", "main~1"], None);
        let shim = write_git_shim(
            shim_dir.path(),
            r#"if [ "$sub" = "show-ref" ]; then
  case " $* " in *" refs/heads/main "*) echo "fatal: simulated" >&2; exit 128 ;; esac
fi
exec "$REAL_GIT" "$@""#,
        );
        vc(&f.root)
            .env("PATH", shimmed_path(shim.parent().unwrap()))
            .args(["worktree", "doctor"])
            .assert()
            .failure()
            .code(1)
            .stderr(predicates::str::contains("base candidate refs/heads/main"));
        // And the ordinary unset case still falls through cleanly.
        let v = doctor_json(&f.root, &[]);
        assert_eq!(v["base_source"], "fallback");
        assert_eq!(v["base"], "main");
    }

    #[test]
    fn a_base_ref_that_moves_mid_scan_does_not_change_the_comparison() {
        let f = fixture();
        let tip = head_of(&f.root);
        let shim_dir = tempfile::tempdir().unwrap();
        // The FIRST `merge-base` call — i.e. after the base was resolved
        // and before any comparison — rewinds `main` by one commit (a
        // mutation performed by the test's shim, not by the doctor).
        // Everything the doctor computes must still be against the
        // captured tip: the merged worktree (at that tip) stays `merged`;
        // re-resolving the moved ref would have made it `merge-ready`.
        let marker = shim_dir.path().join("moved");
        let shim = write_git_shim(
            shim_dir.path(),
            &format!(
                r#"if [ "$sub" = "merge-base" ] && [ ! -e '{m}' ]; then
  touch '{m}'
  "$REAL_GIT" "$1" "$2" update-ref refs/heads/main refs/heads/main~1
fi
exec "$REAL_GIT" "$@""#,
                m = marker.display()
            ),
        );
        let out = vc(&f.root)
            .env("PATH", shimmed_path(shim.parent().unwrap()))
            .args(["--json", "worktree", "doctor"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(marker.exists(), "the shim did move the ref");
        assert_ne!(head_of(&f.root), tip, "main really was rewound");
        assert_eq!(v["base_commit"], tip);
        let merged = by_path(&v, &f.wt["merged"]);
        assert_eq!(merged["state"], "merged", "{merged:#}");
        assert_eq!(merged["ahead"], 0);
        assert_eq!(merged["behind"], 0);
        assert_eq!(by_path(&v, &f.wt["conflict"])["state"], "conflict");
    }

    #[test]
    fn a_linked_branch_that_moves_after_listing_cannot_become_base_with_a_stale_row() {
        let f = fixture();
        let parent = f.root.parent().unwrap();
        let remote = parent.join("mv.git");
        git(
            &f.root,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                "--object-format=sha1",
                remote.to_str().unwrap(),
            ],
            None,
        );
        git(
            &f.root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
            None,
        );
        let main_tip = head_of(&f.root);
        // A clean linked worktree on `feat/twin`, pushed so that
        // `refs/remotes/origin/feat/twin` (the base) has branch identity
        // `feat/twin` at main's commit.
        let twin = parent.join("twin mv");
        git(
            &f.root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat/twin",
                twin.to_str().unwrap(),
                "main",
            ],
            None,
        );
        git(&f.root, &["push", "-q", "origin", "feat/twin"], None);
        // Twin then moves to an EMPTY commit: distinct commit, identical
        // tree, so the worktree stays clean.
        git(
            &twin,
            &["commit", "-q", "--allow-empty", "-m", "empty"],
            None,
        );
        let empty_tip = head_of(&twin);
        assert_ne!(empty_tip, main_tip);
        // The shim moves `feat/twin` back to main's commit right after the
        // porcelain listing was produced (a mutation by the test's shim,
        // not by the doctor).
        let marker = parent.join("moved");
        let shim_dir = tempfile::tempdir().unwrap();
        let shim = write_git_shim(
            shim_dir.path(),
            &format!(
                r#"if [ "$sub" = "worktree" ] && [ ! -e '{m}' ]; then
  "$REAL_GIT" "$@"; rc=$?
  touch '{m}'
  "$REAL_GIT" "$1" "$2" update-ref refs/heads/feat/twin {tip}
  exit $rc
fi
exec "$REAL_GIT" "$@""#,
                m = marker.display(),
                tip = main_tip
            ),
        );
        let out = vc(&f.root)
            .env("PATH", shimmed_path(shim.parent().unwrap()))
            .args([
                "--json",
                "worktree",
                "doctor",
                "--base",
                "refs/remotes/origin/feat/twin",
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(marker.exists());
        assert_eq!(head_of(&twin), main_tip, "the branch really moved");
        let row = by_path(&v, &twin);
        // The row is internally consistent with the HEAD it lists: the
        // empty commit, one ahead of the base, merge-ready — never `base`
        // on the strength of a lookup made after the branch moved.
        assert_eq!(row["head"], empty_tip);
        assert_ne!(row["state"], "base", "{row:#}");
        assert_eq!(row["ahead"], 1);
        assert_eq!(row["state"], "merge-ready", "{row:#}");
    }

    #[test]
    fn an_unreadable_commit_date_is_unknown_not_merge_ready() {
        let f = fixture();
        let shim_dir = tempfile::tempdir().unwrap();
        let shim = write_git_shim(
            shim_dir.path(),
            r#"if [ "$sub" = "log" ]; then echo "fatal: simulated" >&2; exit 128; fi
exec "$REAL_GIT" "$@""#,
        );
        let out = vc(&f.root)
            .env("PATH", shimmed_path(shim.parent().unwrap()))
            .args(["--json", "worktree", "doctor"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        for key in ["merge-ready", "stale", "locked"] {
            let w = by_path(&v, &f.wt[key]);
            assert_eq!(w["state"], "unknown", "{key}: {w:#}");
            assert!(w["last_commit_unix"].is_null());
        }
        // Precedence above the age question is untouched.
        assert_eq!(by_path(&v, &f.wt["merged"])["state"], "merged");
        assert_eq!(by_path(&v, &f.wt["conflict"])["state"], "conflict");
        assert_eq!(by_path(&v, &f.wt["dirty"])["state"], "dirty");
    }

    #[test]
    fn inherited_git_env_overrides_do_not_redirect_the_scan() {
        let f = fixture();
        // A foreign repository whose GIT_DIR / object store, if honoured,
        // would make every ref lookup fail or answer for the wrong repo.
        let foreign = tempfile::tempdir().unwrap();
        let fr = foreign.path().join("other");
        std::fs::create_dir_all(&fr).unwrap();
        git(
            &fr,
            &["init", "-q", "-b", "main", "--object-format=sha1"],
            None,
        );
        commit_file(&fr, "z", "z\n", "foreign", None);
        let out = vc(&f.root)
            .env("GIT_DIR", fr.join(".git"))
            .env("GIT_WORK_TREE", &fr)
            .env("GIT_OBJECT_DIRECTORY", fr.join(".git/objects"))
            .env("GIT_INDEX_FILE", fr.join(".git/index"))
            .args(["--json", "worktree", "doctor"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["worktrees"].as_array().unwrap().len(), f.wt.len());
        assert_eq!(by_path(&v, &f.wt["conflict"])["state"], "conflict");
        assert_eq!(by_path(&v, &f.wt["merged"])["state"], "merged");
        assert_eq!(by_path(&v, &f.wt["merge-ready"])["state"], "merge-ready");
    }
}

#[test]
fn missing_git_binary_is_an_io_refusal() {
    let f = fixture();
    let empty = tempfile::tempdir().unwrap();
    vc(&f.root)
        .env("PATH", empty.path().display().to_string())
        .args(["worktree", "doctor"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicates::str::contains("io: git: failed to run"));
}

#[test]
fn a_tag_sharing_a_branch_name_cannot_redirect_the_comparison() {
    let f = fixture();
    let first = git(&f.root, &["rev-list", "--max-parents=0", "HEAD"], None);
    // A tag named `main` at the root commit: the fallback base must
    // still be refs/heads/main, so nothing changes.
    git(&f.root, &["tag", "main", first.trim()], None);
    let v = doctor_json(&f.root, &[]);
    assert_eq!(v["base"], "main");
    assert_eq!(v["base_full"], "refs/heads/main");
    assert_eq!(by_path(&v, &f.wt["merged"])["state"], "merged");
    assert_eq!(by_path(&v, &f.wt["conflict"])["state"], "conflict");
    assert_eq!(by_path(&v, &f.wt["merge-ready"])["ahead"], 1);

    // A tag named after a CLASSIFIED linked branch, at the root commit.
    git(&f.root, &["tag", "feat/ready", first.trim()], None);
    // The canonical branch ref is the base: the ready worktree is `base`.
    let v = doctor_json(&f.root, &["--base", "refs/heads/feat/ready"]);
    assert_eq!(v["base_full"], "refs/heads/feat/ready");
    assert_eq!(by_path(&v, &f.wt["merge-ready"])["state"], "base");
    // The ambiguous short name is refused, naming both refs, rather
    // than silently resolved to the tag (git's own precedence) and
    // used as the anchor for every comparison in the report.
    vc(&f.root)
        .args(["worktree", "doctor", "--base", "feat/ready"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains(
            "ambiguous (refs/tags/feat/ready, refs/heads/feat/ready)",
        ));
    // The tag, named canonically, is a fine base — and no branch is
    // `base` against it; the summary shows the canonical ref because
    // the display name alone would mislead.
    let v = doctor_json(&f.root, &["--base", "refs/tags/feat/ready"]);
    assert_eq!(v["base_full"], "refs/tags/feat/ready");
    let ready = by_path(&v, &f.wt["merge-ready"]);
    assert_ne!(ready["state"], "base");
    assert_eq!(ready["ahead"], 2, "root..ready = base 2 + ready");
    let out = vc(&f.root)
        .args(["worktree", "doctor", "--base", "refs/tags/feat/ready"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("base refs/tags/feat/ready (from --base)"),
        "{text}"
    );
    // A short name that is NOT ambiguous still expands to its
    // canonical ref and the summary keeps it short.
    let out = vc(&f.root)
        .args(["worktree", "doctor", "--base", "feat/merged"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("base feat/merged (from --base)")
    );
    // --base naming nothing is a refusal, not a scan.
    vc(&f.root)
        .args(["worktree", "doctor", "--base", "no-such-ref"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicates::str::contains("does not name a commit"));
}

#[test]
fn a_remote_whose_name_contains_a_slash_still_identifies_the_base_branch() {
    let f = fixture();
    let remote = f.root.parent().unwrap().join("slashed.git");
    git(
        &f.root,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            "--object-format=sha1",
            remote.to_str().unwrap(),
        ],
        None,
    );
    git(
        &f.root,
        &["remote", "add", "team/upstream", remote.to_str().unwrap()],
        None,
    );
    git(&f.root, &["push", "-q", "team/upstream", "main"], None);
    // A local branch literally named `upstream/main` at main's commit —
    // the false positive a naive one-segment split would produce.
    let decoy = f.root.parent().unwrap().join("decoy");
    git(
        &f.root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "upstream/main",
            decoy.to_str().unwrap(),
            "main",
        ],
        None,
    );
    // And a linked worktree on a local `main`-named branch is impossible
    // (main is checked out in the main worktree), so the true positive is
    // checked through the twin of `main`: a branch named `main` cannot
    // exist twice, so use the remote ref's own branch name via --base on
    // a second remote branch instead.
    git(
        &f.root,
        &["push", "-q", "team/upstream", "feat/merged"],
        None,
    );
    let v = doctor_json(
        &f.root,
        &["--base", "refs/remotes/team/upstream/feat/merged"],
    );
    assert_eq!(v["base_full"], "refs/remotes/team/upstream/feat/merged");
    assert_eq!(by_path(&v, &f.wt["merged"])["state"], "base", "{v:#}");
    // The decoy shares main's commit but is `upstream/main`, not `main`.
    let v = doctor_json(&f.root, &["--base", "refs/remotes/team/upstream/main"]);
    assert_eq!(by_path(&v, &decoy)["state"], "merged", "{v:#}");
}

#[test]
fn a_tag_named_head_cannot_capture_a_head_relative_base_expression() {
    let f = fixture();
    let first = git(&f.root, &["rev-list", "--max-parents=0", "HEAD"], None);
    // `refs/tags/HEAD` at the root commit. `--base HEAD~1` must mean the
    // parent of the CURRENT HEAD (base 2), not something derived from
    // the tag — and `--base HEAD` must be HEAD itself.
    // `git tag HEAD` refuses the name; a raw ref write is how such a
    // ref ends up existing, and is exactly the pathological case.
    git(
        &f.root,
        &["update-ref", "refs/tags/HEAD", first.trim()],
        None,
    );
    let v = doctor_json(&f.root, &["--base", "HEAD~1"]);
    assert_eq!(v["base_full"], "HEAD~1");
    // The merged worktree sits at main's tip, one commit past main~1:
    // against the CURRENT HEAD's parent it is exactly 1 ahead and
    // merges cleanly. Against the tag's parent (none — the tag is at
    // the root) resolution would have failed outright.
    let merged = by_path(&v, &f.wt["merged"]);
    assert_eq!(merged["state"], "merge-ready", "{merged:#}");
    assert_eq!(merged["ahead"], 1);
    assert_eq!(merged["behind"], 0);
    // `HEAD` itself now matches both the pseudo-ref and the tag: refused
    // (git would pick $GIT_DIR/HEAD and warn; the doctor does not guess).
    vc(&f.root)
        .args(["worktree", "doctor", "--base", "HEAD"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains(
            "ambiguous (HEAD, refs/tags/HEAD)",
        ));
    // Without the tag, `HEAD` is the pseudo-ref — a symbolic ref, so it
    // dereferences to the branch the main worktree has checked out.
    git(&f.root, &["update-ref", "-d", "refs/tags/HEAD"], None);
    let v = doctor_json(&f.root, &["--base", "HEAD"]);
    assert_eq!(v["base_full"], "refs/heads/main");
    assert_eq!(by_path(&v, &f.wt["conflict"])["state"], "conflict");
}

#[test]
fn an_uppercase_branch_name_is_a_branch_not_a_pseudo_ref() {
    let f = fixture();
    let p = f.root.parent().unwrap().join("release wt");
    git(
        &f.root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "RELEASE",
            p.to_str().unwrap(),
            "main",
        ],
        None,
    );
    let v = doctor_json(&f.root, &["--base", "RELEASE"]);
    assert_eq!(v["base_full"], "refs/heads/RELEASE");
    assert_eq!(by_path(&v, &p)["state"], "base", "{v:#}");
    // A tag of the same name makes it ambiguous, not silently the tag.
    git(&f.root, &["tag", "RELEASE", "main~1"], None);
    vc(&f.root)
        .args(["worktree", "doctor", "--base", "RELEASE"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains(
            "ambiguous (refs/tags/RELEASE, refs/heads/RELEASE)",
        ));
}

#[test]
fn a_branch_named_like_an_ordinary_git_dir_file_is_not_ambiguous() {
    let f = fixture();
    assert!(f.root.join(".git/index").is_file(), "fixture precondition");
    let p = f.root.parent().unwrap().join("index wt");
    git(
        &f.root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "index",
            p.to_str().unwrap(),
            "main",
        ],
        None,
    );
    let v = doctor_json(&f.root, &["--base", "index"]);
    assert_eq!(v["base_full"], "refs/heads/index");
    assert_eq!(by_path(&v, &p)["state"], "base", "{v:#}");
    // `config` too — a text file, but not a ref.
    let p2 = f.root.parent().unwrap().join("config wt");
    git(
        &f.root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "config",
            p2.to_str().unwrap(),
            "main",
        ],
        None,
    );
    let v = doctor_json(&f.root, &["--base", "config"]);
    assert_eq!(v["base_full"], "refs/heads/config");
}

#[test]
fn a_populated_fetch_head_is_a_pseudo_ref_and_collides_with_a_same_named_branch() {
    let f = fixture();
    let first = git(&f.root, &["rev-list", "--max-parents=0", "HEAD"], None);
    std::fs::write(
        f.root.join(".git/FETCH_HEAD"),
        format!("{}\t\tbranch 'main' of /nowhere\n", first.trim()),
    )
    .unwrap();
    // Alone, FETCH_HEAD is a fine base: its id peels to the root commit,
    // so every branch is ahead of it.
    let v = doctor_json(&f.root, &["--base", "FETCH_HEAD"]);
    assert_eq!(v["base_full"], "FETCH_HEAD");
    assert_eq!(by_path(&v, &f.wt["merged"])["ahead"], 2);
    // With a branch of the same name at a different commit: ambiguous.
    git(&f.root, &["branch", "FETCH_HEAD", "main"], None);
    vc(&f.root)
        .args(["worktree", "doctor", "--base", "FETCH_HEAD"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains(
            "ambiguous (FETCH_HEAD, refs/heads/FETCH_HEAD)",
        ));
}

#[test]
fn submodule_changes_count_as_dirty_even_when_configured_ignored() {
    let f = fixture();
    // A submodule in the merged worktree, then its checkout modified,
    // with `submodule.<name>.ignore=all` set — the setting that makes
    // plain `git status` hide the change.
    let sub_src = f.root.parent().unwrap().join("sub.git");
    let sub_work = f.root.parent().unwrap().join("subwork");
    std::fs::create_dir_all(&sub_work).unwrap();
    git(
        &sub_work,
        &["init", "-q", "-b", "main", "--object-format=sha1"],
        None,
    );
    commit_file(&sub_work, "s.txt", "s\n", "sub", None);
    git(
        &sub_work,
        &["clone", "-q", "--bare", ".", sub_src.to_str().unwrap()],
        None,
    );
    let wt = &f.wt["merged"];
    git(
        wt,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            sub_src.to_str().unwrap(),
            "sub",
        ],
        None,
    );
    git(wt, &["commit", "-q", "-m", "add sub"], None);
    git(wt, &["config", "submodule.sub.ignore", "all"], None);
    std::fs::write(wt.join("sub/s.txt"), "changed\n").unwrap();
    std::fs::write(wt.join("sub/new.txt"), "new\n").unwrap();
    // Plain status (the configured view) hides it...
    let plain = git(wt, &["status", "--porcelain"], None);
    assert!(plain.trim().is_empty(), "fixture precondition: {plain}");
    // ...the doctor does not.
    let v = doctor_json(&f.root, &[]);
    let w = by_path(&v, wt);
    assert_eq!(w["state"], "dirty", "{w:#}");
}

#[test]
fn an_explicit_symbolic_base_is_dereferenced_to_its_target_branch() {
    let f = fixture();
    let remote = f.root.parent().unwrap().join("sym.git");
    git(
        &f.root,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            "--object-format=sha1",
            remote.to_str().unwrap(),
        ],
        None,
    );
    git(
        &f.root,
        &["remote", "add", "origin", remote.to_str().unwrap()],
        None,
    );
    git(
        &f.root,
        &["push", "-q", "origin", "main", "feat/merged"],
        None,
    );
    git(
        &f.root,
        &["remote", "set-head", "origin", "feat/merged"],
        None,
    );
    // Three spellings of the same symbolic ref; each must land on the
    // target branch, making the clean linked `feat/merged` worktree
    // `base` rather than `merged`.
    for spelling in ["origin/HEAD", "refs/remotes/origin/HEAD", "origin"] {
        let v = doctor_json(&f.root, &["--base", spelling]);
        assert_eq!(
            v["base_full"], "refs/remotes/origin/feat/merged",
            "{spelling}: {v:#}"
        );
        assert_eq!(
            by_path(&v, &f.wt["merged"])["state"],
            "base",
            "{spelling}: {v:#}"
        );
    }
}

#[test]
fn fetch_refreshes_the_remote_the_base_lives_on_not_the_default_one() {
    let f = fixture();
    let parent = f.root.parent().unwrap();
    let origin = parent.join("origin.git");
    let upstream = parent.join("upstream.git");
    for r in [&origin, &upstream] {
        git(
            &f.root,
            &["init", "-q", "--bare", "-b", "main", r.to_str().unwrap()],
            None,
        );
    }
    git(
        &f.root,
        &["remote", "add", "origin", origin.to_str().unwrap()],
        None,
    );
    git(
        &f.root,
        &["remote", "add", "upstream", upstream.to_str().unwrap()],
        None,
    );
    git(&f.root, &["push", "-q", "origin", "main"], None);
    git(&f.root, &["push", "-q", "upstream", "main"], None);
    let before = git(&f.root, &["rev-parse", "refs/remotes/upstream/main"], None);
    // upstream advances by one commit made elsewhere.
    let other = parent.join("other clone");
    git(
        &f.root,
        &[
            "clone",
            "-q",
            upstream.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
        None,
    );
    commit_file(&other, "up.txt", "u\n", "upstream moved", None);
    git(&other, &["push", "-q", "origin", "main"], None);
    let new_tip = git(&other, &["rev-parse", "HEAD"], None);

    // `fetch.prune=true` plus an obsolete tracking ref: a `--fetch` with
    // a LOCAL base runs the plain fetch, and the ref must survive it.
    git(&f.root, &["config", "fetch.prune", "true"], None);
    git(
        &f.root,
        &["update-ref", "refs/remotes/origin/obsolete", before.trim()],
        None,
    );
    // Arm fetch's automatic maintenance too. `gc --auto` fires when the
    // pack count exceeds `gc.autoPackLimit` (the loose-object estimate
    // samples `objects/17/`, which a tiny fixture never fills), so make
    // two packs and set the limit to 1; `gc.worktreePruneExpire=now`
    // then lets it prune the fixture's prunable registration — which
    // must still be listed after a `--fetch` scan.
    git(&f.root, &["repack", "-q"], None);
    commit_file(&f.root, "pack2.txt", "p\n", "second pack", None);
    git(&f.root, &["repack", "-q"], None);
    let packs = std::fs::read_dir(f.root.join(".git/objects/pack"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "pack")
        })
        .count();
    assert!(packs >= 2, "fixture precondition: {packs} packs");
    git(&f.root, &["config", "gc.autoPackLimit", "1"], None);
    git(&f.root, &["config", "gc.autoDetach", "false"], None);
    git(&f.root, &["config", "gc.worktreePruneExpire", "now"], None);
    let v = doctor_json(&f.root, &["--base", "main", "--fetch"]);
    assert_eq!(
        by_path(&v, &f.wt["prunable"])["state"],
        "prunable",
        "auto maintenance pruned the registration: {v:#}"
    );
    let kept = git(
        &f.root,
        &["rev-parse", "refs/remotes/origin/obsolete"],
        None,
    );
    assert_eq!(kept.trim(), before.trim(), "prune ran under --fetch");
    // ...and under a remote-tracking base's targeted fetch as well.
    doctor_json(&f.root, &["--base", "refs/remotes/origin/main", "--fetch"]);
    let kept = git(
        &f.root,
        &["rev-parse", "refs/remotes/origin/obsolete"],
        None,
    );
    assert_eq!(
        kept.trim(),
        before.trim(),
        "prune ran under targeted --fetch"
    );
    git(&f.root, &["config", "--unset", "fetch.prune"], None);

    // Without --fetch the stale remote-tracking ref is what is compared.
    let v = doctor_json(&f.root, &["--base", "refs/remotes/upstream/main"]);
    assert_eq!(v["base_commit"], before.trim());
    // With --fetch, the base's OWN remote (upstream, not the default
    // origin) is fetched and the base re-resolved to the new tip.
    let v = doctor_json(
        &f.root,
        &["--base", "refs/remotes/upstream/main", "--fetch"],
    );
    assert_eq!(v["base_commit"], new_tip.trim(), "{v:#}");
    // origin/main was not what moved, and the doctor did not need it.
    let origin_main = git(&f.root, &["rev-parse", "refs/remotes/origin/main"], None);
    assert_eq!(origin_main.trim(), before.trim());

    // A base branch the remote's configured refspec does NOT cover:
    // `upstream/topic` was fetched explicitly once, then upstream's
    // fetch refspec is narrowed to main only, and topic advances.
    git(&other, &["checkout", "-q", "-b", "topic"], None);
    commit_file(&other, "t1.txt", "t\n", "topic 1", None);
    git(&other, &["push", "-q", "origin", "topic"], None);
    git(
        &f.root,
        &[
            "fetch",
            "-q",
            "upstream",
            "+refs/heads/topic:refs/remotes/upstream/topic",
        ],
        None,
    );
    let topic_before = git(&f.root, &["rev-parse", "refs/remotes/upstream/topic"], None);
    git(
        &f.root,
        &[
            "config",
            "remote.upstream.fetch",
            "+refs/heads/main:refs/remotes/upstream/main",
        ],
        None,
    );
    commit_file(&other, "t2.txt", "t\n", "topic 2", None);
    git(&other, &["push", "-q", "origin", "topic"], None);
    let topic_new = git(&other, &["rev-parse", "HEAD"], None);
    assert_ne!(topic_before.trim(), topic_new.trim());
    let v = doctor_json(
        &f.root,
        &["--base", "refs/remotes/upstream/topic", "--fetch"],
    );
    assert_eq!(v["base_commit"], topic_new.trim(), "{v:#}");
    // The same-named fallback source is subject to negative refspecs
    // too: with `^refs/heads/topic` configured, the fetch is refused and
    // the tracking ref stays put.
    git(
        &f.root,
        &[
            "config",
            "--add",
            "remote.upstream.fetch",
            "^refs/heads/topic",
        ],
        None,
    );
    commit_file(&other, "t3.txt", "t\n", "topic 3", None);
    git(&other, &["push", "-q", "origin", "topic"], None);
    let topic_tracking = git(&f.root, &["rev-parse", "refs/remotes/upstream/topic"], None);
    vc(&f.root)
        .args([
            "worktree",
            "doctor",
            "--base",
            "refs/remotes/upstream/topic",
            "--fetch",
        ])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains(
            "the only source (refs/heads/topic)",
        ));
    let after = git(&f.root, &["rev-parse", "refs/remotes/upstream/topic"], None);
    assert_eq!(after, topic_tracking);
    git(
        &f.root,
        &[
            "config",
            "--unset",
            "--fixed-value",
            "remote.upstream.fetch",
            "^refs/heads/topic",
        ],
        None,
    );

    // A RENAMED mapping: upstream's `release` is what lands in
    // `upstream/main` locally. Remote `main` and `release` have different
    // tips; `--fetch` on `upstream/main` must pull `release`, not `main`.
    git(&other, &["checkout", "-q", "-b", "release", "main"], None);
    commit_file(&other, "rel.txt", "r\n", "release 1", None);
    git(&other, &["push", "-q", "origin", "release"], None);
    let release_tip = git(&other, &["rev-parse", "HEAD"], None);
    let main_tip = git(&other, &["rev-parse", "refs/remotes/origin/main"], None);
    assert_ne!(release_tip.trim(), main_tip.trim());
    git(
        &f.root,
        &[
            "config",
            "remote.upstream.fetch",
            "+refs/heads/release:refs/remotes/upstream/main",
        ],
        None,
    );
    let v = doctor_json(
        &f.root,
        &["--base", "refs/remotes/upstream/main", "--fetch"],
    );
    assert_eq!(v["base_commit"], release_tip.trim(), "{v:#}");
    // ...and a glob mapping resolves the same way.
    git(
        &f.root,
        &[
            "config",
            "remote.upstream.fetch",
            "+refs/heads/release*:refs/remotes/upstream/main*",
        ],
        None,
    );
    commit_file(&other, "rel2.txt", "r\n", "release 2", None);
    git(&other, &["push", "-q", "origin", "release"], None);
    let release_tip2 = git(&other, &["rev-parse", "HEAD"], None);
    let v = doctor_json(
        &f.root,
        &["--base", "refs/remotes/upstream/main", "--fetch"],
    );
    assert_eq!(v["base_commit"], release_tip2.trim(), "{v:#}");

    // Negative refspecs: `main` is mapped, then excluded, then `release`
    // is mapped onto the same destination — the effective source is
    // `release`.
    commit_file(&other, "rel3.txt", "r\n", "release 3", None);
    git(&other, &["push", "-q", "origin", "release"], None);
    let release_tip3 = git(&other, &["rev-parse", "HEAD"], None);
    git(
        &f.root,
        &[
            "config",
            "--replace-all",
            "remote.upstream.fetch",
            "+refs/heads/main:refs/remotes/upstream/main",
        ],
        None,
    );
    git(
        &f.root,
        &[
            "config",
            "--add",
            "remote.upstream.fetch",
            "^refs/heads/main",
        ],
        None,
    );
    git(
        &f.root,
        &[
            "config",
            "--add",
            "remote.upstream.fetch",
            "+refs/heads/release:refs/remotes/upstream/main",
        ],
        None,
    );
    let v = doctor_json(
        &f.root,
        &["--base", "refs/remotes/upstream/main", "--fetch"],
    );
    assert_eq!(v["base_commit"], release_tip3.trim(), "{v:#}");
    // Two surviving distinct sources for one destination: refused.
    git(
        &f.root,
        &[
            "config",
            "--add",
            "remote.upstream.fetch",
            "+refs/heads/topic:refs/remotes/upstream/main",
        ],
        None,
    );
    vc(&f.root)
        .args([
            "worktree",
            "doctor",
            "--base",
            "refs/remotes/upstream/main",
            "--fetch",
        ])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("more than one source maps onto"));

    // Every mapped source excluded: refused, and the tracking ref is
    // NOT overwritten by a same-named-branch guess (remote `main` exists
    // and differs).
    git(
        &f.root,
        &[
            "config",
            "--replace-all",
            "remote.upstream.fetch",
            "+refs/heads/release:refs/remotes/upstream/main",
        ],
        None,
    );
    git(
        &f.root,
        &[
            "config",
            "--add",
            "remote.upstream.fetch",
            "^refs/heads/release",
        ],
        None,
    );
    let tracking_before = git(&f.root, &["rev-parse", "refs/remotes/upstream/main"], None);
    vc(&f.root)
        .args([
            "worktree",
            "doctor",
            "--base",
            "refs/remotes/upstream/main",
            "--fetch",
        ])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("excluded by a negative refspec"));
    let tracking_after = git(&f.root, &["rev-parse", "refs/remotes/upstream/main"], None);
    assert_eq!(tracking_after, tracking_before);

    // An EXPRESSION base under `--fetch` is refused: the tracking refs it
    // depends on cannot be derived from its text, so a fetch could leave
    // the very ref it reads stale while appearing to refresh it. Nothing
    // is fetched — the tracking ref is unchanged — and the same
    // expressions work fine WITHOUT `--fetch`.
    git(
        &f.root,
        &[
            "config",
            "--replace-all",
            "remote.upstream.fetch",
            "+refs/heads/*:refs/remotes/upstream/*",
        ],
        None,
    );
    let stale_value = git(
        &f.root,
        &["rev-parse", "refs/remotes/upstream/main~1"],
        None,
    );
    // `fetch --all` would skip this remote; the doctor must fetch it by name.
    git(
        &f.root,
        &["config", "remote.upstream.skipFetchAll", "true"],
        None,
    );
    let tracking_before = git(&f.root, &["rev-parse", "refs/remotes/upstream/main"], None);
    // `other` is still on `release` from the mapping cases above.
    git(&other, &["checkout", "-q", "main"], None);
    commit_file(&other, "up2.txt", "u2\n", "upstream moved again", None);
    git(&other, &["push", "-q", "origin", "main"], None);
    for spelling in ["refs/remotes/upstream/main~1", "upstream/main~1"] {
        vc(&f.root)
            .args(["worktree", "doctor", "--base", spelling, "--fetch"])
            .assert()
            .failure()
            .code(2)
            .stderr(predicates::str::contains(
                "is a revision expression, not a ref",
            ));
        let v = doctor_json(&f.root, &["--base", spelling]);
        assert_eq!(v["base_full"], "refs/remotes/upstream/main~1", "{v:#}");
        assert_eq!(v["base_commit"], stale_value.trim(), "{v:#}");
    }
    // An exact ref that is not a branch — a tag — is still fetchable:
    // the refusal is for expressions only.
    git(&f.root, &["tag", "v-base", tracking_before.trim()], None);
    let v = doctor_json(&f.root, &["--base", "refs/tags/v-base", "--fetch"]);
    assert_eq!(v["base_full"], "refs/tags/v-base", "{v:#}");
    assert_eq!(v["base_commit"], tracking_before.trim(), "{v:#}");
    let tracking_after = git(&f.root, &["rev-parse", "refs/remotes/upstream/main"], None);
    assert_eq!(
        tracking_after, tracking_before,
        "an expression base must not fetch"
    );
}

#[test]
fn a_sixty_four_hex_tag_name_is_a_name_in_a_sha1_repository() {
    // A dedicated sha1 repository, requested explicitly so the caller's
    // GIT_DEFAULT_HASH cannot turn this into a sha256 fixture.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("sha1 repo");
    std::fs::create_dir(&root).unwrap();
    git(
        &root,
        &["init", "-q", "-b", "main", "--object-format=sha1"],
        None,
    );
    commit_file(&root, "a.txt", "a\n", "first", None);
    commit_file(&root, "b.txt", "b\n", "second", None);
    let fmt = git(&root, &["rev-parse", "--show-object-format"], None);
    assert_eq!(fmt.trim(), "sha1");
    let first = git(&root, &["rev-list", "--max-parents=0", "HEAD"], None);
    let name = "a".repeat(64);
    git(&root, &["tag", &name, first.trim()], None);
    // 64 hex chars is not a full object id here, so the tag is the only
    // hit and resolves — no "also the name of" refusal.
    let v = doctor_json(&root, &["--base", &name]);
    assert_eq!(v["base_commit"], first.trim(), "{v:#}");
    assert_eq!(v["base_full"], format!("refs/tags/{name}"), "{v:#}");
}

#[test]
fn an_expression_over_an_ambiguous_name_is_refused_not_resolved_by_precedence() {
    let f = fixture();
    let tip = head_of(&f.root);
    let first = git(&f.root, &["rev-list", "--max-parents=0", "HEAD"], None);
    // Branch `release` at the tip, tag `release` one commit back: BOTH
    // have a parent, so git would silently answer `release~1` from the
    // tag (with a warning) and the report would hang off the wrong
    // commit; the doctor refuses the operand instead.
    git(&f.root, &["branch", "release", &tip], None);
    let tip_parent = git(&f.root, &["rev-parse", "HEAD~1"], None);
    assert_ne!(tip_parent.trim(), first.trim());
    git(&f.root, &["tag", "release", tip_parent.trim()], None);
    vc(&f.root)
        .args(["worktree", "doctor", "--base", "release~1"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("ambiguous operand release"));
    // With the tag gone the operand is unique and the expression is
    // spelled canonically off the branch.
    git(&f.root, &["tag", "-d", "release"], None);
    let expect = git(&f.root, &["rev-parse", "refs/heads/release~1"], None);
    let v = doctor_json(&f.root, &["--base", "release~1"]);
    assert_eq!(v["base_full"], "refs/heads/release~1", "{v:#}");
    assert_eq!(v["base_commit"], expect.trim(), "{v:#}");
    // A bare `HEAD~1` still resolves through the pseudo-ref.
    let expect = git(&f.root, &["rev-parse", "HEAD~1"], None);
    let v = doctor_json(&f.root, &["--base", "HEAD~1"]);
    assert_eq!(v["base_commit"], expect.trim(), "{v:#}");
}

#[test]
fn a_full_object_id_base_is_the_commit_unless_a_ref_is_spelled_like_it() {
    let f = fixture();
    let tip = head_of(&f.root);
    let first = git(&f.root, &["rev-list", "--max-parents=0", "HEAD"], None);
    let v = doctor_json(&f.root, &["--base", &tip]);
    assert_eq!(v["base_commit"], tip);
    assert_eq!(by_path(&v, &f.wt["merged"])["state"], "merged");
    // A tag spelled exactly like that id, pointing elsewhere: refused,
    // never silently either one.
    git(&f.root, &["tag", &tip, first.trim()], None);
    vc(&f.root)
        .args(["worktree", "doctor", "--base", &tip])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains(
            "a full object id, but also the name of",
        ));
    // The `next:` hint's spelling disambiguates to the commit.
    let v = doctor_json(&f.root, &["--base", &format!("{tip}^{{commit}}")]);
    assert_eq!(v["base_commit"], tip);
}

#[test]
fn branch_keyed_selectors_survive_operand_disambiguation() {
    let f = fixture();
    let remote = f.root.parent().unwrap().join("sel.git");
    git(
        &f.root,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            "--object-format=sha1",
            remote.to_str().unwrap(),
        ],
        None,
    );
    git(
        &f.root,
        &["remote", "add", "origin", remote.to_str().unwrap()],
        None,
    );
    git(&f.root, &["push", "-q", "-u", "origin", "main"], None);
    git(&f.root, &["config", "push.default", "simple"], None);
    // Move local main on so upstream and local differ.
    commit_file(&f.root, "local.txt", "l\n", "local only", None);
    let upstream = git(&f.root, &["rev-parse", "refs/remotes/origin/main"], None);
    let upstream_parent = git(&f.root, &["rev-parse", "refs/remotes/origin/main~1"], None);
    assert_ne!(upstream.trim(), head_of(&f.root));
    for (expr, want) in [
        ("main@{upstream}", upstream.trim()),
        ("main@{u}", upstream.trim()),
        ("main@{u}~1", upstream_parent.trim()),
        ("main@{push}", upstream.trim()),
    ] {
        let v = doctor_json(&f.root, &["--base", expr]);
        assert_eq!(v["base_commit"], want, "{expr}: {v:#}");
        assert_eq!(v["base_full"], expr, "{expr}: {v:#}");
    }
    // A numeric reflog selector on a QUALIFIED operand keeps the
    // qualification: branch `release` at A and a same-named tag at B with
    // its own reflog — `refs/heads/release@{0}` must be A, never the
    // tag's reflog entry.
    let a = head_of(&f.root);
    let first = git(&f.root, &["rev-list", "--max-parents=0", "HEAD"], None);
    git(&f.root, &["branch", "release", &a], None);
    git(
        &f.root,
        &["tag", "--create-reflog", "release", first.trim()],
        None,
    );
    let v = doctor_json(&f.root, &["--base", "refs/heads/release@{0}"]);
    assert_eq!(v["base_commit"], a, "{v:#}");
    assert_eq!(v["base_full"], "refs/heads/release@{0}", "{v:#}");
    // ...while the unqualified spelling is the ambiguity it always was.
    vc(&f.root)
        .args(["worktree", "doctor", "--base", "release@{0}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("ambiguous operand release"));
    // Still refused when the operand itself is ambiguous.
    git(&f.root, &["tag", "main", first.trim()], None);
    vc(&f.root)
        .args(["worktree", "doctor", "--base", "main@{upstream}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("ambiguous operand main"));
}

#[test]
fn a_ref_name_containing_unicode_whitespace_is_not_confused_with_its_trimmed_twin() {
    let f = fixture();
    let remote = f.root.parent().unwrap().join("nbsp.git");
    git(
        &f.root,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            "--object-format=sha1",
            remote.to_str().unwrap(),
        ],
        None,
    );
    git(
        &f.root,
        &["remote", "add", "origin", remote.to_str().unwrap()],
        None,
    );
    let tip = head_of(&f.root);
    let first = git(&f.root, &["rev-list", "--max-parents=0", "HEAD"], None);
    // `topic<NBSP>` at the tip, plain `topic` at the root; origin/HEAD
    // points at the NBSP one.
    let nbsp = "refs/remotes/origin/topic\u{a0}";
    git(&f.root, &["update-ref", nbsp, &tip], None);
    git(
        &f.root,
        &["update-ref", "refs/remotes/origin/topic", first.trim()],
        None,
    );
    git(
        &f.root,
        &["symbolic-ref", "refs/remotes/origin/HEAD", nbsp],
        None,
    );
    // Automatic selection.
    let v = doctor_json(&f.root, &[]);
    assert_eq!(v["base_source"], "origin/HEAD");
    assert_eq!(v["base_full"], nbsp, "{v:#}");
    assert_eq!(v["base_commit"], tip, "{v:#}");
    // Explicit symbolic selection.
    let v = doctor_json(&f.root, &["--base", "origin/HEAD"]);
    assert_eq!(v["base_full"], nbsp, "{v:#}");
    assert_eq!(v["base_commit"], tip, "{v:#}");
    // Explicit exact name with the NBSP.
    let v = doctor_json(&f.root, &["--base", nbsp]);
    assert_eq!(v["base_commit"], tip, "{v:#}");
}

#[test]
fn a_symbolic_fallback_main_identifies_its_target_branch() {
    let f = fixture();
    // `refs/heads/main` becomes a symbolic ref to `feat/merged` (an odd
    // but legal layout). Both automatic fallback and `--base main` must
    // then treat the clean linked `feat/merged` worktree as `base`.
    git(
        &f.root,
        &["symbolic-ref", "refs/heads/main", "refs/heads/feat/merged"],
        None,
    );
    let v = doctor_json(&f.root, &[]);
    assert_eq!(v["base_source"], "fallback");
    assert_eq!(v["base"], "main");
    assert_eq!(v["base_full"], "refs/heads/feat/merged", "{v:#}");
    assert_eq!(by_path(&v, &f.wt["merged"])["state"], "base", "{v:#}");
    let v = doctor_json(&f.root, &["--base", "main"]);
    assert_eq!(v["base_full"], "refs/heads/feat/merged", "{v:#}");
    assert_eq!(by_path(&v, &f.wt["merged"])["state"], "base", "{v:#}");
}

#[test]
fn origin_head_pointing_at_a_local_branch_identifies_it_on_every_path() {
    let f = fixture();
    // A legal oddity: `refs/remotes/origin/HEAD` symbolically targets a
    // LOCAL branch. Automatic selection and `--base origin/HEAD` must
    // agree on every field and both call the `feat/merged` worktree
    // `base`.
    git(
        &f.root,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/heads/feat/merged",
        ],
        None,
    );
    let auto = doctor_json(&f.root, &[]);
    assert_eq!(auto["base_source"], "origin/HEAD");
    let explicit = doctor_json(&f.root, &["--base", "origin/HEAD"]);
    for v in [&auto, &explicit] {
        assert_eq!(v["base_full"], "refs/heads/feat/merged", "{v:#}");
        assert_eq!(v["base_commit"], head_of(&f.wt["merged"]), "{v:#}");
        assert_eq!(by_path(v, &f.wt["merged"])["state"], "base", "{v:#}");
    }
}

#[test]
fn a_remote_named_like_an_option_is_still_fetched_as_a_remote() {
    let f = fixture();
    let parent = f.root.parent().unwrap();
    let remote = parent.join("dashdash.git");
    git(
        &f.root,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            "--object-format=sha1",
            remote.to_str().unwrap(),
        ],
        None,
    );
    git(
        &f.root,
        &["remote", "add", "--", "--all", remote.to_str().unwrap()],
        None,
    );
    git(&f.root, &["push", "-q", "--", "--all", "main"], None);
    let before = git(&f.root, &["rev-parse", "refs/remotes/--all/main"], None);
    let other = parent.join("dash clone");
    git(
        &f.root,
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
        None,
    );
    commit_file(&other, "d.txt", "d\n", "moved", None);
    git(&other, &["push", "-q", "origin", "main"], None);
    let new_tip = git(&other, &["rev-parse", "HEAD"], None);
    assert_ne!(before.trim(), new_tip.trim());
    let v = doctor_json(&f.root, &["--base", "refs/remotes/--all/main", "--fetch"]);
    assert_eq!(v["base_commit"], new_tip.trim(), "{v:#}");
}
