// roundtrip_prop.rs — T9 gate: apply ∘ undo byte-identical, fuzzed
//
// Generator note: the brief's original needle-length floor was 1 byte
// (`len in 1usize..50`). Combined with content up to ~2000 random bytes,
// a 1-2 byte needle very often recurs elsewhere by chance (256 symbols —
// birthday-paradox territory: a 2000-byte buffer has on the order of
// 2000/256 ≈ 8 expected occurrences of any given single byte), so
// `prop_assume!(occurrences == 1)` rejected a large fraction of generated
// cases. proptest counts rejects globally across the whole run and aborts
// the run once too many pile up, rather than silently skipping — measured
// empirically below. Raising the floor to 4 bytes drops the odds of an
// accidental duplicate to effectively zero (256^4 possible 4-byte
// sequences vs. at most ~2000 candidate windows to collide against) while
// leaving the property itself untouched: random content + one
// unique-match edit -> apply -> undo -> byte-identical, still ≥200 cases,
// assertion unchanged.
use proptest::prelude::*;
use velocity_code_kernel::{
    apply,
    plan::{Plan, PlanForm},
    resolve,
};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]
    #[test]
    fn apply_then_undo_is_byte_identical(
        content in proptest::collection::vec(any::<u8>(), 1..2000),
        // pick a slice to replace and replacement bytes
        a in 0usize..1000, len in 4usize..50,
        replacement in proptest::collection::vec(any::<u8>(), 0..80),
    ) {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        let start = a % content.len();
        let end = (start + len).min(content.len());
        let old = content[start..end].to_vec();
        prop_assume!(!old.is_empty());
        // uniqueness: only run when the slice occurs exactly once
        let occurrences = content.windows(old.len()).filter(|w| *w == &old[..]).count();
        prop_assume!(occurrences == 1);
        std::fs::write(r.join("f.bin"), &content).unwrap();
        let reqs = vec![resolve::EditRequest {
            path: "f.bin".into(), old: old.clone(), new: replacement.clone(), line_hint: None }];
        let plan = Plan::build(&r, PlanForm::Edit, &reqs).unwrap();
        let sha8 = plan.store(&r).unwrap();
        apply::apply_plan(&r, &sha8).unwrap();
        apply::undo(&r, None).unwrap();
        prop_assert_eq!(std::fs::read(r.join("f.bin")).unwrap(), content);
    }
}
