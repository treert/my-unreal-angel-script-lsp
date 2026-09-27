//! CST 访问辅助（LSP实现规划 §2.2）：声明种类判定、名字 span 提取、
//! 说明符解析、语法层类型解析、doc 注释块收集。
//!
//! 只回答「树长什么样」；不做任何跨文件/语义判断。所有输入都是
//! 已 parse 好的 CST 节点 + 源文本（as-core 无 IO）。

use as_syntax::tree_sitter::Node;

use crate::id::Sym;
use crate::intern::intern_sym;
use crate::range::TextRange;
use crate::symbol::{BaseRef, DefFlags, DefKind, ParamDecl};
use crate::types::{RefKind, SynType};

/// 声明所在的容器语境（function/variable 的 kind 由此决定，规划 §3.2）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeclCtx {
    /// source_file 顶层 / namespace 体
    Global,
    /// class / struct 体
    TypeBody,
}

/// 全部子节点（含匿名 token）及其字段名。
pub fn children_with_fields<'t>(node: Node<'t>) -> Vec<(Option<String>, Node<'t>)> {
    let mut out = Vec::new();
    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            let field = c.field_name().map(|s| s.to_string());
            out.push((field, c.node()));
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
    out
}

pub fn text<'s>(node: Node<'_>, src: &'s str) -> &'s str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

pub fn span(node: Node<'_>) -> TextRange {
    TextRange::new(node.start_byte() as u32, node.end_byte() as u32)
}

/// 声明节点 → DefKind。非声明节点（comment / empty_declaration /
/// access_declaration / default_statement / ERROR…）返回 None。
pub fn classify_decl(node: Node<'_>, src: &str, ctx: DeclCtx) -> Option<DefKind> {
    let kind = match node.kind() {
        "class_declaration" => DefKind::Class,
        "struct_declaration" => DefKind::Struct,
        "enum_declaration" => DefKind::Enum,
        "enumerator" => DefKind::EnumValue,
        "namespace_declaration" => DefKind::Namespace,
        "delegate_declaration" => DefKind::Delegate,
        "event_declaration" => DefKind::Event,
        "asset_declaration" => DefKind::AssetDecl,
        "constructor_declaration" => DefKind::Constructor,
        "destructor_declaration" => DefKind::Destructor,
        "virtual_property_declaration" => DefKind::VirtualProperty,
        "function_declaration" => {
            let name = node.child_by_field_name("name").map(|n| text(n, src)).unwrap_or("");
            if is_operator_name(name) {
                DefKind::Operator
            } else if ctx == DeclCtx::TypeBody {
                DefKind::Method
            } else {
                DefKind::Function
            }
        }
        "variable_declaration" => {
            if ctx == DeclCtx::TypeBody {
                DefKind::Field
            } else {
                DefKind::GlobalVar
            }
        }
        _ => return None,
    };
    Some(kind)
}

/// 运算符重载判定：AS 的运算符一律是 `opXxx` 命名（opAdd / opImplConv /
/// opIndex…），`op` 后接大写字母视为运算符（`open` 这类普通名不误伤）。
/// 引擎语法层无独立节点，索引期判定即「事前」而非消费侧临时判断（§3.2）。
pub fn is_operator_name(name: &str) -> bool {
    name.len() > 2
        && name.starts_with("op")
        && name.as_bytes()[2].is_ascii_uppercase()
}

