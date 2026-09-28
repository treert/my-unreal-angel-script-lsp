//! 表达式定型管线（LSP实现规划 §4.2 / D14，D28 欠账的 M5 清偿）。
//!
//! M3/M4 的最小子集（标识符 / this / 成员链 / 调用返回 / 字面量 / Cast /
//! parenthesized）从 resolve.rs 迁入并完整化：
//! - **运算符重载**：opAdd/opMul/… 经 `overload::disambiguate` 按右操作数基名
//!   消歧（唯一命中取其返回；消歧失败时全部候选返回基一致才取——宁缺毋假）；
//!   比较 / 逻辑运算恒为 bool（引擎 opEquals/opCmp 的结果类型）；
//! - **primitive 算术的最小提升近似**：数值域取较高档（int + float → float），
//!   位运算遇浮点 → None。这是隐式转换表（架构设计 §4.3）之外的最小子集，
//!   不追引擎精确矩阵（P5 诊断期再对齐）——M5 决策记录见 docs；
//! - **f-string → FString；n"…" → FName**；数字后缀 / 进制与 D25 同规则
//!   （裸 float 宽度随 `IndexConfig`）；
//! - **range-for 双跳协议**（引擎真值 `as_compiler.cpp:5745-5873`）：
//!   `容器类型.Iterator()`（0 参 + 返回对象 + constness 匹配，非 const 优先、
//!   回退 const）→ 返回类型上找 `.Iterate()`（0 参）→ 其返回类型即元素类型
//!   （**保留引用性**）。`.d.as` 里 const 迭代器的 `Iterate` 因导出器丢 `&`
//!   规则（架构设计 §2.4.6）可能显示为值返回，两条都接受；
//! - **模板形参替换**：`TArray<FVector>` / `FVector[]` 的成员返回类型里出现
//!   模板形参（`T& Iterate()`、`T& opIndex(int)`）时，按容器声明侧实参做
//!   SynType 级符号替换（`T` → `FVector`）。这是完整模板实例化（规划 §3.3
//!   Phase 3）的最小可用子集：只在使用点替换、不克隆成员；
//! - **auto 惰性定型**：auto 局部变量 / range-for 迭代变量在 SemCtx 构建期
//!   取本管线的定型结果（请求驱动，D14 的「扫描阶段不碰定型」不变）。
//!
//! 失败一律 `None`（宁缺毋假，D14）：表达式含 ERROR / 符号未解析 / 类型未知 /
//! 运算符无匹配。调用点重载消歧失败但候选真实存在时不返回 None（取首个
//! arity 匹配者——与 M3 行为一致；候选都存在，只是选不准）。
//!
//! Phase B（D37）：后端 `WorkspaceIndex` → `Workspace`（DefId → DeclRef）；
//! 委托/事件接收者的合成方法调用（`OnHit.Execute(...)`）走查询期
//! `synthetic_named`（B4）取返回类型。

use std::collections::HashMap;

use as_syntax::tree_sitter::Node;

use crate::aggregation::DeclRef;
use crate::id::Sym;
use crate::intern::{intern_sym, sym_str};
use crate::resolve::{
    builtin_target, find_accessors, member_search_space, members_named, resolve_callee,
    resolve_plain, Resolution, SemCtx, Target,
};
use crate::summary::RawExtra;
use crate::symbol::{DefFlags, DefKind};
use crate::syntax;
use crate::types::SynType;
use crate::workspace::Workspace;

/// 表达式的定型结果：成员查找基 + 声明侧语法类型。
///
/// `base` 是成员查找的基类型（class 沿闭包 / struct 单层 / 模板落本体 /
/// primitive 落合成内建）；`syn` 保留模板实参与数组元素、引用性
/// （`TArray<FVector>` / `FVector[]` / `T&`），字面量为 None。
#[derive(Clone, Debug)]
pub(crate) struct ExprTy {
    pub base: DeclRef,
    pub syn: Option<SynType>,
}

// ---------------------------------------------------------------------------
// 主入口
// ---------------------------------------------------------------------------

