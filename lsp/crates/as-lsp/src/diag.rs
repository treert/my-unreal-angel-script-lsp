//! 诊断发布管道（Phase D / D40：调度队列消费侧）。
//!
//! 时序（spec §2）：
//! - didOpen / didChange → 调度器标热 + 300ms 防抖 → [`drain`]；
//! - 结构变化（surface_changed，含改基类——P6）或快照发布（`request_full`）
//!   → 全量队列（打开文件在前，其余 FileId 升序，P3/P4）；
//! - didClose → 立即推空数组清空（D36 行为保留）+ invalidate；
//! - Loading 期不 schedule（调用方 is_ready 门控），快照发布的
//!   request_full 兜底。
//!
//! 规则本体与抑制过滤在 `as_core::diag`（纯函数）；本层只做 UTF-16 换算
//! 与协议映射（§3.2.1：换算只在本层发生）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tower_lsp_server::ls_types::{self as ls, *};
use tower_lsp_server::Client;

use as_core::diag::{filter_suppressions, script_diags, undefined_call_diags, Diag, DiagSeverity};
use as_core::id::FileId;
use as_core::workspace::Workspace;
use as_core::{as_syntax, FileKind, LineIndex};

use crate::diagnostic_scheduler::DiagnosticScheduler;
use crate::docs::DocStore;
use crate::workspace::{kind_of_path, WorkspaceState};

/// 一轮 drain 的工作区事实（每 drain 算一次，ms 级）。
pub struct DrainFacts {
    /// 索引中无真实 `.d.as`（builtin 伪文件不算——B2 口径）。仅作
    /// undefined-function 的门控（decl 全缺失时引擎函数一概不可解析，
    /// 整类跳过；`missing-type-decls` 诊断已移除，码表 v2.2）。
    pub decl_missing: bool,
    /// cyclic-inheritance：文件 → 环诊断（未经抑制过滤，`file_ls_diags` 统一滤）。
    pub cycle_diags: HashMap<FileId, Vec<Diag>>,
}

/// per-file 统一入口（打开 / 未打开同构，P9）：
/// parse-error ∪ undefined-function（`Script && !decl_missing`——decl 全缺失时
/// 引擎函数一概不可解析，语义诊断整类跳过）∪ cyclic-inheritance，经抑制过滤。
fn file_ls_diags(
    ws: &Workspace,
    file: FileId,
    tree: &as_syntax::tree_sitter::Tree,
    text: &str,
    lines: &LineIndex,
    is_script: bool,
    facts: &DrainFacts,
) -> Vec<ls::Diagnostic> {
    let mut diags = script_diags(tree, text);
    if is_script && !facts.decl_missing {
        diags.extend(undefined_call_diags(ws, file, tree, text));
    }
    if let Some(cs) = facts.cycle_diags.get(&file) {
        diags.extend(cs.iter().cloned());
    }
    diags.sort_by_key(|d| d.range.start);
    // cyclic-inheritance 不经 script_diags 内部过滤，统一再滤（对已滤部分幂等）
    let diags = filter_suppressions(diags, text);
    diags
        .into_iter()
        .map(|d| {
            let (sl, sc) = lines.line_col_utf16(text, d.range.start);
            let (el, ec) = lines.line_col_utf16(text, d.range.end);
            Diagnostic {
                range: Range::new(Position::new(sl, sc), Position::new(el, ec)),
                severity: Some(match d.severity {
                    DiagSeverity::Error => DiagnosticSeverity::ERROR,
                    DiagSeverity::Warning => DiagnosticSeverity::WARNING,
                    DiagSeverity::Information => DiagnosticSeverity::INFORMATION,
                    DiagSeverity::Hint => DiagnosticSeverity::HINT,
                }),
                code: Some(NumberOrString::String(d.code.to_string())),
                code_description: None,
                source: Some("my-as-lsp".to_string()),
                message: d.message,
                tags: None,
                related_information: None,
                data: None,
            }
        })
        .collect()
}

