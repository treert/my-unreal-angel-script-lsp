//! 诊断发布管道（M6：LSP实现规划 §8.1 publishDiagnostics 行，D36）。
//!
//! 时序：
//! - `didOpen` / `didChange` 后（**Ready 时**）同步计算该文件诊断并推送——
//!   `verify_tree` 是 O(文件) 树走、毫秒级，**不做防抖**；
//! - `didClose` → 推空数组清空（Loading 期清空同样安全）；
//! - **Loading 期不推**；Ready 发布瞬间（`publish_and_replay`）经 diag 通道
//!   补推全部已打开文档——覆盖 Loading 期打开的文件，且 AS0902 的 decl
//!   计数随每次重建（冷启动 / `.d.as` 防抖 / 配置变更）刷新；
//! - AS0902 仅对 Script 文档发布（decl 计数来自当前索引快照，锁序
//!   docs → index 读，与既有路径一致）。
//!
//! 规则本体与抑制过滤在 `as_core::diag`（纯函数）；本层只做 UTF-16 换算
//! 与协议映射（§3.2.1：换算只在本层发生）。

use std::sync::Mutex;

use tower_lsp_server::ls_types::{self as ls, *};
use tower_lsp_server::Client;

use as_core::diag::{script_diags, DiagSeverity};
use as_core::FileKind;

use crate::docs::{Doc, DocStore};
use crate::workspace::{kind_of_path, WorkspaceState};

/// 一个文档的诊断 → LSP 协议形态（调用方须持有 docs 锁；`ws.with` 在锁内
/// 完成——docs → index 读的既有锁序）。
pub fn doc_ls_diags(doc: &Doc, is_script: bool, ws: &WorkspaceState) -> Vec<ls::Diagnostic> {
    let decl_missing = is_script
        && ws
            .with(|idx| idx.files.values().all(|s| s.kind != FileKind::Decl))
            .unwrap_or(false);
    script_diags(&doc.tree, &doc.text, decl_missing)
        .into_iter()
        .map(|d| {
            let (sl, sc) = doc.lines.line_col_utf16(&doc.text, d.range.start);
            let (el, ec) = doc.lines.line_col_utf16(&doc.text, d.range.end);
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

/// 计算并发布一个已打开文档的诊断（didOpen / didChange 后调用，需 Ready；
/// 文档未打开（无 overlay）则不发）。
pub async fn publish_file(
    client: &Client,
    docs: &Mutex<DocStore>,
    ws: &WorkspaceState,
    path: &str,
) {
    let Some(file) = as_core::intern::file_id_of_path(path) else { return };
    let Some(uri) = ls::Uri::from_file_path(path) else { return };
    let computed = {
        let store = docs.lock().unwrap();
        let Some(doc) = store.get(file) else { return };
        let is_script = kind_of_path(path) == FileKind::Script;
        Some((doc_ls_diags(doc, is_script, ws), doc.version))
    };
    if let Some((diags, version)) = computed {
        client.publish_diagnostics(uri, diags, Some(version)).await;
    }
}

/// Ready / 每次重建后：对全部已打开文档补推一轮。后台 std 线程不能跨 await
/// 调 client——经 unbounded channel 转发到本 async 上下文（与
/// `myas/indexStatus` 同一模式，main.rs 持有转发任务）。
pub async fn publish_all_open(client: &Client, docs: &Mutex<DocStore>, ws: &WorkspaceState) {
    let entries: Vec<(String, Vec<Diagnostic>, i32)> = {
        let store = docs.lock().unwrap();
        store
            .entries()
            .filter_map(|(file, doc)| {
                let path = as_core::intern::file_path(file)?.to_string();
                let is_script = kind_of_path(&path) == FileKind::Script;
                Some((path, doc_ls_diags(doc, is_script, ws), doc.version))
            })
            .collect()
    };
    for (path, diags, version) in entries {
        if let Some(uri) = ls::Uri::from_file_path(&path) {
            client.publish_diagnostics(uri, diags, Some(version)).await;
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

    #[test]
    fn doc_ls_diags_maps_code_and_severity() {
        let ws = WorkspaceState::new();
        let mut store = DocStore::new();
        // 未闭合宏（AS0903）+ 索引 0 个 .d.as（AS0902）
        let src = "UFUNCTION(Blueprint\nvoid F() {}\n".to_string();
        let file = store.open("unique://lspdiag/a.as", 1, src);
        // 空索引发布 → Ready（dirty 空，重放为空操作）
        let idx = as_core::WorkspaceIndex::build(as_core::IndexConfig::default(), vec![]);
        ws.publish_and_replay(idx, &Mutex::new(DocStore::new()));
        let doc = store.get(file).unwrap();
        let diags = doc_ls_diags(doc, true, &ws);
        let as0902 = diags.iter().find(|d| d.code == code("AS0902")).unwrap();
        assert_eq!(as0902.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(as0902.range.start, Position { line: 0, character: 0 });
        let as0903 = diags.iter().find(|d| d.code == code("AS0903")).unwrap();
        assert_eq!(as0903.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(as0903.source.as_deref(), Some("my-as-lsp"));
        assert_eq!(as0903.range.start.line, 0);
    }

    #[test]
    fn doc_ls_diags_no_as0902_when_decls_present() {
        let ws = WorkspaceState::new();
        let mut store = DocStore::new();
        let file = store.open("unique://lspdiag/b.as", 1, "int X = 1;\n".to_string());
        let inputs = vec![as_core::FileInput {
            file: as_core::intern::intern_file("unique://lspdiag/decl.d.as", 0),
            kind: FileKind::Decl,
            source: "struct FVector { float X; }\n".to_string(),
            module: None,
        }];
        let idx = as_core::WorkspaceIndex::build(as_core::IndexConfig::default(), inputs);
        ws.publish_and_replay(idx, &Mutex::new(DocStore::new()));
        let doc = store.get(file).unwrap();
        assert!(doc_ls_diags(doc, true, &ws).is_empty());
    }
}
