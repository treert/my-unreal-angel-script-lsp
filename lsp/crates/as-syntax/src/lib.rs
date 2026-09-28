//! tree-sitter 包装层（LSP实现规划 §2.1）：Unreal Angelscript 文法的 Rust 入口。
//!
//! 文法工程在仓库 `grammar/`（`grammar.js` + 手写 `scanner.c`），其生成物
//! `src/parser.c` 由本 crate 的 `build.rs` 以 cc 编译。`.as` 与 `.d.as` 用同一
//! parser（语法集合是交叉关系，不是子集——见 grammar/README.md）。
//!
//! 本层只回答「怎么 parse、树上有什么」；一切语义判断归 as-core。
//! `pub use tree_sitter;` 使下游（as-core / as-cli / as-lsp）不必直接依赖
//! tree-sitter crate，维持 LSP实现规划 §2 的单向依赖图。

use tree_sitter::{Language, Parser, Tree};

pub mod node;

pub use tree_sitter;

extern "C" {
    fn tree_sitter_angelscript() -> Language;
}

/// Angelscript（`.as` / `.d.as` 共用）的 tree-sitter `Language`。
pub fn language() -> Language {
    // SAFETY: `tree_sitter_angelscript` 由 build.rs 链接的 parser.c 导出，
    // 返回静态 TSLanguage。
    unsafe { tree_sitter_angelscript() }
}

/// 预配置好语言的 `Parser`。
pub fn new_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&language())
        .expect("tree-sitter ABI mismatch: regenerate grammar/src/parser.c with a matching tree-sitter-cli (0.25.x)");
    parser
}

/// 解析入口。增量 parse 传 `old_tree`（编辑时序见 LSP实现规划 §5.2）。
pub fn parse(src: &str, old_tree: Option<&Tree>) -> Tree {
    let mut parser = new_parser();
    parser
        .parse(src, old_tree)
        .expect("parser returned no tree")
}

/// 错误恢复节点的分类（`Node::is_error()` / `Node::is_missing()`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxErrorKind {
    /// 错误恢复产生的 `ERROR` 节点
    Error,
    /// 错误恢复插入的 `MISSING` 占位节点
    Missing,
}

/// 一个错误恢复节点（P5 诊断「解析错误」分类的素材，M0 验收判据）。
#[derive(Debug, Clone)]
pub struct SyntaxError {
    pub kind: SyntaxErrorKind,
    /// 节点种类：`Error` 时为 `"ERROR"`；`Missing` 时为被期望的种类名
    pub node_kind: String,
    pub start_byte: usize,
    pub end_byte: usize,
}

/// 校验源码：收集全部顶层 `ERROR` / `MISSING` 节点（不深入 ERROR 子树重复计数）。
pub fn verify(src: &str) -> Vec<SyntaxError> {
    verify_tree(&parse(src, None))
}

/// 校验既有 CST（复用树时避免重复 parse）。
///
/// `has_error()` 剪枝（借鉴 mylua）：tree-sitter 每节点维护「子树含错」位，
/// 合法文件的根即 false ⇒ 整棵树 O(1) 跳过——错误收集按需（诊断期）调用
/// 时，合法文件零遍历。遍历全程共用一个 `TreeCursor`（每节点 `node.walk()`
/// 的 FFI malloc/free 是 `.d.as` 量级的性能热点，见 uses.rs 同款修复）。
pub fn verify_tree(tree: &Tree) -> Vec<SyntaxError> {
    let mut out = Vec::new();
    let root = tree.root_node();
    if root.has_error() {
        let mut c = root.walk();
        walk_errors(&mut c, &mut out);
    }
    out
}

/// `cursor` 定位在待检节点上（is_error / is_missing 的节点不深入——
/// 既有契约：不深入 ERROR 子树重复计数）。
fn walk_errors(c: &mut tree_sitter::TreeCursor, out: &mut Vec<SyntaxError>) {
    let node = c.node();
    if node.is_missing() {
        out.push(SyntaxError {
            kind: SyntaxErrorKind::Missing,
            node_kind: node.kind().to_string(),
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
        });
        return;
    }
    if node.is_error() {
        out.push(SyntaxError {
            kind: SyntaxErrorKind::Error,
            node_kind: node.kind().to_string(),
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
        });
        return;
    }
    // 清洁子树剪枝：has_error() false ⇒ 子树内无 ERROR/MISSING，跳过
    if !node.has_error() {
        return;
    }
    if c.goto_first_child() {
        loop {
            walk_errors(c, out);
            if !c.goto_next_sibling() {
                break;
            }
        }
        c.goto_parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_class() {
        let src = "class Foo : UObject { int X; }";
        let tree = parse(src, None);
        assert_eq!(tree.root_node().kind(), node::SOURCE_FILE);
        assert!(verify_tree(&tree).is_empty());
    }

    #[test]
    fn parse_d_as_declaration() {
        // .d.as 声明形态（架构设计 §2.4.2 的最小摘录，内置单测用例）
        let src = "// @group /Script/Engine\nstruct FVector { float X; }\n";
        assert!(verify(src).is_empty());
    }

    #[test]
    fn verify_reports_error() {
        let errs = verify(")))");
        assert!(errs.iter().any(|e| e.kind == SyntaxErrorKind::Error));
    }

    #[test]
    fn verify_reports_error_on_incomplete_decl() {
        // 本文法的错误恢复实际只产出 ERROR 节点（corpus 的 error_recovery 快照
        // 里也没有 MISSING 形态）；Missing 分支是 tree-sitter 的通用恢复行为，
        // 为 verify 的契约完整性保留。
        let errs = verify("class {");
        assert!(errs.iter().any(|e| e.kind == SyntaxErrorKind::Error));
        assert!(!errs.is_empty());

        let errs = verify("void f(");
        assert!(errs.iter().any(|e| e.kind == SyntaxErrorKind::Error));
    }

    #[test]
    fn node_kinds_sorted_and_complete() {
        // 排序是 is_named_kind 二分查找的前提；94 = grammar/README 的具名节点数
        assert!(node::ALL.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(node::ALL.len(), 94);
        assert!(node::is_named_kind(node::SOURCE_FILE));
        assert!(!node::is_named_kind(node::ERROR));
    }
}
