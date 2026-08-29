//! velocity-code language layer: tree-sitter-backed symbol extraction.
//!
//! `vc-lang` is read-only — it parses source text handed to it and returns
//! `Symbol`s. It never touches the filesystem and exposes no write API.

mod rust_symbols;

use velocity_code_kernel::VcResult;

#[derive(Clone, Debug, PartialEq)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Impl,
    Const,
    Static,
    Module,
    TypeAlias,
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// 1-based, inclusive.
    pub start_line: usize,
    /// 1-based, inclusive.
    pub end_line: usize,
    /// Header text up to (not including) the body brace, whitespace-collapsed
    /// to one line, trimmed.
    pub signature: String,
    /// False for every Rust construct in M2 — Rust extraction is always
    /// grammar-driven, never a heuristic fallback.
    pub syntax_inferred: bool,
}

/// Extract top-level and nested symbols from `src`. `lang` selects the
/// grammar: `"rust"` extracts via tree-sitter-rust; any other value
/// (including `"python"`, which arrives in PR B) returns `Ok(vec![])`.
pub fn symbols(src: &str, lang: &str) -> VcResult<Vec<Symbol>> {
    match lang {
        "rust" => rust_symbols::extract(src),
        _ => Ok(vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_functions_methods_structs_and_impls_with_lines() {
        let src = r#"
/// doc
pub struct Plan { pub version: u32 }

impl Plan {
    pub fn id(&self) -> String { String::new() }
}

fn free() {}
"#;
        let syms = symbols(src, "rust").unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).unwrap();
        assert_eq!(find("Plan").kind, SymbolKind::Struct);
        assert_eq!(find("id").kind, SymbolKind::Method);
        assert_eq!(find("free").kind, SymbolKind::Function);
        assert!(find("id").signature.contains("pub fn id(&self) -> String"));
        assert!(
            find("id").start_line < find("id").end_line
                || find("id").start_line == find("id").end_line
        );
        assert!(syms.iter().all(|s| !s.syntax_inferred));
    }

    #[test]
    fn broken_region_does_not_kill_other_symbols() {
        let src = "fn ok() {}\nfn broken( {\nfn after() {}\n";
        let syms = symbols(src, "rust").unwrap();
        assert!(syms.iter().any(|s| s.name == "ok"));
    }

    #[test]
    fn unknown_language_yields_empty() {
        assert!(symbols("x", "").unwrap().is_empty());
    }
}
