//! semantic tokens 的 CST 映射（LSP实现规划 §8.1 / 架构设计 §4.6）。
//!
//! legend 对齐 Hazelight 扩展（`semantic_highlighting.ts` 的 SemanticTypeList，
//! 19 类、同序）——验收即「与 Hazelight 同文件截图对照」（§9 M2）。
//!
//! 两层：
//! - **M2 CST 直映射**（[`semantic_tokens`]，§6.1：冷启动 Loading 期间即可
//!   服务，不依赖索引）——语法可判定的子集：类型位（`type` 子树 / 基类 /
//!   asset 类型 / 构造调用的模板头）→ typename 族；声明名 → function/variable
//!   族（按声明语境，不解析作用域）。
//! - **M3 语义着色**（[`semantic_tokens_indexed`]，索引 Ready 后）：表达式位
//!   裸标识符经查找链（`resolve`）消歧局部/形参/成员/全局后补 token——
//!   使用点与声明同色。actor/component 等 subtype 分类（typename 族细分）
//!   需基类链归类，仍留待后续。
//!
//! 产出为字节偏移 token；UTF-16 换算与 delta 编码归 as-lsp 边界。

use as_syntax::tree_sitter::Node;

use crate::id::FileId;
use crate::resolve;
use crate::symbol::DefKind;
use crate::syntax;
use crate::workspace::Workspace;

/// Legend（**wire 名**，`as_` 前缀 + 与 Hazelight `SemanticTypeList` 同序——
/// 其 server.ts 在声明 legend 时对内部名单做 `"as_" + t` 映射；索引即
/// token type 下标）。语义着色 scope 映射（`semanticTokenScopes`）按此名对齐。
pub const LEGEND: &[&str] = &[
    "as_namespace",
    "as_template_base_type",
    "as_parameter",
    "as_local_variable",
    "as_member_variable",
    "as_member_accessor",
    "as_global_variable",
    "as_global_accessor",
    "as_member_function",
    "as_global_function",
    "as_unknown_error",
    "as_typename",
    "as_typename_actor",
    "as_typename_component",
    "as_typename_struct",
    "as_typename_event",
    "as_typename_delegate",
    "as_typename_primitive",
    "as_access_specifier",
];

// legend 下标常量（按 LEGEND 顺序；LEGEND 不变则不变）
const NAMESPACE: u8 = 0;
const TEMPLATE_BASE_TYPE: u8 = 1;
const PARAMETER: u8 = 2;
const LOCAL_VARIABLE: u8 = 3;
const MEMBER_VARIABLE: u8 = 4;
const MEMBER_ACCESSOR: u8 = 5;
const GLOBAL_VARIABLE: u8 = 6;
const GLOBAL_ACCESSOR: u8 = 7;
const MEMBER_FUNCTION: u8 = 8;
const GLOBAL_FUNCTION: u8 = 9;
const UNKNOWN_ERROR: u8 = 10;
const TYPENAME: u8 = 11;
const TYPENAME_STRUCT: u8 = 14;
const TYPENAME_EVENT: u8 = 15;
const TYPENAME_DELEGATE: u8 = 16;
const TYPENAME_PRIMITIVE: u8 = 17;
const ACCESS_SPECIFIER: u8 = 18;

/// 一个语义 token（字节偏移；长度即 token 字节长）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SemanticToken {
    pub start: u32,
    pub len: u32,
    /// LEGEND 下标
    pub ty: u8,
}

/// 声明语境（着色用：M2 在 tokens 内单独定义，Local = 函数体/块内）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Global,
    TypeBody,
    Local,
}

/// 计算全文件 token（按 start 升序）。
pub fn semantic_tokens(root: Node<'_>, src: &str) -> Vec<SemanticToken> {
    let mut out = Vec::new();
    walk(root, src, false, Ctx::Global, &mut out);
    out.sort_by_key(|t| (t.start, t.len));
    out
}

