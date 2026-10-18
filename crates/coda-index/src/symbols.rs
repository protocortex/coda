// SPDX-License-Identifier: AGPL-3.0-only

//! Tree-sitter symbol extraction.

use tree_sitter::{Node, Parser};

use crate::lang::Language;

/// What kind of definition a [`Symbol`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Module,
    TypeAlias,
    Const,
    Class,
    Interface,
    Static,
    Macro,
    Union,
    Impl,
}

/// A named definition with its 1-based line span and enclosing definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub line: usize,
    pub end_line: usize,
    /// Name of the nearest enclosing definition, if any.
    pub parent: Option<String>,
}

/// Why symbols could not be extracted from a source file.
#[derive(Debug, thiserror::Error)]
pub enum SymbolError {
    #[error("tree-sitter rejected the {0:?} grammar")]
    Grammar(Language),
    #[error("tree-sitter produced no parse tree")]
    Parse,
}

/// Extracts definitions from `source`. Syntax errors do not fail the call:
/// definitions in the parseable parts are still returned.
///
/// # Errors
/// Returns [`SymbolError`] if the grammar cannot be loaded or parsing is aborted.
pub fn extract(language: Language, source: &[u8]) -> Result<Vec<Symbol>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&language.grammar())
        .map_err(|_| SymbolError::Grammar(language))?;
    let tree = parser.parse(source, None).ok_or(SymbolError::Parse)?;
    let mut symbols = Vec::new();
    collect(language, tree.root_node(), source, &mut symbols);
    Ok(symbols)
}

/// Depth-first walk with an explicit cursor, so nesting depth cannot exhaust the stack.
fn collect(language: Language, root: Node<'_>, source: &[u8], out: &mut Vec<Symbol>) {
    let mut cursor = root.walk();
    // Enclosing definitions as (tree depth, name), innermost last.
    let mut scopes: Vec<(usize, String)> = Vec::new();
    let mut depth = 0usize;
    loop {
        let node = cursor.node();
        while scopes.last().is_some_and(|(d, _)| *d >= depth) {
            scopes.pop();
        }
        if let Some(kind) = classify(language, node)
            && let Some(name) = name_of(kind, node, source)
        {
            out.push(Symbol {
                name: name.clone(),
                kind,
                line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                parent: scopes.last().map(|(_, n)| n.clone()),
            });
            scopes.push((depth, name));
        }
        if cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return;
            }
            depth -= 1;
        }
    }
}

/// Impl blocks have no `name` field; they are named by the type they implement.
fn name_of(kind: SymbolKind, node: Node<'_>, source: &[u8]) -> Option<String> {
    let field = if kind == SymbolKind::Impl {
        "type"
    } else {
        "name"
    };
    let name = node.child_by_field_name(field)?;
    Some(String::from_utf8_lossy(&source[name.byte_range()]).into_owned())
}

fn classify(language: Language, node: Node<'_>) -> Option<SymbolKind> {
    match language {
        Language::Rust => classify_rust(node),
        Language::TypeScript | Language::Tsx => classify_typescript(node),
    }
}

fn classify_rust(node: Node<'_>) -> Option<SymbolKind> {
    match node.kind() {
        "function_item" | "function_signature_item" => Some(if is_rust_member(node) {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        }),
        "struct_item" => Some(SymbolKind::Struct),
        "enum_item" => Some(SymbolKind::Enum),
        "trait_item" => Some(SymbolKind::Trait),
        "mod_item" => Some(SymbolKind::Module),
        "type_item" => Some(SymbolKind::TypeAlias),
        "const_item" => Some(SymbolKind::Const),
        "static_item" => Some(SymbolKind::Static),
        "macro_definition" => Some(SymbolKind::Macro),
        "union_item" => Some(SymbolKind::Union),
        "impl_item" => Some(SymbolKind::Impl),
        _ => None,
    }
}

