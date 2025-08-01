#![expect(clippy::print_stdout)]
//! # Source Map Generation Example
//!
//! This example demonstrates how to generate source maps alongside JavaScript code.
//! It generates a visualization URL for inspecting the source map.
//!
//! ## Usage
//!
//! Create a `test.js` file and run:
//! ```bash
//! cargo run -p oxc_codegen --example sourcemap [filename]
//! ```

use std::{env, path::Path};

use base64::{Engine, prelude::BASE64_STANDARD};
use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions, CodegenReturn};
use oxc_parser::Parser;
use oxc_span::SourceType;

// Instruction:
// 1. create a `test.js`
// 2. run `cargo run -p oxc_codegen --example sourcemap`

/// Generate source maps and provide a visualization URL
fn main() -> std::io::Result<()> {
    let name = env::args().nth(1).unwrap_or_else(|| "test.js".to_string());
    let path = Path::new(&name);
    let source_text = std::fs::read_to_string(path)?;
    let source_type = SourceType::from_path(path).unwrap();
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, &source_text, source_type).parse();

    // Handle parsing errors
    if !ret.errors.is_empty() {
        for error in ret.errors {
            let error = error.with_source_code(source_text.clone());
            println!("{error:?}");
        }
        return Ok(());
    }

    // Generate code with source map
    let CodegenReturn { code, map, .. } = Codegen::new()
        .with_options(CodegenOptions {
            source_map_path: Some(path.to_path_buf()),
            ..CodegenOptions::default()
        })
        .build(&ret.program);

    // Create visualization URL if source map was generated
    if let Some(source_map) = map {
        let result = source_map.to_json_string();
        let hash =
            BASE64_STANDARD.encode(format!("{}\0{}{}\0{}", code.len(), code, result.len(), result));
        println!("https://evanw.github.io/source-map-visualization/#{hash}");
    }

    Ok(())
}
