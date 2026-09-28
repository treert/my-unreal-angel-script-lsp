//! signatureHelp 内核（M5d，规划 §8.1 M5 行：重载集排序 + 激活项）。
//!
//! - **定位**：byte 所在 call_expression（光标在 `(` 与最后一个实参尾之间；
//!   未闭合调用走 ERROR 归约——completion.rs 同族形态 B）；
//! - **排序**：arity（Exact 在前 / ArityMiss 尾部）+ 实参定型消歧
//!   （`overload::disambiguate`，M5a 全量定型后的直接红利——f-string/
//!   运算符实参现在可定型）；唯一命中提到首位；active 取首个 arity
//!   匹配项；
//! - **activeParameter**：顺序实参按逗号槽位计数；**命名实参感知**——
//!   光标在 `Name: v` 或正在输入的 `Name|` 内 → 按激活重载的形参表
//!   **按名定位**下标（`f(Duration: |` 指向 Duration 的形参位，不是
//!   位置序）；
//! - 渲染复用 hover 的签名（label 即紧凑单行）；InArgN 占位**原样显示**
//!   （声明文本忠实；「跳过」只约束补全，架构设计 §2.4.6）。

use as_syntax::tree_sitter::Node;

use crate::aggregation::DeclRef;
use crate::expr::expr_type;
use crate::hover;
use crate::id::FileId;
use crate::intern::intern_sym;
use crate::resolve::{
    find_accessors, member_search_space, resolve_callee, SemCtx, Target,
};
use crate::summary::RawExtra;
use crate::symbol::DefKind;
use crate::syntax;
use crate::workspace::Workspace;

/// 签名帮助结果。
pub struct SignatureHelpData {
    /// 排序后的重载（消歧命中 > arity 匹配 > 其余）
    pub overloads: Vec<DeclRef>,
    /// 激活项下标
    pub active: u32,
    /// 激活形参序号（命名实参感知）
    pub active_parameter: u32,
}

/// 光标处签名帮助。字节偏移；None = 不在调用内 / callee 不可解析。
pub fn signature_help(ws: &Workspace, file: FileId, byte: u32) -> Option<SignatureHelpData> {
    let entry = ws.files.get(&file)?;
    let src = &entry.source;
    let root = entry.tree.root_node();
    if byte > root.end_byte() as u32 {
        return None;
    }
    let node = deepest_in_call(root, src, byte)?;
    let ctx = SemCtx::at_byte(ws, file, src, byte, node);
    let (callee, args_node) = enclosing_call(node)?;
    let defs = callee_overloads(ws, &ctx, src, callee)?;
    if defs.is_empty() {
        return None;
    }

    let args: Vec<Node<'_>> = syntax::children_with_fields(args_node)
        .into_iter()
        .filter(|(_, c)| c.kind() == "argument" || c.kind() == "named_argument")
        .map(|(_, c)| c)
        .collect();

    // activeParameter 先算（命名实参感知）——排序要用它（见下）
    // arity 排序的口径 = **光标所在槽位序**（用户正在填第 active+1 个参数）
    // 而非已完成实参数：`Print("x", |` 处已完成 1 个，但用户在填第 2 个，
    // 2 参重载应置首（1 参重载在光标槽位上装不下第 2 参）
    let active_slot = {
        let a = active_parameter_at(ws, defs[0], src, &args, byte);
        (a + 1) as usize
    };
    // 槽位匹配组在前（params.len() >= active_slot——装得下光标所在参数），
    // 其余尾部
    let mut head: Vec<DeclRef> = defs
        .iter()
        .copied()
        .filter(|&d| {
            matches!(&ws.decl(&d).extra, RawExtra::Callable { params, .. } if params.len() >= active_slot)
        })
        .collect();
    let tail: Vec<DeclRef> = defs.iter().copied().filter(|d| !head.contains(d)).collect();

    // 实参定型消歧：**只在槽位匹配组内**做（M5a 红利：f-string/运算符实参
    // 可定型）。光标在尾槽时已完成实参数 < active_slot——更短的重载即使
    // arity 贴合已完成实参也不是用户目标（正在填的参数装不下）
    let arg_types: Vec<Option<DeclRef>> = args
        .iter()
        .map(|&a| expr_type(ws, &ctx, src, a).map(|t| t.base))
        .collect();
    if let Some(w) = crate::overload::disambiguate(ws, &head, &arg_types) {
        head.retain(|&d| d != w);
        head.insert(0, w);
    }

    let mut overloads = head;
    overloads.extend(tail);

    // 激活项：消歧命中已置首；否则首个即槽位匹配组头
    let active = 0;

    // activeParameter（命名实参感知，按激活重载的形参表定位）
    let active_parameter =
        active_parameter_at(ws, overloads[active as usize], src, &args, byte);
    Some(SignatureHelpData { overloads, active, active_parameter })
}