/// True for functions declared directly inside an `impl` or `trait` body.
fn is_rust_member(node: Node<'_>) -> bool {
    node.parent()
        .and_then(|body| body.parent())
        .is_some_and(|owner| matches!(owner.kind(), "impl_item" | "trait_item"))
}

fn classify_typescript(node: Node<'_>) -> Option<SymbolKind> {
    match node.kind() {
        "function_declaration" => Some(SymbolKind::Function),
        "method_definition" | "method_signature" => Some(SymbolKind::Method),
        "class_declaration" | "abstract_class_declaration" => Some(SymbolKind::Class),
        "interface_declaration" => Some(SymbolKind::Interface),
        "type_alias_declaration" => Some(SymbolKind::TypeAlias),
        "enum_declaration" => Some(SymbolKind::Enum),
        "internal_module" | "module" => Some(SymbolKind::Module),
        "variable_declarator" if is_function_binding(node) => Some(SymbolKind::Function),
        _ => None,
    }
}

/// True for `const f = () => {}` style bindings of a plain identifier.
fn is_function_binding(node: Node<'_>) -> bool {
    node.child_by_field_name("name")
        .is_some_and(|n| n.kind() == "identifier")
        && node.child_by_field_name("value").is_some_and(|v| {
            matches!(
                v.kind(),
                "arrow_function" | "function_expression" | "generator_function"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(language: Language, src: &str) -> Vec<(String, SymbolKind, usize)> {
        extract(language, src.as_bytes())
            .unwrap()
            .into_iter()
            .map(|s| (s.name, s.kind, s.line))
            .collect()
    }

    #[test]
    fn extracts_rust_definitions() {
        // Arrange
        let src = "mod m {}\nstruct S;\nenum E { A }\ntrait T { fn req(&self); }\ntype A = u8;\nconst C: u8 = 1;\nfn free() {}\nimpl S { fn meth(&self) {} }\n";
        // Act
        let got = pairs(Language::Rust, src);
        // Assert
        let expected = [
            ("m", SymbolKind::Module, 1),
            ("S", SymbolKind::Struct, 2),
            ("E", SymbolKind::Enum, 3),
            ("T", SymbolKind::Trait, 4),
            ("req", SymbolKind::Method, 4),
            ("A", SymbolKind::TypeAlias, 5),
            ("C", SymbolKind::Const, 6),
            ("free", SymbolKind::Function, 7),
            ("S", SymbolKind::Impl, 8),
            ("meth", SymbolKind::Method, 8),
        ];
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(n, k, l)| (n.to_owned(), k, l))
            .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn extracts_typescript_definitions() {
        // Arrange
        let src =
            "function f() {}\nclass C { m() {} }\ninterface I {}\ntype T = string;\nenum E { A }\n";
        // Act
        let got = pairs(Language::TypeScript, src);
        // Assert
        let expected = [
            ("f", SymbolKind::Function, 1),
            ("C", SymbolKind::Class, 2),
            ("m", SymbolKind::Method, 2),
            ("I", SymbolKind::Interface, 3),
            ("T", SymbolKind::TypeAlias, 4),
            ("E", SymbolKind::Enum, 5),
        ];
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(n, k, l)| (n.to_owned(), k, l))
            .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn extracts_tsx_definitions() {
        // Arrange
        let src = "function App() { return <div/>; }\n";
        // Act
        let got = pairs(Language::Tsx, src);
        // Assert
        assert_eq!(got, [("App".to_owned(), SymbolKind::Function, 1)]);
    }

    #[test]
    fn syntax_errors_keep_valid_definitions() {
        // Arrange
        let src = "fn good() {}\nfn broken( {{{\nstruct After;\n";
        // Act
        let got = extract(Language::Rust, src.as_bytes());
        // Assert
        let names: Vec<_> = got.unwrap().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"good".to_owned()), "{names:?}");
    }

    #[test]
    fn empty_source_has_no_symbols() {
        // Arrange / Act
        let got = extract(Language::TypeScript, b"").unwrap();
        // Assert
        assert_eq!(got, []);
    }

    fn nested(depth: usize, open: &str, close: &str) -> String {
        format!("{}{}", open.repeat(depth), close.repeat(depth))
    }

    #[test]
    fn deeply_nested_rust_does_not_overflow_the_stack() {
        // Arrange
        let src = format!("fn f() {{ let _ = {}; }}\n", nested(200_000, "(", ")"));
        // Act
        let got = pairs(Language::Rust, &src);
        // Assert
        assert!(got.iter().any(|(n, ..)| n == "f"), "{}", got.len());
    }

    #[test]
    fn deeply_nested_typescript_does_not_overflow_the_stack() {
        // Arrange
        let src = format!("function f() {{ {} }}\n", nested(100_000, "{", "}"));
        // Act
        let got = pairs(Language::TypeScript, &src);
        // Assert
        assert!(got.iter().any(|(n, ..)| n == "f"), "{}", got.len());
    }

    #[test]
    fn extracts_typescript_arrow_methods_signatures_and_namespaces() {
        // Arrange
        let src = "const a = () => 1;\nconst fe = function () {};\nconst plain = 3;\ninterface I { sig(): void; }\nnamespace N { export function inner() {} }\n";
        // Act
        let got = pairs(Language::TypeScript, src);
        // Assert
        let expected = [
            ("a", SymbolKind::Function, 1),
            ("fe", SymbolKind::Function, 2),
            ("I", SymbolKind::Interface, 4),
            ("sig", SymbolKind::Method, 4),
            ("N", SymbolKind::Module, 5),
            ("inner", SymbolKind::Function, 5),
        ];
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(n, k, l)| (n.to_owned(), k, l))
            .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn extracts_rust_statics_macros_unions_and_impls() {
        // Arrange
        let src = "static S: u8 = 1;\nmacro_rules! m { () => {} }\nunion U { a: u8 }\nimpl Foo {}\nimpl Tr for Bar {}\n";
        // Act
        let got = pairs(Language::Rust, src);
        // Assert
        let expected = [
            ("S", SymbolKind::Static, 1),
            ("m", SymbolKind::Macro, 2),
            ("U", SymbolKind::Union, 3),
            ("Foo", SymbolKind::Impl, 4),
            ("Bar", SymbolKind::Impl, 5),
        ];
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(n, k, l)| (n.to_owned(), k, l))
            .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn records_end_line_and_parent() {
        // Arrange
        let src = "struct S;\nimpl S {\n    fn m(&self) {\n        fn inner() {}\n    }\n}\n";
        // Act
        let got = extract(Language::Rust, src.as_bytes()).unwrap();
        // Assert
        let spans: Vec<_> = got
            .iter()
            .map(|s| (s.name.as_str(), s.line, s.end_line, s.parent.as_deref()))
            .collect();
        assert_eq!(
            spans,
            [
                ("S", 1, 1, None),
                ("S", 2, 6, None),
                ("m", 3, 5, Some("S")),
                ("inner", 4, 4, Some("m")),
            ]
        );
    }

    #[test]
    fn fn_nested_in_a_method_is_a_function() {
        // Arrange
        let src = "impl S { fn m(&self) { fn inner() {} } }\n";
        // Act
        let got = pairs(Language::Rust, src);
        // Assert
        assert_eq!(got[1], ("m".to_owned(), SymbolKind::Method, 1));
        assert_eq!(got[2], ("inner".to_owned(), SymbolKind::Function, 1));
    }

    #[test]
    fn non_utf8_names_are_kept_lossily() {
        // Arrange
        let src = b"class C { \"a\xff\"() {} }\n";
        // Act
        let got = extract(Language::TypeScript, src).unwrap();
        // Assert
        let names: Vec<_> = got.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["C", "\"a\u{fffd}\""]);
    }
}
