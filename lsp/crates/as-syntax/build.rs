//! 编译 `grammar/` 的 tree-sitter 生成物（parser.c + 手写 scanner.c）。
//!
//! 生成物不入库（grammar/.gitignore 约定，与 ai-mylua-lsp 一致）：
//! `src/parser.c` 缺失时给出友好报错，提示先在 `grammar/` 里 generate
//! （LSP实现规划 §2.1 / 风险 G1）。

use std::env;
use std::path::PathBuf;

fn main() {
    let crate_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    // crates/as-syntax -> crates -> lsp -> 仓库根 -> grammar/src
    let grammar_src = crate_dir.join("../../../grammar/src");

    let parser_c = grammar_src.join("parser.c");
    let scanner_c = grammar_src.join("scanner.c");

    if !parser_c.exists() {
        panic!(
            "tree-sitter parser not generated: {} is missing.\n\
             \n\
             The generated grammar sources are NOT committed to this repository.\n\
             Generate them first (see grammar/README.md):\n\
             \n\
                 cd grammar\n\
                 npm install\n\
                 npx tree-sitter generate\n",
            parser_c.display()
        );
    }

    if !scanner_c.exists() {
        panic!(
            "grammar/src/scanner.c is missing (it is hand-written and should be committed\n\
             with the grammar; the repository is in a broken state)"
        );
    }

    cc::Build::new()
        .include(&grammar_src)
        .file(&parser_c)
        .file(&scanner_c)
        .compile("tree_sitter_angelscript");

    println!("cargo:rerun-if-changed={}", parser_c.display());
    println!("cargo:rerun-if-changed={}", scanner_c.display());
}