/// 光标处最深节点（含尾部空白左偏，同行）——completion::deepest_at 同族。
fn deepest_in_call<'t>(root: Node<'t>, src: &str, byte: u32) -> Option<Node<'t>> {
    let mut node = root;
    loop {
        let children = syntax::children_with_fields(node);
        let next = children
            .iter()
            .find(|(_, c)| (c.start_byte() as u32) <= byte && byte <= (c.end_byte() as u32))
            .map(|(_, c)| *c)
            .or_else(|| {
                children
                    .iter()
                    .rev()
                    .find(|(_, c)| {
                        (c.end_byte() as u32) <= byte
                            && !src[c.end_byte()..byte as usize].contains('\n')
                    })
                    .map(|(_, c)| *c)
            });
        match next {
            Some(c) => node = c,
            None => return Some(node),
        }
    }
}

/// 从节点上溯找所在调用的 (callee, args 容器)。未闭合调用：ERROR 直接孩子
/// 归约（completion::recover_error_ctx 形态 B 同族）。
fn enclosing_call<'t>(node: Node<'t>) -> Option<(Node<'t>, Node<'t>)> {
    let mut cur = Some(node);
    while let Some(n) = cur {
        match n.kind() {
            "call_expression" => {
                let callee = n.child_by_field_name("function")?;
                let args = n.child_by_field_name("arguments")?;
                return Some((callee, args));
            }
            "ERROR" => {
                let children = syntax::children_with_fields(n);
                let lp = children.iter().find(|(_, c)| !c.is_named() && c.kind() == "(")?;
                let callee = children
                    .iter()
                    .find(|(_, c)| c.is_named() && c.end_byte() <= lp.1.start_byte())?;
                return Some((callee.1, n));
            }
            _ => {}
        }
        cur = n.parent();
    }
    None
}

/// callee 的重载组。
fn callee_overloads(
    ws: &Workspace,
    ctx: &SemCtx,
    src: &str,
    callee: Node<'_>,
) -> Option<Vec<DeclRef>> {
    match callee.kind() {
        "identifier" => {
            let name = intern_sym(syntax::text(callee, src));
            let res = resolve_callee(ws, ctx, name, 0)?;
            let defs: Vec<DeclRef> = res
                .targets
                .iter()
                .filter_map(|t| match t {
                    Target::Def(r) => Some(*r),
                    Target::Synthetic(_) | Target::Local(_) => None,
                })
                .filter(|&r| {
                    matches!(
                        ws.decl(&r).kind,
                        DefKind::Function
                            | DefKind::Method
                            | DefKind::Constructor
                            | DefKind::Destructor
                    )
                })
                .collect();
            (!defs.is_empty()).then_some(defs)
        }
        "member_expression" => {
            let recv = expr_type(ws, ctx, src, callee.child_by_field_name("object")?)?;
            let prop = callee.child_by_field_name("property")?;
            let name = intern_sym(syntax::text(prop, src));
            let space = member_search_space(ws, recv.base);
            let mut cands = crate::resolve::members_named(ws, &space, name, |d| {
                matches!(d.kind, DefKind::Method | DefKind::Function)
            });
            if cands.is_empty() {
                cands = find_accessors(ws, &space, name);
            }
            (!cands.is_empty()).then_some(cands)
        }
        _ => None,
    }
}

