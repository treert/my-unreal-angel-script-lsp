//! UseSite 记录（LSP实现规划 §4 Phase 2 产物表 / D5）。
//!
//! 每文件所有「标识符使用点」：`(name, span, 语法角色)`——**只做语法层提取，
//! 不做任何解析**（解析是 Phase 3 请求驱动 + 按文件缓存的事，`references.rs`）。
//! 引用倒排（name → 文件集合）由 `index.rs` 随快照维护。
//!
//! 记录规则（M4 定案）：
//! - 只收 `identifier`。`primitive_type` 是内建类型使用点（无源码声明，
//!   references/rename 无意义，且 `float` 随配置归一化）——不记录；
//!   `type_parameter` 是 identifier 的 alias 节点（kind 不同），天然不匹配；
//! - **排除声明名**：parent 为声明节点且字段为 `name`（class/struct/enum/
//!   function/constructor/destructor/delegate/event/asset/virtual_property、
//!   `variable_declarator`、`parameter`、`enumerator`、`for_each_statement`
//!   迭代变量）。`namespace_declaration` 的 `scoped_name` 只有**最后一段**
//!   是声明名，前段是 namespace 使用点；
//! - **排除 specifier 语境**（`in_specifier_context`：UPROPERTY 宏参数、
//!   方法属性、说明符列表）与 `access_specifier` 的 level 名（`access:Editor`）；
//! - **`named_argument` 的名字标识符不记录**——它是形参引用，解析留 M5
//!   （与签名帮助同批，既定方针）；
//! - **f-string 插值段是正常表达式子树**，全树 identifier 遍历天然纳入
//!   （架构设计 §4.6 硬性要求，专项单测钉死）。

use as_syntax::tree_sitter::Node;

use crate::id::Sym;
use crate::intern::intern_sym;
use crate::range::TextRange;
use crate::resolve::{role_of, Role};
use crate::syntax;

/// 使用点的语法角色（与 resolve 的 Role 对齐；记录时不做解析——D5）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UseRole {
    /// 类型位置（`type` / `template_type` 的 name）
    TypeUse,
    /// `A::B` 的 A / namespace 声明 scoped_name 的前段
    ScopedFirst,
    /// `A::B` 的 B
    ScopedLast,
    /// `expr.Name` 的 Name
    MemberProperty,
    /// 调用 callee
    Callee,
    /// 其余裸标识符（赋值 / 实参 / return……）
    Plain,
}

/// 一个标识符使用点。
#[derive(Clone, Debug)]
pub struct UseSite {
    pub name: Sym,
    pub span: TextRange,
    pub role: UseRole,
}

/// 提取一个文件的全部「标识符使用点」（Phase 2；与声明提取同趟消费）。
pub fn collect_use_sites(root: Node<'_>, src: &str) -> Vec<UseSite> {
    let mut out = Vec::new();
    walk_identifiers(root, src, &mut out);
    out
}

fn walk_identifiers(node: Node<'_>, src: &str, out: &mut Vec<UseSite>) {
    if node.kind() == "identifier" {
        if let Some(site) = use_site_of(node, src) {
            out.push(site);
        }
        return; // identifier 是叶子
    }
    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            walk_identifiers(c.node(), src, out);
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
}

