//! as-cli：调试与验收工具（LSP实现规划 §2 / §9）。
//!
//! - `dump-tree`（M0）：解析 `.as` / `.d.as` 并输出 CST，任一 ERROR/MISSING
//!   节点 ⇒ 退出码非 0（与 grammar P2 验收同口径，证明包装层无损）。
//! - `dump-index`（M1）：构建 WorkspaceIndex 并输出声明统计——与
//!   `_manifest.dctx` 的 `type_count` / `member_count` **人工对账**的开发期
//!   动作（运行时不读 manifest——D20，该文件仅作参照）。

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use as_core::as_syntax::tree_sitter::{Node, Tree};
use as_core::as_syntax::{self, SyntaxErrorKind};
use as_core::id::DefId;
use as_core::intern::{file_path, intern_file};
use as_core::{DefFlags, DefKind, FileInput, FileKind, IndexConfig, WorkspaceIndex};

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

    /// 构建 WorkspaceIndex 并输出声明统计（M1 验收：与 manifest 人工对账）
    DumpIndex {
        /// 文件或目录；目录递归收集 *.as / *.d.as
        paths: Vec<PathBuf>,

        /// 裸 float 归一化为 float32（默认 float64，对齐引擎 bScriptFloatIsFloat64）
        #[arg(long)]
        float_is_float32: bool,

        /// 只列出指定名字的符号（kind + 位置 + tags + doc 首行）
        #[arg(long)]
        sym: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::DumpTree { paths, trees } => dump_tree(&paths, trees),
        Command::DumpIndex { paths, float_is_float32, sym } => {
            dump_index(&paths, !float_is_float32, sym)
        }
    }
}

// ===========================================================================
// dump-tree（M0）
// ===========================================================================