/// activeParameter：光标落在第几个「实参槽」。
/// - 命名实参（正在输入名字段或值段内）→ 按激活重载的形参表**按名定位**；
/// - 其余按槽位计数（光标在某实参内即该槽）。
fn active_parameter_at(
    ws: &Workspace,
    active_def: DeclRef,
    src: &str,
    args: &[Node<'_>],
    byte: u32,
) -> u32 {
    let mut slot = 0u32;
    for &a in args {
        let in_this = (a.start_byte() as u32) <= byte && byte <= (a.end_byte() as u32);
        if a.kind() == "named_argument" {
            if in_this {
                if let Some(n) = a.child_by_field_name("name") {
                    return param_index_by_name(ws, active_def, syntax::text(n, src), slot);
                }
            }
            slot += 1;
            continue;
        }
        if in_this {
            return slot;
        }
        slot += 1;
    }
    slot
}

/// 命名实参名 → 激活重载形参表的下标；找不到（名字拼错）退回位置槽。
fn param_index_by_name(ws: &Workspace, def: DeclRef, name: &str, fallback: u32) -> u32 {
    if let RawExtra::Callable { params, .. } = &ws.decl(&def).extra {
        if let Some(i) = params.iter().position(|p| crate::intern::sym_str(p.name) == name) {
            return i as u32;
        }
    }
    fallback
}

/// 单个重载的 label（hover 签名同源）。
pub fn overload_label(ws: &Workspace, def: DeclRef) -> String {
    hover::signature(ws, &Target::Def(def)).unwrap_or_default()
}

/// 单个重载的文档（doc 首段首行）。
pub fn overload_doc(ws: &Workspace, def: DeclRef) -> Option<String> {
    let d = ws.decl(&def);
    let doc = d.doc.as_ref()?;
    let first = doc.lines().find(|l| !l.trim().is_empty())?;
    Some(first.trim_start_matches("// ").trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IndexConfig;
    use crate::id::FileId;
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

    fn off(src: &str, needle: &str) -> u32 {
        src.find(needle).unwrap_or_else(|| panic!("定位标记缺失: {needle}")) as u32
    }

    fn at(src: &str, needle: &str, n: usize) -> u32 {
        off(src, needle) + n as u32
    }

    fn file_of(path: &str) -> FileId {
        intern_file(path, 0)
    }

    fn label_at(ws: &Workspace, sh: &SignatureHelpData, i: usize) -> String {
        overload_label(ws, sh.overloads[i])
    }

    #[test]
    fn arity_ordering_and_active() {
        const SRC: &str = "\
void Print(const FString&in Message) {}
void Print(const FString&in Message, float64 Duration = 0.0) {}
void Print(const FString&in Message, FName Category) {}
void F()
{
    Print(\"x\", 
}
";
        let ws = build(&[("unique://sig/arity.as", SRC)]);
        let file = file_of("unique://sig/arity.as");
        // 2 实参（光标在尾空格 = needle 11 字符处）：前两个 2 参重载在前
        let sh = signature_help(&ws, file, at(SRC, "Print(\"x\", ", 11)).unwrap();
        assert_eq!(sh.overloads.len(), 3, "全部重载");
        let l0 = label_at(&ws, &sh, 0);
        assert!(l0.contains("Duration"), "2 参重载置首: {l0}");
        assert_eq!(sh.active_parameter, 1, "第 2 槽");
    }

    #[test]
    fn disambiguation_pulls_exact_overload() {
        // f-string 实参定型（M5a 红利）→ FString 重载唯一命中
        // （FString/FName 需有声明——引擎类型非内建）
        const SRC: &str = "\
struct FString { int Length; }
struct FName {}
void Log(FString S) {}
void Log(FName N) {}
void F()
{
    Log(f\"msg {1}\", 
}
";
        let ws = build(&[("unique://sig/dis.as", SRC)]);
        let file = file_of("unique://sig/dis.as");
        let sh = signature_help(&ws, file, at(SRC, "Log(f\"msg {1}\", ", 16)).unwrap();
        let l0 = label_at(&ws, &sh, 0);
        assert!(l0.contains("FString"), "f-string 定型消歧 FString 置首: {l0}");
    }

    #[test]
    fn named_argument_locates_param_by_name() {
        const SRC: &str = "\
void Print(const FString&in Message, float64 Duration = 0.0, FName Category) {}
void F()
{
    Print(\"x\", Duration=1.0, 
}
";
        let ws = build(&[("unique://sig/narg.as", SRC)]);
        let file = file_of("unique://sig/narg.as");
        // 光标在 "Duration=1.0, " 尾空格（needle 14 字符 + 14）——第 3 槽
        let sh = signature_help(&ws, file, at(SRC, "Duration=1.0, ", 14)).unwrap();
        assert_eq!(sh.active_parameter, 2, "第 3 槽");
        // 光标在命名实参名字段内（D|uration）→ 按名定位到下标 1
        let sh = signature_help(&ws, file, at(SRC, "Duration=1.0", 1)).unwrap();
        assert_eq!(sh.active_parameter, 1, "命名实参按名定位 Duration（下标 1）");
    }

    #[test]
    fn member_call_and_unclosed() {
        const DECL: &str = "struct FVector { float DotProduct(FVector Other) const; float GetMax() const; }";
        const SRC: &str = "\
void F()
{
    FVector V;
    V.DotProduct(
}
";
        let ws = build(&[
            ("unique://sig/vec.d.as", DECL),
            ("unique://sig/mem.as", SRC),
        ]);
        let file = file_of("unique://sig/mem.as");
        // 未闭合调用（ERROR 形态 B）：仍能给出签名
        let sh = signature_help(&ws, file, at(SRC, "V.DotProduct(", 13)).unwrap();
        let l0 = label_at(&ws, &sh, 0);
        assert!(l0.contains("DotProduct"), "成员调用未闭合也可: {l0}");
        assert_eq!(sh.active_parameter, 0);
    }

    #[test]
    fn not_in_call_returns_none() {
        const SRC: &str = "\
void F()
{
    int X = 1;
}
";
        let ws = build(&[("unique://sig/none.as", SRC)]);
        let file = file_of("unique://sig/none.as");
        assert!(signature_help(&ws, file, at(SRC, "int X = 1", 8)).is_none());
    }
}
