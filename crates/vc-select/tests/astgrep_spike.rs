// crates/vc-select/tests/astgrep_spike.rs
// Spike: ast-grep-core drives our tree-sitter-rust grammar end-to-end.
//
// API delta from the brief's sketch: ast-grep-core 0.45.2 exposes no
// free-standing `vc_spike::match_and_rewrite` function to plug a grammar
// into — that's the shape of `ast-grep` the CLI tool, not `ast-grep-core`
// the library. The library's real shape is a `Language` trait (core
// pattern/meta-var machinery) plus a `tree_sitter::LanguageExt` trait
// (wires in an actual `tree_sitter::Language`), which a caller implements
// once per grammar. Upstream's own built-in language impls (e.g. `Tsx`)
// are `#[cfg(test)]`-only inside ast-grep-core itself, so a real caller —
// this spike, and Task 10's `matcher.rs` after it — needs its own.
//
// Once `RustLang` below exists: `RustLang.ast_grep(src)` parses source
// into an `AstGrep<StrDoc<RustLang>>`; `.root().find_all(pattern)` yields
// every match (a `&str` pattern implements `Matcher` directly, `$$$NAME`
// captures a variadic argument list); `AstGrep::replace(pattern,
// replacement)` finds and rewrites one match at a time, reparsing after
// each edit, and `.generate()` returns the rewritten source. This module
// wraps that shape behind the `match_and_rewrite(src, pattern,
// replacement) -> Vec<Match>` helper the brief specifies.
mod vc_spike {
    use std::borrow::Cow;

    use ast_grep_core::matcher::PatternBuilder;
    use ast_grep_core::tree_sitter::{LanguageExt, StrDoc, TSLanguage};
    use ast_grep_core::{Language, Pattern, PatternError};

    /// Minimal `ast-grep-core` `Language` impl over the `tree-sitter-rust`
    /// grammar.
    #[derive(Clone)]
    pub struct RustLang;

    impl Language for RustLang {
        // Rust's grammar only accepts `$` inside macro_rules bodies, so a
        // bare `$` in an ordinary pattern like `fetch_config($$$A)` sends
        // the parser into an ERROR node (verified: parsing "$$$A" alone
        // through tree-sitter-rust yields `(ERROR) (metavariable)`, not a
        // clean identifier) — this is the exact ast-grep-core <->
        // tree-sitter version-coupling risk this spike exists to catch.
        // ast-grep-core's `expando_char` mechanism exists for precisely
        // this case: substitute `$` for a char the grammar accepts as an
        // identifier lead (µ, U+00B5) before parsing, matching upstream's
        // own non-ASCII-expando test in `meta_var.rs`.
        fn pre_process_pattern<'q>(&self, query: &'q str) -> Cow<'q, str> {
            if query.contains(self.meta_var_char()) {
                Cow::Owned(query.replace(self.meta_var_char(), &self.expando_char().to_string()))
            } else {
                Cow::Borrowed(query)
            }
        }

        fn expando_char(&self) -> char {
            'µ'
        }

        fn kind_to_id(&self, kind: &str) -> u16 {
            self.get_ts_language()
                .id_for_node_kind(kind, /* named */ true)
        }

        fn field_to_id(&self, field: &str) -> Option<u16> {
            self.get_ts_language()
                .field_id_for_name(field)
                .map(|f| f.get())
        }

        fn build_pattern(&self, builder: &PatternBuilder) -> Result<Pattern, PatternError> {
            builder.build(|src| StrDoc::try_new(src, self.clone()))
        }
    }

    impl LanguageExt for RustLang {
        fn get_ts_language(&self) -> TSLanguage {
            tree_sitter_rust::LANGUAGE.into()
        }
    }

    /// One matched-and-rewritten occurrence of `pattern` in `src`.
    pub struct Match {
        /// The full source text after `pattern` -> `replacement` has been
        /// applied to every occurrence found in `src`. A spike doesn't
        /// need per-node replacement text, only proof that the
        /// whole-document rewrite through the grammar is correct.
        pub replacement: String,
    }

    /// Parse `src` as Rust (via `tree-sitter-rust`), find every occurrence
    /// of `pattern` (a metavariable like `$$$A` captures a variadic
    /// argument list), and rewrite each to `replacement`. Returns one
    /// `Match` per occurrence found, each carrying the fully rewritten
    /// source.
    pub fn match_and_rewrite(src: &str, pattern: &str, replacement: &str) -> Vec<Match> {
        let lang = RustLang;

        let match_count = lang.ast_grep(src).root().find_all(pattern).count();

        let mut root = lang.ast_grep(src);
        for _ in 0..match_count {
            let replaced = root
                .replace(pattern, replacement)
                .expect("replace should not error against a valid pattern/replacement");
            assert!(
                replaced,
                "expected a match on every iteration up to match_count"
            );
        }
        let rewritten = root.generate();

        (0..match_count)
            .map(|_| Match {
                replacement: rewritten.clone(),
            })
            .collect()
    }
}

#[test]
fn astgrep_matches_and_rewrites_with_metavariable() {
    let src = "fn main() { fetch_config(a, b); other(); }";
    // Build an ast-grep language from tree-sitter-rust, parse, match
    // pattern "fetch_config($$$A)", rewrite to "load_config($$$A)".
    // Assert: exactly one match; rewritten text contains "load_config(a, b)"
    // and still contains "other();".
    let matched = vc_spike::match_and_rewrite(src, "fetch_config($$$A)", "load_config($$$A)");

    assert_eq!(matched.len(), 1);
    assert!(matched[0].replacement.contains("load_config(a, b)"));
    assert!(matched[0].replacement.contains("other();"));
}