/// M3 语义着色：CST 直映射 + 使用点解析（索引 Ready 后的完整版）。
///
/// 遍历收集 CST 层未着色的 identifier（表达式位裸标识符 / 限定段 / 成员
/// 访问段），逐个走查找链解析，按落点 DefKind 映射 legend 类型补 token。
/// 解析失败（未定义 / 歧义不出唯一解）保持不着色；声明名等已有 CST
/// token 的位置按 span 覆盖去重（解析它们只会得到同色，白费一次查找）。
///
/// 文件不在索引中（未入收集根）→ None（调用方回落 CST 直映射）。
pub fn semantic_tokens_indexed(ws: &Workspace, file: FileId) -> Option<Vec<SemanticToken>> {
    let entry = ws.files.get(&file)?;
    let src = &entry.source;
    let root = entry.tree.root_node();

    // 覆盖判定只看 CST 基础 token（保持有序；语义 token 收集到独立 vec，
    // 最后合并——混入同一 vec 会破坏 covered 的有序前提，M3 期实测曾因此
    // 误跳过成员链上的 identifier）
    let mut out = semantic_tokens(root, src);
    let mut cands: Vec<Node<'_>> = Vec::new();
    collect_usage_idents(root, src, &mut cands);
    let mut usage: Vec<SemanticToken> = Vec::new();
    for ident in cands {
        let (s, e) = (ident.start_byte() as u32, ident.end_byte() as u32);
        // 叶子节点与任意 token 的 span 只能是包含或不相交：起点早于 s 且
        // 终点晚于 s 的 token（只可能是祖先）必已完整覆盖
        if covered(&out, s) {
            continue;
        }
        let Some(res) = resolve::resolve_at_node(ws, file, src, ident) else {
            continue;
        };
        let Some(ty) = res.targets.first().and_then(|t| target_token_ty(ws, t)) else {
            continue;
        };
        usage.push(SemanticToken { start: s, len: e - s, ty });
    }
    out.extend(usage);
    out.sort_by_key(|t| (t.start, t.len));
    Some(out)
}

/// 收集全部 identifier 节点（ERROR 子树不下钻——其内容本不该正常着色；
/// 覆盖去重与 specifier 语境过滤在解析侧完成）。
/// `this` 跳过：关键字表达式而非符号使用点（resolve 会给类声明，
/// 着成 typename 是误导；`Super`/`super` 指代基类本身，保持 typename）。
fn collect_usage_idents<'a>(node: Node<'a>, src: &str, out: &mut Vec<Node<'a>>) {
    if node.is_error() {
        return;
    }
    if node.kind() == "identifier" {
        if syntax::text(node, src) != "this" {
            out.push(node);
        }
        return;
    }
    for (_, child) in syntax::children_with_fields(node) {
        collect_usage_idents(child, src, out);
    }
}

/// `out` 已按 start 升序：存在 start ≤ s 且 end > s 的 token 即被覆盖。
fn covered(out: &[SemanticToken], s: u32) -> bool {
    let i = out.partition_point(|t| t.start <= s);
    out[..i].iter().rev().any(|t| t.start + t.len > s)
}

/// 解析落点 → legend 下标（使用点与声明位同色：CST 层声明名的分类依据）。
fn target_token_ty(ws: &Workspace, t: &resolve::Target) -> Option<u8> {
    match t {
        resolve::Target::Def(r) => {
            let d = ws.decl(r);
            Some(match d.kind {
                DefKind::Namespace | DefKind::Module => NAMESPACE,
                DefKind::Class | DefKind::Enum | DefKind::TypeParam => TYPENAME,
                DefKind::Struct => TYPENAME_STRUCT,
                DefKind::Delegate => TYPENAME_DELEGATE,
                DefKind::Event => TYPENAME_EVENT,
                DefKind::Function => GLOBAL_FUNCTION,
                DefKind::Method
                | DefKind::Constructor
                | DefKind::Destructor
                | DefKind::Operator => MEMBER_FUNCTION,
                DefKind::GlobalVar | DefKind::AssetDecl => GLOBAL_VARIABLE,
                DefKind::Field | DefKind::EnumValue => MEMBER_VARIABLE,
                DefKind::Param => PARAMETER,
                DefKind::LocalVar => LOCAL_VARIABLE,
                // 虚属性按父声明归类：类型体成员 → member，否则 global
                DefKind::VirtualProperty => {
                    let in_type = d.parent.is_some_and(|p| {
                        ws.files.get(&r.file).is_some_and(|e| {
                            e.summary.decls.get(p as usize).is_some_and(|pd| pd.kind.is_type_decl())
                        })
                    });
                    if in_type { MEMBER_ACCESSOR } else { GLOBAL_ACCESSOR }
                }
            })
        }
        // 合成成员（delegate/event 展开的 Execute 等 / StaticClass）：
        // 依其 kind 同口径映射
        resolve::Target::Synthetic(m) => Some(match m.kind {
            DefKind::Method | DefKind::Function | DefKind::Constructor => MEMBER_FUNCTION,
            DefKind::Field => MEMBER_VARIABLE,
            _ => return None,
        }),
        resolve::Target::Local(l) => Some(match l.kind {
            DefKind::Param => PARAMETER,
            DefKind::LocalVar => LOCAL_VARIABLE,
            _ => return None,
        }),
    }
}

