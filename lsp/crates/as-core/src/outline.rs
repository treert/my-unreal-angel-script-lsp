//! 文档符号与折叠区间（LSP实现规划 §8.1：documentSymbol / foldingRange 均
//! 为「CST 直映射」，零语义依赖——冷启动 Loading 期间即可服务，§6.1）。
//!
//! 产出是 LSP 无关的字节偏移结构；行列/UTF-16 换算与 `SymbolKind` /
//! `FoldingRangeKind` 的映射归 as-lsp 边界（§3.2.1）。

use as_syntax::tree_sitter::Node;

use crate::range::{LineIndex, TextRange};
use crate::syntax::{self, DeclCtx};

// ---------------------------------------------------------------------------
// documentSymbol
// ---------------------------------------------------------------------------

/// 符号种类（as-lsp 映射到 `lsp_types::SymbolKind`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutlineKind {
    Class,
    Struct,
    Enum,
    EnumValue,
    Namespace,
    Delegate,
    Event,
    Function,
    Method,
    Constructor,
    Destructor,
    Operator,
    GlobalVar,
    Field,
    VirtualProperty,
    Asset,
}

/// 一个文档符号（层级结构对齐 CST 嵌套）。
#[derive(Clone, Debug)]
pub struct OutlineSymbol {
    pub name: String,
    pub kind: OutlineKind,
    /// 整个声明
    pub range: TextRange,
    /// 名字 token（Breadcrumbs / outline 选中锚点）
    pub selection_range: TextRange,
    pub children: Vec<OutlineSymbol>,
}

/// 顶层符号树（声明顺序）。
pub fn document_symbols(root: Node<'_>, src: &str) -> Vec<OutlineSymbol> {
    let mut out = Vec::new();
    for (_field, child) in syntax::children_with_fields(root) {
        collect_symbol(child, src, DeclCtx::Global, &mut out);
    }
    out
}

fn collect_symbol(node: Node<'_>, src: &str, ctx: DeclCtx, out: &mut Vec<OutlineSymbol>) {
    // variable_declaration 的名字在 declarator 上（可能多个），走专用分支
    if node.kind() == "variable_declaration" {
        let Some(kind) = outline_kind(node, src, ctx) else {
            return;
        };
        for (_f, child) in syntax::children_with_fields(node) {
            if child.kind() != "variable_declarator" {
                continue;
            }
            if let Some(name_node) = child.child_by_field_name("name") {
                out.push(OutlineSymbol {
                    name: syntax::text(name_node, src).to_string(),
                    kind,
                    range: syntax::span(child),
                    selection_range: syntax::span(name_node),
                    children: Vec::new(),
                });
            }
        }
        return;
    }

    let Some(kind) = outline_kind(node, src, ctx) else {
        return;
    };
    let Some(name_node) = syntax::decl_name_node(node) else {
        return;
    };
    let name = syntax::text(name_node, src).to_string();

    let mut children = Vec::new();
    match node.kind() {
        "class_declaration" | "struct_declaration" => {
            if let Some(body) = node.child_by_field_name("body") {
                for (_f, child) in syntax::children_with_fields(body) {
                    collect_symbol(child, src, DeclCtx::TypeBody, &mut children);
                }
            }
        }
        "enum_declaration" => {
            if let Some(body) = node.child_by_field_name("body") {
                for (_f, child) in syntax::children_with_fields(body) {
                    if child.kind() == "enumerator" {
                        collect_symbol(child, src, ctx, &mut children);
                    }
                }
            }
        }
        "namespace_declaration" => {
            if let Some(body) = node.child_by_field_name("body") {
                for (_f, child) in syntax::children_with_fields(body) {
                    collect_symbol(child, src, DeclCtx::Global, &mut children);
                }
            }
        }
        _ => {}
    }

    out.push(OutlineSymbol {
        name,
        kind,
        range: syntax::span(node),
        selection_range: syntax::span(name_node),
        children,
    });
}