/// 表达式定型（规划 §4.2 的单一原语；字节 / 节点粒度，无行列概念）。
pub(crate) fn expr_type(
    ws: &Workspace,
    ctx: &SemCtx,
    src: &str,
    node: Node<'_>,
) -> Option<ExprTy> {
    match node.kind() {
        // 实参包装（调用点消歧 / signatureHelp 共用）：argument 包一层表达式
        "argument" => {
            let inner = syntax::children_with_fields(node)
                .into_iter()
                .find(|(_, c)| c.is_named())
                .map(|(_, c)| c)?;
            expr_type(ws, ctx, src, inner)
        }
        "named_argument" => expr_type(ws, ctx, src, node.child_by_field_name("value")?),
        "void_argument" => None,
        // 字面量（数字后缀 / 进制与 D25 同规则；f-string → FString；n"" → FName）。
        // FString/FName 是引擎类型（.d.as 声明），非内建——索引里没有该类型
        // 声明时返回 None（宁缺毋假）
        "number" => base_ty(
            ws,
            number_base_name(syntax::text(node, src), ws.config.float_is_float64),
        ),
        "string_literal" | "heredoc_string" | "format_string" => base_ty(ws, "FString"),
        "name_literal" => base_ty(ws, "FName"),
        "boolean_literal" => base_ty(ws, "bool"),
        "null_literal" => None,
        "identifier" => ident_type(ws, ctx, src, node),
        "member_expression" => member_type(ws, ctx, src, node),
        "call_expression" => call_type(ws, ctx, src, node),
        "subscript_expression" => subscript_type(ws, ctx, src, node),
        "binary_expression" => binary_type(ws, ctx, src, node),
        "unary_expression" => unary_type(ws, ctx, src, node),
        // ++/-- 结果即操作数新值
        "update_expression" => expr_type(ws, ctx, src, node.child_by_field_name("argument")?),
        // 赋值表达式的值 = 左值类型（引擎语义）
        "assignment_expression" => expr_type(ws, ctx, src, node.child_by_field_name("left")?),
        "conditional_expression" => {
            // 两分支基类型一致才定型（引擎要求同型或可隐转；隐转表 M5 不建）
            let a = expr_type(ws, ctx, src, node.child_by_field_name("consequence")?)?;
            let b = expr_type(ws, ctx, src, node.child_by_field_name("alternative")?)?;
            (a.base == b.base).then_some(a)
        }
        "cast_expression" => {
            let syn = syntax::parse_syn_type(node.child_by_field_name("type")?, src)?;
            let base = syn_type_base(ws, &syn)?;
            Some(ExprTy { base, syn: Some(syn) })
        }
        "parenthesized_expression" => {
            let inner = syntax::children_with_fields(node)
                .into_iter()
                .find(|(_, c)| c.is_named())
                .map(|(_, c)| c)?;
            expr_type(ws, ctx, src, inner)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 各节点形态
// ---------------------------------------------------------------------------

fn ident_type(ws: &Workspace, ctx: &SemCtx, src: &str, node: Node<'_>) -> Option<ExprTy> {
    if syntax::text(node, src) == "this" {
        let base = ctx.type_def?;
        return Some(ExprTy { base, syn: Some(syn_of_base(ws, base)) });
    }
    let name = intern_sym(syntax::text(node, src));
    match resolve_plain(ws, ctx, name)?.targets.into_iter().next()? {
        Target::Def(r) => def_expr_ty(ws, r),
        Target::Synthetic(_) => None, // 合成成员不是值（构造/opAssign 定型无消费方）
        Target::Local(l) => local_expr_ty(ws, &l),
    }
}

fn member_type(ws: &Workspace, ctx: &SemCtx, src: &str, node: Node<'_>) -> Option<ExprTy> {
    let recv = expr_type(ws, ctx, src, node.child_by_field_name("object")?)?;
    let prop = node.child_by_field_name("property")?;
    let name = intern_sym(syntax::text(prop, src));
    let space = member_search_space(ws, recv.base);
    let map = template_map(ws, recv.base, recv.syn.as_ref());
    // 字段优先；否则访问器 Get 的返回类型
    if let Some(&f) = members_named(ws, &space, name, |d| d.kind == DefKind::Field).first() {
        let syn = def_decl_syn(ws, f)?;
        let syn = subst_syn(&syn, &map);
        return Some(ExprTy { base: syn_type_base(ws, &syn)?, syn: Some(syn) });
    }
    for a in find_accessors(ws, &space, name) {
        if let Some(syn) = def_decl_syn(ws, a) {
            let syn = subst_syn(&syn, &map);
            if let Some(base) = syn_type_base(ws, &syn) {
                return Some(ExprTy { base, syn: Some(syn) });
            }
        }
    }
    None
}

fn call_type(ws: &Workspace, ctx: &SemCtx, src: &str, node: Node<'_>) -> Option<ExprTy> {
    let f = node.child_by_field_name("function")?;
    let args = call_args(node);
    match f.kind() {
        "identifier" => {
            let name = intern_sym(syntax::text(f, src));
            let res = resolve_callee(ws, ctx, name, args.len())?;
            match pick_call_target(ws, ctx, src, res, &args)? {
                Target::Def(r) => match ws.decl(&r).kind {
                    // 构造调用 `FVector(1,2,3)`：返回类型即类型本身
                    DefKind::Class | DefKind::Struct | DefKind::Enum => {
                        Some(ExprTy { base: r, syn: Some(syn_of_base(ws, r)) })
                    }
                    _ => callable_return(ws, r, &HashMap::new()),
                },
                // 合成成员调用（局部 delegate 变量 `Del(...)` 场景不含合成——
                // 此分支防御 `OnHit.Execute` 经 callee 消歧收敛到 Synthetic）
                Target::Synthetic(m) => synthetic_return(ws, &m),
                Target::Local(l) => local_expr_ty(ws, &l),
            }
        }
        // 构造模板实例 `TArray<int>(...)`：callee 是 template_type → 类型本体
        "template_type" => {
            let name_node = f.child_by_field_name("name")?;
            let base = type_def_of(ws, intern_sym(syntax::text(name_node, src)))?;
            let syn = syntax::parse_syn_type(f, src)?;
            Some(ExprTy { base, syn: Some(syn) })
        }
        "member_expression" => {
            let recv = expr_type(ws, ctx, src, f.child_by_field_name("object")?)?;
            let prop = f.child_by_field_name("property")?;
            let name = intern_sym(syntax::text(prop, src));
            let space = member_search_space(ws, recv.base);
            let map = template_map(ws, recv.base, recv.syn.as_ref());
            let mut cands = members_named(ws, &space, name, |d| {
                matches!(d.kind, DefKind::Method | DefKind::Function | DefKind::Operator)
            });
            if cands.is_empty() {
                cands = find_accessors(ws, &space, name);
            }
            if let Some(def) = pick_overload(ws, ctx, src, &cands, &args) {
                return callable_return(ws, def, &map);
            }
            // B4：合成成员（`OnHit.Execute(5)` 的返回类型从委托声明克隆）
            let mut syns = ws.synthetic_named(&space, name);
            if let Some(m) = syns.pop() {
                return synthetic_return(ws, &m);
            }
            None
        }
        _ => None,
    }
}

/// 合成成员调用的返回类型（Execute / Broadcast 从委托声明克隆；
/// StaticClass → UClass）。
fn synthetic_return(ws: &Workspace, m: &crate::workspace::SyntheticMember) -> Option<ExprTy> {
    let syn = m.return_type.clone()?;
    Some(ExprTy { base: syn_type_base(ws, &syn)?, syn: Some(syn) })
}

/// `Arr[i]` → opIndex（模板实参替换后即元素类型）。
fn subscript_type(ws: &Workspace, ctx: &SemCtx, src: &str, node: Node<'_>) -> Option<ExprTy> {
    let recv = expr_type(ws, ctx, src, node.child_by_field_name("object")?)?;
    let space = member_search_space(ws, recv.base);
    let map = template_map(ws, recv.base, recv.syn.as_ref());
    let cands = members_named(ws, &space, intern_sym("opIndex"), |d| {
        matches!(d.kind, DefKind::Method | DefKind::Function | DefKind::Operator)
    });
    let args: Vec<Node<'_>> = syntax::children_with_fields(node)
        .into_iter()
        .filter(|(_, c)| c.kind() == "argument")
        .map(|(_, c)| c)
        .collect();
    let def = pick_overload(ws, ctx, src, &cands, &args)?;
    callable_return(ws, def, &map)
}

fn binary_type(ws: &Workspace, ctx: &SemCtx, src: &str, node: Node<'_>) -> Option<ExprTy> {
    let op = node
        .child_by_field_name("operator")
        .map(|o| syntax::text(o, src))
        .unwrap_or("");
    // 比较 / 逻辑：结果恒为 bool（引擎 opEquals/opCmp 的比较结果类型）
    if matches!(op, "==" | "!=" | "<" | "<=" | ">" | ">=" | "&&" | "||") {
        return base_ty(ws, "bool");
    }
    let left = expr_type(ws, ctx, src, node.child_by_field_name("left")?)?;
    let right = expr_type(ws, ctx, src, node.child_by_field_name("right")?);
    arithmetic_type(ws, op, left, right)
}

fn unary_type(ws: &Workspace, ctx: &SemCtx, src: &str, node: Node<'_>) -> Option<ExprTy> {
    let op = node
        .child_by_field_name("operator")
        .map(|o| syntax::text(o, src))
        .unwrap_or("");
    match op {
        "!" => base_ty(ws, "bool"),
        "+" => expr_type(ws, ctx, src, node.child_by_field_name("argument")?),
        "-" | "~" => {
            let t = expr_type(ws, ctx, src, node.child_by_field_name("argument")?)?;
            if is_builtin_primitive(ws, t.base) {
                let name = primitive_name(ws, t.base);
                let is_float = matches!(name, "float" | "float32" | "float64" | "double");
                if op == "~" && is_float {
                    return None; // 位补全只对整数域
                }
                if numeric_rank(name).is_none() {
                    return None; // void / bool 不参与数值一元运算
                }
                return Some(t);
            }
            let name = if op == "-" { "opNeg" } else { "opCompl" };
            let space = member_search_space(ws, t.base);
            let cands = members_named(ws, &space, intern_sym(name), |d| {
                matches!(d.kind, DefKind::Method | DefKind::Function | DefKind::Operator)
            });
            callable_return(ws, *cands.first()?, &HashMap::new())
        }
        _ => None,
    }
}

/// 算术 / 位运算（`+ - * / % ** & | ^ << >> >>>`）。
///
/// primitive 左操作数：同型保持；数值域不同型取较高档（最小提升近似）；
/// 位运算遇浮点 → None。对象左操作数：查 opXxx 重载（右操作数基名消歧；
/// 消歧失败时全部候选返回基一致才取）。
fn arithmetic_type(
    ws: &Workspace,
    op: &str,
    left: ExprTy,
    right: Option<ExprTy>,
) -> Option<ExprTy> {
    let is_bitwise = matches!(op, "&" | "|" | "^" | "<<" | ">>" | ">>>");
    if is_builtin_primitive(ws, left.base) {
        let lt = primitive_name(ws, left.base);
        let lrank = numeric_rank(lt)?;
        return match right {
            Some(r) if is_builtin_primitive(ws, r.base) => {
                let rt = primitive_name(ws, r.base);
                if rt == lt {
                    return Some(left);
                }
                if is_bitwise {
                    return None; // 位运算不跨域
                }
                match numeric_rank(rt) {
                    // rt 已验内建 primitive → base_ty 必命中
                    Some(rrank) if rrank > lrank => base_ty(ws, rt),
                    Some(_) => Some(left),
                    None => None, // bool/void 不参与
                }
            }
            // 右操作数不可定型 → 宁缺毋假（不猜提升方向）
            _ => None,
        };
    }
    let opname = operator_method(op)?;
    let space = member_search_space(ws, left.base);
    let cands = members_named(ws, &space, intern_sym(opname), |d| {
        matches!(d.kind, DefKind::Method | DefKind::Function | DefKind::Operator)
    });
    if cands.is_empty() {
        return None; // 无重载：引擎侧也无从定型（宁缺毋假）
    }
    let arg_bases = [right.as_ref().map(|r| r.base)];
    if let Some(w) = crate::overload::disambiguate(ws, &cands, &arg_bases) {
        return callable_return(ws, w, &HashMap::new());
    }
    // 消歧失败：全部候选返回基一致才取（返回类型分歧时猜哪个都是假）
    let mut rets = cands
        .iter()
        .filter_map(|&c| callable_return(ws, c, &HashMap::new()).map(|t| t.base));
    let first = rets.next()?;
    let uniform = rets.all(|b| b == first);
    uniform.then_some(ExprTy { base: first, syn: None })
}

// ---------------------------------------------------------------------------
// range-for 双跳协议（引擎 as_compiler.cpp:5745-5873）
// ---------------------------------------------------------------------------

/// range-for 迭代元素类型：`容器.Iterator()` 的返回类型上 `.Iterate()` 的
/// 返回类型（保留引用性；模板形参按容器实参替换）。任一跳失败 → None。
pub(crate) fn for_each_element(
    ws: &Workspace,
    ctx: &SemCtx,
    src: &str,
    node: Node<'_>,
) -> Option<ExprTy> {
    let range = node.child_by_field_name("range")?;
    let container = expr_type(ws, ctx, src, range)?;

    // 第一跳：Iterator()——0 参 + 返回对象。constness 匹配按引擎规则近似：
    // 容器声明侧带 const 时优先 const 版本，否则优先非 const（典型场景），
    // 无匹配时回退另一种（引擎 :5791-5804 的回退语义）
    let space = member_search_space(ws, container.base);
    let iters = zero_param_methods(ws, &space, "Iterator");
    if iters.is_empty() {
        return None;
    }
    let want_const = container
        .syn
        .as_ref()
        .is_some_and(|s| matches!(s, SynType::Const(_)));
    let iter_def = pick_constness(ws, &iters, want_const)?;

    // 第二跳：Iterate()——0 参；元素类型 = 返回类型（引用性保留）
    let iter_ret = def_decl_syn(ws, iter_def)?;
    let map1 = template_map(ws, container.base, container.syn.as_ref());
    let iter_syn = subst_syn(&iter_ret, &map1);
    let iter_base = syn_type_base(ws, &iter_syn)?;
    let it_space = member_search_space(ws, iter_base);
    let iterates = zero_param_methods(ws, &it_space, "Iterate");
    if iterates.is_empty() {
        return None;
    }
    let iterate_def = pick_constness(ws, &iterates, false)?;
    let elem_ret = def_decl_syn(ws, iterate_def)?;
    let map2 = template_map(ws, iter_base, Some(&iter_syn));
    let elem_syn = subst_syn(&elem_ret, &map2);
    let elem_base = syn_type_base(ws, &elem_syn)?;
    Some(ExprTy { base: elem_base, syn: Some(elem_syn) })
}

/// 成员空间里的 0 参同名方法集（Iterator/Iterate 筛选：RawExtra::Callable
/// 形参为空——`(void)` 在索引期已归零参）。
fn zero_param_methods(ws: &Workspace, space: &[DeclRef], name: &str) -> Vec<DeclRef> {
    members_named(ws, space, intern_sym(name), |d| {
        matches!(d.kind, DefKind::Method | DefKind::Function | DefKind::Operator)
            && matches!(&d.extra, RawExtra::Callable { params, .. } if params.is_empty())
    })
}

fn pick_constness(ws: &Workspace, cands: &[DeclRef], want_const: bool) -> Option<DeclRef> {
    cands
        .iter()
        .copied()
        .find(|&c| ws.decl(&c).flags.contains(DefFlags::CONST) == want_const)
        .or_else(|| cands.first().copied())
}

// ---------------------------------------------------------------------------
// 模板形参替换（完整实例化的最小可用子集：只在使用点替换，不克隆成员）
// ---------------------------------------------------------------------------

/// 容器声明侧语法类型 → 模板形参映射（`TArray<FVector>` → {T: FVector}；
/// `FVector[]` → {T: FVector}——数组元素即 TArray 的唯一形参）。
/// Ref/Const 包装先剥掉（`TMapIterator<K,V>& Iterate()` 的元素形态）。
fn template_map(ws: &Workspace, def: DeclRef, syn: Option<&SynType>) -> HashMap<Sym, SynType> {
    let mut map = HashMap::new();
    let d = ws.decl(&def);
    let template_params = &d.template_params;
    if template_params.is_empty() {
        return map;
    }
    let mut s = syn;
    while let Some(t) = s {
        match t {
            SynType::Ref(inner, _) | SynType::Const(inner) => s = Some(inner.as_ref()),
            _ => break,
        }
    }
    match s {
        Some(SynType::Array(elem)) if template_params.len() == 1 => {
            map.insert(template_params[0], (**elem).clone());
        }
        Some(SynType::Template { args, .. }) => {
            for (p, a) in template_params.iter().zip(args.iter()) {
                map.insert(*p, a.clone());
            }
        }
        _ => {}
    }
    map
}

/// SynType 级符号替换：形参名（Named）命中映射则替换，包装递归。
fn subst_syn(syn: &SynType, map: &HashMap<Sym, SynType>) -> SynType {
    if map.is_empty() {
        return syn.clone();
    }
    match syn {
        SynType::Named(name, span) => {
            map.get(name).cloned().unwrap_or(SynType::Named(*name, *span))
        }
        SynType::Template { name, name_span, args } => SynType::Template {
            name: *name,
            name_span: *name_span,
            args: args.iter().map(|a| subst_syn(a, map)).collect(),
        },
        SynType::Array(inner) => SynType::Array(Box::new(subst_syn(inner, map))),
        SynType::Const(inner) => SynType::Const(Box::new(subst_syn(inner, map))),
        SynType::Ref(inner, k) => SynType::Ref(Box::new(subst_syn(inner, map)), *k),
        SynType::UnresolvedObject(inner) => {
            SynType::UnresolvedObject(Box::new(subst_syn(inner, map)))
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// 调用点辅助
// ---------------------------------------------------------------------------

fn call_args(call: Node<'_>) -> Vec<Node<'_>> {
    call.child_by_field_name("arguments")
        .map(|args| {
            syntax::children_with_fields(args)
                .into_iter()
                .filter(|(_, c)| c.kind() == "argument")
                .map(|(_, c)| c)
                .collect()
        })
        .unwrap_or_default()
}

/// 裸 callee 的多候选收敛：先按实参定型消歧（overload::disambiguate），
/// 失败取首个（M3 行为——候选真实存在，仅选不准）。
fn pick_call_target(
    ws: &Workspace,
    ctx: &SemCtx,
    src: &str,
    res: Resolution,
    args: &[Node<'_>],
) -> Option<Target> {
    if res.targets.len() > 1 {
        let defs: Option<Vec<DeclRef>> = res
            .targets
            .iter()
            .map(|t| match t {
                Target::Def(r) => Some(*r),
                Target::Synthetic(_) | Target::Local(_) => None,
            })
            .collect();
        if let Some(defs) = defs {
            if defs.iter().all(|&d| {
                matches!(
                    ws.decl(&d).kind,
                    DefKind::Function
                        | DefKind::Method
                        | DefKind::Constructor
                        | DefKind::Destructor
                        | DefKind::Operator
                )
            }) {
                if let Some(w) = pick_overload(ws, ctx, src, &defs, args) {
                    return Some(Target::Def(w));
                }
            }
        }
    }
    res.targets.into_iter().next()
}

/// 成员调用 / 下标候选集收敛：消歧 → 首个 arity 匹配 → 首个候选。
fn pick_overload(
    ws: &Workspace,
    ctx: &SemCtx,
    src: &str,
    cands: &[DeclRef],
    args: &[Node<'_>],
) -> Option<DeclRef> {
    if cands.is_empty() {
        return None;
    }
    let arg_bases: Vec<Option<DeclRef>> = args
        .iter()
        .map(|&a| expr_type(ws, ctx, src, a).map(|t| t.base))
        .collect();
    if let Some(w) = crate::overload::disambiguate(ws, cands, &arg_bases) {
        return Some(w);
    }
    cands
        .iter()
        .copied()
        .find(|&c| {
            matches!(&ws.decl(&c).extra, RawExtra::Callable { params, .. } if params.len() == args.len())
        })
        .or_else(|| cands.first().copied())
}

// ---------------------------------------------------------------------------
// 类型辅助（从 resolve.rs 迁入 / 新增）
// ---------------------------------------------------------------------------

/// 基名 → ExprTy（内建合成 bool/int/float64 一定命中；FString/FName 是
/// 引擎类型（.d.as 声明），索引里缺失时 None——宁缺毋假，D14）。
/// syn 为 None：字面量没有声明侧形态。
fn base_ty(ws: &Workspace, name: &str) -> Option<ExprTy> {
    named_def_of(ws, intern_sym(name)).map(|base| ExprTy { base, syn: None })
}

/// DeclRef 的定型：字段/全局变量走声明 SynType；可调用走返回类型；
/// 类型本身即自身。
fn def_expr_ty(ws: &Workspace, def: DeclRef) -> Option<ExprTy> {
    match ws.decl(&def).kind {
        DefKind::Class | DefKind::Struct | DefKind::Enum | DefKind::Delegate | DefKind::Event => {
            Some(ExprTy { base: def, syn: Some(syn_of_base(ws, def)) })
        }
        DefKind::Field | DefKind::GlobalVar | DefKind::VirtualProperty | DefKind::AssetDecl => {
            // C7（Phase C）：原「声明类型缺失时回落 resolved 归一化表」经语料
            // 取证 0 命中（触发条件 Variable{ty:None} 与填充条件
            // Variable{ty:Some} 不相交）——死路径删除。
            let syn = def_decl_syn(ws, def)?;
            Some(ExprTy { base: syn_type_base(ws, &syn)?, syn: Some(syn) })
        }
        _ => None,
    }
}

fn local_expr_ty(ws: &Workspace, l: &crate::resolve::LocalDecl) -> Option<ExprTy> {
    let ty = l.ty.as_ref()?;
    Some(ExprTy { base: syn_type_base(ws, ty)?, syn: Some(ty.clone()) })
}

/// 声明侧语法类型（字段 / 可调用返回）；类型声明本体为 None。
fn def_decl_syn(ws: &Workspace, def: DeclRef) -> Option<SynType> {
    match &ws.decl(&def).extra {
        RawExtra::Variable { ty: Some(t) } => Some(t.clone()),
        RawExtra::Callable { return_type: Some(t), .. } => Some(t.clone()),
        _ => None,
    }
}

/// 可调用的返回类型（声明 SynType 经模板替换后定型）。
fn callable_return(
    ws: &Workspace,
    def: DeclRef,
    map: &HashMap<Sym, SynType>,
) -> Option<ExprTy> {
    let syn = def_decl_syn(ws, def)?;
    let syn = subst_syn(&syn, map);
    Some(ExprTy { base: syn_type_base(ws, &syn)?, syn: Some(syn) })
}

/// DeclRef → 声明侧 SynType（内建合成 → Primitive；其余 → Named）。
/// auto 定型结果的 syn 缺失时（字面量）由此回填，保证 LocalDecl.ty 可再定型。
pub(crate) fn syn_of_base(ws: &Workspace, base: DeclRef) -> SynType {
    let d = ws.decl(&base);
    if d.flags.contains(DefFlags::SYNTHETIC) {
        SynType::Primitive(d.name, d.name_span)
    } else {
        SynType::Named(d.name, d.name_span)
    }
}

/// 声明类型是否 auto（`auto` / `auto&`——引擎 for-range 的 isAutoReference
/// 形态，引用性保留给 Iterate 返回侧）。
pub(crate) fn is_auto_type(ty: &SynType) -> bool {
    match ty {
        SynType::Auto => true,
        SynType::Ref(inner, _) => matches!(**inner, SynType::Auto),
        _ => false,
    }
}

/// 数字字面量的基类型名（引擎语义：无后缀整数 → int；`1.5f` → float32；
/// 其余浮点 → 裸 float，宽度按 IndexConfig 归一化——与 D25 同规则）。
/// 进制前缀（0x/0b/0o/0d）优先判定，避免 `0x1E` 里的 E 被当科学计数法。
/// `pub(crate)`：summary.rs 局部预推导复用（Phase A，D37）。
pub(crate) fn number_base_name(text: &str, float_is_f64: bool) -> &'static str {
    if text.ends_with('f') || text.ends_with('F') {
        return "float32";
    }
    let radix_prefixed = text
        .get(..2)
        .map_or(false, |p| matches!(p, "0x" | "0X" | "0b" | "0B" | "0o" | "0O" | "0d" | "0D"));
    if radix_prefixed || !(text.contains('.') || text.contains('e') || text.contains('E')) {
        return "int";
    }
    if float_is_f64 {
        "float64"
    } else {
        "float32"
    }
}

/// 名字 → 类型声明（**含内建合成**——bool/int/float64；与 `type_def_of`
/// 的差别只在不排除 SYNTHETIC）。
fn named_def_of(ws: &Workspace, name: Sym) -> Option<DeclRef> {
    ws.lookup(name)
        .iter()
        .copied()
        .find(|&r| ws.decl(&r).kind.is_type_like())
}

/// 语法层类型 → 具名基类型（局部/形参不走 resolved，直接查名）。
fn syn_type_base(ws: &Workspace, syn: &SynType) -> Option<DeclRef> {
    match syn {
        SynType::Primitive(name, _) => {
            let key = if sym_str(*name) == "float" {
                intern_sym(if ws.config.float_is_float64 { "float64" } else { "float32" })
            } else {
                *name
            };
            builtin_target(ws, key).map(|t| match t {
                Target::Def(r) => r,
                _ => unreachable!(),
            })
        }
        SynType::Named(name, _) | SynType::Template { name, .. } => type_def_of(ws, *name),
        SynType::Const(inner) | SynType::Ref(inner, _) | SynType::UnresolvedObject(inner) => {
            syn_type_base(ws, inner)
        }
        // `T[]` 的成员查找落在 TArray 模板本体上（实例化 Phase 3 惰性）
        SynType::Array(_) => type_def_of(ws, intern_sym("TArray")),
        SynType::Qualified(_) | SynType::Auto | SynType::Wildcard => None,
    }
}

fn type_def_of(ws: &Workspace, name: Sym) -> Option<DeclRef> {
    ws.lookup(name)
        .iter()
        .copied()
        .find(|&r| {
            let d = ws.decl(&r);
            d.kind.is_type_like() && !d.flags.contains(DefFlags::SYNTHETIC)
        })
}

// ---------------------------------------------------------------------------
// primitive 域辅助
// ---------------------------------------------------------------------------

/// 引擎 primitive 关键字全集（D25：裸 float 从不落到名为 float 的声明）。
const PRIMITIVES: &[&str] = &[
    "void", "bool", "int", "int8", "int16", "int32", "int64", "uint", "uint8", "uint16",
    "uint32", "uint64", "float", "float64", "float32", "double",
];

fn is_builtin_primitive(ws: &Workspace, def: DeclRef) -> bool {
    let d = ws.decl(&def);
    d.flags.contains(DefFlags::SYNTHETIC) && PRIMITIVES.contains(&sym_str(d.name))
}

fn primitive_name(ws: &Workspace, def: DeclRef) -> &'static str {
    sym_str(ws.decl(&def).name)
}

/// 数值域档位（最小提升近似：取较高档；有符号/无符号混合不追引擎精确矩阵，
/// 完整隐式转换表留给 P5 诊断——架构设计 §4.3）。
fn numeric_rank(name: &str) -> Option<u8> {
    Some(match name {
        "int8" => 1,
        "uint8" => 2,
        "int16" => 3,
        "uint16" => 4,
        "int" | "int32" => 5,
        "uint" | "uint32" => 6,
        "int64" => 7,
        "uint64" => 8,
        "float32" => 9,
        "float64" | "double" => 10,
        _ => return None,
    })
}

/// 二元运算符 token → opXxx 方法名（比较/逻辑已在 binary_type 拦截为 bool）。
fn operator_method(op: &str) -> Option<&'static str> {
    Some(match op {
        "+" => "opAdd",
        "-" => "opSub",
        "*" => "opMul",
        "/" => "opDiv",
        "%" => "opMod",
        "**" => "opPow",
        "&" => "opAnd",
        "|" => "opOr",
        "^" => "opXor",
        "<<" => "opShl",
        ">>" => "opShr",
        ">>>" => "opUShr",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IndexConfig;
    use crate::intern::intern_file;
    use crate::id::FileId;
    use crate::resolve::{resolve_at, Resolution, Target, LEVEL_LOCAL, LEVEL_MEMBER};
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

    fn nth(src: &str, needle: &str, n: usize) -> u32 {
        match src.match_indices(needle).nth(n - 1) {
            Some((i, _)) => i as u32,
            None => panic!("'{needle}' 第 {n} 次出现不存在"),
        }
    }

    fn file_of(path: &str) -> FileId {
        intern_file(path, 0)
    }

    fn first_name(ws: &Workspace, r: &Resolution) -> String {
        r.targets[0].name(ws).to_string()
    }

    /// 消歧可观察面：调用点唯一收敛后 targets 只剩 1 个，
    /// 返回其形参 0 的类型基名。
    fn resolved_param0(ws: &Workspace, r: &Resolution) -> String {
        assert_eq!(r.targets.len(), 1, "消歧应收敛为唯一目标");
        let Target::Def(id) = r.targets[0] else { panic!("应是 Def") };
        let RawExtra::Callable { params, .. } = &ws.decl(&id).extra else { panic!() };
        let ty = params[0].ty.as_ref().unwrap();
        let base = syn_type_base(ws, ty).expect("形参类型应可定型");
        sym_str(ws.decl(&base).name).to_string()
    }

    // ------------------------------------------------------------------
    // 运算符重载 + auto 局部定型
    // ------------------------------------------------------------------

    #[test]
    fn op_overload_return_types_auto_local() {
        const SRC: &str = "\
struct Vec
{
    float X;
    Vec opAdd(Vec Other) { Vec V; return V; }
    Vec opNeg() { Vec V; return V; }
}
void F()
{
    Vec A;
    Vec B;
    auto S = A + B;
    auto N = -A;
    float X1 = S.X;
    float X2 = N.X;
}
";
        let ws = build(&[("unique://expr/op.as", SRC)]);
        let file = file_of("unique://expr/op.as");
        // S.X / N.X：auto 定型 → 运算符返回 → 成员命中（+2 跳过 "S." 落在 X 上）
        let r = resolve_at(&ws, file, off(SRC, "S.X") + 2).unwrap();
        assert_eq!(r.level, LEVEL_MEMBER);
        assert_eq!(first_name(&ws, &r), "X");
        let r = resolve_at(&ws, file, off(SRC, "N.X") + 2).unwrap();
        assert_eq!(r.level, LEVEL_MEMBER);
        assert_eq!(first_name(&ws, &r), "X");
    }

    #[test]
    fn op_missing_yields_none() {
        const SRC: &str = "\
struct No {}
void F()
{
    No A;
    No B;
    auto X = A + B;
    float Y = X.F;
}
";
        let ws = build(&[("unique://expr/opmiss.as", SRC)]);
        let file = file_of("unique://expr/opmiss.as");
        // 无 opAdd → auto 保留 Auto → X.F 不可解析（宁缺毋假；+2 落在 F 上）
        assert!(resolve_at(&ws, file, off(SRC, "X.F") + 2).is_none());
    }

    #[test]
    fn comparison_and_logical_are_bool() {
        const SRC: &str = "\
struct Vec {}
void Sink(bool B) {}
void Sink(Vec V) {}
void F()
{
    Vec A;
    Sink(A == A);
    Sink(A != A);
    Sink(A && A);
}
";
        let ws = build(&[("unique://expr/cmp.as", SRC)]);
        let file = file_of("unique://expr/cmp.as");
        // == != && → bool → Sink(bool) 唯一收敛
        let r = resolve_at(&ws, file, off(SRC, "Sink(A ==")).unwrap();
        assert_eq!(resolved_param0(&ws, &r), "bool");
        let r = resolve_at(&ws, file, nth(SRC, "Sink(A !=", 1)).unwrap();
        assert_eq!(resolved_param0(&ws, &r), "bool");
        let r = resolve_at(&ws, file, off(SRC, "Sink(A &&")).unwrap();
        assert_eq!(resolved_param0(&ws, &r), "bool");
    }

    #[test]
    fn primitive_promotion_and_suffix() {
        const SRC: &str = "\
void SinkF(float64 V) {}
void SinkI(int V) {}
void SinkF32(float32 V) {}
void F()
{
    auto A = 1 + 2.0;
    auto B = 1 + 2;
    auto C = 1.5f;
    SinkF(A);
    SinkI(B);
    SinkF32(C);
}
";
        let ws = build(&[("unique://expr/num.as", SRC)]);
        let file = file_of("unique://expr/num.as");
        // int + float → float64（默认 float_is_float64=true，D25）
        let r = resolve_at(&ws, file, off(SRC, "SinkF(A)")).unwrap();
        assert_eq!(resolved_param0(&ws, &r), "float64");
        let r = resolve_at(&ws, file, off(SRC, "SinkI(B)")).unwrap();
        assert_eq!(resolved_param0(&ws, &r), "int");
        let r = resolve_at(&ws, file, off(SRC, "SinkF32(C)")).unwrap();
        assert_eq!(resolved_param0(&ws, &r), "float32");
    }

    // ------------------------------------------------------------------
    // f-string / FName / 字符串字面量
    // ------------------------------------------------------------------

    #[test]
    fn fstring_and_fname_literals() {
        const SRC: &str = "\
struct FString { int Length; }
struct FName {}
void SinkS(FString S) {}
void SinkN(FName N) {}
void F()
{
    SinkS(f\"hello {1}\");
    SinkN(n\"Foo\");
    SinkS(\"plain\");
}
";
        let ws = build(&[("unique://expr/lit.as", SRC)]);
        let file = file_of("unique://expr/lit.as");
        let r = resolve_at(&ws, file, off(SRC, "SinkS(f\"")).unwrap();
        assert_eq!(resolved_param0(&ws, &r), "FString");
        let r = resolve_at(&ws, file, off(SRC, "SinkN(n\"")).unwrap();
        assert_eq!(resolved_param0(&ws, &r), "FName");
        let r = resolve_at(&ws, file, nth(SRC, "SinkS(", 2)).unwrap();
        assert_eq!(resolved_param0(&ws, &r), "FString");
    }

    // ------------------------------------------------------------------
    // range-for 双跳 + 模板替换
    // ------------------------------------------------------------------

    const TARRAY_DECL: &str = "\
struct TArray<T>
{
    TArrayIterator<T> Iterator();
    TArrayConstIterator<T> Iterator() const;
    T& opIndex(int Index);
    int Num() const;
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

    #[test]
    fn range_for_double_hop_array() {
        const SRC: &str = "\
int[] Plain;
void F()
{
    FVector[] Arr;
    for (auto E : Arr)
    {
        float X = E.X;
    }
}
";
        let ws = build(&[
            ("unique://expr/tarray.d.as", TARRAY_DECL),
            ("unique://expr/hop.as", SRC),
        ]);
        let file = file_of("unique://expr/hop.as");
        // E : FVector[] → TArray.Iterator() → TArrayIterator<FVector> →
        // T& Iterate() → FVector（T 替换为 FVector；+2 落在 X 上）
        let r = resolve_at(&ws, file, off(SRC, "E.X") + 2).unwrap();
        assert_eq!(r.level, LEVEL_MEMBER);
        assert_eq!(first_name(&ws, &r), "X");
    }

    #[test]
    fn subscript_opindex_substitution() {
        const SRC: &str = "\
void F()
{
    FVector[] Arr;
    auto E = Arr[0];
    float X = E.X;
}
";
        let ws = build(&[
            ("unique://expr/tarray2.d.as", TARRAY_DECL),
            ("unique://expr/sub.as", SRC),
        ]);
        let file = file_of("unique://expr/sub.as");
        let r = resolve_at(&ws, file, off(SRC, "E.X") + 2).unwrap();
        assert_eq!(r.level, LEVEL_MEMBER);
        assert_eq!(first_name(&ws, &r), "X");
    }

    #[test]
    fn range_for_tmap_two_params() {
        // TMap 双形参：Iterate 返回迭代器自身（引擎真值——迭代器即元素视图），
        // 迭代器方法返回类型里的 K/V 按容器实参替换
        const DECL: &str = "\
struct TMap<K, V>
{
    TMapIterator<K,V> Iterator();
}
struct TMapIterator<K, V>
{
    K GetKey() const;
    TMapIterator<K,V>& Iterate();
}
struct FString { int Length; }
";
        const SRC: &str = "\
void F()
{
    TMap<FString, int> M;
    for (auto P : M)
    {
        auto Key = P.GetKey();
        int L = Key.Length;
    }
}
";
        let ws = build(&[
            ("unique://expr/tmap.d.as", DECL),
            ("unique://expr/tmap.as", SRC),
        ]);
        let file = file_of("unique://expr/tmap.as");
        // P → TMapIterator<FString,int>（第一跳替换；+2 落在 GetKey 上）
        let r = resolve_at(&ws, file, off(SRC, "P.GetKey") + 2).unwrap();
        assert_eq!(r.level, LEVEL_MEMBER);
        assert_eq!(first_name(&ws, &r), "GetKey");
        // Key = GetKey() 的 K → FString（第二跳替换 + 返回替换；+4 落在 Length 上）
        let r = resolve_at(&ws, file, off(SRC, "Key.Length") + 4).unwrap();
        assert_eq!(r.level, LEVEL_MEMBER);
        assert_eq!(first_name(&ws, &r), "Length");
    }

    #[test]
    fn range_for_without_iterator_degrades() {
        const SRC: &str = "\
struct NoIter {}
void F()
{
    NoIter N;
    for (auto E : N)
    {
        int Y = E.X;
    }
}
";
        let ws = build(&[("unique://expr/noiter.as", SRC)]);
        let file = file_of("unique://expr/noiter.as");
        assert!(resolve_at(&ws, file, off(SRC, "E.X") + 2).is_none(), "无 Iterator → 不定型");
    }

    // ------------------------------------------------------------------
    // auto 链 / 局部可见性
    // ------------------------------------------------------------------

    #[test]
    fn auto_chain_and_plain_local() {
        const SRC: &str = "\
void SinkI(int V) {}
void F()
{
    auto A = 5;
    auto B = A;
    SinkI(B);
    int C = A;
}
";
        let ws = build(&[("unique://expr/chain.as", SRC)]);
        let file = file_of("unique://expr/chain.as");
        let r = resolve_at(&ws, file, off(SRC, "SinkI(B)")).unwrap();
        assert_eq!(resolved_param0(&ws, &r), "int");
        // 声明自身
        let r = resolve_at(&ws, file, off(SRC, "A;")).unwrap();
        assert_eq!(r.level, LEVEL_LOCAL);
    }

    #[test]
    fn conditional_requires_same_base() {
        const SRC: &str = "\
struct A { int X; }
struct B { int Y; }
void F()
{
    bool C = true;
    auto M = C ? A() : A();
    int X1 = M.X;
}
";
        let ws = build(&[("unique://expr/cond.as", SRC)]);
        let file = file_of("unique://expr/cond.as");
        let r = resolve_at(&ws, file, off(SRC, "M.X") + 2).unwrap();
        assert_eq!(r.level, LEVEL_MEMBER);
        assert_eq!(first_name(&ws, &r), "X");
    }

    // ------------------------------------------------------------------
    // B4：委托合成成员调用的返回类型（`OnHit.Execute(5)` 链式定型）
    // ------------------------------------------------------------------

    #[test]
    fn delegate_execute_call_return_type() {
        const SRC: &str = "\
struct FReply { int Handled; }
delegate FReply FOnHit(int Damage);
class A
{
    FOnHit OnHit;
    void M()
    {
        auto R = OnHit.Execute(5);
        int H = R.Handled;
    }
}
";
        let ws = build(&[("unique://expr/dlgret.as", SRC)]);
        let file = file_of("unique://expr/dlgret.as");
        // Execute 的返回类型从委托声明克隆（FReply）→ R.Handled 成员命中
        let r = resolve_at(&ws, file, off(SRC, "R.Handled") + 2).unwrap();
        assert_eq!(r.level, LEVEL_MEMBER);
        assert_eq!(first_name(&ws, &r), "Handled");
    }

    #[test]
    fn syn_type_base_float_normalizes_by_config() {
        // 原 workspace.rs float_dual_config_normalization 的继任（C7）：裸
        // float 归一化的活跃消费点自本期起唯一存在于 syn_type_base
        use crate::range::TextRange;
        let mk = |f64cfg: bool| {
            Workspace::build(
                IndexConfig { float_is_float64: f64cfg },
                vec![FileInput {
                    file: intern_file("unique://exprflt/f.as", 0),
                    kind: FileKind::Script,
                    source: String::new(),
                    module: None,
                }],
            )
        };
        let syn = SynType::Primitive(intern_sym("float"), TextRange::new(0, 0));
        for (cfg, expect) in [(true, "float64"), (false, "float32")] {
            let ws = mk(cfg);
            let base = syn_type_base(&ws, &syn).expect("内建 float 必命中");
            assert_eq!(sym_str(ws.decl(&base).name), expect);
        }
    }

    #[test]
    fn syn_type_base_template_targets_container() {
        // 原 workspace.rs template_field_type_resolution 的继任（C7）：模板
        // 使用位的成员查找落点 = 模板本体；实参替换（template_map/subst_syn）
        // 由 expr.rs 既有模板用例覆盖
        use crate::range::TextRange;
        const SRC: &str = "struct TArray<T> { }\nstruct FVector { }\n";
        let ws = build(&[("unique://exprtpl/t.as", SRC)]);
        let syn = SynType::Template {
            name: intern_sym("TArray"),
            name_span: TextRange::new(0, 0),
            args: vec![SynType::Named(intern_sym("FVector"), TextRange::new(0, 0))],
        };
        let base = syn_type_base(&ws, &syn).unwrap();
        assert_eq!(sym_str(ws.decl(&base).name), "TArray");
    }
}
