//! inlayHint 内核（M5d，规划 §8.1 M5 行：auto 变量推导类型展示）。
//!
//! 请求驱动全文件收集（D14：函数体局部自包含分析，扫描阶段不碰定型）：
//! - `auto X = init;` 局部变量（块内 / classic-for 初始化）；
//! - `for (auto E : container)` 迭代变量（引擎双跳协议，expr::for_each_element）；
//! - 定型成功才产出（`: FVector`——render_syn 渲染声明侧 SynType，模板
//!   实参/数组/引用性保真：`TArray<FVector>` / `FVector[]`）；失败静默
//!   跳过（宁缺毋假，D14）。
//!
//! 位置 = 声明名字的**尾端点**（VSCode 在该列前渲染 hint）。

use as_syntax::tree_sitter::Node;

use crate::expr::{expr_type, for_each_element, is_auto_type};
use crate::hover::render_syn;
use crate::id::FileId;
use crate::resolve::SemCtx;
use crate::syntax;
use crate::types::SynType;
use crate::workspace::Workspace;

/// 一个 inlay hint（as-core 自有结构，as-lsp 映射 InlayHint）。
pub struct Inlay {
    /// hint 锚点（字节；= 名字 token 尾端点）
    pub position: u32,
    /// `: FVector` 形式的类型文本（含前导冒号空格——渲染由消费方拼）
    pub ty: String,
}

/// 全文件 auto 推导 hint。文件不在索引 → 空。
pub fn inlay_hints(ws: &Workspace, file: FileId) -> Vec<Inlay> {
    let Some(entry) = ws.files.get(&file) else { return Vec::new() };
    let src = &entry.source;
    let root = entry.tree.root_node();
    let mut out = Vec::new();
    // 根语境（函数体内部各自的局部由 SemCtx 逐声明重建——auto 是函数体
    // 局部自包含分析，用声明点位置的语境保证先行声明可见）
    walk_nodes(ws, file, src, root, &mut out);
    out
}

fn walk_nodes(ws: &Workspace, file: FileId, src: &str, node: Node<'_>, out: &mut Vec<Inlay>) {
    let children = syntax::children_with_fields(node);
    for (_, child) in children {
        match child.kind() {
            "variable_declaration" => collect_auto_decl(ws, file, src, child, out),
            "for_each_statement" => collect_for_each(ws, file, src, child, out),
            _ => walk_nodes(ws, file, src, child, out),
        }
    }
}

/// 块内 / for 初始化的 auto 声明（多 declarator 逐个取）。
fn collect_auto_decl(
    ws: &Workspace,
    file: FileId,
    src: &str,
    decl: Node<'_>,
    out: &mut Vec<Inlay>,
) {
    let ty = decl
        .child_by_field_name("type")
        .and_then(|t| syntax::parse_syn_type(t, src));
    let Some(ty) = ty else { return };
    if !is_auto_type(&ty) {
        return;
    }
    for (_, child) in syntax::children_with_fields(decl) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else { continue };
        let Some(init) = child.child_by_field_name("value") else { continue };
        // 语境锚点 = 声明点（初始化式开头——先于自身求值点的局部已可见）
        let ctx = SemCtx::at_byte(ws, file, src, init.start_byte() as u32, name_node);
        if let Some(e) = expr_type(ws, &ctx, src, init) {
            let syn = e.syn.unwrap_or_else(|| crate::expr::syn_of_base(ws, e.base));
            push_hint(name_node, src, syn, out);
        }
        // 定型失败：宁缺毋假——不出 hint
    }
}

/// range-for 迭代变量（auto 才出；`for (FVector E : ...)` 显式类型不出）。
fn collect_for_each(
    ws: &Workspace,
    file: FileId,
    src: &str,
    node: Node<'_>,
    out: &mut Vec<Inlay>,
) {
    let Some(ty_node) = node.child_by_field_name("type") else { return };
    let Some(ty) = syntax::parse_syn_type(ty_node, src) else { return };
    if !is_auto_type(&ty) {
        return;
    }
    let Some(name_node) = node.child_by_field_name("name") else { return };
    // 语境锚点 = range 表达式开头
    let Some(range) = node.child_by_field_name("range") else { return };
    let ctx = SemCtx::at_byte(ws, file, src, range.start_byte() as u32, name_node);
    if let Some(e) = for_each_element(ws, &ctx, src, node) {
        let syn = e.syn.unwrap_or_else(|| crate::expr::syn_of_base(ws, e.base));
        push_hint(name_node, src, syn, out);
    }
}

