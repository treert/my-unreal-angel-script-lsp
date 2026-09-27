//! as-cli：调试与验收工具（LSP实现规划 §2 / §9）。
//!
//! M0 仅 `dump-tree`：解析 `.as` / `.d.as` 并输出 CST，任一 ERROR/MISSING
//! 节点 ⇒ 退出码非 0。这是 M0 的验收判据——与 grammar P2 验收（tree-sitter
//! cli 对同语料零 ERROR）同口径，证明 Rust 包装层无损。
//! M1 追加 `dump-index`。

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use as_core::as_syntax::{self, SyntaxErrorKind};
use as_core::as_syntax::tree_sitter::{Node, Tree};

#[derive(Parser)]
#[command(
    name = "as-cli",
    about = "my-as-lsp 调试与验收工具（dump-tree / dump-index）"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 解析 .as / .d.as 并 dump CST；任一 ERROR/MISSING 节点 → 退出码非 0
    DumpTree {
        /// 文件或目录；目录递归收集 *.as / *.d.as
        paths: Vec<PathBuf>,

        /// 目录模式下也逐文件打印 CST（默认只有单文件入参才打印）
        #[arg(long)]
        trees: bool,
    },
}

struct BatchStats {
    files: usize,
    failed_files: usize,
    errors: usize,
    bad_utf8: usize,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::DumpTree { paths, trees } => dump_tree(&paths, trees),
    }
}

fn dump_tree(paths: &[PathBuf], force_trees: bool) -> ExitCode {
    if paths.is_empty() {
        eprintln!("error: no input path (usage: as-cli dump-tree <file-or-dir>...)");
        return ExitCode::FAILURE;
    }

    let mut stats = BatchStats { files: 0, failed_files: 0, errors: 0, bad_utf8: 0 };
    let mut dir_files: Vec<PathBuf> = Vec::new();

    for path in paths {
        if path.is_file() {
            if !is_as_path(path) {
                eprintln!("error: not a .as / .d.as file: {}", path.display());
                return ExitCode::FAILURE;
            }
            process_file(path, true, &mut stats);
        } else if path.is_dir() {
            collect_as_files(path, &mut dir_files);
        } else {
            eprintln!("error: path not found: {}", path.display());
            return ExitCode::FAILURE;
        }
    }

    if !dir_files.is_empty() {
        dir_files.sort();
        for f in &dir_files {
            process_file(f, force_trees, &mut stats);
        }
    }

    if stats.files == 0 {
        eprintln!("error: no .as / .d.as files found");
        return ExitCode::FAILURE;
    }

    println!(
        "---- {} files, {} with errors, {} ERROR/MISSING nodes, {} non-UTF-8",
        stats.files, stats.failed_files, stats.errors, stats.bad_utf8
    );

    if stats.failed_files > 0 || stats.bad_utf8 > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn process_file(path: &Path, print_tree: bool, stats: &mut BatchStats) {
    stats.files += 1;

    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("ERR   {}: read failed: {}", path.display(), e);
            stats.failed_files += 1;
            return;
        }
    };
    let src = match String::from_utf8(bytes) {
        Ok(src) => src,
        Err(_) => {
            println!("ERR   {}: not valid UTF-8", path.display());
            stats.bad_utf8 += 1;
            stats.failed_files += 1;
            return;
        }
    };

    let tree = as_syntax::parse(&src, None);
    let errs = as_syntax::verify_tree(&tree);

    if print_tree {
        print!("{}", render_tree(&tree, &src));
    }

    if errs.is_empty() {
        if !print_tree {
            println!("ok    {}", path.display());
        }
        return;
    }

    stats.failed_files += 1;
    stats.errors += errs.len();
    println!("ERR   {} ({} ERROR/MISSING)", path.display(), errs.len());
    for e in &errs {
        let (line, col) = line_col(&src, e.start_byte);
        let what = match e.kind {
            SyntaxErrorKind::Error => "ERROR".to_string(),
            SyntaxErrorKind::Missing => format!("MISSING {}", e.node_kind),
        };
        println!("      {line}:{col} {what}");
    }
}

// ---------------------------------------------------------------------------
// CST 渲染（对齐 tree-sitter corpus 风格）
// ---------------------------------------------------------------------------

fn render_tree(tree: &Tree, src: &str) -> String {
    let mut out = String::new();
    render_node(tree.root_node(), src, 0, "", &mut out);
    out
}

/// 具名节点 `(kind ...)`（子节点各占一行，`field: ` 前缀标注字段，闭括号接在
/// 末子节点行尾，CLI 风格）；匿名 token 按源文本 `"text"`；恢复节点渲染为
/// `(ERROR ...)` / `(MISSING kind)`。
fn render_node(node: Node<'_>, src: &str, indent: usize, prefix: &str, out: &mut String) {
    let _ = write!(out, "{}{}", "  ".repeat(indent), prefix);

    if node.is_missing() {
        let _ = writeln!(out, "(MISSING {})", node.kind());
        return;
    }
    if !node.is_named() {
        let text = node.utf8_text(src.as_bytes()).unwrap_or("");
        let _ = writeln!(out, "{text:?}");
        return;
    }

    if node.child_count() == 0 {
        let _ = writeln!(out, "({})", node.kind());
        return;
    }

    let _ = writeln!(out, "({}", node.kind());
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let field = cursor
                .field_name()
                .map_or(String::new(), |f| format!("{f}: "));
            render_node(cursor.node(), src, indent + 1, &field, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    // 去掉末子节点行尾换行，把 ')' 接上（CLI 风格）
    out.truncate(out.len() - 1);
    out.push_str(")\n");
}

// ---------------------------------------------------------------------------
// 文件收集与行号
// ---------------------------------------------------------------------------

/// `.as` 与 `.d.as` 都以 `.as` 结尾（`_manifest.dctx` 天然不匹配）。
fn is_as_path(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .map(|name| name.to_ascii_lowercase().ends_with(".as"))
        .unwrap_or(false)
}

fn collect_as_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("warn: cannot read dir {}: {}", dir.display(), e);
            return;
        }
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    children.sort();
    for child in children {
        if child.is_dir() {
            collect_as_files(&child, out);
        } else if is_as_path(&child) {
            out.push(child);
        }
    }
}

/// 字节偏移 → (行, 列)，均 1 起。列按字节数计（调试工具够用；
/// LSP 协议层的 UTF-16 换算是另一回事，见 LSP实现规划 §3.2.1）。
fn line_col(src: &str, byte: usize) -> (usize, usize) {
    let bytes = src.as_bytes();
    let byte = byte.min(bytes.len());
    let (mut line, mut col) = (1usize, 1usize);
    for &c in &bytes[..byte] {
        if c == b'\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}