fn push(out: &mut Vec<SemanticToken>, node: Node<'_>, ty: u8) {
    let start = node.start_byte() as u32;
    let end = node.end_byte() as u32;
    if end > start {
        out.push(SemanticToken { start, len: end - start, ty });
    }
}

fn push_ident(out: &mut Vec<SemanticToken>, node: Node<'_>, src: &str, ty: u8) {
    // 名字节点可能不是 identifier（如 scoped_name 已在外层拆段），统一取整段
    let _ = src;
    push(out, node, ty);
}

fn walk(node: Node<'_>, src: &str, in_type: bool, ctx: Ctx, out: &mut Vec<SemanticToken>) {
    if node.is_error() {
        push(out, node, UNKNOWN_ERROR);
        return; // 不下钻：恢复子树里的标识符本就不该正常着色
    }

    match node.kind() {
        "type" => {
            // 类型位子树：全部按类型着色
            walk_children(node, src, true, ctx, out);
            return;
        }
        "primitive_type" | "auto_type" | "wildcard_type" => {
            if in_type {
                push(out, node, TYPENAME_PRIMITIVE);
            }
            return;
        }
        "identifier" => {
            if in_type {
                push(out, node, TYPENAME);
            }
            return;
        }
        "template_type" => {
            // 使用位模板实例：容器名 → template_base_type，实参是 type 子树
            if let Some(name) = node.child_by_field_name("name") {
                push_ident(out, name, src, TEMPLATE_BASE_TYPE);
            }
            if let Some(args) = node.child_by_field_name("arguments") {
                walk_children(args, src, true, ctx, out);
            }
            return;
        }
        "qualified_identifier" => {
            if in_type {
                // A::B::C：除末段外视为命名空间段，末段是类型名
                let idents: Vec<Node<'_>> = syntax::children_with_fields(node)
                    .into_iter()
                    .filter(|(_, c)| c.kind() == "identifier")
                    .map(|(_, c)| c)
                    .collect();
                for (i, id) in idents.iter().enumerate() {
                    let ty = if i + 1 == idents.len() { TYPENAME } else { NAMESPACE };
                    push(out, *id, ty);
                }
            }
            return;
        }
        "scoped_name" => {
            // namespace 声明名等：每段都是命名空间名
            for (_f, c) in syntax::children_with_fields(node) {
                if c.kind() == "identifier" {
                    push(out, c, NAMESPACE);
                }
            }
            return;
        }
        "access_specifier" => {
            if let Some(name) = node.child_by_field_name("name") {
                push_ident(out, name, src, ACCESS_SPECIFIER);
            }
            return;
        }
        // ---- 声明节点：名字 token + 语境切换 ----
        "class_declaration" | "struct_declaration" => {
            let is_struct = node.kind() == "struct_declaration";
            if let Some(name) = node.child_by_field_name("name") {
                push_ident(out, name, src, if is_struct { TYPENAME_STRUCT } else { TYPENAME });
            }
            if let Some(tp) = node.child_by_field_name("type_parameters") {
                walk(tp, src, true, Ctx::TypeBody, out);
            }
            // 基类列表（field base）：类型位
            for (field, child) in syntax::children_with_fields(node) {
                if field.as_deref() == Some("base") {
                    walk(child, src, true, ctx, out);
                }
            }
            if let Some(body) = node.child_by_field_name("body") {
                walk_children(body, src, false, Ctx::TypeBody, out);
            }
            return;
        }
        "enum_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                push_ident(out, name, src, TYPENAME);
            }
            if let Some(body) = node.child_by_field_name("body") {
                walk_children(body, src, false, ctx, out);
            }
            return;
        }
        "enumerator" => {
            if let Some(name) = node.child_by_field_name("name") {
                push_ident(out, name, src, MEMBER_VARIABLE);
            }
            return;
        }
        "namespace_declaration" => {
            // name 是 scoped_name（上面分支按段处理）
            walk_children(node, src, false, Ctx::Global, out);
            return;
        }
        "delegate_declaration" | "event_declaration" => {
            let ty = if node.kind() == "delegate_declaration" { TYPENAME_DELEGATE } else { TYPENAME_EVENT };
            if let Some(name) = node.child_by_field_name("name") {
                push_ident(out, name, src, ty);
            }
            walk_children(node, src, false, ctx, out);
            return;
        }
        "asset_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                push_ident(out, name, src, GLOBAL_VARIABLE);
            }
            walk_children(node, src, false, ctx, out);
            return;
        }
        "constructor_declaration" | "destructor_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                push_ident(out, name, src, MEMBER_FUNCTION);
            }
            walk_children(node, src, false, Ctx::Local, out);
            return;
        }
        "function_declaration" => {
            let is_member = ctx == Ctx::TypeBody;
            let fn_ty = if is_member { MEMBER_FUNCTION } else { GLOBAL_FUNCTION };
            if let Some(name) = node.child_by_field_name("name") {
                push_ident(out, name, src, fn_ty);
            }
            walk_children(node, src, false, Ctx::Local, out);
            return;
        }
        "virtual_property_declaration" => {
            let acc_ty = match ctx {
                Ctx::TypeBody => MEMBER_ACCESSOR,
                _ => GLOBAL_ACCESSOR,
            };
            if let Some(name) = node.child_by_field_name("name") {
                push_ident(out, name, src, acc_ty);
            }
            walk_children(node, src, false, ctx, out);
            return;
        }
        "variable_declaration" => {
            walk_children(node, src, false, ctx, out);
            return;
        }
        "variable_declarator" => {
            let var_ty = match ctx {
                Ctx::TypeBody => MEMBER_VARIABLE,
                Ctx::Global => GLOBAL_VARIABLE,
                Ctx::Local => LOCAL_VARIABLE,
            };
            if let Some(name) = node.child_by_field_name("name") {
                push_ident(out, name, src, var_ty);
            }
            walk_children(node, src, false, ctx, out);
            return;
        }
        "parameter" => {
            if let Some(name) = node.child_by_field_name("name") {
                push_ident(out, name, src, PARAMETER);
            }
            walk_children(node, src, false, ctx, out);
            return;
        }
        "for_each_statement" => {
            if let Some(name) = node.child_by_field_name("name") {
                push_ident(out, name, src, LOCAL_VARIABLE);
            }
            walk_children(node, src, false, Ctx::Local, out);
            return;
        }
        "for_statement" | "block" | "expression_statement" | "if_statement"
        | "while_statement" | "do_while_statement" | "switch_statement" | "case_clause"
        | "default_clause" | "return_statement" | "switch_body" | "default_block" => {
            let inner = if ctx == Ctx::Local { ctx } else { Ctx::Local };
            walk_children(node, src, false, inner, out);
            return;
        }
        "call_expression" => {
            // 构造调用的模板头 `TArray<FVector>(...)`：callee 是 template_type
            // 时按类型位着色（语法可判定：普通函数名不会带 <>）
            for (field, child) in syntax::children_with_fields(node) {
                let in_type_callee = field.as_deref() == Some("function")
                    && matches!(child.kind(), "template_type" | "primitive_type");
                walk(child, src, in_type_callee, ctx, out);
            }
            return;
        }
        _ => {
            walk_children(node, src, false, ctx, out);
        }
    }
}

