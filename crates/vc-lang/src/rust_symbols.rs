//! Rust symbol extraction: a hand-rolled cursor walk over tree-sitter-rust's
//! named node kinds. A `.scm` query would work too, but for nine node kinds
//! a single match statement is simpler to read and to step through.

use tree_sitter::{Node, Parser};
use velocity_code_kernel::{ErrorKind, VcError, VcResult};

use crate::{Symbol, SymbolKind};

pub(crate) fn extract(src: &str) -> VcResult<Vec<Symbol>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .expect("tree-sitter-rust grammar failed to load");

    let tree = parser
        .parse(src, None)
        .ok_or_else(|| VcError::new(ErrorKind::Malformed, "rust: source did not parse"))?;

    let root = tree.root_node();
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    walk(root, bytes, false, &mut out);

    // tree-sitter is error-tolerant and its root node is always
    // `source_file`, never `ERROR`, even for garbage input (verified against
    // nine garbage inputs: empty, binary, NUL bytes, lone tokens, prose —
    // none produced an ERROR-kind root). So "the whole file failed to parse"
    // has to be read off the walk's actual result instead of the root node's
    // kind: the tree recorded an error, extraction found nothing usable, and
    // the input wasn't merely blank (a blank file legitimately has zero
    // symbols and no error).
    if root.has_error() && out.is_empty() && !src.trim().is_empty() {
        return Err(VcError::new(
            ErrorKind::Malformed,
            "rust: source did not parse",
        ));
    }

    Ok(out)
}

/// Recurse over every named child of `node`, extracting a `Symbol` for each
/// node kind we cover and descending into every node regardless (bodies,
/// nested `impl`/`mod` blocks, and error-recovery nodes alike) so that
/// nested methods and module-scoped items are still found.
fn walk(node: Node, src: &[u8], in_impl: bool, out: &mut Vec<Symbol>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let next_in_impl = in_impl || child.kind() == "impl_item";
        if let Some(kind) = symbol_kind(child.kind(), in_impl)
            && let Some(name) = symbol_name(child, src)
        {
            out.push(Symbol {
                name,
                kind,
                start_line: child.start_position().row + 1,
                end_line: child.end_position().row + 1,
                signature: signature_of(child, src),
                syntax_inferred: false,
            });
        }
        walk(child, src, next_in_impl, out);
    }
}

fn symbol_kind(node_kind: &str, in_impl: bool) -> Option<SymbolKind> {
    Some(match node_kind {
        // `function_signature_item` is a bodyless trait method declaration
        // (`fn foo(&self);`) — a distinct node kind from `function_item`
        // (which covers both free functions and default-bodied trait/impl
        // methods). Same ancestry-based kind logic applies to both.
        "function_item" | "function_signature_item" => {
            if in_impl {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            }
        }
        "struct_item" => SymbolKind::Struct,
        "enum_item" => SymbolKind::Enum,
        "trait_item" => SymbolKind::Trait,
        "impl_item" => SymbolKind::Impl,
        "const_item" => SymbolKind::Const,
        "static_item" => SymbolKind::Static,
        "mod_item" => SymbolKind::Module,
        "type_item" => SymbolKind::TypeAlias,
        _ => return None,
    })
}

/// `impl_item` has no `name` field — its identity is the type it's
/// implemented for, plus the trait when this is a trait impl (`Display for
/// VcError`). Every other covered kind carries a plain `name` field.
fn symbol_name(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() == "impl_item" {
        let ty = node_text(node.child_by_field_name("type")?, src);
        return Some(match node.child_by_field_name("trait") {
            Some(tr) => format!("{} for {}", node_text(tr, src), ty),
            None => ty,
        });
    }
    node.child_by_field_name("name").map(|n| node_text(n, src))
}

/// Header text from the item's start up to (not including) its body brace —
/// the `body` field for kinds that have one (`fn`/`struct`/`enum`/`trait`/
/// `impl`/`mod`), or the item's full span for the brace-less kinds
/// (`const`/`static`/`type`, and bodyless `fn foo(&self);` trait
/// declarations, all of which end in `;` instead of a body).
fn signature_of(node: Node, src: &[u8]) -> String {
    let end = node
        .child_by_field_name("body")
        .map_or_else(|| node.end_byte(), |b| b.start_byte());
    collapse_whitespace(text_range(src, node.start_byte(), end))
}

fn node_text(node: Node, src: &[u8]) -> String {
    collapse_whitespace(text_range(src, node.start_byte(), node.end_byte()))
}

fn text_range(src: &[u8], start: usize, end: usize) -> &str {
    std::str::from_utf8(&src[start..end]).unwrap_or("")
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}
