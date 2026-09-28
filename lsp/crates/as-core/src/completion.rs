//! 补全内核（M5b，规划 §8.1 M5 行）：语境判定 + 候选收集。
//!
//! 语境四类（从光标处最深节点沿祖先链**由内向外**判定，内层语境优先）：
//! - **Member**（`X.` / `X.Ab|`）：接收者定型 → 成员空间（class 闭包 / struct
//!   单层）全量成员 + 访问器折叠 + mixin（显式接收者准入，D23）+ 委托/事件
//!   接收者的**合成成员集**（B4：Execute / Broadcast / ctor / opAssign）；
//! - **Scoped**（`A::` / `A::Na|`）：namespace 聚合成员 + class 兼任 namespace
//!   的 StaticClass（B3/B4）+ enum 值 + `Super::` 父类成员；
//! - **CallArg**（调用实参位）：命名实参候选（callee 重载组形参并集，**跳过
//!   `InArgN` 占位**——UNNAMED_PARAM 索引期标记，架构设计 §2.4.6 硬性）∪
//!   裸标识符全集；
//! - **Plain**（裸标识符位）：关键字 + 局部 + 隐式 this 成员 + ns 链 + 全局/类型。
//!
//! 性能约定：有输入前缀时各层**先按名过滤再渲染 detail**（hover 签名格式化
//! 只对幸存者做）；无前缀时全局/类型层不带 detail，结果封顶 [`MAX_CANDIDATES`]。
//! 排序键 sort_hint 越小越靠前（局部 > 成员 > 命名实参 > ns > 全局 > 关键字）。
//!
//! 注释 / 字符串 / heredoc / f-string 文本段 / 说明符宏参数 / FName 字面量
//! 返回空（后两者 M5c 接入 Specifier / UFUNCTION 名单语境）。
//! 失败降级宁缺毋假（D14）：接收者不可定型 → Member 不给候选。

use std::collections::HashSet;

use as_syntax::tree_sitter::Node;

use crate::aggregation::DeclRef;
use crate::expr::expr_type;
use crate::hover;
use crate::id::{FileId, Sym};
use crate::intern::{intern_sym, sym_str};
use crate::resolve::{
    member_search_space, resolve_callee, SemCtx, Target,
};
use crate::summary::RawExtra;
use crate::symbol::{DefFlags, DefKind};
use crate::syntax;
use crate::workspace::Workspace;

/// 候选种类（as-lsp 负责映射 CompletionItemKind）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CandidateKind {
    Field,
    Method,
    Function,
    /// 访问器折叠出的属性项（`Get*/Set*` → `Name`）
    Property,
    Class,
    Struct,
    Enum,
    EnumValue,
    Namespace,
    GlobalVar,
    Param,
    LocalVar,
    Keyword,
    Delegate,
    Event,
    /// 命名实参（insert 形如 `Name=`）
    NamedArg,
}

/// 一个补全候选。
#[derive(Clone, Debug)]
pub struct Candidate {
    pub label: String,
    pub kind: CandidateKind,
    /// 签名（hover 同源渲染）；无前缀时的全局/类型层可缺省
    pub detail: Option<String>,
    /// 插入文本（label ≠ 插入内容时才有值：命名实参 `Name=`）
    pub insert: Option<String>,
    pub sort_hint: u8,
}

/// 无前缀时的结果封顶（全量喷发无消费场景且体积失控——与 workspaceSymbol
/// 的 2048 封顶同族，D30）。
pub const MAX_CANDIDATES: usize = 4096;

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 光标处补全。字节偏移；返回按 sort_hint + label 排序的候选集。
pub fn complete_at(ws: &Workspace, file: FileId, byte: u32) -> Vec<Candidate> {
    let Some(entry) = ws.files.get(&file) else { return Vec::new() };
    let src = &entry.source;
    let root = entry.tree.root_node();
    if byte > root.end_byte() as u32 {
        return Vec::new();
    }
    let Some(node) = deepest_at(root, byte, src) else { return Vec::new() };
    let prefix = prefix_at(node, src, byte);
    let ctx = SemCtx::at_byte(ws, file, src, byte, node);

    let out = match detect_from(node, src, byte) {
        Ctx::None => Vec::new(),
        Ctx::Member { object } => member_candidates(ws, &ctx, src, object),
        Ctx::Scoped { scope } => scoped_candidates(ws, &ctx, src, scope),
        Ctx::CallArg { callee, args } => {
            call_arg_candidates(ws, &ctx, src, callee, args, prefix.as_deref())
        }
        Ctx::Specifier { macro_name } => specifier_candidates(macro_name, prefix.as_deref()),
        Ctx::FNameUFunction { call } => fname_ufunction_candidates(ws, &ctx, src, call),
        Ctx::Plain => plain_candidates(ws, &ctx, prefix.as_deref()),
    };
    finish(out, prefix.as_deref())
}

// ---------------------------------------------------------------------------
// 语境判定
// ---------------------------------------------------------------------------