fn push_hint(name_node: Node<'_>, _src: &str, syn: SynType, out: &mut Vec<Inlay>) {
    out.push(Inlay {
        position: name_node.end_byte() as u32,
        ty: format!(": {}", render_syn(&syn)),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IndexConfig;
    use crate::intern::intern_file;
    use crate::workspace::{FileInput, FileKind};

    fn build(srcs: &[(&str, &str)]) -> Workspace {
        let inputs = srcs
            .iter()
            .map(|(path, src)| FileInput {
                file: intern_file(path, 0),
                kind: if path.ends_with(".d.as") { FileKind::Decl } else { FileKind::Script },
                source: (*src).to_string(),
                module: None,
            })
            .collect();
        Workspace::build(IndexConfig::default(), inputs)
    }

    fn hints_of(srcs: &[(&str, &str)], path: &str) -> Vec<(u32, String)> {
        let ws = build(srcs);
        let file = intern_file(path, 0);
        inlay_hints(&ws, file).into_iter().map(|i| (i.position, i.ty)).collect()
    }

    #[test]
    fn auto_local_shows_type() {
        const SRC: &str = "\
void F()
{
    auto A = 1;
    auto B = 1.5f;
}
";
        let hs = hints_of(&[("unique://inlay/local.as", SRC)], "unique://inlay/local.as");
        assert_eq!(hs.len(), 2, "两个 auto 局部");
        assert_eq!(hs[0].1, ": int", "无后缀整数 → int: {:?}", hs);
        assert_eq!(hs[1].1, ": float32", "f 后缀 → float32: {:?}", hs);
        // 位置 = 名字尾端点（A 结束于 off+6）
        let a_end = SRC.find("A = 1").unwrap() as u32 + 1;
        assert_eq!(hs[0].0, a_end);
    }

    #[test]
    fn auto_chain_and_call() {
        const DECL: &str = "struct FVector { float X; float GetMax() const; }";
        const SRC: &str = "\
void F()
{
    FVector V;
    auto M = V.GetMax();
    auto X = V.X;
}
";
        let hs = hints_of(&[
            ("unique://inlay/vec.d.as", DECL),
            ("unique://inlay/chain.as", SRC),
        ], "unique://inlay/chain.as");
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0].1, ": float", "调用返回（裸 float 原样渲染）: {:?}", hs);
        assert_eq!(hs[1].1, ": float", "字段访问: {:?}", hs);
    }

    #[test]
    fn auto_failure_no_hint() {
        const SRC: &str = "\
void F()
{
    auto A = Unknown(1);
}
";
        let hs = hints_of(&[("unique://inlay/fail.as", SRC)], "unique://inlay/fail.as");
        assert!(hs.is_empty(), "定型失败不出 hint（宁缺毋假）: {:?}", hs);
    }

    #[test]
    fn explicit_type_no_hint() {
        const SRC: &str = "\
void F()
{
    int X = 1;
}
";
        let hs = hints_of(&[("unique://inlay/explicit.as", SRC)], "unique://inlay/explicit.as");
        assert!(hs.is_empty(), "显式类型不出: {:?}", hs);
    }

    #[test]
    fn range_for_element_hint() {
        const DECL: &str = "\
struct TArray<T>
{
    TArrayIterator<T> Iterator();
    TArrayConstIterator<T> Iterator() const;
}
struct TArrayIterator<T>
{
    T& Iterate();
}
struct TArrayConstIterator<T>
{
    T Iterate();
}
struct FVector { float X; }
";
        const SRC: &str = "\
void F()
{
    FVector[] Arr;
    for (auto E : Arr)
    {
        float X = E.X;
    }
}
";
        let hs = hints_of(&[
            ("unique://inlay/tarr.d.as", DECL),
            ("unique://inlay/fe.as", SRC),
        ], "unique://inlay/fe.as");
        assert_eq!(hs.len(), 1, "迭代变量一条: {:?}", hs);
        // `T& Iterate()` 返回引用——元素类型保留引用性（引擎双跳协议 §4.2）
        assert_eq!(hs[0].1, ": FVector&", "双跳替换 T& → FVector&: {:?}", hs);
    }

    #[test]
    fn template_args_preserved() {
        const DECL: &str = "\
struct TArray<T>
{
    TArray<T>& opAdd(TArray<T> Other);
    TArrayIterator<T> Iterator();
    TArrayConstIterator<T> Iterator() const;
}
struct TArrayIterator<T> { T& Iterate(); }
struct TArrayConstIterator<T> { T Iterate(); }
struct FVector { float X; }
";
        const SRC: &str = "\
void F()
{
    TArray<FVector> A;
    auto B = A;
}
";
        let hs = hints_of(&[
            ("unique://inlay/tmpl.d.as", DECL),
            ("unique://inlay/tmpl.as", SRC),
        ], "unique://inlay/tmpl.as");
        assert_eq!(hs.len(), 1);
        assert_eq!(hs[0].1, ": TArray<FVector>", "模板实参保真: {:?}", hs);
    }
}
