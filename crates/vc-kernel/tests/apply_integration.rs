use std::path::{Path, PathBuf};
use velocity_code_kernel::{ErrorKind, apply, plan::Plan, resolve};

fn setup_repo(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let r = d.path().to_path_buf();
    std::fs::create_dir_all(r.join(".vc")).unwrap();
    for (n, c) in files {
        std::fs::write(r.join(n), c).unwrap();
    }
    (d, r)
}

fn make_plan(root: &Path, edits: &[(&str, &str, &str)]) -> String {
    let reqs: Vec<resolve::EditRequest> = edits
        .iter()
        .map(|(p, o, n)| resolve::EditRequest {
            path: p.into(),
            old: o.as_bytes().to_vec(),
            new: n.as_bytes().to_vec(),
            line_hint: None,
        })
        .collect();
    let plan = Plan::build(root, velocity_code_kernel::plan::PlanForm::Edit, &reqs).unwrap();
    plan.store(root).unwrap()
}

#[test]
fn clean_apply_patches_and_journals() {
    let (_d, r) = setup_repo(&[("a.rs", "fn old() {}\n"), ("b.rs", "call_old();\n")]);
    let sha8 = make_plan(
        &r,
        &[("a.rs", "old", "new"), ("b.rs", "call_old", "call_new")],
    );
    let rep = apply::apply_plan(&r, &sha8).unwrap();
    assert_eq!(rep.files, 2);
    assert_eq!(
        std::fs::read_to_string(r.join("a.rs")).unwrap(),
        "fn new() {}\n"
    );
    assert_eq!(
        std::fs::read_to_string(r.join("b.rs")).unwrap(),
        "call_new();\n"
    );
}

#[test]
fn stale_file_refuses_with_exit3_kind_and_writes_nothing() {
    let (_d, r) = setup_repo(&[("a.rs", "one\n"), ("b.rs", "two\n")]);
    let sha8 = make_plan(&r, &[("a.rs", "one", "ONE"), ("b.rs", "two", "TWO")]);
    std::fs::write(r.join("b.rs"), "two changed\n").unwrap(); // mutate between plan and apply
    let err = apply::apply_plan(&r, &sha8).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Stale));
    assert_eq!(err.exit_code(), 3);
    assert_eq!(
        std::fs::read_to_string(r.join("a.rs")).unwrap(),
        "one\n",
        "clean file untouched"
    );
}

#[test]
fn undo_restores_bytes_exactly() {
    let (_d, r) = setup_repo(&[("a.rs", "alpha\n")]);
    let sha8 = make_plan(&r, &[("a.rs", "alpha", "beta")]);
    apply::apply_plan(&r, &sha8).unwrap();
    apply::undo(&r, None).unwrap();
    assert_eq!(std::fs::read(r.join("a.rs")).unwrap(), b"alpha\n".to_vec());
}

#[cfg(unix)]
#[test]
fn symlinked_target_is_refused() {
    let (_d, r) = setup_repo(&[("real.rs", "x\n")]);
    let sha8 = make_plan(&r, &[("real.rs", "x", "y")]);
    std::fs::remove_file(r.join("real.rs")).unwrap();
    std::os::unix::fs::symlink("/etc/hosts", r.join("real.rs")).unwrap();
    let err = apply::apply_plan(&r, &sha8).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Toctou | ErrorKind::Stale));
}