/// 声明名字节点（variable_declaration 的多 declarator 场景由 index.rs 特判）。
pub fn decl_name_node(node: Node<'_>) -> Option<Node<'_>> {
    let name = node.child_by_field_name("name")?;
    match node.kind() {
        // namespace 的 name 是 scoped_name：取最后一段标识符（成员查找链语义）
        "namespace_declaration" => last_identifier(name),
        _ => Some(name),
    }
}

fn last_identifier(node: Node<'_>) -> Option<Node<'_>> {
    let mut last = None;
    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            if c.node().kind() == "identifier" {
                last = Some(c.node());
            }
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
    last
}

/// 说明符扫描：`private`/`protected` 前缀、`mixin`/`local` qualifier（前置）
/// 与 `mixin` 后置属性、方法尾部 `const`。匿名 token 直接是声明节点的孩子，
/// 不会误吞 type 节点内部的 const（那在更深层）。
/// 另：函数声明的 `specifiers` 字段是 `ufunction_specifiers` → SCRIPT_UFUNCTION
/// （M5c：UFUNCTION 名单候选的数据源，.d.as 侧对应 @ufunction/@event tag）。
pub fn scan_flags(node: Node<'_>, src: &str) -> DefFlags {
    let mut flags = DefFlags::NONE;
    for (field, child) in children_with_fields(node) {
        if child.is_named() {
            if child.kind() == "function_attribute" && text(child, src) == "mixin" {
                // 后置属性形式：`void Heal(...) mixin {}`（架构设计 §4.5.1）
                flags |= DefFlags::MIXIN;
            }
            if field.as_deref() == Some("specifiers") && child.kind() == "ufunction_specifiers" {
                flags |= DefFlags::SCRIPT_UFUNCTION;
            }
            continue;
        }
        match child.kind() {
            "protected" => flags |= DefFlags::PROTECTED,
            "mixin" => flags |= DefFlags::MIXIN,
            "local" => flags |= DefFlags::LOCAL,
            "const" => flags |= DefFlags::CONST,
            _ => {}
        }
    }
    flags
}

/// class/struct 基类列表（field `base`，可能多个）。
pub fn class_bases(node: Node<'_>, src: &str) -> Vec<BaseRef> {
    let mut bases = Vec::new();
    for (field, child) in children_with_fields(node) {
        if field.as_deref() != Some("base") {
            continue;
        }
        let (name_node, simple) = match child.kind() {
            "identifier" => (Some(child), true),
            "template_type" => (child.child_by_field_name("name"), false),
            "qualified_identifier" => (first_identifier(child), false),
            _ => (None, false),
        };
        let Some(name_node) = name_node else { continue };
        let name = intern_sym(text(name_node, src));
        bases.push(BaseRef { name, span: span(name_node), simple });
    }
    bases
}

fn first_identifier(node: Node<'_>) -> Option<Node<'_>> {
    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            if c.node().kind() == "identifier" {
                return Some(c.node());
            }
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// class/struct 模板声明头的形参名（`.d.as` 独有；`type_parameter` 节点
/// 同时承载形参与特化实参，语义层按 tag 消歧——grammar/README 偏差 §4）。
pub fn template_params(node: Node<'_>, src: &str) -> Vec<Sym> {
    let Some(tp) = node.child_by_field_name("type_parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (_field, child) in children_with_fields(tp) {
        if child.kind() == "type_parameter" {
            out.push(intern_sym(text(child, src)));
        }
    }
    out
}

/// 语法层类型解析。接受 `type` 节点，或裸的 identifier / template_type /
/// qualified_identifier / primitive 节点（asset 声明的 type 字段是内联的）。
pub fn parse_syn_type(node: Node<'_>, src: &str) -> Option<SynType> {
    match node.kind() {
        "type" => parse_type_node(node, src),
        "identifier" => Some(SynType::Named(intern_sym(text(node, src)), span(node))),
        "template_type" => parse_template_node(node, src),
        "qualified_identifier" => {
            let segs = collect_identifiers(node, src);
            (!segs.is_empty()).then_some(SynType::Qualified(segs))
        }
        "primitive_type" => Some(SynType::Primitive(intern_sym(text(node, src)), span(node))),
        "auto_type" => Some(SynType::Auto),
        "wildcard_type" => Some(SynType::Wildcard),
        _ => None,
    }
}

fn parse_type_node(node: Node<'_>, src: &str) -> Option<SynType> {
    let mut is_const = false;
    let mut arrays = 0u32;
    let mut is_unresolved = false;
    let mut base: Option<SynType> = None;
    let mut ref_kind: Option<RefKind> = None;

    for (field, child) in children_with_fields(node) {
        if child.is_named() {
            match child.kind() {
                "array_suffix" => arrays += 1,
                "reference_modifier" => ref_kind = Some(parse_ref_kind(child)),
                _ if field.as_deref() == Some("name") => base = parse_syn_type(child, src),
                _ => {}
            }
            continue;
        }
        match child.kind() {
            "const" => is_const = true,
            "unresolved_object" => is_unresolved = true,
            _ => {}
        }
    }

    let mut ty = base?;
    for _ in 0..arrays {
        ty = SynType::Array(Box::new(ty));
    }
    if is_unresolved {
        // D8：语法 flag，解析期剥掉、按基类型 intern（渲染信息 Phase 3 再补）
        ty = SynType::UnresolvedObject(Box::new(ty));
    }
    if is_const {
        ty = SynType::Const(Box::new(ty));
    }
    if let Some(k) = ref_kind {
        ty = SynType::Ref(Box::new(ty), k);
    }
    Some(ty)
}

fn parse_ref_kind(node: Node<'_>) -> RefKind {
    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            let k = c.node().kind();
            if k == "in" {
                return RefKind::In;
            }
            if k == "out" {
                return RefKind::Out;
            }
            if k == "inout" {
                return RefKind::InOut;
            }
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
    RefKind::Plain
}

fn parse_template_node(node: Node<'_>, src: &str) -> Option<SynType> {
    let name_node = node.child_by_field_name("name")?;
    let args_node = node.child_by_field_name("arguments")?;
    let mut args = Vec::new();
    for (_field, child) in children_with_fields(args_node) {
        if child.kind() == "type" {
            if let Some(a) = parse_syn_type(child, src) {
                args.push(a);
            }
        }
    }
    Some(SynType::Template {
        name: intern_sym(text(name_node, src)),
        name_span: span(name_node),
        args,
    })
}

fn collect_identifiers(node: Node<'_>, src: &str) -> Vec<(Sym, TextRange)> {
    let mut out = Vec::new();
    fn walk_ids(node: Node<'_>, src: &str, out: &mut Vec<(Sym, TextRange)>) {
        if node.kind() == "identifier" {
            out.push((intern_sym(text(node, src)), span(node)));
            return;
        }
        let mut c = node.walk();
        if c.goto_first_child() {
            loop {
                walk_ids(c.node(), src, out);
                if !c.goto_next_sibling() {
                    break;
                }
            }
        }
    }
    walk_ids(node, src, &mut out);
    out
}

/// 标识符是否处于说明符语境（UPROPERTY/UFUNCTION/UCLASS 宏参数、方法属性、
/// 说明符列表）——这些标识符是 specifier 而非符号使用点，查找链不解析。
pub fn in_specifier_context(ident: Node<'_>) -> bool {
    let mut a = ident.parent();
    while let Some(p) = a {
        match p.kind() {
            "macro_argument" | "function_attribute" => return true,
            k if k.ends_with("_specifiers") => return true,
            "source_file" => return false,
            _ => {}
        }
        a = p.parent();
    }
    false
}

/// 形参数：`(void)` 视为空参表（grammar 偏差 §3）。
pub fn param_count(node: Node<'_>, src: &str) -> usize {
    let Some(params) = node.child_by_field_name("parameters") else {
        return 0;
    };
    let mut params_list = Vec::new();
    for (_field, child) in children_with_fields(params) {
        if child.kind() == "parameter" {
            params_list.push(child);
        }
    }
    if params_list.len() == 1 {
        let p = params_list[0];
        let only_void = p.child_by_field_name("name").is_none()
            && p
                .child_by_field_name("type")
                .map(|t| text(t, src) == "void")
                .unwrap_or(false);
        if only_void {
            return 0;
        }
    }
    params_list.len()
}

/// 形参列表（M3）：名字 + 语法层类型 + `InArgN` 占位标记（§2.4.6）。
/// `(void)` 视为空参表（与 `param_count` 同口径）；无名参数（`(void)` 之外
/// 属 ERROR 产物）跳过。
pub fn param_decls(node: Node<'_>, src: &str) -> Vec<ParamDecl> {
    let Some(params) = node.child_by_field_name("parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (_field, child) in children_with_fields(params) {
        if child.kind() != "parameter" {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue; // `(void)` 或无名残片
        };
        let name_str = text(name_node, src);
        let mut flags = DefFlags::NONE;
        if is_placeholder_param_name(name_str) {
            flags |= DefFlags::UNNAMED_PARAM;
        }
        let ty = child
            .child_by_field_name("type")
            .and_then(|t| parse_syn_type(t, src));
        out.push(ParamDecl {
            name: intern_sym(name_str),
            span: span(name_node),
            ty,
            flags,
        });
    }
    out
}

/// `InArgN` 占位名判定（引擎侧该参数无名时导出器生成的占位，§2.4.6）。
fn is_placeholder_param_name(s: &str) -> bool {
    match s.strip_prefix("InArg") {
        Some(rest) => !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// doc 注释块
// ---------------------------------------------------------------------------

/// 声明前连续 `//` 注释块（与声明之间至多一个换行；空行断块）。
/// tag/doc 的分流在 `decl_tags` 完成，这里只收集原始文本。
pub fn doc_comment_texts<'s>(node: Node<'_>, src: &'s str) -> Vec<&'s str> {
    let mut comments: Vec<Node<'_>> = Vec::new();
    let mut cur = node.prev_sibling();
    while let Some(c) = cur {
        if c.kind() != "comment" {
            break;
        }
        let next_start = comments
            .last()
            .map(|x| x.start_byte())
            .unwrap_or_else(|| node.start_byte());
        let gap = &src[c.end_byte()..next_start];
        if gap.matches('\n').count() > 1 {
            break; // 空行：块结束
        }
        comments.push(c);
        cur = c.prev_sibling();
    }
    comments.reverse();
    comments.iter().map(|c| &src[c.start_byte()..c.end_byte()]).collect()
}

/// 文件头注释块（source_file 的前导连续注释，`.d.as` 固定 4 行）。
pub fn leading_comment_texts<'s>(root: Node<'_>, src: &'s str) -> Vec<&'s str> {
    let mut out = Vec::new();
    for (_field, child) in children_with_fields(root) {
        if child.kind() != "comment" {
            break;
        }
        let next_start = children_with_fields(root)
            .iter()
            .find(|(_, n)| n.start_byte() > child.end_byte())
            .map(|(_, n)| n.start_byte())
            .unwrap_or_else(|| src.len());
        let gap = &src[child.end_byte()..next_start];
        if gap.matches('\n').count() > 1 {
            break;
        }
        out.push(&src[child.start_byte()..child.end_byte()]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::as_syntax;

    fn root(src: &str) -> as_syntax::tree_sitter::Tree {
        as_syntax::parse(src, None)
    }

    #[test]
    fn operator_name_detection() {
        assert!(is_operator_name("opAdd"));
        assert!(is_operator_name("opImplConv"));
        assert!(is_operator_name("opIndex"));
        assert!(!is_operator_name("open"));
        assert!(!is_operator_name("op"));
        assert!(!is_operator_name("Open"));
    }

    #[test]
    fn class_member_kind_context() {
        let src = "class A { void F() {} int X; }\nvoid G() {}\nint V;\n";
        let tree = root(src);
        let class = tree.root_node().named_child(0).unwrap();
        let body = class.child_by_field_name("body").unwrap();
        let f = body.named_child(0).unwrap();
        let x = body.named_child(1).unwrap();
        assert_eq!(classify_decl(f, src, DeclCtx::TypeBody), Some(DefKind::Method));
        assert_eq!(classify_decl(x, src, DeclCtx::TypeBody), Some(DefKind::Field));
        let g = tree.root_node().named_child(1).unwrap();
        let v = tree.root_node().named_child(2).unwrap();
        assert_eq!(classify_decl(g, src, DeclCtx::Global), Some(DefKind::Function));
        assert_eq!(classify_decl(v, src, DeclCtx::Global), Some(DefKind::GlobalVar));
    }

    #[test]
    fn syn_type_wrappers_canonical_order() {
        // `const FVector&in` → Ref(Const(Named))，与 TypeKind 规范序一致
        let src = "void F(const FVector&in V, float[] Arr, FVector Obj);\n";
        let tree = root(src);
        let f = tree.root_node().named_child(0).unwrap();
        let params = f.child_by_field_name("parameters").unwrap();
        let p0 = params.named_child(0).unwrap();
        let t0 = parse_syn_type(p0.child_by_field_name("type").unwrap(), src).unwrap();
        assert!(matches!(
            t0,
            SynType::Ref(inner, RefKind::In) if matches!(&*inner, SynType::Const(c) if matches!(&**c, SynType::Named(..)))
        ));
        let p1 = params.named_child(1).unwrap();
        let t1 = parse_syn_type(p1.child_by_field_name("type").unwrap(), src).unwrap();
        assert!(matches!(t1, SynType::Array(inner) if matches!(&*inner, SynType::Primitive(..))));
    }

    #[test]
    fn doc_comment_adjacency() {
        let src = "// line1\n// line2\n\n// orphan\nint A;\n// doc\nint B;\n";
        let tree = root(src);
        // comment 也是具名节点——按 kind 取声明，不能用 named_child(下标)
        let vars: Vec<_> = syntax_children_of_kind(tree.root_node(), "variable_declaration");
        assert_eq!(vars.len(), 2);
        let docs_a = doc_comment_texts(vars[0], src);
        assert_eq!(docs_a, vec!["// orphan"], "空行断块：A 只挂紧邻注释");
        let docs_b = doc_comment_texts(vars[1], src);
        assert_eq!(docs_b, vec!["// doc"]);
    }

    fn syntax_children_of_kind<'t>(node: Node<'t>, kind: &str) -> Vec<Node<'t>> {
        children_with_fields(node)
            .into_iter()
            .filter(|(_, c)| c.kind() == kind)
            .map(|(_, c)| c)
            .collect()
    }
}
