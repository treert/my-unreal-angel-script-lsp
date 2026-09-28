//! as-cli：调试与验收工具（LSP实现规划 §2 / §9）。
//!
//! - `dump-tree`（M0）：解析 `.as` / `.d.as` 并输出 CST，任一 ERROR/MISSING
//!   节点 ⇒ 退出码非 0（与 grammar P2 验收同口径，证明包装层无损）。
//! - `dump-index`（M1→Phase B）：构建 Workspace（新架构唯一路径）并输出
//!   声明统计——与 `_manifest.dctx` 的 `type_count` / `member_count`
//!   **人工对账**的开发期动作（运行时不读 manifest——D20，仅作参照）。
//!   `--new-arch` 曾是 Phase A 双轨对账开关，切换完成后保留为 no-op
//!   （脚本兼容），对账输出由单一架构的常规统计行承担。

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use as_core::as_syntax::tree_sitter::{Node, Tree};
use as_core::as_syntax::{self, SyntaxErrorKind};
use as_core::aggregation::DeclRef;
use as_core::id::{FileId, Sym};
use as_core::intern::{file_path, intern_file, intern_sym};
use as_core::workspace::{FileInput, FileKind, Workspace};
use as_core::{DefFlags, DefKind, IndexConfig};

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

    /// 构建 Workspace 并输出声明统计（M1 验收：与 manifest 人工对账）
    DumpIndex {
        /// 文件或目录；目录递归收集 *.as / *.d.as
        paths: Vec<PathBuf>,

        /// 裸 float 归一化为 float32（默认 float64，对齐引擎 bScriptFloatIsFloat64）
        #[arg(long)]
        float_is_float32: bool,

        /// 只列出指定名字的符号（kind + 位置 + tags + doc 首行）
        #[arg(long)]
        sym: Option<String>,

        /// 对全部 .as 脚本的标识符使用点跑查找链（M3 验收：命中率 + 未命中样本）
        #[arg(long)]
        resolve_stats: bool,

        /// 引用解析体检（M4/B1 验收）：top-20 名字的查询期 references
        /// 命中数 + 耗时（B1 基线 25450 hits / ~307ms 不回归）
        #[arg(long)]
        ref_stats: bool,

        /// Phase A 双轨对账开关（D37）——Phase B 切换完成后为 no-op
        /// （新架构唯一路径，兼容既有脚本保留 flag）
        #[arg(long)]
        new_arch: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::DumpTree { paths, trees } => dump_tree(&paths, trees),
        Command::DumpIndex { paths, float_is_float32, sym, resolve_stats, ref_stats, new_arch } => {
            dump_index(&paths, !float_is_float32, sym, resolve_stats, ref_stats, new_arch)
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
// dump-index（M1 → Phase B：Workspace 唯一路径）
// ===========================================================================

fn dump_index(
    paths: &[PathBuf],
    float_is_float64: bool,
    sym_filter: Option<String>,
    resolve_stats: bool,
    ref_stats: bool,
    new_arch: bool,
) -> ExitCode {    let files = match collect_inputs(paths) {
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
    let t_build = std::time::Instant::now();
    let ws = Workspace::build(config, inputs);
    let build_elapsed = t_build.elapsed();
    // builtin 伪文件（B2）的 FileId / 声明数（统计口径排除用）
    let builtin_file = intern_file(as_core::workspace::BUILTIN_FILE_PATH, u32::MAX);
    let builtin_decls = ws
        .files
        .get(&builtin_file)
        .map(|e| e.summary.decls.len())
        .unwrap_or(0);
    if new_arch {
        // Phase A 双轨对账的声明面输出（D37）：切换完成后新架构是唯一路径，
        // 双侧对比不再可能——保留 flag 兼容既有脚本，输出单一架构对账行。
        let decls: usize =
            ws.files.values().map(|e| e.summary.decls.len()).sum::<usize>() - builtin_decls;
        let names = ws.agg.main.len();
        println!("---- new-arch (now the only path) ----");
        println!("decls total: {decls} (non-builtin) | main keys: {names} | OK");
    }

    // 文件统计（错误节点清单按需收集——mylua 同款；CLI 批量校验口径不变。
    // builtin 伪文件是空树，verify 恒 0 错误，天然不干扰）
    // builtin 伪文件（B2）不计入文件统计——与旧架构口径一致（内建无快照）
    let (mut n_script, mut n_decl) = (0usize, 0usize);
    let err_counts: Vec<(FileId, usize)> = ws
        .files
        .iter()
        .filter_map(|(file, entry)| {
            let n = as_syntax::verify_tree(&entry.tree).len();
            (n > 0).then_some((*file, n))
        })
        .collect();
    for (&file, entry) in ws.files.iter() {
        if file == builtin_file {
            continue;
        }
        match entry.kind {
            FileKind::Script => n_script += 1,
            FileKind::Decl => n_decl += 1,
        }
    }
    let n_err = err_counts.len();
    println!(
        "files: {} (script {n_script}, decl {n_decl}, parse-error {n_err})",
        ws.files.len() - 1
    );
    if n_err > 0 {
        for (file, n) in &err_counts {
            let path = file_path(*file).unwrap_or("?");
            println!("  ERR {path}: {n} error node(s)");
        }
    }

    // 声明统计（builtin primitive 不计入；按文件类别分列——manifest 只数 .d.as）
    let mut by_kind: BTreeMap<(&'static str, &'static str), usize> = BTreeMap::new();
    let mut synthetic = 0usize;
    for entry in ws.files.values() {
        for d in &entry.summary.decls {
            if d.flags.contains(DefFlags::SYNTHETIC) {
                synthetic += 1;
                continue;
            }
            *by_kind.entry((entry.kind.label(), d.kind.label())).or_insert(0) += 1;
        }
    }
    println!(
        "symbols: {} (synthetic {synthetic} = builtin primitives; delegate/event expansion & StaticClass are query-time, not stored)",
        ws.files.values().map(|e| e.summary.decls.len()).sum::<usize>() - synthetic
    );
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
    let type_jobs: usize = ws
        .files
        .values()
        .map(|e| {
            e.summary
                .decls
                .iter()
                .filter(|d| {
                    !d.flags.contains(DefFlags::SYNTHETIC)
                        && matches!(
                            d.kind,
                            DefKind::Field | DefKind::GlobalVar | DefKind::AssetDecl | DefKind::VirtualProperty
                        )
                })
                .count()
        })
        .sum();
    let mut classes_total = 0usize;
    let mut chain_links = 0usize;
    for (&file, entry) in ws.files.iter() {
        for (i, d) in entry.summary.decls.iter().enumerate() {
            if d.kind != DefKind::Class || d.flags.contains(DefFlags::SYNTHETIC) {
                continue;
            }
            classes_total += 1;
            let r = DeclRef { file, local: i as u32 };
            chain_links += ws.ancestor_chain(&r).len();
            if d.bases.iter().any(|b| b.simple) {
                classes_with_base += 1;
                if ws.base_class(&r).is_none() {
                    unresolved_bases += 1;
                }
            }
        }
    }
    println!(
        "inheritance: classes {classes_total}, chain links {chain_links}, cycles {}, classes-with-base {classes_with_base} (unresolved base {unresolved_bases})",
        ws.cyclic_classes().len()
    );
    println!(
        "mixins: by-name buckets {} entries {}",
        ws.agg.mixin_by_name.len(),
        ws.agg.mixin_by_name.values().map(Vec::len).sum::<usize>()
    );
    println!(
        "types: interned {}, variable decl types resolved {}/{}",
        ws.types.len(),
        ws.resolved.len(),
        type_jobs
    );
    println!("build: {build_elapsed:?} (parse+summary rayon | aggregation+derived serial)");

    // --sym：查符号明细
    if let Some(name) = sym_filter {
        let sym = intern_sym(&name);
        let hits = ws.lookup(sym);
        println!("--- sym '{name}': {} hit(s) ---", hits.len());
        for &r in hits {
            let d = ws.decl(&r);
            let path = file_path(r.file).unwrap_or("?");
            let (line, col) = ws
                .files
                .get(&r.file)
                .map(|e| e.lines.line_col_debug(d.name_span.start))
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
                "  {} {name} {path}:{line}:{col}{tags}  // {doc_first}",
                d.kind.label(),
            );
        }
    }

    // --resolve-stats：对全部 .as 脚本的标识符使用点跑查找链（M3 验收体检）。
    // specifier 语境（UPROPERTY 宏参数等）与命名实参名不计入。
    if resolve_stats {
        let mut total = 0usize;
        let mut hit = 0usize;
        let mut skipped_spec = 0usize;
        let mut skipped_named_arg = 0usize;
        let mut misses: Vec<String> = Vec::new();
        let script_files: Vec<_> = ws
            .files
            .iter()
            .filter(|(_, e)| e.kind == FileKind::Script)
            .map(|(f, _)| *f)
            .collect();
        for file in script_files {
            let entry = ws.files.get(&file).unwrap();
            let src = &entry.source;
            for ident in collect_identifier_nodes(entry.tree.root_node()) {
                if as_core::syntax::in_specifier_context(ident) {
                    skipped_spec += 1;
                    continue;
                }
                if ident.parent().map(|p| p.kind() == "named_argument").unwrap_or(false) {
                    skipped_named_arg += 1;
                    continue;
                }
                total += 1;
                if as_core::resolve::resolve_at(&ws, file, ident.start_byte() as u32).is_some() {
                    hit += 1;
                } else if misses.len() < 20 {
                    let text = ident.utf8_text(src.as_bytes()).unwrap_or("");
                    let (line, col) = entry.lines.line_col_debug(ident.start_byte() as u32);
                    misses.push(format!(
                        "  MISS {}:{}:{col} {text}",
                        file_path(file).unwrap_or("?"),
                        line
                    ));
                }
            }
        }
        println!(
            "--- resolve-stats (script identifiers) ---\n  hit {hit}/{total} ({:.1}%), skipped: specifiers {skipped_spec}, named-args {skipped_named_arg}",
            if total == 0 { 0.0 } else { hit as f64 / total as f64 * 100.0 }
        );
        if !misses.is_empty() {
            println!("  miss samples:");
            for m in &misses {
                println!("{m}");
            }
        }
    }

    // --ref-stats（B1 基线对账的查询期路径——旧倒排路径已删除）：
    // 引用最多的 top-20 名字跑查询期 references，报命中总数 + 耗时。
    // top-20 口径复刻原 ref_index（name → 出现该名字的文件集合，取证自
    // 已删除的 uses.rs：只收 identifier、排除声明名 / specifier 语境 /
    // named_argument 名 / access_specifier level 名）。此处仅作统计口径
    // 选名，不回建倒排架构。B1 基线：25450 hits / query ~307ms (rayon)。
    if ref_stats {
        use as_core::references::{find_references, RefTarget};
        use std::collections::{BTreeSet, HashMap as Map, HashSet};
        let mut ref_files: Map<Sym, BTreeSet<FileId>> = Map::new();
        for (&file, entry) in &ws.files {
            let src = &entry.source;
            // 该文件声明名 span 集合（namespace scoped_name 尾段等）
            let decl_spans: HashSet<as_core::range::TextRange> =
                entry.summary.decls.iter().map(|d| d.name_span).collect();
            for ident in collect_identifier_nodes(entry.tree.root_node()) {
                if ident.kind() != "identifier" {
                    continue; // 原口径只收 identifier（primitive_type 不记录）
                }
                if as_core::syntax::in_specifier_context(ident) {
                    continue;
                }
                let Some(parent) = ident.parent() else { continue };
                if parent.kind() == "access_specifier" {
                    continue;
                }
                if parent.kind() == "named_argument"
                    && parent
                        .child_by_field_name("name")
                        .is_some_and(|n| n.id() == ident.id())
                {
                    continue;
                }
                if is_decl_name_like(parent, ident) {
                    continue; // 形参 / 局部 declarator / 迭代变量等声明名
                }
                if decl_spans.contains(&as_core::range::TextRange::new(
                    ident.start_byte() as u32,
                    ident.end_byte() as u32,
                )) {
                    continue; // 顶层/类级声明名（含 namespace scoped_name 尾段）
                }
                let name = intern_sym(ident.utf8_text(src.as_bytes()).unwrap_or(""));
                ref_files.entry(name).or_default().insert(file);
            }
        }
        let mut top: Vec<(usize, Sym)> =
            ref_files.iter().map(|(s, fs)| (fs.len(), *s)).collect();
        top.sort_by(|a, b| b.0.cmp(&a.0));
        let mut total_hits = 0usize;
        let mut elapsed = std::time::Duration::ZERO;
        let mut queried = 0usize;
        for (i, (_, sym)) in top.iter().enumerate().take(20) {
            let targets: Vec<RefTarget> = ws
                .lookup(*sym)
                .iter()
                .copied()
                .map(RefTarget::Def)
                .collect();
            if targets.is_empty() {
                println!(
                    "  top-{:02} {} ({} files): no declarations — skipped",
                    i + 1,
                    as_core::intern::sym_str(*sym),
                    ref_files.get(sym).map(|s| s.len()).unwrap_or(0)
                );
                continue; // 无声明的名字不可查（B1 同口径）
            }
            let t0 = std::time::Instant::now();
            let hits = find_references(&ws, &targets, false);
            elapsed += t0.elapsed();
            println!(
                "  top-{:02} {} ({} files, {} decls): {} hits",
                i + 1,
                as_core::intern::sym_str(*sym),
                ref_files.get(sym).map(|s| s.len()).unwrap_or(0),
                targets.len(),
                hits.len()
            );
            total_hits += hits.len();
            queried += 1;
        }
        println!(
            "--- ref-stats (query-time, top-20 names by use-site file count) ---\n  queried {queried} names, {total_hits} hits total, elapsed {elapsed:?} (rayon) | B1 baseline: 25450 hits / ~307ms"
        );
    }

    if n_err > 0 || read_failures > 0 || bad_utf8 > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// 声明名字段判定（ref-stats 的 top-20 统计口径——取证自已删除的
/// uses.rs `is_decl_name`：field == "name" 且父节点是声明形态。形参 /
/// 局部 declarator / 迭代变量不进 summary.decls，须按语法判定排除）。
fn is_decl_name_like(parent: Node<'_>, ident: Node<'_>) -> bool {
    let Some(field) = as_core::syntax::field_of_child(parent, ident) else { return false };
    if field != "name" {
        return false;
    }
    matches!(
        parent.kind(),
        "class_declaration"
            | "struct_declaration"
            | "enum_declaration"
            | "function_declaration"
            | "constructor_declaration"
            | "destructor_declaration"
            | "delegate_declaration"
            | "event_declaration"
            | "asset_declaration"
            | "virtual_property_declaration"
            | "parameter"
            | "enumerator"
            | "variable_declarator"
            | "for_each_statement"
    )
}

/// 收集全部 identifier / primitive_type 节点（使用点 + 声明点都算——
/// 声明点走 LEVEL_DECL_SELF 自指命中，也在统计内）。
fn collect_identifier_nodes(root: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    fn walk<'a>(node: Node<'a>, out: &mut Vec<Node<'a>>) {
        if node.kind() == "identifier" || node.kind() == "primitive_type" {
            out.push(node);
            return; // 叶子，不再下潜
        }
        let mut c = node.walk();
        if c.goto_first_child() {
            loop {
                walk(c.node(), out);
                if !c.goto_next_sibling() {
                    break;
                }
            }
        }
    }
    walk(root, &mut out);
    out
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
fn module_of(roots: &[PathBuf], path: &Path) -> Option<Sym> {
    for root in roots {
        if let Ok(rel) = path.strip_prefix(root) {
            let rel = rel.to_string_lossy();
            if !rel.is_empty() {
                return Some(intern_sym(&as_core::workspace::filename_to_module_name(&rel)));
            }
        }
    }
    let name = path.file_name()?.to_string_lossy().into_owned();
    let stem = name
        .strip_suffix(".d.as")
        .or_else(|| name.strip_suffix(".as"))
        .unwrap_or(&name);
    Some(intern_sym(stem))
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