enum Ctx<'t> {
    /// 注释 / 字符串 / f-string 文本段 / 说明符宏参数 / FName 字面量（M5c 接入）
    None,
    /// `X.` 之后（光标在 object 结束之后；property 可能缺失或部分输入）
    Member { object: Node<'t> },
    /// `A::` 之后（scope 段完整）
    Scoped { scope: Node<'t> },
    /// 调用实参位置（裸标识符 ∪ 命名实参）。`callee` / `args` 分开携带：
    /// 正常形态来自 call_expression 的字段，错误恢复形态（未闭合实参表被
    /// ERROR 吞掉）来自 ERROR 的直接孩子。
    CallArg { callee: Node<'t>, args: Node<'t> },
    /// 说明符宏参数位（`UCLASS(` / `UPROPERTY(Ca|` …）。M5c。
    Specifier { macro_name: &'static str },
    /// FName 字面量在 `AddUFunction(this, n"|")` 第 2 实参位（M5c）：
    /// 候选 = 接收者类型的 UFUNCTION 名单。
    FNameUFunction { call: Node<'t> },
    /// 裸标识符位置
    Plain,
}

/// 光标处最深节点（含 token；end 含端点——光标紧跟 token 之后也算命中，
/// 这是 `X.` 之后判 Member 的关键）。
///
/// **同行空白左偏**：光标落在构造结束后的空白里（`", "` / `". "` 尾部）
/// 时取最后一个结束于光标前的孩子——语境归它（调用实参位 / 成员位）。
/// 跨行不偏（gap 含换行）：语句间空行的语境归外层块，避免上一语句的
/// 局部泄漏到无关位置。
fn deepest_at<'t>(root: Node<'t>, byte: u32, src: &str) -> Option<Node<'t>> {
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

/// 光标处正在输入的标识符前缀（identifier 包含光标、以光标为尾）。
/// FName 字面量（`n"Fo|"`）取引号内光标前文本（M5c UFUNCTION 名单语境）。
fn prefix_at(node: Node<'_>, src: &str, byte: u32) -> Option<String> {
    let (start, end) = if node.kind() == "name_literal" {
        // 首 '"' 之后到字面量尾
        let open = src[node.start_byte()..].find('"')? + node.start_byte() + 1;
        (open, node.end_byte())
    } else if node.kind() == "identifier" {
        (node.start_byte(), node.end_byte())
    } else {
        return None;
    };
    let start = start as u32;
    let end = end as u32;
    if byte < start || byte > end {
        return None;
    }
    let s = &src[start as usize..byte as usize];
    (!s.is_empty()).then(|| s.to_string())
}

/// 从光标处最深节点沿祖先链判定语境（内层优先）。
fn detect_from<'t>(node: Node<'t>, src: &str, byte: u32) -> Ctx<'t> {
    let mut cur = Some(node);
    while let Some(n) = cur {
        match n.kind() {
            // 噪声语境：不给候选
            "comment" | "preproc_line" | "string_literal" | "heredoc_string"
            | "format_string_content" | "format_spec" => return Ctx::None,
            // FName 字面量：仅 `AddUFunction(this, n"|")` 第 2 实参位给
            // UFUNCTION 名单（M5c——限定语境，非任何 n"" 都给）
            "name_literal" => {
                if let Some(call) = enclosing_call_of(n) {
                    if let Some(f) = call.child_by_field_name("function") {
                        if f.kind() == "identifier"
                            && syntax::text(f, src) == "AddUFunction"
                            && is_second_string_arg(call, n)
                        {
                            return Ctx::FNameUFunction { call };
                        }
                    }
                }
                return Ctx::None;
            }
            // 说明符宏参数：说明符名位 / 实参表位上溯到 *_specifiers 节点给
            // schema 候选（M5c）；**值位**（macro_value——字符串/数字）不给。
            // 引擎消费的四种宏给表（ustruct/umeta 无消费——specifiers.rs 取证）
            "macro_value" => return Ctx::None,
            "macro_argument" | "macro_argument_list" => {}
            k if k.ends_with("_specifiers") => {
                return match crate::specifiers::macro_of_specifier_node(k) {
                    Some(m) => Ctx::Specifier { macro_name: m },
                    None => Ctx::None,
                };
            }
            // 错误恢复形态：ERROR 吞掉了中途编辑的构造（缺 property 的成员
            // 访问 / 未闭合实参表）——按直接孩子归约
            "ERROR" => {
                if let Some(c) = recover_error_ctx(n, byte) {
                    return c;
                }
            }
            // 实参位置：裸标识符 ∪ 命名实参
            "argument_list" => {
                if let Some(call) = n.parent() {
                    if call.kind() == "call_expression" {
                        if let Some(callee) = call.child_by_field_name("function") {
                            return Ctx::CallArg { callee, args: n };
                        }
                    }
                }
            }
            // `X.`：光标严格在 object 结束之后（= 或越过 '.'）
            "member_expression" => {
                if let Some(o) = n.child_by_field_name("object") {
                    if (o.end_byte() as u32) < byte {
                        return Ctx::Member { object: o };
                    }
                }
            }
            // `A::`：光标在 scope 段结束之后
            "qualified_identifier" => {
                if let Some(s) = n.child_by_field_name("scope") {
                    if (s.end_byte() as u32) < byte {
                        return Ctx::Scoped { scope: s };
                    }
                }
            }
            _ => {}
        }
        cur = n.parent();
    }
    let _ = src;
    Ctx::Plain
}