/// 消费一轮调度队列（consumer 任务调用：
/// `loop { notified().await; drain().await }`）。
pub async fn drain(
    client: &Client,
    docs: &Mutex<DocStore>,
    ws: &WorkspaceState,
    sched: &Arc<DiagnosticScheduler>,
) {
    if !ws.is_ready() {
        return;
    }
    // ① 全部打开文档保鲜（工作区事实要看最新 overlay），收集 surface 变化
    let open: Vec<FileId> = docs.lock().unwrap().entries().map(|(f, _)| f).collect();
    let mut surface_changed = false;
    for file in open {
        surface_changed |= ws.ensure_file_fresh(file, docs);
    }
    // ② 全量队列：request_full（快照发布）或本轮检出结构变化（P4）
    if sched.take_full_flag() || surface_changed {
        let open_set: HashSet<FileId> = docs.lock().unwrap().entries().map(|(f, _)| f).collect();
        let mut files: Vec<FileId> = ws
            .with(|idx| {
                idx.files
                    .iter()
                    .filter(|(_, e)| e.kind == FileKind::Script)
                    .map(|(&f, _)| f)
                    .collect()
            })
            .unwrap_or_default();
        // 打开文件在前，其余 FileId 升序（P3）
        files.sort_by_key(|f| (!open_set.contains(f), *f));
        sched.set_full_queue(files);
    }
    // ③ 工作区事实
    let facts = ws
        .with(|idx| DrainFacts {
            decl_missing: !idx.has_decl_files(),
            cycle_diags: idx.cycle_diags(),
        })
        .unwrap_or(DrainFacts { decl_missing: false, cycle_diags: HashMap::new() });
    // ④ pop 循环：热优先 → 队列；打开取 overlay（带 version）+ 索引（语义
    //    诊断需要工作区），未打开取索引 FileEntry（version=None）；都不在
    //    （并发删除）→ 跳过。锁序 docs → index 读（既有约定）：打开文档
    //    路径在 docs 守卫内取 index 读（publish/reindex 侧无反向嵌套）。
    while let Some(file) = sched.pop() {
        let Some(path) = as_core::intern::file_path(file) else { continue };
        let Some(uri) = ls::Uri::from_file_path(path) else { continue };
        let is_script = kind_of_path(path) == FileKind::Script;
        let computed = {
            let store = docs.lock().unwrap();
            match store.get(file) {
                Some(doc) => ws
                    .with(|idx| {
                        Some((
                            file_ls_diags(
                                idx,
                                file,
                                &doc.tree,
                                &doc.text,
                                &doc.lines,
                                is_script,
                                &facts,
                            ),
                            Some(doc.version),
                        ))
                    })
                    .flatten(),
                None => None,
            }
        };
        let computed = match computed {
            Some(x) => Some(x),
            None => ws
                .with(|idx| {
                    idx.files.get(&file).map(|e| {
                        (
                            file_ls_diags(idx, file, &e.tree, &e.source, &e.lines, is_script, &facts),
                            None,
                        )
                    })
                })
                .flatten(),
        };
        if let Some((diags, version)) = computed {
            client.publish_diagnostics(uri, diags, version).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docs::DocStore;

    // 单测用例内置于源码（D1）。

    fn code(s: &str) -> Option<NumberOrString> {
        Some(NumberOrString::String(s.to_string()))
    }

    fn script_entry(file: &str, src: &str) -> as_core::FileInput {
        as_core::FileInput {
            file: as_core::intern::intern_file(file, 0),
            kind: FileKind::Script,
            module: None,
            source: src.to_string(),
        }
    }

    #[test]
    fn file_ls_diags_maps_code_and_severity() {
        let mut store = DocStore::new();
        // 未闭合宏（parse-error）；decl_missing 也不再产 missing-type-decls（已移除）
        let src = "UFUNCTION(Blueprint\nvoid F() {}\n".to_string();
        let file = store.open("unique://lspdiag/a.as", 1, src);
        // 空索引：decl_missing ⇒ 语义诊断整类跳过（脚本无需入索引）
        let idx = as_core::workspace::Workspace::build(as_core::IndexConfig::default(), vec![]);
        let doc = store.get(file).unwrap();
        let facts = DrainFacts { decl_missing: true, cycle_diags: HashMap::new() };
        let diags = file_ls_diags(&idx, file, &doc.tree, &doc.text, &doc.lines, true, &facts);
        assert!(
            diags.iter().all(|d| d.code != code("missing-type-decls")),
            "missing-type-decls 已移除（码表 v2.2）：{diags:?}"
        );
        let parse_diag = diags.iter().find(|d| d.code == code("parse-error")).unwrap();
        assert_eq!(parse_diag.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(parse_diag.source.as_deref(), Some("my-as-lsp"));
        assert_eq!(parse_diag.range.start.line, 0);
    }

    #[test]
    fn file_ls_diags_clean_when_decls_present() {
        let mut store = DocStore::new();
        let src = "int X = 1;\n";
        let file = store.open("unique://lspdiag/b.as", 1, src.to_string());
        let inputs = vec![
            as_core::FileInput {
                file: as_core::intern::intern_file("unique://lspdiag/decl.d.as", 0),
                kind: FileKind::Decl,
                source: "struct FVector { float X; }\n".to_string(),
                module: None,
            },
            as_core::FileInput {
                // 语义诊断需要脚本在索引内（SemCtx 查 scope_tree）
                file,
                kind: FileKind::Script,
                source: src.to_string(),
                module: None,
            },
        ];
        let idx = as_core::workspace::Workspace::build(as_core::IndexConfig::default(), inputs);
        let doc = store.get(file).unwrap();
        let facts = DrainFacts { decl_missing: false, cycle_diags: HashMap::new() };
        assert!(
            file_ls_diags(&idx, file, &doc.tree, &doc.text, &doc.lines, true, &facts).is_empty()
        );
    }

    /// decl_missing 门控（missing-type-decls 移除后仍保留）：decl 全缺失 ⇒
    /// undefined-function 整类跳过；decl 在位 ⇒ 报。
    #[test]
    fn file_ls_diags_skips_undefined_function_when_decls_missing() {
        const SRC: &str = "void F()\n{\n    Printxxx(1);\n}\n";
        let inputs = vec![script_entry("unique://lspdiag/gate.as", SRC)];
        let idx = as_core::workspace::Workspace::build(as_core::IndexConfig::default(), inputs);
        let file = as_core::intern::intern_file("unique://lspdiag/gate.as", 0);
        let e = idx.files.get(&file).unwrap();
        let missing = DrainFacts { decl_missing: true, cycle_diags: HashMap::new() };
        assert!(
            file_ls_diags(&idx, file, &e.tree, &e.source, &e.lines, true, &missing).is_empty(),
            "decl 缺失时 undefined-function 整类跳过"
        );
        let present = DrainFacts { decl_missing: false, cycle_diags: HashMap::new() };
        let diags = file_ls_diags(&idx, file, &e.tree, &e.source, &e.lines, true, &present);
        assert_eq!(diags.len(), 1, "decl 在位时报 Printxxx：{diags:?}");
        assert_eq!(diags[0].code, code("undefined-function"));
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn file_ls_diags_unopened_entry_path_with_cycle() {
        // 未打开文件路径（索引 FileEntry）：环诊断并入 + as-ignore 抑制
        let src_a = "class A : B {} // as-ignore: cyclic-inheritance\n";
        let ws_idx = as_core::workspace::Workspace::build(
            as_core::IndexConfig::default(),
            vec![
                script_entry("unique://lspdiag/cyc_a.as", src_a),
                script_entry("unique://lspdiag/cyc_b.as", "class B : A {}\n"),
            ],
        );
        let facts = DrainFacts { decl_missing: false, cycle_diags: ws_idx.cycle_diags() };
        let a = as_core::intern::intern_file("unique://lspdiag/cyc_a.as", 0);
        let e = ws_idx.files.get(&a).unwrap();
        let diags = file_ls_diags(&ws_idx, a, &e.tree, &e.source, &e.lines, true, &facts);
        assert!(diags.is_empty(), "cyclic-inheritance 被同行 as-ignore 抑制，且无其他诊断");

        // 去掉抑制注释 → cyclic-inheritance 出现
        let ws_idx2 = as_core::workspace::Workspace::build(
            as_core::IndexConfig::default(),
            vec![
                script_entry("unique://lspdiag/cyc2_a.as", "class A : B {}\n"),
                script_entry("unique://lspdiag/cyc2_b.as", "class B : A {}\n"),
            ],
        );
        let facts2 = DrainFacts { decl_missing: false, cycle_diags: ws_idx2.cycle_diags() };
        let a2 = as_core::intern::intern_file("unique://lspdiag/cyc2_a.as", 0);
        let e2 = ws_idx2.files.get(&a2).unwrap();
        let diags = file_ls_diags(&ws_idx2, a2, &e2.tree, &e2.source, &e2.lines, true, &facts2);
        assert_eq!(diags.len(), 1, "只有一条 cyclic-inheritance");
        assert_eq!(diags[0].code, code("cyclic-inheritance"));
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diags[0].range.start.line, 0, "落在 base 所在行");
    }

    #[test]
    fn file_ls_diags_undefined_function_and_suppression() {
        // 用户 case 同款：Print 在 Decl 文件声明 → 不报；Printxxx → 报（Error，
        // 落名字所在行）；同行 as-ignore: undefined-function 可抑制
        const DECL: &str = concat!(
            "void Print(FString Text, float32 Duration = 5.f);\n",
            "struct FString { int Len; }\n",
        );
        const SRC: &str = "\
void test_001()
{
    Print(\"123\");
    Printxxx(\"123\");
    Printyyy(\"123\"); // as-ignore: undefined-function
}
";
        let inputs = vec![
            as_core::FileInput {
                file: as_core::intern::intern_file("unique://lspdiaguf/global.d.as", 0),
                kind: FileKind::Decl,
                source: DECL.to_string(),
                module: None,
            },
            as_core::FileInput {
                file: as_core::intern::intern_file("unique://lspdiaguf/test.as", 0),
                kind: FileKind::Script,
                source: SRC.to_string(),
                module: None,
            },
        ];
        let idx = as_core::workspace::Workspace::build(as_core::IndexConfig::default(), inputs);
        let file = as_core::intern::intern_file("unique://lspdiaguf/test.as", 0);
        let e = idx.files.get(&file).unwrap();
        let facts = DrainFacts { decl_missing: false, cycle_diags: HashMap::new() };
        let diags = file_ls_diags(&idx, file, &e.tree, &e.source, &e.lines, true, &facts);
        assert_eq!(diags.len(), 1, "只报 Printxxx（Print 解析成功，Printyyy 被抑制）：{diags:?}");
        assert_eq!(diags[0].code, code("undefined-function"));
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diags[0].range.start.line, 3, "落在 Printxxx 所在行");
        assert_eq!(diags[0].range.end.line, 3);
        assert_eq!(diags[0].source.as_deref(), Some("my-as-lsp"));
    }
}
