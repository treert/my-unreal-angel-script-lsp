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
use as_core::id::{DefId, FileId};
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

        /// 对全部 .as 脚本的标识符使用点跑查找链（M3 验收：命中率 + 未命中样本）
        #[arg(long)]
        resolve_stats: bool,

        /// 对全部文件的 UseSite 跑引用解析内核并计时（M4 验收：站点数 / 解析率 / 耗时）
        #[arg(long)]
        ref_stats: bool,

        /// 双轨对账（Phase A 验收，D37）：同时构建新架构（summary +
        /// Aggregation）与旧 WorkspaceIndex，比对声明数 / 名字集合，
        /// 任何不一致 → 退出码非 0；并打印两侧耗时
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
// dump-index（M1）
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
    // 双轨对账（Phase A，D37）：旧侧先建（inputs 被 move）；新侧用
    // 独立 parse（source 克隆一份），两侧同源同配置。
    let new_arch_sources: Vec<(as_core::id::FileId, as_core::FileKind, Option<as_core::id::Sym>, String)> =
        inputs
            .iter()
            .map(|i| (i.file, i.kind, i.module, i.source.clone()))
            .collect();
    let t_old = std::time::Instant::now();
    let idx = WorkspaceIndex::build(config, inputs);
    let old_elapsed = t_old.elapsed();
    if new_arch {
        use rayon::prelude::*;
        use std::collections::HashMap;
        let t_new = std::time::Instant::now();
        let summaries: HashMap<as_core::id::FileId, as_core::summary::FileSummary> =
            new_arch_sources
                .into_par_iter()
                .map(|(file, kind, module, source)| {
                    let tree = as_core::as_syntax::parse(&source, None);
                    let s = as_core::summary::extract_summary(&tree, &source, kind, module, &config);
                    (file, s)
                })
                .collect();
        let agg = as_core::Aggregation::build(
            &summaries.iter().map(|(f, s)| (*f, s)).collect::<std::collections::HashMap<_, _>>(),
        );
        let new_elapsed = t_new.elapsed();

        // 对账 ①：非合成声明总数（旧侧 SYNTHETIC = 内建 + delegate 展开
        // + namespace 合成，均为索引期产物，新侧不迁移）
        let new_decls: usize = summaries.values().map(|s| s.decls.len()).sum();
        let old_real = idx
            .symbols
            .iter()
            .filter(|(_, d)| !d.flags.contains(as_core::DefFlags::SYNTHETIC))
            .count();
        // 对账 ②：名字集合（旧侧桶内须有非合成成员——合成 namespace 不计）
        let old_names: std::collections::BTreeSet<as_core::id::Sym> = idx
            .main
            .iter()
            .filter(|(_, defs)| {
                defs.iter().any(|&id| {
                    !idx.symbols.get(id).flags.contains(as_core::DefFlags::SYNTHETIC)
                })
            })
            .map(|(s, _)| *s)
            .collect();
        let new_names: std::collections::BTreeSet<as_core::id::Sym> =
            agg.main.keys().copied().collect();

        println!("---- new-arch reconcile ----");
        println!(
            "decls total: new {new_decls} vs old {old_real} | {}",
            if new_decls == old_real { "OK" } else { "MISMATCH" }
        );
        println!(
            "main keys: new {} vs old {} | {}",
            new_names.len(),
            old_names.len(),
            if new_names == old_names { "OK" } else { "MISMATCH" }
        );
        if new_names != old_names {
            use std::fmt::Write as _;
            let mut sample = String::new();
            for s in new_names.symmetric_difference(&old_names).take(8) {
                let _ = write!(sample, " {}", as_core::intern::sym_str(*s));
            }
            println!("  diff sample:{sample}");
        }
        println!(
            "timing: old {old_elapsed:?} | new {new_elapsed:?} (parse+summary+aggregation, rayon)"
        );
        if new_decls != old_real || new_names != old_names {
            return ExitCode::FAILURE;
        }
    }

    // 文件统计（错误节点清单已不在快照里——诊断期按需收集（mylua 同款）；
    // CLI 的批量校验口径不变：has_error 剪枝对合法文件 O(1)）
    let (mut n_script, mut n_decl, mut n_err) = (0usize, 0usize, 0usize);
    let err_counts: Vec<(FileId, usize)> = idx
        .files
        .iter()
        .filter_map(|(file, snap)| {
            let n = as_syntax::verify_tree(&snap.tree).len();
            (n > 0).then_some((*file, n))
        })
        .collect();
    for snap in idx.files.values() {
        match snap.kind {
            FileKind::Script => n_script += 1,
            FileKind::Decl => n_decl += 1,
        }
    }
    n_err = err_counts.len();
    println!("files: {} (script {n_script}, decl {n_decl}, parse-error {n_err})", idx.files.len());
    if n_err > 0 {
        for (file, n) in &err_counts {
            let path = file_path(*file).unwrap_or("?");
            println!("  ERR {path}: {n} error node(s)");
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

    // --resolve-stats：对全部 .as 脚本的标识符使用点跑查找链（M3 验收体检）。
    // specifier 语境（UPROPERTY 宏参数等）与命名实参名不计入——前者不是符号
    // 使用点，后者的解析（callee 形参匹配）随 M5 签名帮助同批。
    if resolve_stats {
        let mut total = 0usize;
        let mut hit = 0usize;
        let mut skipped_spec = 0usize;
        let mut skipped_named_arg = 0usize;
        let mut misses: Vec<String> = Vec::new();
        let script_files: Vec<_> = idx
            .files
            .iter()
            .filter(|(_, s)| s.kind == FileKind::Script)
            .map(|(f, _)| *f)
            .collect();
        for file in script_files {
            let snap = idx.files.get(&file).unwrap();
            let src = &snap.source;
            for ident in collect_identifier_nodes(snap.tree.root_node()) {
                if as_core::syntax::in_specifier_context(ident) {
                    skipped_spec += 1;
                    continue;
                }
                if ident.parent().map(|p| p.kind() == "named_argument").unwrap_or(false) {
                    skipped_named_arg += 1;
                    continue;
                }
                total += 1;
                if as_core::resolve::resolve_at(&idx, file, ident.start_byte() as u32).is_some() {
                    hit += 1;
                } else if misses.len() < 20 {
                    let text = ident.utf8_text(src.as_bytes()).unwrap_or("");
                    let (line, col) = snap.lines.line_col_debug(ident.start_byte() as u32);
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

    // --ref-stats：对全部文件的 UseSite 跑引用解析内核（M4 验收体检——
    // 站点数 / 解析率 / 耗时）。references 首次请求的成本即此（后续命中缓存）。
    if ref_stats {
        let start = std::time::Instant::now();
        let sites: usize = idx.files.values().map(|s| s.uses.len()).sum();
        let mut resolved = 0usize;
        let mut targets_total = 0usize;
        let mut per_file: Vec<(std::time::Duration, usize, as_core::id::FileId)> = Vec::new();
        let all_files: Vec<_> = idx.files.keys().copied().collect();
        for file in all_files {
            let fstart = std::time::Instant::now();
            let fsites = idx.files.get(&file).map(|s| s.uses.len()).unwrap_or(0);
            for u in as_core::references::resolve_file_uses(&idx, file) {
                resolved += 1;
                targets_total += u.targets.len();
            }
            per_file.push((fstart.elapsed(), fsites, file));
        }
        per_file.sort_by(|a, b| b.0.cmp(&a.0));
        println!("  slowest files:");
        for (dur, n, file) in per_file.iter().take(5) {
            println!("    {:>10.?}  {n:>5} sites  {}", dur, file_path(*file).unwrap_or("?"));
        }
        println!(
            "--- ref-stats (all use sites) ---\n  sites {sites}, resolved {resolved} ({:.1}%), targets {targets_total}, elapsed {:?}",
            if sites == 0 { 0.0 } else { resolved as f64 / sites as f64 * 100.0 },
            start.elapsed()
        );

        // B1 A/B 对账：引用最多的 top-20 名字，倒排路径 vs 查询期路径
        //（字符串扫 + 逐点解析验证）结果必须逐位相等——D5 翻案的语料级验证
        use as_core::references::{find_references, find_references_query};
        use as_core::id::Sym;
        let mut top: Vec<(usize, Sym)> = idx
            .ref_index
            .iter()
            .map(|(sym, fs)| (fs.len(), *sym))
            .collect();
        top.sort_by(|a, b| b.0.cmp(&a.0));
        let t_ab = std::time::Instant::now();
        let mut ab_fail = 0usize;
        let mut old_total = 0usize;
        let mut new_total = 0usize;
        let mut old_elapsed = std::time::Duration::ZERO;
        let mut new_elapsed = std::time::Duration::ZERO;
        for (_, sym) in top.iter().take(20) {
            // 目标 = 该名字的全部声明（重载组整体——references 的真实查询形态）
            let targets: Vec<as_core::RefTarget> = idx
                .main
                .get(sym)
                .map(|ds| ds.iter().copied().map(as_core::RefTarget::Def).collect())
                .unwrap_or_default();
            if targets.is_empty() {
                continue;
            }
            let t0 = std::time::Instant::now();
            let old = find_references(&idx, &targets);
            old_elapsed += t0.elapsed();
            let t1 = std::time::Instant::now();
            let new = find_references_query(&idx, &targets, false);
            new_elapsed += t1.elapsed();
            old_total += old.len();
            new_total += new.len();
            if old != new {
                ab_fail += 1;
                println!(
                    "  AB MISMATCH {}: old {} hits vs new {} hits",
                    as_core::intern::sym_str(*sym),
                    old.len(),
                    new.len()
                );
            }
        }
        println!(
            "  A/B (top-20 names): old {old_total} hits | query {new_total} hits | {} | old {:?} / query {:?} (rayon)",
            if ab_fail == 0 { "MATCH" } else { "MISMATCH" },
            old_elapsed,
            new_elapsed,
        );
        let _ = t_ab;
        if ab_fail > 0 {
            return ExitCode::FAILURE;
        }
    }

    if n_err > 0 || read_failures > 0 || bad_utf8 > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
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