/// ERROR 节点的归约（中途编辑形态，语料实测）：
/// - `[expr, "."]`：成员访问缺 property → Member（object = "." 之前的具名孩子）；
/// - `[callee, "(", argument...]`：调用未闭合 → CallArg（callee = "(" 之前的
///   具名孩子，args 即 ERROR 本身——argument 孩子直接挂其下）。
fn recover_error_ctx<'t>(err: Node<'t>, byte: u32) -> Option<Ctx<'t>> {
    let children = syntax::children_with_fields(err);
    // 形态 A：成员访问缺 property（"." 是直接孩子且光标在其后）
    let dot = children
        .iter()
        .rev()
        .find(|(_, c)| !c.is_named() && c.kind() == ".");
    if let Some((_, dot)) = dot {
        if (dot.end_byte() as u32) <= byte {
            // object = "." 之前最近的具名孩子（<=：紧邻形态 `Unknown.`）
            let obj = children
                .iter()
                .rev()
                .find(|(_, c)| c.is_named() && c.end_byte() <= dot.start_byte());
            if let Some((_, o)) = obj {
                return Some(Ctx::Member { object: *o });
            }
        }
    }
    // 形态 C（先于 B——宏 token 特征更强，否则宏的 "(" 会被当调用）：
    // 宏参数中途（`UCLASS(Pla` 未闭合——uclass_specifiers 未形成，宏 token
    // 与 "(" 是 ERROR 直接孩子）。光标在宏的 "(" 之后 → Specifier。
    // 宏名 token 的 kind 即字面量文本（UCLASS/UFUNCTION/UPROPERTY/UENUM；
    // USTRUCT/UMETA 引擎无消费不给——specifiers.rs 取证）
    for (_, c) in children.iter() {
        let macro_name = match c.kind() {
            "UCLASS" => "UCLASS",
            "UFUNCTION" => "UFUNCTION",
            "UPROPERTY" => "UPROPERTY",
            "UENUM" => "UENUM",
            _ => continue,
        };
        if let Some((_, lp)) = children
            .iter()
            .skip_while(|(_, x)| x.id() != c.id())
            .skip(1)
            .find(|(_, x)| !x.is_named() && x.kind() == "(")
        {
            if byte > lp.start_byte() as u32 {
                return Some(Ctx::Specifier { macro_name });
            }
        }
    }
    // 形态 B：调用未闭合（"(" 是直接孩子、callee 在其前、光标在 "(" 后）
    let lp = children
        .iter()
        .find(|(_, c)| !c.is_named() && c.kind() == "(");
    if let Some((_, lp)) = lp {
        if byte > lp.start_byte() as u32 {
            let callee = children
                .iter()
                .find(|(_, c)| c.is_named() && c.end_byte() <= lp.start_byte());
            if let Some((_, c)) = callee {
                return Some(Ctx::CallArg { callee: *c, args: err });
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Member：`X.` 成员补全
// ---------------------------------------------------------------------------

fn member_candidates(
    ws: &Workspace,
    ctx: &SemCtx,
    src: &str,
    object: Node<'_>,
) -> Vec<Candidate> {
    // 接收者定型（宁缺毋假）：不可定型不给候选
    let Some(recv) = expr_type(ws, ctx, src, object) else {
        return Vec::new();
    };
    let space = member_search_space(ws, recv.base);
    let mut out = Vec::new();
    let mut seen: HashSet<Sym> = HashSet::new();

    // 1. 真实成员（近者在前：子类遮蔽父类；opXxx / 构造 / notCallable 不补）
    for &t in &space {
        extend_member_candidates(ws, t, &mut seen, &mut out);
    }
    // 2. 合成成员（B4）：仅 delegate/event 接收者（Execute / Broadcast /
    //    ctor / opAssign）。class 的 StaticClass 是静态语境成员，不进
    //    `X.` 实例补全（与旧架构一致——它挂在合成 namespace 下）。
    if matches!(ws.decl(&recv.base).kind, DefKind::Delegate | DefKind::Event) {
        for m in ws.synthetic_members(&recv.base) {
            if matches!(m.kind, DefKind::Constructor | DefKind::Destructor | DefKind::Operator) {
                continue; // ctor / opAssign 不进补全（与真实成员同规则）
            }
            if !seen.insert(m.name) {
                continue;
            }
            out.push(synthetic_candidate(ws, &m, HINT_MEMBER));
        }
    }
    // 3. 访问器折叠（反向默认：不带 NOT_PROPERTY 即候选，§2.4.5）
    fold_accessors(ws, &space, &seen, &mut out);
    // 4. mixin（显式接收者准入：无 scope 限定 ✓；沿闭包查名字倒排 =
    //    DerivesOrShadows，D23 翻案后的名字键语义）
    extend_mixin_candidates(ws, recv.base, &ctx.ns_syms, HINT_MEMBER + 2, &mut out);
    out
}

/// 一个成员空间的全量候选（去重键 = 名字；NOT_CALLABLE / Operator /
/// Constructor / Destructor 不进补全）。
fn extend_member_candidates(
    ws: &Workspace,
    owner: DeclRef,
    seen: &mut HashSet<Sym>,
    out: &mut Vec<Candidate>,
) {
    for m in ws.members(&owner) {
        let d = ws.decl(&m);
        if d.flags.contains(DefFlags::NOT_CALLABLE) {
            continue;
        }
        if matches!(
            d.kind,
            DefKind::Constructor | DefKind::Destructor | DefKind::Operator
        ) {
            continue;
        }
        if !seen.insert(d.name) {
            continue; // 近层遮蔽
        }
        out.push(def_candidate(ws, m, HINT_MEMBER));
    }
}

/// `Get*/Set*`（无 NOT_PROPERTY）折叠为属性项；与真实成员同名（含跨层
/// 遮蔽）则跳过。detail 显示读写两侧的原签名。
fn fold_accessors(ws: &Workspace, space: &[DeclRef], seen: &HashSet<Sym>, out: &mut Vec<Candidate>) {
    let mut folded: HashSet<Sym> = HashSet::new();
    for &t in space {
        for m in ws.members(&t) {
            let d = ws.decl(&m);
            if d.flags.contains(DefFlags::NOT_PROPERTY)
                || !matches!(d.kind, DefKind::Method | DefKind::Function)
            {
                continue;
            }
            let name = sym_str(d.name);
            let Some(rest) = name.strip_prefix("Get").or_else(|| name.strip_prefix("Set"))
            else { continue };
            if rest.is_empty() || !rest.as_bytes()[0].is_ascii_uppercase() {
                continue;
            }
            let prop = intern_sym(rest);
            // 真实成员优先（字段 / 方法已列）；同属性多访问器只出一项
            if seen.contains(&prop) || !folded.insert(prop) {
                continue;
            }
            let get = format!("{}{}", "Get", rest);
            out.push(Candidate {
                label: rest.to_string(),
                kind: CandidateKind::Property,
                detail: Some(format!("{get}(...) / Set{rest}(...)")),
                insert: None,
                sort_hint: HINT_MEMBER + 1,
            });
        }
    }
}

/// mixin 候选（沿接收者闭包逐级查名字倒排 `mixin_by_name`；ns 准入同
/// resolve::mixin_candidates——全局 mixin 任何链可见，ns 内 mixin 须在
/// 当前位置的 ns 链上）。hint 由调用方给（Member / Plain 两场景层位不同）。
fn extend_mixin_candidates(
    ws: &Workspace,
    recv: DeclRef,
    ns_syms: &[Sym],
    hint: u8,
    out: &mut Vec<Candidate>,
) {
    let mut chain = vec![recv];
    if let Some(cl) = ws.closures.get(&recv) {
        chain.extend(cl.iter().copied());
    }
    let mut seen: HashSet<DeclRef> = HashSet::new();
    for base in chain {
        let base_name = ws.decl(&base).name;
        let Some(ms) = ws.agg.mixin_by_name.get(&base_name) else { continue };
        for &m in ms {
            if !seen.insert(m) {
                continue;
            }
            let ns_ok = match ws.parent_of(&m) {
                None => true,
                Some(ns) => ns_syms.contains(&ws.decl(&ns).name),
            };
            if ns_ok {
                out.push(def_candidate(ws, m, hint));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Scoped：`A::` 命名空间 / enum / Super
// ---------------------------------------------------------------------------

fn scoped_candidates(
    ws: &Workspace,
    ctx: &SemCtx,
    src: &str,
    scope: Node<'_>,
) -> Vec<Candidate> {
    let scope_text = syntax::text(scope, src);

    // `Super::` → 直接父类成员（§4.5 第 0 级）
    if scope_text == "Super" || scope_text == "super" {
        if let Some(base) = ctx
            .type_def
            .and_then(|t| ws.closures.get(&t).and_then(|c| c.first().copied()))
        {
            let space = member_search_space(ws, base);
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for &t in &space {
                extend_member_candidates(ws, t, &mut seen, &mut out);
            }
            fold_accessors(ws, &space, &seen, &mut out);
            return out;
        }
        return Vec::new();
    }

    let sym = intern_sym(scope_text);
    let mut out = Vec::new();
    let mut seen: HashSet<Sym> = HashSet::new();
    // B3：namespace 聚合成员（跨文件）+ class 兼任 namespace 的 StaticClass
    //（B4）+ enum 值（`EColor::`——裸值不可用，asEP_REQUIRE_ENUM_SCOPE=1）。
    // struct 兼任 namespace 但无合成成员（无 UClass）。
    for nsdef in ws.namespaces_named(sym) {
        match ws.decl(&nsdef).kind {
            DefKind::Enum => {
                extend_member_candidates(ws, nsdef, &mut seen, &mut out);
            }
            DefKind::Namespace => {
                extend_member_candidates(ws, nsdef, &mut seen, &mut out);
            }
            _ => {
                // class / struct：只合成成员（StaticClass），不列实例成员
                for m in ws.synthetic_members(&nsdef) {
                    if seen.insert(m.name) {
                        out.push(synthetic_candidate(ws, &m, HINT_MEMBER));
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// CallArg：命名实参 ∪ 裸标识符全集
// ---------------------------------------------------------------------------

fn call_arg_candidates(
    ws: &Workspace,
    ctx: &SemCtx,
    src: &str,
    callee: Node<'_>,
    args: Node<'_>,
    prefix: Option<&str>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    // 命名实参：callee 重载组形参并集（跳过 InArgN 占位 / 已提供名）
    if let Some(defs) = callee_overloads(ws, ctx, src, callee) {
        let provided: HashSet<Sym> = provided_named_args(args, src, prefix);
        let mut seen: HashSet<Sym> = HashSet::new();
        for &d in &defs {
            let RawExtra::Callable { params, .. } = &ws.decl(&d).extra else { continue };
            for p in params {
                // InArgN 是占位而非真名（架构设计 §2.4.6 硬性）——
                // UNNAMED_PARAM 索引期已标记（syntax::param_decls）
                if p.flags.contains(DefFlags::UNNAMED_PARAM) {
                    continue;
                }
                if provided.contains(&p.name) || !seen.insert(p.name) {
                    continue;
                }
                let label = sym_str(p.name).to_string();
                // 有前缀时先过滤（含命名实参层——前缀通常就是它的前缀）
                if let Some(pfx) = prefix {
                    if !label.to_lowercase().starts_with(&pfx.to_lowercase()) {
                        continue;
                    }
                }
                let ty = p.ty.as_ref().map(hover::render_syn).unwrap_or_else(|| "?".into());
                out.push(Candidate {
                    label: label.clone(),
                    kind: CandidateKind::NamedArg,
                    detail: Some(format!("{ty} {label}")),
                    // 语料主导 `=` 形态（Duration=5.0；grammar 双形态皆收）
                    insert: Some(format!("{label}=")),
                    sort_hint: HINT_NAMED_ARG,
                });
            }
        }
    }
    // 裸标识符全集（局部 / 成员 / ns / 全局；hint 顺延）
    out.extend(plain_universe(ws, ctx, prefix, /*detail_globs=*/ prefix.is_some(), HINT_NAMED_ARG + 1));
    out
}

/// 调用 callee 的重载组（不消歧——命名实参要的是形参并集）。
/// `callee` 是调用函数段节点（identifier / member_expression；错误恢复
/// 形态下由 recover_error_ctx 从 ERROR 直接孩子中取出）。
fn callee_overloads(
    ws: &Workspace,
    ctx: &SemCtx,
    src: &str,
    callee: Node<'_>,
) -> Option<Vec<DeclRef>> {
    let f = callee;
    match f.kind() {
        "identifier" => {
            let name = intern_sym(syntax::text(f, src));
            let res = resolve_callee(ws, ctx, name, 0)?;
            let defs: Vec<DeclRef> = res
                .targets
                .iter()
                .filter_map(|t| match t {
                    Target::Def(r) => Some(*r),
                    Target::Synthetic(_) | Target::Local(_) => None,
                })
                .collect();
            (!defs.is_empty()).then_some(defs)
        }
        "member_expression" => {
            let recv = expr_type(ws, ctx, src, f.child_by_field_name("object")?)?;
            let prop = f.child_by_field_name("property")?;
            let name = intern_sym(syntax::text(prop, src));
            let space = member_search_space(ws, recv.base);
            let mut cands = crate::resolve::members_named(ws, &space, name, |d| {
                matches!(d.kind, DefKind::Method | DefKind::Function | DefKind::Operator)
            });
            if cands.is_empty() {
                cands = crate::resolve::find_accessors(ws, &space, name);
            }
            (!cands.is_empty()).then_some(cands)
        }
        _ => None,
    }
}

/// 本次调用已提供的命名实参名（光标所在的那条不算——正在输入）。
/// `args` 是 argument_list 或吞掉未闭合调用的 ERROR：named_argument 可能
/// 直接挂其下，也可能包在 argument 包装节点里（正常形态）。
fn provided_named_args(args: Node<'_>, src: &str, prefix: Option<&str>) -> HashSet<Sym> {
    let mut out = HashSet::new();
    let push_if = |name_node: Node<'_>, out: &mut HashSet<Sym>| {
        // 正在输入的这条（前缀正是它的部分文本）不算已提供
        if let Some(p) = prefix {
            let t = syntax::text(name_node, src);
            if t.starts_with(p) || p.starts_with(t) {
                return;
            }
        }
        out.insert(intern_sym(syntax::text(name_node, src)));
    };
    for (_f, child) in syntax::children_with_fields(args) {
        match child.kind() {
            "named_argument" => {
                if let Some(n) = child.child_by_field_name("name") {
                    push_if(n, &mut out);
                }
            }
            // 正常形态：argument 包装内是 named_argument
            "argument" => {
                for (_f2, inner) in syntax::children_with_fields(child) {
                    if inner.kind() == "named_argument" {
                        if let Some(n) = inner.child_by_field_name("name") {
                            push_if(n, &mut out);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Specifier：说明符 schema（M5c）
// ---------------------------------------------------------------------------

fn specifier_candidates(macro_name: &'static str, prefix: Option<&str>) -> Vec<Candidate> {
    crate::specifiers::specifiers_of(macro_name)
        .iter()
        .filter(|s| prefix_match(s.name, prefix))
        .map(|s| Candidate {
            label: s.name.to_string(),
            kind: CandidateKind::Keyword,
            detail: Some(s.doc.to_string()),
            // 带值说明符补 `Name=`（值由用户续写）；无值说明符与 label 同，
            // 无需特殊插入
            insert: s.takes_value.then(|| format!("{}=", s.name)),
            sort_hint: 0,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// FName：AddUFunction 第 2 实参的 UFUNCTION 名单（M5c）
// ---------------------------------------------------------------------------

/// name_literal 所属的 call_expression（其 argument 包装的父链上溯）。
fn enclosing_call_of<'t>(lit: Node<'t>) -> Option<Node<'t>> {
    let mut cur = lit.parent();
    while let Some(p) = cur {
        if p.kind() == "call_expression" {
            return Some(p);
        }
        if p.kind() != "argument" {
            return None; // 越过 argument 还没到 call：不是调用实参
        }
        cur = p.parent();
    }
    None
}

/// 该 name_literal 是否为调用的第 2 个实参（argument 顺序计数，
/// `AddUFunction(this, n"|", ...)` 的函数名位）。
fn is_second_string_arg(call: Node<'_>, lit: Node<'_>) -> bool {
    let Some(args) = call.child_by_field_name("arguments") else { return false };
    syntax::children_with_fields(args)
        .into_iter()
        .filter(|(_, c)| c.kind() == "argument")
        .nth(1)
        .is_some_and(|(_, a)| {
            // 字面量直接是 argument 的孩子；或经 named_argument 包装（不常见）
            lit.parent().is_some_and(|p| p.id() == a.id())
                || lit.parent().is_some_and(|p| {
                    p.parent().is_some_and(|gp| gp.id() == a.id())
                })
        })
}

/// UFUNCTION 名单：接收者（第 1 实参定型，通常 this）类型的成员中——
/// 脚本侧 `UFUNCTION()` 宏（SCRIPT_UFUNCTION flag）∪ `.d.as` 侧
/// `@ufunction`/`@event` tag（TagKind::UFunction/Event）——的方法名。
fn fname_ufunction_candidates(
    ws: &Workspace,
    ctx: &SemCtx,
    src: &str,
    call: Node<'_>,
) -> Vec<Candidate> {
    // callee 必须是 AddUFunction（n"" 其它场景不给——限定语境）
    let Some(f) = call.child_by_field_name("function") else { return Vec::new() };
    if f.kind() != "identifier" || syntax::text(f, src) != "AddUFunction" {
        return Vec::new();
    }
    // 接收者：第 1 实参定型（this → 所在类）；失败回落 ctx.type_def
    let args = syntax::children_with_fields(call)
        .into_iter()
        .filter(|(_, c)| c.kind() == "argument")
        .map(|(_, c)| c)
        .collect::<Vec<_>>();
    let recv = args
        .first()
        .and_then(|a| expr_type(ws, ctx, src, *a).map(|t| t.base))
        .or(ctx.type_def);
    let Some(recv) = recv else { return Vec::new() };
    let space = member_search_space(ws, recv);
    let mut out = Vec::new();
    let mut seen: HashSet<Sym> = HashSet::new();
    for &t in &space {
        for m in ws.members(&t) {
            let d = ws.decl(&m);
            if d.kind != DefKind::Method && d.kind != DefKind::Function {
                continue;
            }
            let is_ufunc = d.flags.contains(DefFlags::SCRIPT_UFUNCTION)
                || d.tags.iter().any(|tag| {
                    matches!(
                        tag.kind,
                        crate::decl_tags::TagKind::UFunction | crate::decl_tags::TagKind::Event
                    )
                });
            if is_ufunc && seen.insert(d.name) {
                out.push(def_candidate(ws, m, 0));
            }
        }
    }
    out
}

const HINT_NAMED_ARG: u8 = 10;
const HINT_MEMBER: u8 = 20;

fn plain_candidates(ws: &Workspace, ctx: &SemCtx, prefix: Option<&str>) -> Vec<Candidate> {
    plain_universe(ws, ctx, prefix, /*detail_globs=*/ prefix.is_some(), HINT_MEMBER)
}

/// 裸标识符全集：局部 → 隐式 this 成员（含折叠/mixin）→ ns 链 → 全局/类型
/// → 关键字。`base_hint` 是局部层的起始 hint（CallArg 场景顺延）。
fn plain_universe(
    ws: &Workspace,
    ctx: &SemCtx,
    prefix: Option<&str>,
    detail_globs: bool,
    base_hint: u8,
) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();

    // 0. 局部（参数 + 块内；内层与外层同名都给，排序自然靠前）
    for l in &ctx.locals {
        let label = sym_str(l.name).to_string();
        if !prefix_match(&label, prefix) {
            continue;
        }
        out.push(Candidate {
            label,
            kind: if l.kind == DefKind::Param { CandidateKind::Param } else { CandidateKind::LocalVar },
            detail: Some(hover::signature(ws, &Target::Local(l.clone())).unwrap_or_default()),
            insert: None,
            sort_hint: base_hint,
        });
    }

    // 1. 隐式 this 成员（方法体内；引擎在类语境同样按成员解析，包括 default 块）
    if let Some(t) = ctx.type_def {
        let space = member_search_space(ws, t);
        let mut seen: HashSet<Sym> = HashSet::new();
        let mut members = Vec::new();
        for &u in &space {
            extend_member_candidates_filtered(ws, u, &mut seen, &mut members, prefix);
        }
        fold_accessors(ws, &space, &seen, &mut members);
        for mut c in members {
            c.sort_hint = base_hint + 1;
            out.push(c);
        }
        // mixin（隐式 this 准入：方法体内引擎自动压 this，§4.5.1 条 2）
        extend_mixin_candidates(ws, t, &ctx.ns_syms, base_hint + 1, &mut out);
    }

    // 2. ns 链成员（逐级回退，近者在前）
    for &ns in ctx.ns_defs.iter().rev() {
        let sym = ws.decl(&ns).name;
        for nsdef in ws.namespaces_named(sym) {
            let mut seen: HashSet<Sym> = HashSet::new();
            let mut members = Vec::new();
            extend_member_candidates_filtered(ws, nsdef, &mut seen, &mut members, prefix);
            for mut c in members {
                c.sort_hint = base_hint + 2;
                out.push(c);
            }
        }
    }

    // 3. 全局符号 + 类型（agg.main 全表；有前缀先按名过滤，无前缀不带 detail）
    for (sym, defs) in ws.agg.main.iter() {
        let label = sym_str(*sym);
        if !prefix_match(label, prefix) {
            continue;
        }
        // 该名字下的候选：取首个可见者（局部/成员层未覆盖时才落到这——
        // finish 的按 label 去重会保 hint 更小者）
        let mut picked: Option<(DeclRef, CandidateKind)> = None;
        for &r in defs {
            let d = ws.decl(&r);
            if d.flags.contains(DefFlags::NOT_CALLABLE) {
                continue;
            }
            let kind = match d.kind {
                DefKind::Class => CandidateKind::Class,
                DefKind::Struct => CandidateKind::Struct,
                DefKind::Enum => CandidateKind::Enum,
                DefKind::EnumValue => CandidateKind::EnumValue,
                DefKind::Namespace => CandidateKind::Namespace,
                DefKind::Delegate | DefKind::Event => CandidateKind::Delegate,
                DefKind::Function => CandidateKind::Function,
                DefKind::GlobalVar | DefKind::AssetDecl => CandidateKind::GlobalVar,
                // 成员类符号不进裸标识符全局层（局部/成员/ns 层已覆盖）
                DefKind::Method
                | DefKind::Field
                | DefKind::Constructor
                | DefKind::Destructor
                | DefKind::Operator
                | DefKind::VirtualProperty => continue,
                DefKind::Param | DefKind::LocalVar | DefKind::TypeParam | DefKind::Module => {
                    continue
                }
            };
            // local 函数：跨模块不可见（visible_global 同语义；模块信息缺失
            // 时不过滤）
            if d.flags.contains(DefFlags::LOCAL) {
                match (ws.module_of(r.file), ws.module_of(ctx.file)) {
                    (Some(a), Some(b)) if a != b => continue,
                    _ => {}
                }
            }
            match picked {
                None => picked = Some((r, kind)),
                // 类型声明优先于其它（同名时补全里类型更有辨识度）
                Some((_, CandidateKind::Class | CandidateKind::Struct | CandidateKind::Enum | CandidateKind::Delegate)) => {}
                Some((pid, pk)) => {
                    if matches!(kind, CandidateKind::Class | CandidateKind::Struct | CandidateKind::Enum | CandidateKind::Delegate)
                        && !matches!(pk, CandidateKind::Class | CandidateKind::Struct | CandidateKind::Enum | CandidateKind::Delegate)
                    {
                        picked = Some((r, kind));
                        let _ = pid;
                    }
                }
            }
        }
        if let Some((r, kind)) = picked {
            out.push(Candidate {
                label: label.to_string(),
                kind,
                detail: if detail_globs {
                    hover::signature(ws, &Target::Def(r))
                } else {
                    None
                },
                insert: None,
                sort_hint: base_hint + 3,
            });
        }
    }

    // 4. 关键字（无前缀时也全给——量级可控）
    for kw in KEYWORDS {
        if !prefix_match(kw, prefix) {
            continue;
        }
        out.push(Candidate {
            label: (*kw).to_string(),
            kind: CandidateKind::Keyword,
            detail: None,
            insert: None,
            sort_hint: base_hint + 4,
        });
    }
    out
}

/// 有前缀时大小写不敏感的前缀匹配；无前缀恒真。
fn prefix_match(label: &str, prefix: Option<&str>) -> bool {
    match prefix {
        Some(p) if !p.is_empty() => {
            let lp = p.to_lowercase();
            label.to_lowercase().starts_with(&lp)
        }
        _ => true,
    }
}

/// [`extend_member_candidates`] 的带前缀版本（先按名过滤，detail 只对幸存者
/// 渲染——无前缀时 member 层带 detail，量级 = 单类型成员数，可承受）。
fn extend_member_candidates_filtered(
    ws: &Workspace,
    owner: DeclRef,
    seen: &mut HashSet<Sym>,
    out: &mut Vec<Candidate>,
    prefix: Option<&str>,
) {
    for m in ws.members(&owner) {
        let d = ws.decl(&m);
        if d.flags.contains(DefFlags::NOT_CALLABLE) {
            continue;
        }
        if matches!(
            d.kind,
            DefKind::Constructor | DefKind::Destructor | DefKind::Operator
        ) {
            continue;
        }
        if !seen.insert(d.name) {
            continue;
        }
        let label = sym_str(d.name);
        if !prefix_match(label, prefix) {
            continue;
        }
        out.push(def_candidate(ws, m, 0)); // hint 由调用方改写
    }
}

// ---------------------------------------------------------------------------
// 公共构件
// ---------------------------------------------------------------------------

fn def_candidate(ws: &Workspace, r: DeclRef, sort_hint: u8) -> Candidate {
    let d = ws.decl(&r);
    Candidate {
        label: sym_str(d.name).to_string(),
        kind: kind_of(d.kind),
        detail: hover::signature(ws, &Target::Def(r)),
        insert: None,
        sort_hint,
    }
}

/// 合成成员候选（B4）：签名经 hover::signature(Target::Synthetic) 同源渲染。
fn synthetic_candidate(ws: &Workspace, m: &crate::workspace::SyntheticMember, sort_hint: u8) -> Candidate {
    Candidate {
        label: sym_str(m.name).to_string(),
        kind: kind_of(m.kind),
        detail: hover::signature(ws, &Target::Synthetic(m.clone())),
        insert: None,
        sort_hint,
    }
}

fn kind_of(kind: DefKind) -> CandidateKind {
    match kind {
        DefKind::Class => CandidateKind::Class,
        DefKind::Struct => CandidateKind::Struct,
        DefKind::Enum => CandidateKind::Enum,
        DefKind::EnumValue => CandidateKind::EnumValue,
        DefKind::Namespace => CandidateKind::Namespace,
        DefKind::Module => CandidateKind::Namespace,
        DefKind::Delegate | DefKind::Event => CandidateKind::Delegate,
        DefKind::Function => CandidateKind::Function,
        DefKind::Method => CandidateKind::Method,
        DefKind::Constructor | DefKind::Destructor | DefKind::Operator => CandidateKind::Method,
        DefKind::GlobalVar | DefKind::AssetDecl => CandidateKind::GlobalVar,
        DefKind::Field => CandidateKind::Field,
        DefKind::Param => CandidateKind::Param,
        DefKind::LocalVar => CandidateKind::LocalVar,
        DefKind::TypeParam => CandidateKind::Struct,
        DefKind::VirtualProperty => CandidateKind::Property,
    }
}

/// 排序 + 按 label 去重（保 hint 最小者 = 近层遮蔽）+ 封顶。
/// 重载组只出一项（同名同层取首个——signatureHelp 展示完整重载）。
fn finish(mut out: Vec<Candidate>, prefix: Option<&str>) -> Vec<Candidate> {
    if let Some(p) = prefix {
        if !p.is_empty() {
            let lp = p.to_lowercase();
            out.retain(|c| c.label.to_lowercase().starts_with(&lp));
        }
    }
    out.sort_by(|a, b| a.sort_hint.cmp(&b.sort_hint).then_with(|| a.label.cmp(&b.label)));
    out.dedup_by(|a, b| a.label == b.label);
    out.truncate(MAX_CANDIDATES);
    out
}

/// 关键字表（grammar token 集实用子集：类型 / 字面量 / 声明 / 控制流 /
/// 方法属性。运算符与标点不进）。
const KEYWORDS: &[&str] = &[
    // 类型
    "void", "bool", "int", "int8", "int16", "int32", "int64", "uint", "uint8", "uint16",
    "uint32", "uint64", "float", "float32", "float64", "double", "auto",
    // 字面量 / 特殊名
    "true", "false", "null", "nullptr", "this", "Super", "Cast",
    // 声明
    "class", "struct", "enum", "namespace", "delegate", "event", "asset", "mixin", "local",
    "default",
    // 修饰
    "const", "protected", "final", "override", "property", "no_discard",
    // 控制流
    "if", "else", "for", "while", "do", "switch", "case", "break", "continue", "return",
    "fallthrough",
];

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

    fn off(src: &str, needle: &str) -> u32 {
        src.find(needle).unwrap_or_else(|| panic!("定位标记缺失: {needle}")) as u32
    }

    /// needle 后偏移 n 字节（跳过前缀落在补全锚点上）。
    fn at(src: &str, needle: &str, n: usize) -> u32 {
        off(src, needle) + n as u32
    }

    fn file_of(path: &str) -> FileId {
        intern_file(path, 0)
    }

    fn labels(cands: &[Candidate]) -> Vec<String> {
        cands.iter().map(|c| c.label.clone()).collect()
    }

    // ------------------------------------------------------------------
    // `X.` 成员补全
    // ------------------------------------------------------------------

    #[test]
    fn member_completion_includes_fields_and_methods() {
        const SRC: &str = "\
class ABase
{
    int BaseField;
    void BaseFn() {}
}
class CDerived : ABase
{
    int DerivedField;
    void DerivedFn() {}
    void M()
    {
        CDerived D;
        D.
    }
}
";
        let ws = build(&[("unique://cmp/member.as", SRC)]);
        let file = file_of("unique://cmp/member.as");
        // 光标在 "D." 之后（'.' 的下一字节）
        let cands = complete_at(&ws, file, at(SRC, "D.", 2));
        let ls = labels(&cands);
        for want in ["BaseField", "BaseFn", "DerivedField", "DerivedFn"] {
            assert!(ls.contains(&want.to_string()), "应含 {want}: {ls:?}");
        }
        // opXxx / 构造不进补全
        assert!(!ls.iter().any(|l| l.starts_with("op")), "opXxx 不补: {ls:?}");
    }

    #[test]
    fn member_completion_accessor_folding() {
        // 访问器折叠：GetHealth/SetHealth → Health 属性项；带 @notProperty 不折
        const SRC: &str = "\
class C
{
    int GetHealth() { return 1; }
    void SetHealth(int V) {}
    // @notProperty
    int GetMana() { return 2; }
    void M()
    {
        C C2;
        C2.
    }
}
";
        let ws = build(&[("unique://cmp/fold.as", SRC)]);
        let file = file_of("unique://cmp/fold.as");
        let cands = complete_at(&ws, file, at(SRC, "C2.", 3));
        let ls = labels(&cands);
        assert!(ls.contains(&"Health".to_string()), "访问器折叠出 Health: {ls:?}");
        // GetMana 带 @notProperty：不折叠，但方法本体照列
        assert!(!ls.contains(&"Mana".to_string()), "notProperty 不折叠: {ls:?}");
        assert!(ls.contains(&"GetMana".to_string()), "方法本体照列: {ls:?}");
    }

    #[test]
    fn member_completion_untypable_receiver_empty() {
        // 接收者不可定型（未知标识符）→ 宁缺毋假，不给候选
        const SRC: &str = "\
void F()
{
    Unknown.
}
";
        let ws = build(&[("unique://cmp/unk.as", SRC)]);
        let file = file_of("unique://cmp/unk.as");
        let cands = complete_at(&ws, file, at(SRC, "Unknown.", 8));
        assert!(cands.is_empty(), "不可定型接收者不给候选");
    }

    #[test]
    fn member_completion_delegate_synthetic_members() {
        // B4：委托接收者的 `X.` 补全含合成成员（Execute / ExecuteIfBound /
        // BindUFunction）；ctor / opAssign 不进（与真实成员同规则）
        const SRC: &str = "\
delegate void FOnHit(int Damage);
class A
{
    FOnHit OnHit;
    void M()
    {
        OnHit.
    }
}
";
        let ws = build(&[("unique://cmp/dlg.as", SRC)]);
        let file = file_of("unique://cmp/dlg.as");
        let cands = complete_at(&ws, file, at(SRC, "OnHit.", 6));
        let ls = labels(&cands);
        for want in ["Execute", "ExecuteIfBound", "BindUFunction"] {
            assert!(ls.contains(&want.to_string()), "委托合成成员 {want}: {ls:?}");
        }
        assert!(!ls.contains(&"opAssign".to_string()), "opAssign 不补: {ls:?}");
        assert!(
            !ls.iter().any(|l| l == "FOnHit"),
            "构造函数不进补全: {ls:?}"
        );
    }

    #[test]
    fn member_completion_class_receiver_no_static_class() {
        // class 实例接收者的 `X.` 补全不含 StaticClass（静态语境成员，
        // 与旧架构一致——只在 `类名::` 语境出现）
        const SRC: &str = "\
class C
{
    int Field;
    void M()
    {
        C X;
        X.
    }
}
";
        let ws = build(&[("unique://cmp/sc.as", SRC)]);
        let file = file_of("unique://cmp/sc.as");
        let cands = complete_at(&ws, file, at(SRC, "X.", 2));
        let ls = labels(&cands);
        assert!(ls.contains(&"Field".to_string()), "实例成员: {ls:?}");
        assert!(!ls.contains(&"StaticClass".to_string()), "StaticClass 不进实例补全: {ls:?}");
    }

    // ------------------------------------------------------------------
    // `A::` 命名空间 / enum / StaticClass
    // ------------------------------------------------------------------

    #[test]
    fn scoped_completion_namespace_enum_and_static_class() {
        const SRC: &str = "\
namespace FVector
{
    float64 ZeroVector;
    float64 OneVector;
}
enum EColor { Red, Green, Blue }
class AActor {}
void F()
{
    FVector::
    EColor C = EColor::
    UClass D = AActor::S
}
";
        let ws = build(&[("unique://cmp/scoped.as", SRC)]);
        let file = file_of("unique://cmp/scoped.as");
        let cands = complete_at(&ws, file, at(SRC, "FVector::", 10));
        let ls = labels(&cands);
        assert!(ls.contains(&"ZeroVector".to_string()), "ns 成员: {ls:?}");
        assert!(ls.contains(&"OneVector".to_string()), "ns 成员: {ls:?}");
        // enum 值（第 2 个 "EColor::"）
        let cands = complete_at(&ws, file, at(SRC, "EColor C = EColor::", 20));
        let ls = labels(&cands);
        for want in ["Red", "Green", "Blue"] {
            assert!(ls.contains(&want.to_string()), "enum 值 {want}: {ls:?}");
        }
        // class 兼任 namespace：StaticClass（B3/B4）——`AActor::S|`（部分
        // 输入形态；裸 `AActor::` 行尾会被 GLR 解析吞并，语境归不到 Scoped）
        let cands = complete_at(&ws, file, at(SRC, "AActor::S", 9));
        let ls = labels(&cands);
        assert_eq!(ls, vec!["StaticClass".to_string()], "AActor:: 只给 StaticClass: {ls:?}");
    }

    // ------------------------------------------------------------------
    // 命名实参（InArgN 硬性跳过）
    // ------------------------------------------------------------------

    #[test]
    fn named_arg_completion_skips_inarg_placeholders() {
        const SRC: &str = "\
void Print(const FString&in Message, float64 Duration = 0.0) {}
void Print(const FString&in Message, FName Category) {}
void F()
{
    Print(\"x\", Du
}
";
        let ws = build(&[("unique://cmp/narg.as", SRC)]);
        let file = file_of("unique://cmp/narg.as");
        // 光标在 "Du" 之后（实参位、前缀 Du；needle 7 字节 + 尾后 0 偏移）
        let cands = complete_at(&ws, file, at(SRC, "\"x\", Du", 7));
        let named: Vec<&Candidate> = cands.iter().filter(|c| c.kind == CandidateKind::NamedArg).collect();
        let ls: Vec<&str> = named.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(ls, vec!["Duration"], "命中唯一命名实参 Duration: {ls:?}");
        assert_eq!(named[0].insert.as_deref(), Some("Duration="), "插入 = 形态");
    }

    #[test]
    fn named_arg_inarg_placeholder_never_offered() {
        // 架构设计 §2.4.6 硬性：InArgN 占位不得出现在命名实参补全
        const SRC: &str = "\
void SetX(float64 InArg0, float64 InArg1, float64 Real) {}
void F()
{
    SetX(1.0, 
}
";
        let ws = build(&[("unique://cmp/inarg.as", SRC)]);
        let file = file_of("unique://cmp/inarg.as");
        // 光标在 ", " 之后（10 = needle 长度；空白左偏落回实参语境）
        let cands = complete_at(&ws, file, at(SRC, "SetX(1.0, ", 10));
        for c in &cands {
            if c.kind == CandidateKind::NamedArg {
                assert!(
                    !c.label.starts_with("InArg"),
                    "InArgN 占位不得补出: {}",
                    c.label
                );
            }
        }
        let named: Vec<&str> =
            cands.iter().filter(|c| c.kind == CandidateKind::NamedArg).map(|c| c.label.as_str()).collect();
        assert_eq!(named, vec!["Real"], "只剩真名形参: {named:?}");
    }

    #[test]
    fn named_arg_already_provided_not_repeated() {
        const SRC: &str = "\
void Print(const FString&in Message, float64 Duration = 0.0, FName Category = 0) {}
void F()
{
    Print(\"x\", Duration=1.0, 
}
";
        let ws = build(&[("unique://cmp/prov.as", SRC)]);
        let file = file_of("unique://cmp/prov.as");
        let cands = complete_at(&ws, file, at(SRC, "Duration=1.0, ", 15));
        let named: Vec<&str> =
            cands.iter().filter(|c| c.kind == CandidateKind::NamedArg).map(|c| c.label.as_str()).collect();
        assert!(!named.contains(&"Duration"), "已提供的不重复: {named:?}");
    }

    // ------------------------------------------------------------------
    // 裸标识符 / 前缀过滤 / 噪声语境
    // ------------------------------------------------------------------

    #[test]
    fn plain_completion_locals_members_and_prefix() {
        const SRC: &str = "\
class C
{
    int MemberField;
    void M(int ParamOne)
    {
        int LocalOne = 1;
        int X = Lo
    }
}
";
        let ws = build(&[("unique://cmp/plain.as", SRC)]);
        let file = file_of("unique://cmp/plain.as");
        // 前缀 Lo → LocalOne（局部层）在最前；关键字 local 同前缀属预期
        //（大小写不敏感匹配）
        let cands = complete_at(&ws, file, at(SRC, "= Lo", 4));
        let ls = labels(&cands);
        assert_eq!(ls.first(), Some(&"LocalOne".to_string()), "局部层最前: {ls:?}");
        assert!(ls.contains(&"local".to_string()), "关键字 local 同前缀: {ls:?}");
        // 无前缀：局部 + 成员 + 关键字等
        const SRC2: &str = "\
class C
{
    int MemberField;
    void M(int ParamOne)
    {
        int X = 
    }
}
";
        let ws2 = build(&[("unique://cmp/plain2.as", SRC2)]);
        let file2 = file_of("unique://cmp/plain2.as");
        let cands = complete_at(&ws2, file2, at(SRC2, "int X = ", 8));
        let ls = labels(&cands);
        for want in ["ParamOne", "MemberField", "this", "return"] {
            assert!(ls.contains(&want.to_string()), "应含 {want}: {ls:?}");
        }
    }

    #[test]
    fn noise_contexts_give_nothing() {
        const SRC: &str = "\
void F()
{
    // comment here
    FString S = \"inside string\";
}
";
        let ws = build(&[("unique://cmp/noise.as", SRC)]);
        let file = file_of("unique://cmp/noise.as");
        assert!(complete_at(&ws, file, at(SRC, "// comment", 5)).is_empty(), "注释内不补");
        assert!(complete_at(&ws, file, at(SRC, "\"inside", 3)).is_empty(), "字符串内不补");
    }

    // ------------------------------------------------------------------
    // M5c：说明符 schema / FName UFUNCTION 名单
    // ------------------------------------------------------------------

    #[test]
    fn specifier_completion_by_macro() {
        // 真实形态：宏挂声明前（UCLASS→class / UFUNCTION→function），
        // 光标在宏参数表内
        const SRC: &str = "\
UCLASS(Pla
class C
{
}
UFUNCTION(
void F() {}
";
        let ws = build(&[("unique://cmp/spec.as", SRC)]);
        let file = file_of("unique://cmp/spec.as");
        // UCLASS(Pla| ：光标在 Pla 尾端点（needle 10 字符）→ 前缀 Pla
        let cands = complete_at(&ws, file, at(SRC, "UCLASS(Pla", 10));
        let ls = labels(&cands);
        assert_eq!(ls, vec!["Placeable"], "前缀 Pla: {ls:?}");
        // UFUNCTION(| ：全表（首项按 label 序）
        let cands = complete_at(&ws, file, at(SRC, "UFUNCTION(", 10));
        let ls = labels(&cands);
        assert!(ls.contains(&"BlueprintCallable".to_string()), "UFUNCTION 表: {ls:?}");
        assert!(ls.contains(&"Meta".to_string()), "UFUNCTION 表: {ls:?}");
        assert!(!ls.contains(&"EditAnywhere".to_string()), "不串 UPROPERTY 表: {ls:?}");
    }

    #[test]
    fn specifier_value_position_empty() {
        // 宏值位（macro_value / 字符串内）不给候选
        const SRC: &str = "\
UFUNCTION(Category=\"Math\")
void F() {}
";
        let ws = build(&[("unique://cmp/specval.as", SRC)]);
        let file = file_of("unique://cmp/specval.as");
        // "Math" 字符串内
        assert!(complete_at(&ws, file, at(SRC, "\"Math", 3)).is_empty(), "宏值字符串内不给");
    }

    #[test]
    fn specifier_prefix_filter_and_insert() {
        const SRC: &str = "\
UFUNCTION(Blue
void F() {}
";
        let ws = build(&[("unique://cmp/specpfx.as", SRC)]);
        let file = file_of("unique://cmp/specpfx.as");
        // 光标在 Blue 尾端点（needle 14 字符）→ 前缀 Blue
        let cands = complete_at(&ws, file, at(SRC, "UFUNCTION(Blue", 14));
        let ls = labels(&cands);
        for want in ["BlueprintCallable", "BlueprintEvent", "BlueprintOverride", "BlueprintPure", "BlueprintProtected"] {
            assert!(ls.contains(&want.to_string()), "前缀 Blue 应含 {want}: {ls:?}");
        }
        assert!(!ls.contains(&"Category".to_string()), "Cat 不匹配 Blue 前缀: {ls:?}");
        let bc = cands.iter().find(|c| c.label == "BlueprintCallable").unwrap();
        assert_eq!(bc.insert.as_deref(), None, "无值说明符原样插入");
        let cat = crate::specifiers::specifiers_of("UFUNCTION")
            .iter()
            .find(|s| s.name == "Category")
            .unwrap();
        assert_eq!(cat.insert(), "Category=", "带值说明符补 =");
    }

    #[test]
    fn fname_ufunction_list_for_addufunction() {
        const SRC: &str = "\
class C
{
    UFUNCTION()
    void MyEvent() {}
    void PlainFn() {}
    void Bind()
    {
        AddUFunction(this, n\"My
    }
}
";
        let ws = build(&[("unique://cmp/fname.as", SRC)]);
        let file = file_of("unique://cmp/fname.as");
        // 光标在 n"My 之后（第 2 实参、前缀 My）
        let cands = complete_at(&ws, file, at(SRC, "n\"My", 4));
        let ls = labels(&cands);
        assert_eq!(ls, vec!["MyEvent"], "UFUNCTION 名单 + 前缀 My: {ls:?}");
    }

    #[test]
    fn fname_other_name_literals_empty() {
        // 非 AddUFunction 第 2 实参的 n"" 不给候选（限定语境）
        const SRC: &str = "\
void F()
{
    FName N = n\"Any
}
";
        let ws = build(&[("unique://cmp/fname2.as", SRC)]);
        let file = file_of("unique://cmp/fname2.as");
        assert!(complete_at(&ws, file, at(SRC, "n\"Any", 5)).is_empty());
    }

    #[test]
    fn call_arg_position_offers_plain_universe_too() {
        // 实参位：命名实参 + 裸标识符全集（局部可见）。
        // 声明放后面，让 needle "Print(" 首次出现即调用点
        const SRC: &str = "\
void F()
{
    int LocalVal = 1;
    Print(
}
void Print(const FString&in Message, float64 Duration = 0.0) {}
";
        let ws = build(&[("unique://cmp/carg.as", SRC)]);
        let file = file_of("unique://cmp/carg.as");
        let cands = complete_at(&ws, file, at(SRC, "Print(", 6));
        let ls = labels(&cands);
        assert!(ls.contains(&"Duration".to_string()), "命名实参: {ls:?}");
        assert!(ls.contains(&"LocalVal".to_string()), "局部也可见: {ls:?}");
    }
}