struct BatchStats {
    files: usize,
    failed_files: usize,
    errors: usize,
    bad_utf8: usize,
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

// ===========================================================================
// dump-index（M1）
// ===========================================================================

fn dump_index(paths: &[PathBuf], float_is_float64: bool, sym_filter: Option<String>) -> ExitCode {    let files = match collect_inputs(paths) {
        Ok(files) => files,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    if files.is_empty() {
        eprintln!("error: no .as / .d.as files found");
        return ExitCode::FAILURE;
    }

    let mut read_failures = 0usize;
    let mut bad_utf8 = 0usize;
    let mut inputs = Vec::new();
    for path in &files {
        let Ok(bytes) = fs::read(path) else {
            eprintln!("ERR   {}: read failed", path.display());
            read_failures += 1;
            continue;
        };
        let Ok(source) = String::from_utf8(bytes) else {
            eprintln!("ERR   {}: not valid UTF-8", path.display());
            bad_utf8 += 1;
            continue;
        };
        let path_str = path.to_string_lossy().into_owned();
        let file = intern_file(&path_str, 0);
        inputs.push(FileInput {
            file,
            kind: file_kind_of(path),
            module: module_of(paths, path),
            source,
        });
    }

    let config = IndexConfig { float_is_float64 };
    println!("config: float_is_float64={float_is_float64}");
    let idx = WorkspaceIndex::build(config, inputs);

    // 文件统计
    let (mut n_script, mut n_decl, mut n_err) = (0usize, 0usize, 0usize);
    for snap in idx.files.values() {
        match snap.kind {
            FileKind::Script => n_script += 1,
            FileKind::Decl => n_decl += 1,
        }
        if !snap.errors.is_empty() {
            n_err += 1;
        }
    }
    println!("files: {} (script {n_script}, decl {n_decl}, parse-error {n_err})", idx.files.len());
    if n_err > 0 {
        for (file, snap) in &idx.files {
            if !snap.errors.is_empty() {
                let path = file_path(*file).unwrap_or("?");
                println!("  ERR {path}: {} error node(s)", snap.errors.len());
            }
        }
    }

    // 声明统计（SYNTHETIC 内建不计入；按文件类别分列——manifest 只数 .d.as）
    let mut by_kind: BTreeMap<(&'static str, &'static str), usize> = BTreeMap::new();
    let mut synthetic = 0usize;
    for (_id, def) in idx.symbols.iter() {
        if def.flags.contains(DefFlags::SYNTHETIC) {
            synthetic += 1;
            continue;
        }
        let kind = idx.files.get(&def.file).map(|s| s.kind).unwrap_or(FileKind::Script);
        *by_kind.entry((kind.label(), def.kind.label())).or_insert(0) += 1;
    }
    println!("symbols: {} (synthetic {synthetic} = builtins + delegate/event expansion + StaticClass)", idx.symbols.len());
    for kind_label in ["decl", "script"] {
        let rows: Vec<String> = by_kind
            .iter()
            .filter(|((k, _), _)| *k == kind_label)
            .map(|((_, dk), n)| format!("{dk} {n}"))
            .collect();
        if rows.is_empty() {
            continue;
        }
        println!("--- {kind_label}-file symbols ---");
        println!("  {}", rows.join("  "));
    }
    // 对账口径提示（manifest 只统计 .d.as 侧）
    let decl = |k: &'static str| by_kind.get(&("decl", k)).copied().unwrap_or(0);
    println!(
        "--- reconcile (decl-only) ---\n  type_count~ {} (class {} + struct {} + enum {})\n  member_count~ {} (field {} + method {} + ctor {} + dtor {} + operator {} + vprop {})",
        decl("class") + decl("struct") + decl("enum"),
        decl("class"),
        decl("struct"),
        decl("enum"),
        decl("field") + decl("method") + decl("constructor") + decl("destructor") + decl("operator") + decl("virtual_property"),
        decl("field"),
        decl("method"),
        decl("constructor"),
        decl("destructor"),
        decl("operator"),
        decl("virtual_property"),
    );

    // 继承与类型解析健康度
    let mut unresolved_bases = 0usize;
    let mut classes_with_base = 0usize;
    for (id, def) in idx.symbols.iter() {
        if def.kind != DefKind::Class || def.flags.contains(DefFlags::SYNTHETIC) {
            continue;
        }
        if let as_core::DefExtra::TypeDecl { bases, .. } = &def.extra {
            if bases.iter().any(|b| b.simple) {
                classes_with_base += 1;
                if idx.resolve_base_class(id).is_none() {
                    unresolved_bases += 1;
                }
            }
        }
    }
    let type_jobs: Vec<DefId> = idx
        .symbols
        .iter()
        .filter(|(_, d)| {
            !d.flags.contains(DefFlags::SYNTHETIC)
                && matches!(
                    d.kind,
                    DefKind::Field | DefKind::GlobalVar | DefKind::AssetDecl | DefKind::VirtualProperty
                )
        })
        .map(|(id, _)| id)
        .collect();
    println!(
        "inheritance: class closures {}, cycles {}, classes-with-base {classes_with_base} (unresolved base {unresolved_bases})",
        idx.closures.len(),
        idx.cycle_classes.len()
    );
    println!(
        "mixins: indexed {}, pending {}",
        idx.mixin_index.values().map(Vec::len).sum::<usize>(),
        idx.mixin_pending.len()
    );
    println!(
        "types: interned {}, variable decl types resolved {}/{}",
        idx.types.len(),
        idx.resolved.len(),
        type_jobs.len()
    );

    // --sym：查符号明细
    if let Some(name) = sym_filter {
        let sym = as_core::intern::intern_sym(&name);
        let hits = idx.main.get(&sym).map(Vec::as_slice).unwrap_or(&[]);
        println!("--- sym '{name}': {} hit(s) ---", hits.len());
        for &def in hits {
            let d = idx.def(def);
            let path = file_path(d.file).unwrap_or("?");
            let (line, col) = idx
                .files
                .get(&d.file)
                .map(|s| s.lines.line_col_debug(d.name_span.start))
                .unwrap_or((0, 0));
            let mut tags = String::new();
            for t in &d.tags {
                let _ = write!(tags, " {}", t.kind.name());
            }
            let doc_first = d
                .doc
                .as_deref()
                .and_then(|doc| doc.lines().next())
                .unwrap_or("")
                .chars()
                .take(60)
                .collect::<String>();
            println!(
                "  {} {} {path}:{line}:{col}{tags}  // {doc_first}",
                d.kind.label(),
                name,
            );
        }
    }

    if n_err > 0 || read_failures > 0 || bad_utf8 > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// `.as` 与 `.d.as` 都以 `.as` 结尾；`.d.as` → Decl（Phase 0 文件类别标签）。
fn file_kind_of(path: &Path) -> FileKind {
    let name = path.file_name().and_then(OsStr::to_str).unwrap_or("");
    if name.to_ascii_lowercase().ends_with(".d.as") {
        FileKind::Decl
    } else {
        FileKind::Script
    }
}

/// 模块名：相对首个包含它的 CLI 收集根；直接传入的文件取文件名主干。
fn module_of(roots: &[PathBuf], path: &Path) -> Option<as_core::id::Sym> {
    for root in roots {
        if let Ok(rel) = path.strip_prefix(root) {
            let rel = rel.to_string_lossy();
            if !rel.is_empty() {
                return Some(as_core::intern::intern_sym(
                    &as_core::index::filename_to_module_name(&rel),
                ));
            }
        }
    }
    let name = path.file_name()?.to_string_lossy().into_owned();
    let stem = name
        .strip_suffix(".d.as")
        .or_else(|| name.strip_suffix(".as"))
        .unwrap_or(&name);
    Some(as_core::intern::intern_sym(stem))
}

fn collect_inputs(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for path in paths {
        if path.is_file() {
            if !is_as_path(path) {
                return Err(format!("not a .as / .d.as file: {}", path.display()));
            }
            out.push(path.clone());
        } else if path.is_dir() {
            collect_as_files(path, &mut out);
        } else {
            return Err(format!("path not found: {}", path.display()));
        }
    }
    out.sort();
    Ok(out)
}

// ===========================================================================
// 共用
// ===========================================================================

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