fn outline_kind(node: Node<'_>, src: &str, ctx: DeclCtx) -> Option<OutlineKind> {
    Some(match node.kind() {
        "class_declaration" => OutlineKind::Class,
        "struct_declaration" => OutlineKind::Struct,
        "enum_declaration" => OutlineKind::Enum,
        "enumerator" => OutlineKind::EnumValue,
        "namespace_declaration" => OutlineKind::Namespace,
        "delegate_declaration" => OutlineKind::Delegate,
        "event_declaration" => OutlineKind::Event,
        "constructor_declaration" => OutlineKind::Constructor,
        "destructor_declaration" => OutlineKind::Destructor,
        "asset_declaration" => OutlineKind::Asset,
        "virtual_property_declaration" => OutlineKind::VirtualProperty,
        "function_declaration" => {
            let name = node.child_by_field_name("name").map(|n| syntax::text(n, src)).unwrap_or("");
            if syntax::is_operator_name(name) {
                OutlineKind::Operator
            } else if ctx == DeclCtx::TypeBody {
                OutlineKind::Method
            } else {
                OutlineKind::Function
            }
        }
        "variable_declaration" => {
            if ctx == DeclCtx::TypeBody {
                OutlineKind::Field
            } else {
                OutlineKind::GlobalVar
            }
        }
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// foldingRange
// ---------------------------------------------------------------------------

/// 折叠种类（as-lsp 映射到 `FoldingRangeKind`；`Block` → 无 kind）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FoldKind {
    /// 花括号块（class/namespace/enum/block/switch…）
    Block,
    /// 连续注释块
    Comment,
}

#[derive(Clone, Copy, Debug)]
pub struct Fold {
    /// 折叠内容首字节
    pub start: u32,
    /// 折叠内容末字节（含）
    pub end: u32,
    pub kind: FoldKind,
}

/// 全部折叠区间。花括号块：`{` 起始行 → `}` 所在行；注释块按相邻行合并。
pub fn folding_ranges(root: Node<'_>, src: &str, lines: &LineIndex) -> Vec<Fold> {
    let mut comments: Vec<Node<'_>> = Vec::new();
    let mut blocks: Vec<(u32, u32)> = Vec::new(); // (start_byte, end_byte) of braces
    collect_folds(root, src, &mut comments, &mut blocks);

    let mut folds: Vec<Fold> = Vec::new();
    for (start, end) in blocks {
        folds.push(Fold { start, end, kind: FoldKind::Block });
    }

    // 注释块：按「下一行首列相同 + 紧邻行」合并相邻行注释；跨行块注释独立成块
    comments.sort_by_key(|c| c.start_byte());
    let mut i = 0;
    while i < comments.len() {
        let cur = comments[i];
        let cur_text = syntax::text(cur, src);
        let is_line_comment = cur_text.starts_with("//");
        let start_line = lines.line_of(cur.start_byte() as u32);
        let end_line = lines.line_of(cur.end_byte().saturating_sub(1) as u32);
        if !is_line_comment || end_line > start_line {
            // 块注释（或跨行）独立成块
            if end_line > start_line {
                folds.push(Fold {
                    start: cur.start_byte() as u32,
                    end: cur.end_byte() as u32,
                    kind: FoldKind::Comment,
                });
            }
            i += 1;
            continue;
        }
        // 行注释：向后合并紧邻行的行注释
        let mut last_line = start_line;
        let mut j = i + 1;
        while j < comments.len() {
            let next = comments[j];
            let next_text = syntax::text(next, src);
            let next_line = lines.line_of(next.start_byte() as u32);
            if next_line == last_line + 1 && next_text.starts_with("//") {
                last_line = next_line;
                j += 1;
            } else {
                break;
            }
        }
        if last_line > start_line {
            folds.push(Fold {
                start: cur.start_byte() as u32,
                end: comments[j - 1].end_byte() as u32,
                kind: FoldKind::Comment,
            });
        }
        i = j;
    }

    folds.sort_by_key(|f| f.start);
    folds
}

fn collect_folds<'t>(
    node: Node<'t>,
    src: &str,
    comments: &mut Vec<Node<'t>>,
    blocks: &mut Vec<(u32, u32)>,
) {
    match node.kind() {
        "comment" => {
            comments.push(node);
            return;
        }
        "class_body" | "namespace_body" | "enum_body" | "block" | "switch_body"
        | "default_block" => {
            let open = node.start_byte();
            let close = node.end_byte();
            // 只在块本身跨多行时才有折叠意义
            if src[open..close].contains('\n') {
                blocks.push((open as u32, close as u32));
            }
        }
        _ => {}
    }
    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            collect_folds(c.node(), src, comments, blocks);
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::as_syntax;
    use crate::range::LineIndex;

    fn parse(src: &str) -> as_syntax::tree_sitter::Tree {
        as_syntax::parse(src, None)
    }

    // 用例源码内置（D1）。

    #[test]
    fn outline_hierarchy() {
        let src = "\
class AActor : UObject
{
    int X;
    void Tick(float Delta)
    {
        int Local = 1;
    }
}

enum EMode { Off, On }

namespace Math { float64 Sqrt(float64 V); }
void Main() {}
int G;
";
        let tree = parse(src);
        let syms = document_symbols(tree.root_node(), src);
        assert_eq!(syms.len(), 5, "class / enum / namespace / function / global var");

        let (class, en, ns) = (&syms[0], &syms[1], &syms[2]);
        assert_eq!(class.name, "AActor");
        assert_eq!(class.kind, OutlineKind::Class);
        assert_eq!(class.children.len(), 2, "X + Tick");
        assert_eq!(class.children[0].kind, OutlineKind::Field);
        assert_eq!(class.children[1].kind, OutlineKind::Method);
        // 函数体内的局部变量不进 outline
        assert!(class.children[1].children.is_empty());

        assert_eq!(en.name, "EMode");
        assert_eq!(en.children.len(), 2);
        assert_eq!(en.children[0].kind, OutlineKind::EnumValue);

        assert_eq!(ns.name, "Math");
        assert_eq!(ns.kind, OutlineKind::Namespace);
        assert_eq!(ns.children.len(), 1);
        assert_eq!(ns.children[0].kind, OutlineKind::Function);

        // 顶层函数/变量
        assert_eq!(syms[3].name, "Main");
        assert_eq!(syms[3].kind, OutlineKind::Function);
        assert_eq!(syms[4].name, "G");
        assert_eq!(syms[4].kind, OutlineKind::GlobalVar);
    }

    #[test]
    fn outline_operator_and_multi_declarator() {
        let src = "\
struct S
{
    S opAdd(const S Other) const;
    int A, B;
}
";
        let tree = parse(src);
        let syms = document_symbols(tree.root_node(), src);
        let s = &syms[0];
        assert_eq!(s.kind, OutlineKind::Struct);
        assert_eq!(s.children[0].kind, OutlineKind::Operator);
        assert_eq!(s.children[0].name, "opAdd");
        // 多 declarator：A、B 各一个符号
        assert_eq!(s.children.len(), 3);
        assert_eq!(s.children[1].name, "A");
        assert_eq!(s.children[2].name, "B");
    }

    #[test]
    fn folds_blocks_and_comments() {
        let src = "\
// file header line1
// file header line2

class A
{
    // member doc
    int X;
    void F()
    {
        if (true)
        {
        }
    }
}
";
        let tree = parse(src);
        let lines = LineIndex::new(src);
        let folds = folding_ranges(tree.root_node(), src, &lines);
        let comment_folds: Vec<_> = folds.iter().filter(|f| f.kind == FoldKind::Comment).collect();
        let block_folds: Vec<_> = folds.iter().filter(|f| f.kind == FoldKind::Block).collect();
        // 注释折叠：文件头两行合并 1 块；单行 "// member doc" 不成块
        assert_eq!(comment_folds.len(), 1);
        // 块折叠：class_body + F 的 block + if 的 block
        assert_eq!(block_folds.len(), 3);
    }
}