/// 声明名字段（`field == "name"`）的父节点集合。`template_type` / `type` 的
/// name 字段是**使用点**（TypeUse），不在此列。
fn is_decl_name(parent: Node<'_>, field: Option<&str>) -> bool {
    if field != Some("name") {
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

fn use_site_of(ident: Node<'_>, src: &str) -> Option<UseSite> {
    let parent = ident.parent()?;
    let field = syntax::children_with_fields(parent)
        .into_iter()
        .find(|(_, c)| c.id() == ident.id())
        .and_then(|(f, _)| f);
    let field = field.as_deref();

    // specifier 语境（UPROPERTY 宏参数 / 方法属性 / 说明符列表）不是使用点
    if syntax::in_specifier_context(ident) {
        return None;
    }
    // `access:Editor` 的 level 名不是符号使用点
    if parent.kind() == "access_specifier" {
        return None;
    }
    // 命名实参名字 = 形参引用，留 M5（与签名帮助同批）
    if parent.kind() == "named_argument" && field == Some("name") {
        return None;
    }
    let name = intern_sym(syntax::text(ident, src));
    let span = syntax::span(ident);

    // namespace 声明的 scoped_name：尾段是声明名（decl_name_node 同规则），
    // 前段是 namespace 使用点；access_grant 目标里的 scoped_name 全段是使用点
    if parent.kind() == "scoped_name" {
        let is_last = syntax::children_with_fields(parent)
            .into_iter()
            .filter(|(_, c)| c.kind() == "identifier")
            .last()
            .map(|(_, c)| c.id() == ident.id())
            .unwrap_or(false);
        let in_ns_decl = parent.parent().map_or(false, |g| g.kind() == "namespace_declaration");
        if in_ns_decl && is_last {
            return None;
        }
        return Some(UseSite { name, span, role: UseRole::ScopedFirst });
    }

    if is_decl_name(parent, field) {
        return None;
    }

    let role = match role_of(ident, parent) {
        Role::TypeUse => UseRole::TypeUse,
        Role::ScopedFirst => UseRole::ScopedFirst,
        Role::ScopedLast { .. } => UseRole::ScopedLast,
        Role::MemberProperty { .. } => UseRole::MemberProperty,
        Role::Callee { .. } => UseRole::Callee,
        Role::Plain => UseRole::Plain,
    };
    Some(UseSite { name, span, role })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::as_syntax;

    /// 用例源码全部内置（AGENTS.md 硬性规则 / D1）。

    fn sites(src: &str) -> Vec<(String, UseRole)> {
        let tree = as_syntax::parse(src, None);
        collect_use_sites(tree.root_node(), src)
            .into_iter()
            .map(|s| (crate::intern::sym_str(s.name).to_string(), s.role))
            .collect()
    }

    #[test]
    fn excludes_decl_names_specifiers_and_named_args() {
        const SRC: &str = "\
class AActor
{
    // @editable
    int Field;
    UPROPERTY()
    float Health;
    void M(int Param)
    {
        int Local = Field + Param;
        Print(f\"{Local} got {Param} points\", Duration=30);
    }
}
struct FString {}
void Print(FString S, float D) {}
";
        let got = sites(SRC);
        let count = |n: &str| got.iter().filter(|(s, _)| s == n).count();
        assert_eq!(count("Field"), 1, "成员使用点");
        assert_eq!(count("Param"), 2, "表达式 + f-string 插值段（架构设计 §4.6）");
        assert_eq!(count("Local"), 1, "f-string 插值段必须纳入扫描");
        assert_eq!(count("Print"), 1, "调用 callee");
        assert_eq!(count("FString"), 1, "类型使用点（Print 形参）");
        assert_eq!(count("AActor"), 0, "声明名");
        assert_eq!(count("Health"), 0, "字段声明名（无使用）");
        assert_eq!(count("Duration"), 0, "命名实参名字留 M5");
        assert_eq!(count("S"), 0, "形参声明名");
        // primitive（float D）不是 identifier 节点，天然不记录
        let print = got.iter().find(|(s, _)| s == "Print").unwrap();
        assert_eq!(print.1, UseRole::Callee);
        let fstring = got.iter().find(|(s, _)| s == "FString").unwrap();
        assert_eq!(fstring.1, UseRole::TypeUse);
    }

    #[test]
    fn namespace_scoped_name_and_access_level() {
        const SRC: &str = "\
namespace Outer::Inner
{
    class C {}
}
class Host
{
    access:Editor
    int X;
}
";
        let got = sites(SRC);
        let count = |n: &str| got.iter().filter(|(s, _)| s == n).count();
        assert_eq!(count("Outer"), 1, "scoped_name 前段是 namespace 使用点");
        assert_eq!(count("Inner"), 0, "scoped_name 尾段是声明名");
        assert_eq!(count("Editor"), 0, "access level 名不是符号使用点");
        assert_eq!(count("C"), 0, "类声明名");
        assert_eq!(count("Host"), 0);
    }

    #[test]
    fn for_each_loop_var_decl_excluded() {
        const SRC: &str = "\
int[] Items;
void F()
{
    for (int Elem : Items) { int A = Elem; }
}
";
        let got = sites(SRC);
        let count = |n: &str| got.iter().filter(|(s, _)| s == n).count();
        assert_eq!(count("Elem"), 1, "迭代变量声明名排除、循环体使用点记录");
        assert_eq!(count("Items"), 1);
        assert_eq!(count("F"), 0, "函数声明名");
        assert_eq!(count("A"), 0, "局部声明名");
    }
}
