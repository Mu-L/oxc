#![expect(clippy::print_stdout)]
//! # Simple Linter Example
//!
//! This example demonstrates how to create a basic linter using Oxc's parser and semantic analyzer.
//! It implements simple rules to detect debugger statements and empty destructuring patterns.
//!
//! ## Usage
//!
//! Create a `test.js` file and run:
//! ```bash
//! cargo run -p oxc_linter --example linter [filename]
//! ```

use std::{env, path::Path};

use oxc_allocator::Allocator;
use oxc_ast::AstKind;
use oxc_diagnostics::OxcDiagnostic;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{SourceType, Span};

// Instruction:
// create a `test.js`,
// run `cargo run -p oxc_linter --example linter`
// or `cargo watch -x "run -p oxc_linter --example linter"`

/// Run a simple linter on a JavaScript file
fn main() -> std::io::Result<()> {
    let name = env::args().nth(1).unwrap_or_else(|| "test.js".to_string());
    let path = Path::new(&name);
    let source_text = std::fs::read_to_string(path)?;
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap();
    let ret = Parser::new(&allocator, &source_text, source_type).parse();

    // Handle parser errors
    if !ret.errors.is_empty() {
        print_errors(&source_text, ret.errors);
        return Ok(());
    }

    // Build semantic model for AST analysis
    let semantic_ret = SemanticBuilder::new().build(&ret.program);

    let mut errors: Vec<OxcDiagnostic> = vec![];

    // Check for linting violations
    for node in semantic_ret.semantic.nodes() {
        match node.kind() {
            AstKind::DebuggerStatement(stmt) => {
                errors.push(no_debugger(stmt.span));
            }
            AstKind::ArrayPattern(array) if array.elements.is_empty() => {
                errors.push(no_empty_pattern("array", array.span));
            }
            AstKind::ObjectPattern(object) if object.properties.is_empty() => {
                errors.push(no_empty_pattern("object", object.span));
            }
            _ => {}
        }
    }

    // Report results
    if errors.is_empty() {
        println!("Success!");
    } else {
        print_errors(&source_text, errors);
    }

    Ok(())
}

/// Print diagnostic errors with source context
fn print_errors(source_text: &str, errors: Vec<OxcDiagnostic>) {
    for error in errors {
        let error = error.with_source_code(source_text.to_string());
        println!("{error:?}");
    }
}

/// Create a diagnostic for debugger statements
// This prints:
//
//   ⚠ `debugger` statement is not allowed
//   ╭────
// 1 │ debugger;
//   · ─────────
//   ╰────
fn no_debugger(debugger_span: Span) -> OxcDiagnostic {
    OxcDiagnostic::error("`debugger` statement is not allowed").with_label(debugger_span)
}

/// Create a diagnostic for empty destructuring patterns
// This prints:
//
//   ⚠ empty destructuring pattern is not allowed
//   ╭────
// 1 │ let {} = {};
//   ·     ─┬
//   ·      ╰── Empty object binding pattern
//   ╰────
fn no_empty_pattern(binding_kind: &str, span: Span) -> OxcDiagnostic {
    OxcDiagnostic::error("empty destructuring pattern is not allowed")
        .with_label(span.label(format!("Empty {binding_kind} binding pattern")))
}