fn walk_children(node: Node<'_>, src: &str, in_type: bool, ctx: Ctx, out: &mut Vec<SemanticToken>) {
    for (_field, child) in syntax::children_with_fields(node) {
        walk(child, src, in_type, ctx, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::as_syntax;

    fn tokens_of(src: &str) -> Vec<(String, u8)> {
        let tree = as_syntax::parse(src, None);
        semantic_tokens(tree.root_node(), src)
            .into_iter()
            .map(|t| {
                let text = &src[t.start as usize..(t.start + t.len) as usize];
                (text.to_string(), t.ty)
            })
            .collect()
    }

    fn assert_has(tokens: &[(String, u8)], text: &str, ty: u8) {
        assert!(
            tokens.iter().any(|(t, k)| t == text && *k == ty),
            "expected ({text}, {ty}) in {tokens:?}"
        );
    }

    // 用例源码内置（D1）。legend 序号见 LEGEND 注释。

    #[test]
    fn legend_matches_hazelight() {
        // wire 名 = "as_" + Hazelight semantic_highlighting.ts 的 SemanticTypeList（同序）
        assert_eq!(LEGEND[0], "as_namespace");
        assert_eq!(LEGEND[4], "as_member_variable");
        assert_eq!(LEGEND[9], "as_global_function");
        assert_eq!(LEGEND[10], "as_unknown_error");
        assert_eq!(LEGEND[11], "as_typename");
        assert_eq!(LEGEND[14], "as_typename_struct");
        assert_eq!(LEGEND[17], "as_typename_primitive");
        assert_eq!(LEGEND[18], "as_access_specifier");
        assert_eq!(LEGEND.len(), 19);
    }

    #[test]
    fn declaration_names_by_context() {
        let src = "\
class AActor : UObject
{
    int Health;
    void Tick(float Delta) {}
}
void Main() {}
int Global;
namespace Math { float64 Sqrt(float64 V); }
";
        let tokens = tokens_of(src);
        assert_has(&tokens, "AActor", TYPENAME);
        assert_has(&tokens, "UObject", TYPENAME); // 基类：类型位
        assert_has(&tokens, "Health", MEMBER_VARIABLE);
        assert_has(&tokens, "Tick", MEMBER_FUNCTION);
        assert_has(&tokens, "Delta", PARAMETER);
        assert_has(&tokens, "Main", GLOBAL_FUNCTION);
        assert_has(&tokens, "Global", GLOBAL_VARIABLE);
        assert_has(&tokens, "Math", NAMESPACE);
        assert_has(&tokens, "Sqrt", GLOBAL_FUNCTION);
        assert_has(&tokens, "float64", TYPENAME_PRIMITIVE);
    }

    #[test]
    fn struct_delegate_event_enum_colors() {
        let src = "\
struct FVector { float X; }
delegate void OnTick(float DT);
event void OnHit(FVector Pos);
enum EMode { Off, On }
";
        let tokens = tokens_of(src);
        assert_has(&tokens, "FVector", TYPENAME_STRUCT);
        assert_has(&tokens, "OnTick", TYPENAME_DELEGATE);
        assert_has(&tokens, "OnHit", TYPENAME_EVENT);
        assert_has(&tokens, "EMode", TYPENAME);
        assert_has(&tokens, "Off", MEMBER_VARIABLE);
        assert_has(&tokens, "On", MEMBER_VARIABLE);
    }

    #[test]
    fn type_positions_and_template_usage() {
        let src = "struct Holder { TArray<FVector> Arr; void F(const FVector&in V) {} }\n";
        let tokens = tokens_of(src);
        assert_has(&tokens, "TArray", TEMPLATE_BASE_TYPE);
        assert_has(&tokens, "FVector", TYPENAME);
        assert_has(&tokens, "Arr", MEMBER_VARIABLE);
        assert_has(&tokens, "V", PARAMETER);
    }

    #[test]
    fn locals_and_template_ctor_callee() {
        let src = "\
void Main()
{
    auto Arr = TArray<FVector>();
    int Local = 1;
    for (int I : Arr) {}
}
";
        let tokens = tokens_of(src);
        assert_has(&tokens, "TArray", TEMPLATE_BASE_TYPE);
        assert_has(&tokens, "FVector", TYPENAME);
        assert_has(&tokens, "Local", LOCAL_VARIABLE);
        assert_has(&tokens, "I", LOCAL_VARIABLE);
        assert_has(&tokens, "auto", TYPENAME_PRIMITIVE);
    }

    #[test]
    fn expression_identifiers_not_colored() {
        // M2 CST 层：表达式位裸标识符不着色（语义着色见下方 indexed 用例）
        let src = "void Main() { Foo(Bar); }\n";
        let tokens = tokens_of(src);
        assert!(!tokens.iter().any(|(t, _)| t == "Foo" || t == "Bar"));
        assert_has(&tokens, "Main", GLOBAL_FUNCTION);
    }

    // ---- M3 语义着色（索引版）----

    use crate::config::IndexConfig;
    use crate::intern::intern_file;
    use crate::workspace::{FileInput, FileKind};

    fn indexed_tokens_of(src: &str) -> Vec<(String, u8)> {
        let path = "unique://tokens/usage.as";
        let file = intern_file(path, 0);
        let inputs = vec![FileInput {
            file,
            kind: FileKind::Script,
            module: None,
            source: src.to_string(),
        }];
        let ws = Workspace::build(IndexConfig::default(), inputs);
        semantic_tokens_indexed(&ws, file)
            .expect("单文件已入索引")
            .into_iter()
            .map(|t| {
                let text = &src[t.start as usize..(t.start + t.len) as usize];
                (text.to_string(), t.ty)
            })
            .collect()
    }

    fn assert_count(tokens: &[(String, u8)], text: &str, ty: u8, n: usize) {
        let hit = tokens.iter().filter(|(t, k)| t == text && *k == ty).count();
        assert_eq!(hit, n, "expected {n}x ({text}, {ty}) in {tokens:?}");
    }

    #[test]
    fn global_function_usage_colored() {
        // 声明 + 使用点同色（用户报告的缺口：g_func 调用处不着色）
        let src = "void g_func(int d);\nvoid l_test() {\n    g_func(123);\n}\n";
        let tokens = indexed_tokens_of(src);
        assert_count(&tokens, "g_func", GLOBAL_FUNCTION, 2);
        assert_count(&tokens, "l_test", GLOBAL_FUNCTION, 1);
    }

    #[test]
    fn locals_params_and_members_usage_colored() {
        let src = "\
struct FVector { float X; }
class Holder {
    int Health;
    void Tick(float Delta) { Health = int(Delta) + 1; }
    void Call() { this.Tick(0.5); }
}
void Main() {
    int Local = 1;
    Local = Local + 1;
}
";
        let tokens = indexed_tokens_of(src);
        // 声明 + 使用点同色
        assert_count(&tokens, "Health", MEMBER_VARIABLE, 2);
        assert_count(&tokens, "Delta", PARAMETER, 2);
        assert_count(&tokens, "Local", LOCAL_VARIABLE, 3);
        assert_count(&tokens, "Tick", MEMBER_FUNCTION, 2);
    }

    #[test]
    fn qualified_enum_and_type_usage_colored() {
        let src = "\
enum EMode { Off, On }
struct FVector { float X; }
EMode M = EMode::Off;
FVector V = FVector();
";
        let tokens = indexed_tokens_of(src);
        // 使用位的 EMode（限定首段）与构造调用名同色于声明
        assert_count(&tokens, "EMode", TYPENAME, 3);
        assert_count(&tokens, "Off", MEMBER_VARIABLE, 2);
        // 声明名 + 构造调用 → struct；类型位（CST 层）保持 typename
        assert_count(&tokens, "FVector", TYPENAME_STRUCT, 2);
        assert_count(&tokens, "FVector", TYPENAME, 1);
    }

    #[test]
    fn unresolved_usage_stays_uncolored() {
        let src = "void Main() { Foo(Bar); }\n";
        let tokens = indexed_tokens_of(src);
        assert!(!tokens.iter().any(|(t, _)| t == "Foo" || t == "Bar"));
    }

    #[test]
    fn member_function_usage_patterns() {
        // 接收者形态全景：this / 隐式 this / 调用链 / 局部 / 形参 /
        // 字段 / 继承 / cast
        let src = "\
class Helper {
    void Clean() {}
}
class Base { void Bm() {} }
class AActor : Base {
    Helper H;
    Helper GetHelper() { return H; }
    void Tick() {}
    void Test(Helper p) {
        this.Tick();
        Tick();
        GetHelper().Clean();
        Helper local;
        local.Clean();
        p.Clean();
        H.Clean();
        Bm();
    }
}
void g(AActor a) {
    a.Tick();
    a.Bm();
}
";
        let tokens = indexed_tokens_of(src);
        // Tick：声明 + this. + 隐式 + a.（×2）
        assert_count(&tokens, "Tick", MEMBER_FUNCTION, 4);
        // Clean：声明 + 调用链 + local + p + 字段 H
        assert_count(&tokens, "Clean", MEMBER_FUNCTION, 5);
        // Bm：Base 声明 + 隐式继承调用 + a.Bm
        assert_count(&tokens, "Bm", MEMBER_FUNCTION, 3);
    }

    #[test]
    fn error_nodes_colored() {
        let tokens = tokens_of(")))");
        assert!(tokens.iter().any(|(t, _)| t == ")))"));
        assert!(tokens.iter().any(|(_, k)| *k == UNKNOWN_ERROR));
    }
}
