//! 符号查找链（架构设计 §4.5 的 0-6 级 + §4.5.1 mixin 五条准入条件）。
//!
//! ```text
//! 0. this / super（语境限定：类/struct 方法体内）
//! 1. 局部变量（作用域链上溯；形参属最外层局部帧）
//! 2. 当前类成员（class 沿继承闭包上溯；struct 单层——D16）
//! 3. 属性访问器模拟（Get<X>/Set<X>，反向默认：不带 NOT_PROPERTY 即候选，§2.4.5）
//! 4. mixin 函数（fallback：2/3 级全空 + 对象上下文 + 无 scope 限定才查，§4.5.1）
//! 5. 命名空间链（逐级回退父命名空间；同名 namespace 跨文件按 Sym 聚合）
//! 6. 全局符号 / 类型本身（local 函数按模块归属过滤；type/namespace 按语境择一）
//! ```
//!
//! 参数是**字节偏移**而非 Pos——as-core 不引入行列概念（规划 §3.2.1）。
//! 查询基于 `idx.files[file]` 的 CST 快照（server 侧保证 open 文件先重索引）。
//!
//! 表达式定型管线在 `crate::expr`（M5a 从本模块迁出并完整化：字面量 /
//! 运算符重载 / f-string / range-for 双跳 / 模板实参替换——D14/D28 欠账
//! 清偿）。auto 局部与 range-for 迭代变量在 SemCtx 构建期惰性定型
//!（请求驱动，扫描阶段仍不碰定型，D14）。

use as_syntax::tree_sitter::Node;

use crate::id::{DefId, FileId, Sym};
use crate::index::WorkspaceIndex;
use crate::intern::{intern_sym, sym_str};
use crate::range::TextRange;
use crate::symbol::{DefData, DefFlags, DefKind};
use crate::syntax;
use crate::types::SynType;

/// 查找链命中级数（§4.5 的 0-6）。
pub const LEVEL_THIS_SUPER: u8 = 0;
pub const LEVEL_LOCAL: u8 = 1;
pub const LEVEL_MEMBER: u8 = 2;
pub const LEVEL_ACCESSOR: u8 = 3;
pub const LEVEL_MIXIN: u8 = 4;
pub const LEVEL_NAMESPACE: u8 = 5;
pub const LEVEL_GLOBAL: u8 = 6;
/// 光标落在声明自身的名字 token 上（hover 声明、definition 自指）。
pub const LEVEL_DECL_SELF: u8 = 99;

/// 局部变量 / 形参：未入索引的语法层声明（索引不收函数体，规划 §4），
/// hover/definition 就地取材。
#[derive(Clone, Debug)]
pub struct LocalDecl {
    pub name: Sym,
    /// LocalVar / Param
    pub kind: DefKind,
    pub name_span: TextRange,
    pub full_span: TextRange,
    pub ty: Option<SynType>,
}

/// 查找链的落点。
#[derive(Clone, Debug)]
pub enum Target {
    Def(DefId),
    Local(LocalDecl),
}

impl Target {
    pub fn name<'a>(&self, idx: &'a WorkspaceIndex) -> &'a str {
        match self {
            Target::Def(id) => sym_str(idx.def(*id).name),
            Target::Local(l) => sym_str(l.name),
        }
    }
}

/// 一次解析的结果：同一级上的全部候选（重载组/同名多落点），最优在前。
#[derive(Clone, Debug)]
pub struct Resolution {
    pub targets: Vec<Target>,
    pub level: u8,
}

// ---------------------------------------------------------------------------
// 语境（SemCtx）
// ---------------------------------------------------------------------------

/// 光标处的语义语境：沿根→叶路径收集 namespace 链 / 所在类型 / 所在函数 /
/// 可见局部。
pub(crate) struct SemCtx<'t> {
    pub(crate) ns_defs: Vec<DefId>, // innermost → outermost
    pub(crate) ns_syms: Vec<Sym>,
    pub(crate) type_def: Option<DefId>,
    pub(crate) fn_def: Option<DefId>,
    /// 参数 + 块内局部（入栈序：参数 → 外块 → 内块；查找**逆序** = 内层优先）
    pub(crate) locals: Vec<LocalDecl>,
    pub(crate) file: FileId,
    _tree: std::marker::PhantomData<&'t ()>,
}

impl<'t> SemCtx<'t> {
    /// 从 identifier 节点沿 parent 链上溯建语境。
    ///
    /// M4 性能修正：原实现从 root 下潜逐层物化兄弟节点（`children_with_fields`
    /// 每具名孩子一次 String 分配）——巨型文件（Core.d.as 的 class_body 数百
    /// 成员、source_file 数百顶层声明）里每站点要重做数千次分配，首次
    /// references 全语料 163s。parent 链上溯是 O(深度)，与下潜语义等价：
    /// 祖先集合相同，处理序（outermost → innermost）由反转保证，局部入栈序
    /// （参数 → 外块 → 内块）不变。
    fn from_ancestors(
        ident: Node<'t>,
        src: &str,
        idx: &WorkspaceIndex,
        file: FileId,
    ) -> SemCtx<'t> {
        let mut ctx = SemCtx {
            ns_defs: Vec::new(),
            ns_syms: Vec::new(),
            type_def: None,
            fn_def: None,
            locals: Vec::new(),
            file,
            _tree: std::marker::PhantomData,
        };
        let byte = ident.start_byte() as u32;
        let mut chain: Vec<Node<'t>> = Vec::new();
        let mut cur = ident.parent();
        while let Some(n) = cur {
            chain.push(n);
            cur = n.parent();
        }
        for node in chain.into_iter().rev() {
            match node.kind() {
                "namespace_declaration" => {
                    // name 是 scoped_name，取最后一段标识符（decl_name_node 同规则）
                    if let Some(name_node) = syntax::decl_name_node(node) {
                        let span = syntax::span(name_node);
                        let sym = intern_sym(syntax::text(name_node, src));
                        if let Some(def) = def_id_at(idx, file, span, sym, &[DefKind::Namespace]) {
                            ctx.ns_defs.push(def);
                            ctx.ns_syms.push(sym);
                        }
                    }
                }
                "class_declaration" | "struct_declaration" => {
                    if let Some(name_node) = node.child_by_field_name("name") {
                        let span = syntax::span(name_node);
                        let sym = intern_sym(syntax::text(name_node, src));
                        if let Some(def) =
                            def_id_at(idx, file, span, sym, &[DefKind::Class, DefKind::Struct])
                        {
                            ctx.type_def = Some(def);
                        }
                    }
                }
                "function_declaration" | "constructor_declaration" | "destructor_declaration" => {
                    if let Some(name_node) = node.child_by_field_name("name") {
                        let span = syntax::span(name_node);
                        let sym = intern_sym(syntax::text(name_node, src));
                        if let Some(def) = def_id_at(
                            idx,
                            file,
                            span,
                            sym,
                            &[
                                DefKind::Function,
                                DefKind::Method,
                                DefKind::Constructor,
                                DefKind::Destructor,
                                DefKind::Operator,
                            ],
                        ) {
                            ctx.fn_def = Some(def);
                        }
                    }
                    // 形参：函数体全域可见，进最外层局部帧
                    for p in syntax::param_decls(node, src) {
                        ctx.locals.push(LocalDecl {
                            name: p.name,
                            kind: DefKind::Param,
                            name_span: p.span,
                            full_span: p.span,
                            ty: p.ty,
                        });
                    }
                }
                "block" => {
                    collect_block_locals(node, src, byte, idx, &mut ctx);
                }
                "for_statement" => {
                    // classic for 的初始化声明在整条 for 语句内可见
                    for (_f, child) in syntax::children_with_fields(node) {
                        if child.kind() == "variable_declaration" {
                            collect_declarators(&child, src, byte, idx, &mut ctx);
                        }
                    }
                }
                "for_each_statement" => {
                    // range-for 迭代变量：整条语句内可见（range 表达式里不可见
                    // ——声明点之后才入栈）。M4 修正：M3 写的 "range_for_statement"
                    // 是不存在的节点 kind（死分支），正确 kind 是 for_each_statement。
                    // M5a：声明类型是 auto 时按引擎双跳协议定型
                    // （expr::for_each_element，as_compiler.cpp:5745-5873）；
                    // 失败保留 Auto（宁缺毋假，D14）
                    if let Some(name_node) = node.child_by_field_name("name") {
                        let span = syntax::span(name_node);
                        if span.start <= byte {
                            let declared = node
                                .child_by_field_name("type")
                                .and_then(|t| syntax::parse_syn_type(t, src));
                            let ty = match &declared {
                                Some(t) if crate::expr::is_auto_type(t) => {
                                    crate::expr::for_each_element(idx, &ctx, src, node)
                                        .map(|e| {
                                            e.syn
                                                .clone()
                                                .unwrap_or_else(|| crate::expr::syn_of_base(idx, e.base))
                                        })
                                        .or(declared)
                                }
                                _ => declared,
                            };
                            ctx.locals.push(LocalDecl {
                                name: intern_sym(syntax::text(name_node, src)),
                                kind: DefKind::LocalVar,
                                name_span: span,
                                full_span: syntax::span(node),
                                ty,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        ctx
    }
}

/// 块内直接子声明语句的 declarator（声明点在 byte 之前才可见）。
/// **边收集边入栈**：auto 定型要看见同块先行声明（`Vec A; auto S = A + B;`），
/// 攒批回填会让先行局部在定型瞬间不可见。
fn collect_block_locals(
    node: Node<'_>,
    src: &str,
    byte: u32,
    idx: &WorkspaceIndex,
    ctx: &mut SemCtx,
) {
    for (_f, child) in syntax::children_with_fields(node) {
        if child.kind() == "variable_declaration" {
            collect_declarators(&child, src, byte, idx, ctx);
        }
    }
}

fn collect_declarators(
    decl: &Node<'_>,
    src: &str,
    byte: u32,
    idx: &WorkspaceIndex,
    ctx: &mut SemCtx,
) {
    let ty = decl
        .child_by_field_name("type")
        .and_then(|t| syntax::parse_syn_type(t, src));
    for (_f, child) in syntax::children_with_fields(*decl) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else { continue };
        let span = syntax::span(name_node);
        if span.start > byte {
            continue; // 声明点在使用点之后：不可见
        }
        // auto 定型（M5a，D14）：声明类型 auto + 有初始化式 + 查询点不在自身
        // 初始化式内 → 取初始化表达式定型（失败保留 Auto——宁缺毋假）。
        // 查询点在初始化式内时跳过（`auto X = F(|X|)` 的自引用防抖）
        let ty = match (&ty, child.child_by_field_name("value")) {
            (Some(t), Some(init)) if crate::expr::is_auto_type(t) => {
                let inside_init =
                    (init.start_byte() as u32) <= byte && byte < (init.end_byte() as u32);
                if inside_init {
                    ty.clone()
                } else {
                    match crate::expr::expr_type(idx, ctx, src, init) {
                        Some(e) => Some(
                            e.syn
                                .clone()
                                .unwrap_or_else(|| crate::expr::syn_of_base(idx, e.base)),
                        ),
                        None => ty.clone(),
                    }
                }
            }
            _ => ty.clone(),
        };
        ctx.locals.push(LocalDecl {
            name: intern_sym(syntax::text(name_node, src)),
            kind: DefKind::LocalVar,
            name_span: span,
            full_span: syntax::span(*decl),
            ty,
        });
    }
}

/// (file, name_span) → DefId（同一文件的声明锚点匹配；SYNTHETIC 排除——
/// 合成符号 name_span 与源头同名同位，声明自指须回落真实声明）。
fn def_id_at(
    idx: &WorkspaceIndex,
    file: FileId,
    span: TextRange,
    name: Sym,
    kinds: &[DefKind],
) -> Option<DefId> {
    idx.main.get(&name)?.iter().copied().find(|&id| {
        let d = idx.def(id);
        !d.flags.contains(DefFlags::SYNTHETIC)
            && d.file == file
            && d.name_span == span
            && kinds.contains(&d.kind)
    })
}

// ---------------------------------------------------------------------------
// 角色分派
// ---------------------------------------------------------------------------

pub(crate) enum Role<'t> {
    /// 类型位置（type / template_type 的 name）
    TypeUse,
    /// `A::B` 的首段 A
    ScopedFirst,
    /// `A::B` 的尾段 B（scope 节点）
    ScopedLast { scope: Node<'t> },
    /// `expr.Name` 的 Name（object 节点）
    MemberProperty { object: Node<'t> },
    /// 调用 callee：裸标识符
    Callee { call: Node<'t> },
    /// 裸标识符（赋值侧 / 实参 / return 值……）
    Plain,
}

pub(crate) fn role_of<'t>(ident: Node<'t>, parent: Node<'t>) -> Role<'t> {
    let field = syntax::children_with_fields(parent)
        .into_iter()
        .find(|(_, c)| c.id() == ident.id())
        .and_then(|(f, _)| f);
    match (parent.kind(), field.as_deref()) {
        ("type", Some("name")) | ("template_type", Some("name")) => Role::TypeUse,
        ("qualified_identifier", Some("scope")) => Role::ScopedFirst,
        ("qualified_identifier", Some("name")) => Role::ScopedLast {
            scope: parent.child_by_field_name("scope").unwrap_or(parent),
        },
        ("member_expression", Some("property")) => Role::MemberProperty {
            object: parent.child_by_field_name("object").unwrap_or(parent),
        },
        ("call_expression", Some("function")) => Role::Callee { call: parent },
        _ => Role::Plain,
    }
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 在 offset 处解析一个名字（规划 §7.1；字节偏移）。
pub fn resolve_at(idx: &WorkspaceIndex, file: FileId, byte: u32) -> Option<Resolution> {
    let snap = idx.files.get(&file)?;
    let src = &snap.source;
    let root = snap.tree.root_node();
    if byte < root.start_byte() as u32 || byte >= root.end_byte() as u32 {
        return None;
    }
    let ident = identifier_at(root, byte)?;
    resolve_at_node(idx, file, src, ident)
}

/// 在**已知** identifier 节点处解析（批量 UseSite 解析入口——节点已知，
/// 免从根下潜的 O(路径兄弟节点数) 重遍历，见 `SemCtx::from_ancestors`）。
pub fn resolve_at_node(
    idx: &WorkspaceIndex,
    file: FileId,
    src: &str,
    ident: Node<'_>,
) -> Option<Resolution> {
    if ident.kind() != "identifier" && ident.kind() != "primitive_type" {
        return None;
    }
    // specifier 语境的标识符（UPROPERTY(...) 宏参数 / 方法属性 / 类宏说明符）
    // 不是符号使用点——不解析（语法接受 ≠ 语义合法清单同族）
    if syntax::in_specifier_context(ident) {
        return None;
    }
    let name = intern_sym(syntax::text(ident, src));
    let ident_span = syntax::span(ident);
    let ctx = SemCtx::from_ancestors(ident, src, idx, file);

    // 声明自身（名字即锚点）：类/函数/字段声明名、局部/形参声明名
    if let Some(def) = def_id_at_decl(idx, file, ident_span, name) {
        return Some(Resolution { targets: vec![Target::Def(def)], level: LEVEL_DECL_SELF });
    }
    if let Some(l) = ctx.locals.iter().find(|l| l.name_span == ident_span).cloned() {
        return Some(Resolution { targets: vec![Target::Local(l)], level: LEVEL_DECL_SELF });
    }

    let parent = ident.parent()?;
    let mut res = match role_of(ident, parent) {
        Role::TypeUse => resolve_type_use(idx, &ctx, name),
        Role::ScopedFirst => {
            // `Super::` 首段 = 显式父类（§4.5 第 0 级）
            let s = sym_str(name);
            if s == "Super" || s == "super" {
                return ctx
                    .type_def
                    .and_then(|t| idx.closures.get(&t).and_then(|c| c.first().copied()))
                    .map(|b| Resolution { targets: vec![Target::Def(b)], level: LEVEL_THIS_SUPER });
            }
            resolve_scoped_first(idx, name)
        }
        Role::ScopedLast { scope } => {
            let scope_name = intern_sym(syntax::text(scope, src));
            resolve_scoped_last(idx, &ctx, scope_name, name)
        }
        Role::MemberProperty { object } => {
            let recv = crate::expr::expr_type(idx, &ctx, src, object).map(|t| t.base);
            resolve_member(idx, &ctx, recv, name, /*scoped=*/false)
        }
        Role::Callee { call } => {
            let argc = argument_count(call);
            resolve_callee(idx, &ctx, name, argc)
        }
        Role::Plain => resolve_plain(idx, &ctx, name),
    };
    // M4 消歧（架构设计 §4.6）：调用点上的重载组——arity + 可定型实参
    // （字面量 / 标识符 / 链式成员）能唯一命中时收敛为单目标；不能则原样
    // 保留（消歧失败报全部重载）。运算符 / f-string 插值 / range-for 的
    // 实参定型留 M5（与 signatureHelp 同批）。
    if let Some(r) = res.as_mut() {
        if r.targets.len() > 1 {
            if let Some(call) = enclosing_call(ident) {
                disambiguate_in_call(idx, &ctx, src, call, r);
            }
        }
    }
    res
}

/// 光标处的 identifier 叶子（byte ∈ [start, end)）。primitive 关键字
/// （float/int/…）是独立 token 种类，同样可解析（内建类型使用点）。
fn identifier_at<'t>(root: Node<'t>, byte: u32) -> Option<Node<'t>> {
    let mut node = root;
    loop {
        if node.kind() == "identifier" || node.kind() == "primitive_type" {
            return Some(node);
        }
        let next = syntax::children_with_fields(node)
            .into_iter()
            .find(|(_, c)| c.start_byte() as u32 <= byte && byte < c.end_byte() as u32);
        match next {
            Some((_, c)) => node = c,
            None => return None,
        }
    }
}

/// 声明自指（不限 kind，但排除合成）。
fn def_id_at_decl(idx: &WorkspaceIndex, file: FileId, span: TextRange, name: Sym) -> Option<DefId> {
    idx.main.get(&name)?.iter().copied().find(|&id| {
        let d = idx.def(id);
        !d.flags.contains(DefFlags::SYNTHETIC) && d.file == file && d.name_span == span
    })
}

fn argument_count(call: Node<'_>) -> usize {
    call.child_by_field_name("arguments")
        .map(|args| {
            syntax::children_with_fields(args)
                .into_iter()
                .filter(|(_, c)| c.kind() == "argument")
                .count()
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 调用点重载消歧（M4，overload::disambiguate 的实参侧）
// ---------------------------------------------------------------------------

/// 标识符是某 call_expression 的 callee（裸 / `A.B(...)` / `NS::F(...)` /
/// `A::B::C(...)` 尾段）。不是调用点（赋值 / 实参 / return）返回 None。
fn enclosing_call<'t>(ident: Node<'t>) -> Option<Node<'t>> {
    let parent = ident.parent()?;
    let is_call_function = |call: Node<'t>, target: Node<'t>| {
        syntax::children_with_fields(call)
            .into_iter()
            .any(|(f, c)| c.id() == target.id() && f.as_deref() == Some("function"))
    };
    match parent.kind() {
        "call_expression" if is_call_function(parent, ident) => Some(parent),
        "member_expression" | "qualified_identifier" | "scoped_name" => {
            let call = parent.parent()?;
            if call.kind() == "call_expression" && is_call_function(call, parent) {
                Some(call)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// 调用点重载消歧：只在「全部候选是可调用 Def」时尝试；结果不能唯一确定
/// 就不动（保留整组——「报全部重载」）。排序判定在 `overload::disambiguate`。
fn disambiguate_in_call(
    idx: &WorkspaceIndex,
    ctx: &SemCtx<'_>,
    src: &str,
    call: Node<'_>,
    r: &mut Resolution,
) {
    let defs: Option<Vec<DefId>> = r
        .targets
        .iter()
        .map(|t| match t {
            Target::Def(id) => Some(*id),
            Target::Local(_) => None,
        })
        .collect();
    let Some(defs) = defs else { return };
    if !defs.iter().all(|&id| {
        matches!(
            idx.def(id).kind,
            DefKind::Function
                | DefKind::Method
                | DefKind::Constructor
                | DefKind::Destructor
                | DefKind::Operator
        )
    }) {
        return;
    }
    let arg_bases = arg_type_bases(idx, ctx, src, call);
    if let Some(winner) = crate::overload::disambiguate(idx, &defs, &arg_bases) {
        r.targets = vec![Target::Def(winner)];
    }
}

fn arg_type_bases(
    idx: &WorkspaceIndex,
    ctx: &SemCtx<'_>,
    src: &str,
    call: Node<'_>,
) -> Vec<Option<DefId>> {
    // M5a：实参定型统一走 expr::expr_type（字面量 / 运算符 / f-string /
    // 链式成员 / 调用返回……全量子集，D28 欠账清偿）
    call.child_by_field_name("arguments")
        .map(|args| {
            syntax::children_with_fields(args)
                .into_iter()
                .filter(|(_, c)| c.kind() == "argument")
                .map(|(_, a)| crate::expr::expr_type(idx, ctx, src, a).map(|t| t.base))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 各角色解析
// ---------------------------------------------------------------------------

/// 类型位置的名字：内建 primitive（含 float 归一化）→ ns 链类型成员 → 全局类型。
fn resolve_type_use(idx: &WorkspaceIndex, ctx: &SemCtx, name: Sym) -> Option<Resolution> {
    // float 归一化（D25：从不落到名为 float 的 DefId）
    let key = if sym_str(name) == "float" {
        intern_sym(if idx.config.float_is_float64 { "float64" } else { "float32" })
    } else {
        name
    };
    if let Some(t) = builtin_target(idx, key) {
        return Some(Resolution { targets: vec![t], level: LEVEL_GLOBAL });
    }
    // ns 链上的类型成员（namespace 内声明的 class/struct/enum）
    for &ns in ctx.ns_defs.iter().rev() {
        let sym = idx.def(ns).name;
        for nsdef in namespaces_named(idx, sym) {
            let hits = members_named(idx, &[nsdef], name, |d| d.kind.is_type_decl());
            if !hits.is_empty() {
                return Some(Resolution {
                    targets: hits.into_iter().map(Target::Def).collect(),
                    level: LEVEL_NAMESPACE,
                });
            }
        }
    }
    // 全局类型
    let hits: Vec<DefId> = idx
        .main
        .get(&name)?
        .iter()
        .copied()
        .filter(|&id| {
            idx.def(id).kind.is_type_like() && !idx.def(id).flags.contains(DefFlags::SYNTHETIC)
        })
        .collect();
    (!hits.is_empty()).then(|| Resolution {
        targets: hits.into_iter().map(Target::Def).collect(),
        level: LEVEL_GLOBAL,
    })
}

/// `A::B` 的 A：namespace 优先（§2.2.1 推论 1 的语境择一：`X::` 限定 → namespace）。
fn resolve_scoped_first(idx: &WorkspaceIndex, name: Sym) -> Option<Resolution> {
    let nss = namespaces_named(idx, name);
    if !nss.is_empty() {
        return Some(Resolution {
            targets: nss.into_iter().map(Target::Def).collect(),
            level: LEVEL_NAMESPACE,
        });
    }
    // 无同名 namespace 时回落类型（enum：`EColor::Red`）
    let hits: Vec<DefId> = idx
        .main
        .get(&name)?
        .iter()
        .copied()
        .filter(|&id| {
            idx.def(id).kind.is_type_like() && !idx.def(id).flags.contains(DefFlags::SYNTHETIC)
        })
        .collect();
    (!hits.is_empty()).then(|| Resolution {
        targets: hits.into_iter().map(Target::Def).collect(),
        level: LEVEL_GLOBAL,
    })
}

/// `A::B` 的 B。
fn resolve_scoped_last(
    idx: &WorkspaceIndex,
    ctx: &SemCtx,
    scope_name: Sym,
    name: Sym,
) -> Option<Resolution> {
    // Super::Foo —— 显式父类调用（§4.5 第 0 级）
    let scope_str = sym_str(scope_name);
    if scope_str == "Super" || scope_str == "super" {
        let base = ctx
            .type_def
            .and_then(|t| idx.closures.get(&t).and_then(|c| c.first().copied()))?;
        let space = member_search_space(idx, base);
        let hits = members_named(idx, &space, name, |_| true);
        return (!hits.is_empty()).then(|| Resolution {
            targets: hits.into_iter().map(Target::Def).collect(),
            level: LEVEL_THIS_SUPER,
        });
    }

    // namespace 成员（同名聚合，含合成 StaticClass 命名空间）
    let nss = namespaces_named(idx, scope_name);
    if !nss.is_empty() {
        let hits: Vec<DefId> = nss
            .iter()
            .flat_map(|&ns| idx.members.get(&ns).map(|ms| ms.iter().copied()).unwrap_or_default())
            .filter(|&m| idx.def(m).name == name)
            .collect();
        if !hits.is_empty() {
            return Some(Resolution {
                targets: hits.into_iter().map(Target::Def).collect(),
                level: LEVEL_NAMESPACE,
            });
        }
        return None;
    }

    // enum 成员（`EColor::Red`——裸值不可用，asEP_REQUIRE_ENUM_SCOPE=1）
    if let Some(enm) = idx
        .main
        .get(&scope_name)?
        .iter()
        .copied()
        .find(|&id| {
            idx.def(id).kind == DefKind::Enum && !idx.def(id).flags.contains(DefFlags::SYNTHETIC)
        })
    {
        let hits = members_named(idx, &[enm], name, |d| d.kind == DefKind::EnumValue);
        return (!hits.is_empty()).then(|| Resolution {
            targets: hits.into_iter().map(Target::Def).collect(),
            level: LEVEL_GLOBAL,
        });
    }
    None
}

/// 成员访问（`recv.Name` 或隐式 this 的裸 Name）：
/// 2 成员 → 3 访问器 → 4 mixin（显式接收者或隐式 this 皆可走 mixin，§4.5.1 条 2）。
fn resolve_member(
    idx: &WorkspaceIndex,
    ctx: &SemCtx,
    recv: Option<DefId>,
    name: Sym,
    scoped: bool,
) -> Option<Resolution> {
    // 对象上下文：显式接收者优先，否则隐式 this。**类体 default 语句也是类
    // 语境**（type_def 即够——引擎在 default 语句里同样按类成员解析）；
    // 全局函数体内 type_def 为 None → 无对象上下文（mixin 准入条件 2）
    let recv = recv.or_else(|| ctx.type_def)?;
    let space = member_search_space(idx, recv);

    // 2. 成员（class 沿闭包 / struct 单层 / delegate·event 展开集）
    let hits = members_named(idx, &space, name, |_| true);
    if !hits.is_empty() {
        return Some(Resolution {
            targets: hits.into_iter().map(Target::Def).collect(),
            level: LEVEL_MEMBER,
        });
    }
    // 3. 属性访问器（反向默认：不带 NOT_PROPERTY 即候选；读写侧区分留给 M5）
    let acc = find_accessors(idx, &space, name);
    if !acc.is_empty() {
        return Some(Resolution {
            targets: acc.into_iter().map(Target::Def).collect(),
            level: LEVEL_ACCESSOR,
        });
    }
    // 4. mixin fallback（条件 3：带 `::` 限定不走）
    if !scoped {
        let mixins = mixin_candidates(idx, recv, name, &ctx.ns_syms);
        if !mixins.is_empty() {
            return Some(Resolution {
                targets: mixins.into_iter().map(Target::Def).collect(),
                level: LEVEL_MIXIN,
            });
        }
    }
    None
}

/// 裸标识符（非调用）：0 this/super → 1 局部 → 2/3 成员(隐式 this) → 5/6。
pub(crate) fn resolve_plain(idx: &WorkspaceIndex, ctx: &SemCtx, name: Sym) -> Option<Resolution> {
    let name_str = sym_str(name);

    // 0. this / super（仅类/struct 方法体内；this 在静态/命名空间函数中不存在）
    if name_str == "this" {
        if let (Some(t), Some(_)) = (ctx.type_def, ctx.fn_def) {
            return Some(Resolution { targets: vec![Target::Def(t)], level: LEVEL_THIS_SUPER });
        }
        return None;
    }
    if name_str == "Super" || name_str == "super" {
        let base = ctx
            .type_def
            .and_then(|t| idx.closures.get(&t).and_then(|c| c.first().copied()))?;
        return Some(Resolution { targets: vec![Target::Def(base)], level: LEVEL_THIS_SUPER });
    }

    // 1. 局部（内层优先）
    if let Some(l) = ctx.locals.iter().rev().find(|l| l.name == name) {
        return Some(Resolution { targets: vec![Target::Local(l.clone())], level: LEVEL_LOCAL });
    }

    // 2/3. 成员 + 访问器（隐式 this；mixin 不在裸标识符路径——引擎 mixin 块
    // 在函数调用编译路径，非调用的裸标识符不查）
    if ctx.type_def.is_some() {
        if let Some(r) = resolve_member(idx, ctx, None, name, false) {
            if r.level == LEVEL_MEMBER || r.level == LEVEL_ACCESSOR {
                return Some(r);
            }
        }
    }

    // 5. 命名空间链（ns 里的函数/变量可无限定使用）
    for &ns in ctx.ns_defs.iter().rev() {
        let sym = idx.def(ns).name;
        for nsdef in namespaces_named(idx, sym) {
            let hits = members_named(idx, &[nsdef], name, |d| {
                matches!(d.kind, DefKind::Function | DefKind::GlobalVar)
            });
            if !hits.is_empty() {
                return Some(Resolution {
                    targets: hits.into_iter().map(Target::Def).collect(),
                    level: LEVEL_NAMESPACE,
                });
            }
        }
    }

    // 6. 全局符号 / 类型本身
    let hits: Vec<DefId> = idx
        .main
        .get(&name)?
        .iter()
        .copied()
        .filter(|&id| visible_global(idx, id, ctx))
        .collect();
    (!hits.is_empty()).then(|| Resolution {
        targets: hits.into_iter().map(Target::Def).collect(),
        level: LEVEL_GLOBAL,
    })
}

/// 调用 callee（裸标识符）：1 局部 → 2/3 方法(隐式 this) → 4 mixin → 5/6。
/// 与 plain 的差别：mixin 生效（引擎 :13436-13449——方法体内隐式压 this）。
pub(crate) fn resolve_callee(
    idx: &WorkspaceIndex,
    ctx: &SemCtx,
    name: Sym,
    _argc: usize,
) -> Option<Resolution> {
    let name_str = sym_str(name);

    if name_str == "this" || name_str == "Super" || name_str == "super" {
        return resolve_plain(idx, ctx, name);
    }

    // 1. 局部（delegate 局部变量的调用）
    if let Some(l) = ctx.locals.iter().rev().find(|l| l.name == name) {
        return Some(Resolution { targets: vec![Target::Local(l.clone())], level: LEVEL_LOCAL });
    }

    // 2/3/4. 成员 → 访问器 → mixin（隐式 this 场景）
    if ctx.type_def.is_some() {
        if let Some(r) = resolve_member(idx, ctx, None, name, false) {
            if r.level <= LEVEL_MIXIN {
                return Some(r);
            }
        }
    }

    // 5. 命名空间链
    for &ns in ctx.ns_defs.iter().rev() {
        let sym = idx.def(ns).name;
        for nsdef in namespaces_named(idx, sym) {
            let hits = members_named(idx, &[nsdef], name, |d| d.kind == DefKind::Function);
            if !hits.is_empty() {
                return Some(Resolution {
                    targets: hits.into_iter().map(Target::Def).collect(),
                    level: LEVEL_NAMESPACE,
                });
            }
        }
    }

    // 6. 全局函数 / 类型（构造调用 `FVector(1,2,3)` 落到类型本身）
    let hits: Vec<DefId> = idx
        .main
        .get(&name)?
        .iter()
        .copied()
        .filter(|&id| visible_global(idx, id, ctx))
        .filter(|&id| {
            matches!(
                idx.def(id).kind,
                DefKind::Function
                    | DefKind::GlobalVar
                    | DefKind::Delegate
                    | DefKind::Event
                    | DefKind::Class
                    | DefKind::Struct
                    | DefKind::Enum
            )
        })
        .collect();
    (!hits.is_empty()).then(|| Resolution {
        targets: hits.into_iter().map(Target::Def).collect(),
        level: LEVEL_GLOBAL,
    })
}

/// 全局可见性过滤：顶层（parent None）+ local 模块隔离 + 排除合成。
fn visible_global(idx: &WorkspaceIndex, id: DefId, ctx: &SemCtx) -> bool {
    let d = idx.def(id);
    if d.flags.contains(DefFlags::SYNTHETIC) || d.parent.is_some() {
        return false;
    }
    if d.flags.contains(DefFlags::LOCAL) {
        // local 函数：仅声明所在模块可见（BNF §2.6）。模块信息缺失时不过滤
        // （单测/无 Phase 0 场景），两侧都有模块且不同才隐藏
        match (idx.modules.get(&d.file), idx.modules.get(&ctx.file)) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    } else {
        true
    }
}

// ---------------------------------------------------------------------------
// 成员 / 访问器 / mixin 查找原语
// ---------------------------------------------------------------------------

/// 成员查找的逐层序列（近者在前）：class = self + 继承闭包；struct 单层（D16）；
/// delegate/event = 展开成员集（M3b）。内建 primitive 无成员。
pub(crate) fn member_search_space(idx: &WorkspaceIndex, def: DefId) -> Vec<DefId> {
    let d = idx.def(def);
    if d.flags.contains(DefFlags::SYNTHETIC) {
        return vec![def];
    }
    match d.kind {
        DefKind::Class => {
            let mut space = vec![def];
            if let Some(chain) = idx.closures.get(&def) {
                space.extend(chain.iter().copied());
            }
            space
        }
        _ => vec![def],
    }
}

pub(crate) fn members_named(
    idx: &WorkspaceIndex,
    space: &[DefId],
    name: Sym,
    pred: impl Fn(&DefData) -> bool,
) -> Vec<DefId> {
    space
        .iter()
        .flat_map(|&t| idx.members.get(&t).map(|ms| ms.iter().copied()).unwrap_or_default())
        .filter(|&m| {
            let d = idx.def(m);
            d.name == name && pred(d)
        })
        .collect()
}

/// 属性访问器模拟（§4.5 第 3 级）：Get<Name>/Set<Name>。
/// 反向默认——不带 NOT_PROPERTY 即候选（§2.4.5）。
/// 命名匹配两种拼写：`Get` + 原名 / `Get` + 首字母大写。
pub(crate) fn find_accessors(idx: &WorkspaceIndex, space: &[DefId], name: Sym) -> Vec<DefId> {
    let name_str = sym_str(name);
    let mut keys: Vec<Sym> = Vec::with_capacity(4);
    for prefix in ["Get", "Set"] {
        keys.push(intern_sym(&format!("{prefix}{name_str}")));
        let mut cs = name_str.chars();
        if let Some(c) = cs.next() {
            let capped = c.to_uppercase().collect::<String>() + cs.as_str();
            keys.push(intern_sym(&format!("{prefix}{capped}")));
        }
    }
    let mut out = Vec::new();
    for key in keys {
        for m in members_named(idx, space, key, |_| true) {
            let d = idx.def(m);
            if !d.flags.contains(DefFlags::NOT_PROPERTY)
                && matches!(d.kind, DefKind::Method | DefKind::Function)
                && !out.contains(&m)
            {
                out.push(m);
            }
        }
    }
    out
}

/// mixin 倒排查询（D23）：沿接收者的继承闭包逐级查（键不预展开到子类），
/// 且必须**同名**（引擎 `as_compiler.cpp:13387-13450` 查的是该名字的 mixin）。
/// 准入条件 4：mixin 自身所在命名空间须在当前位置的命名空间链上（或全局）。
/// shadow 语义（AS 脚本类 shadow C++ 类）M3 不建模——继承侧已覆盖脚本语料。
fn mixin_candidates(idx: &WorkspaceIndex, recv: DefId, name: Sym, ns_syms: &[Sym]) -> Vec<DefId> {
    let mut chain = vec![recv];
    if let Some(cl) = idx.closures.get(&recv) {
        chain.extend(cl.iter().copied());
    }
    let mut out = Vec::new();
    for base in chain {
        if let Some(ms) = idx.mixin_index.get(&base) {
            for &m in ms {
                if idx.def(m).name != name {
                    continue;
                }
                let ns_ok = match idx.def(m).parent {
                    None => true, // 全局 mixin：任何命名空间链终点都查到
                    Some(ns) => ns_syms.contains(&idx.def(ns).name),
                };
                if ns_ok && !out.contains(&m) {
                    out.push(m);
                }
            }
        }
    }
    out
}

/// 同名 namespace 聚合（跨文件 + 合成 StaticClass 命名空间）。
fn namespaces_named(idx: &WorkspaceIndex, name: Sym) -> Vec<DefId> {
    idx.main
        .get(&name)
        .map(|ds| {
            ds.iter()
                .copied()
                .filter(|&id| idx.def(id).kind == DefKind::Namespace)
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn builtin_target(idx: &WorkspaceIndex, name: Sym) -> Option<Target> {
    idx.main
        .get(&name)?
        .iter()
        .copied()
        .find(|&id| {
            let d = idx.def(id);
            d.flags.contains(DefFlags::SYNTHETIC) && d.kind == DefKind::Struct
        })
        .map(Target::Def)
}

// ---------------------------------------------------------------------------
// 表达式定型：已迁出至 crate::expr（M5a 完整化——字面量 / 运算符重载 /
// f-string / range-for 双跳 / 模板实参替换 / auto 惰性定型）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IndexConfig;
    use crate::index::{FileInput, FileKind};
    use crate::intern::intern_file;

    fn build(srcs: &[(&str, &str)]) -> WorkspaceIndex {
        let inputs = srcs
            .iter()
            .map(|(path, src)| FileInput {
                file: intern_file(path, 0),
                kind: if path.ends_with(".d.as") { FileKind::Decl } else { FileKind::Script },
                source: (*src).to_string(),
                module: None,
            })
            .collect();
        WorkspaceIndex::build(IndexConfig::default(), inputs)
    }

    /// 用例源码全部内置（AGENTS.md 硬性规则 / D1）。
    /// `off` 的 needle 必须**以目标标识符开头**（find 返回起始位置）。
    fn off(src: &str, needle: &str) -> u32 {
        src.find(needle).unwrap_or_else(|| panic!("定位标记缺失: {needle}")) as u32
    }

    /// 第 n 次出现（1-based）的起始偏移——声明先于使用时的歧义消解。
    fn nth(src: &str, needle: &str, n: usize) -> u32 {
        match src.match_indices(needle).nth(n - 1) {
            Some((i, _)) => i as u32,
            None => panic!("'{needle}' 第 {n} 次出现不存在"),
        }
    }

    fn file_of(path: &str) -> FileId {
        intern_file(path, 0)
    }

    fn first_name(idx: &WorkspaceIndex, r: &Resolution) -> String {
        r.targets[0].name(idx).to_string()
    }

    // ------------------------------------------------------------------
    // 0-6 级命中序
    // ------------------------------------------------------------------

    const CHAIN: &str = "\
class CBase
{
    int Field;
    int GetBaseValue() { return 0; }
    void BaseFn() {}
}
class CDerived : CBase
{
    int GetDerivedValue() { return 1; }
    void Run(int Param)
    {
        int Local = 1;
        int Shadow = 2;
        int A = Shadow;
        int B = Field;
        int B2 = BaseFn();
        int C = DerivedValue;
        int D = this.Field;
        int E = GlobalVar;
        int F = Param;
    }
}
int Shadow;
int GlobalVar;
";

    #[test]
    fn chain_level_ordering() {
        let idx = build(&[("unique://res/chain.as", CHAIN)]);
        let file = file_of("unique://res/chain.as");

        let case_at = |byte: u32, want_level: u8, want_name: &str| {
            let r = resolve_at(&idx, file, byte).expect("应命中");
            assert_eq!(r.level, want_level, "级数");
            assert_eq!(first_name(&idx, &r), want_name, "命中名");
        };

        // 1：同名局部遮蔽全局（"Shadow;" 首次出现 = 使用点；声明在文件尾）
        case_at(off(CHAIN, "Shadow;"), LEVEL_LOCAL, "Shadow");
        // 2：成员（本类没有、父类有——沿闭包上溯；第 2 次出现 = 使用点）
        case_at(nth(CHAIN, "Field;", 2), LEVEL_MEMBER, "Field");
        // 2：父类方法
        case_at(nth(CHAIN, "BaseFn(", 2), LEVEL_MEMBER, "BaseFn");
        // 3：访问器（DerivedValue 无字段，GetDerivedValue 存在；第 2 次 = 使用点）
        case_at(nth(CHAIN, "DerivedValue", 2), LEVEL_ACCESSOR, "GetDerivedValue");
        // 0：this
        case_at(off(CHAIN, "this."), LEVEL_THIS_SUPER, "CDerived");
        // 6：全局（使用点在声明之前——类先于全局区）
        case_at(off(CHAIN, "GlobalVar;"), LEVEL_GLOBAL, "GlobalVar");
        // 1：形参（"Param;" 仅使用点）
        case_at(off(CHAIN, "Param;"), LEVEL_LOCAL, "Param");
        // 声明自身
        case_at(off(CHAIN, "Run(int Param"), LEVEL_DECL_SELF, "Run");
    }

    #[test]
    fn struct_single_layer_lookup() {
        // struct 单层（D16）：父 struct 的成员不可达
        const SRC: &str = "\
struct SBase { int BaseField; }
struct SChild { int ChildField; }
void F()
{
    SChild S;
    int A = S.ChildField;
    int B = S.BaseField;
}
";
        let idx = build(&[("unique://res/struct.as", SRC)]);
        let file = file_of("unique://res/struct.as");
        let r = resolve_at(&idx, file, nth(SRC, "ChildField", 2)).unwrap();
        assert_eq!(r.level, LEVEL_MEMBER);
        assert_eq!(first_name(&idx, &r), "ChildField");
        assert!(
            resolve_at(&idx, file, nth(SRC, "BaseField", 2)).is_none(),
            "struct 成员查找单层，父 struct 不可达"
        );
    }

    #[test]
    fn accessor_reverse_default() {
        // 反向默认（§2.4.5）：不带 NOT_PROPERTY 即访问器候选；带 tag 的不是
        const SRC: &str = "\
class A
{
    // @notProperty
    int GetX() { return 0; }
    int GetY() { return 1; }
    void M() { int A = X; int B = Y; }
}
";
        let idx = build(&[("unique://res/acc.d.as", SRC)]);
        let file = file_of("unique://res/acc.d.as");
        // Y → GetY（无 tag，默认是访问器）
        let r = resolve_at(&idx, file, off(SRC, "Y;")).unwrap();
        assert_eq!(r.level, LEVEL_ACCESSOR);
        assert_eq!(first_name(&idx, &r), "GetY");
        // X → GetX 带 @notProperty，不是访问器 → 不命中
        assert!(resolve_at(&idx, file, off(SRC, "X;")).is_none());
    }

    // ------------------------------------------------------------------
    // mixin 五条准入条件（引擎 as_compiler.cpp:13387-13450）
    // ------------------------------------------------------------------

    const MIXIN: &str = "\
class AActor {}
class APawn : AActor {}
namespace NS
{
    void Heal(AActor Target) {}
    mixin void NSMixin(AActor Target) {}
}
namespace NSInner
{
    mixin void InnerHeal(AActor Target, float Amount) {}
}
mixin void Heal(AActor Target, float Amount) {}
mixin void GlobalHeal(APawn Target, float Amount) {}
class C1 : AActor
{
    void RealHeal(float Amount) {}
    void M()
    {
        Heal(1.0);
        RealHeal(1.0);
        NSMixin(1.0);
        InnerHeal(1.0);
        GlobalHeal(1.0);
        NS::Heal(this);
    }
}
class C3 : APawn
{
    void M() { GlobalHeal(1.0); }
}
class C2
{
    void M2() { Heal(2.0); }
}
void GlobalFn() { Heal(null); }
";

    #[test]
    fn mixin_five_admission_conditions() {
        let idx = build(&[("unique://res/mixin.as", MIXIN)]);
        let file = file_of("unique://res/mixin.as");

        // ① 真实成员优先于 mixin（短路，绝不合并候选）
        let r = resolve_at(&idx, file, off(MIXIN, "RealHeal(1.0);")).unwrap();
        assert_eq!(r.level, LEVEL_MEMBER, "真实成员命中，绝不查 mixin");
        assert_eq!(first_name(&idx, &r), "RealHeal");

        // ②+⑤：方法体内隐式 this；全局 mixin 首参 AActor 是 C1 祖先 → 命中
        let r = resolve_at(&idx, file, off(MIXIN, "Heal(1.0);")).unwrap();
        assert_eq!(r.level, LEVEL_MIXIN, "隐式 this + 首参 AActor ∈ C1 闭包");
        assert_eq!(first_name(&idx, &r), "Heal");

        // ④ 反例 A：NS 的 mixin 不在 C1 的（空）命名空间链上 → 不命中
        assert!(
            resolve_at(&idx, file, off(MIXIN, "NSMixin(1.0)")).is_none(),
            "NS 的 mixin 不对全局位置可见"
        );
        // ④ 反例 B：NSInner 的 mixin 同理
        assert!(
            resolve_at(&idx, file, off(MIXIN, "InnerHeal(1.0)")).is_none(),
            "NSInner 的 mixin 不对全局位置可见"
        );

        // ⑤ 反例：GlobalHeal 首参 APawn 不是 C1（祖先链 AActor）的祖先 →
        // 不落在 MIXIN 级；mixin 本身是全局函数（D23），落到 GLOBAL 级
        let r = resolve_at(&idx, file, off(MIXIN, "GlobalHeal(1.0)")).unwrap();
        assert_eq!(r.level, LEVEL_GLOBAL, "首参不是接收者祖先 → 非 mixin 命中");
        assert_eq!(first_name(&idx, &r), "GlobalHeal");
        // ⑤ 正例：C3 : APawn，GlobalHeal 首参 APawn 是 C3 直接父类 → 命中（第 2 次出现）
        let r = resolve_at(&idx, file, nth(MIXIN, "GlobalHeal(1.0);", 2)).unwrap();
        assert_eq!(r.level, LEVEL_MIXIN);
        assert_eq!(first_name(&idx, &r), "GlobalHeal");

        // ③ scope 限定不走 mixin：`NS::Heal` 是 namespace 成员查找
        let r = resolve_at(&idx, file, off(MIXIN, "Heal(this")).unwrap();
        assert_eq!(r.level, LEVEL_NAMESPACE, "带 :: 限定走 namespace 成员");
        assert_eq!(first_name(&idx, &r), "Heal");

        // ⑤ 反例：首参 AActor 不是 C2 的祖先 → 非 mixin，落到 GLOBAL（全局函数本体）
        let r = resolve_at(&idx, file, off(MIXIN, "Heal(2.0)")).unwrap();
        assert_eq!(r.level, LEVEL_GLOBAL, "首参不是接收者祖先 → 非 mixin 命中");

        // ② 反例：全局函数体内无 this → 不查 mixin（同样落到 GLOBAL）
        let r = resolve_at(&idx, file, off(MIXIN, "Heal(null")).unwrap();
        assert_eq!(r.level, LEVEL_GLOBAL, "全局函数体内无对象上下文 → 非 mixin 命中");
    }

    #[test]
    fn mixin_first_param_ancestor_hits() {
        // ⑤ 正例：首参为接收者祖先类时命中（receiver C，mixin 首参 AActor 是其祖先）
        const SRC: &str = "\
class AActor {}
class APawn : AActor {}
mixin void Heal(AActor Target, float Amount) {}
class C : APawn
{
    void M() { Heal(1.0); }
}
";
        let idx = build(&[("unique://res/mixanc.as", SRC)]);
        let file = file_of("unique://res/mixanc.as");
        let r = resolve_at(&idx, file, off(SRC, "Heal(1.0)")).unwrap();
        assert_eq!(r.level, LEVEL_MIXIN);
        assert_eq!(first_name(&idx, &r), "Heal");
    }

    #[test]
    fn mixin_explicit_receiver_and_ns_visibility() {
        // 显式接收者 + 命名空间链：ns 内位置可见该 ns 声明的 mixin
        // （Holder 须是 AActor 子类——隐式 this 的接收者是 Holder）
        const SRC: &str = "\
class AActor {}
namespace Lib
{
    mixin void Boost(AActor Target) {}
    class Holder : AActor
    {
        void M() { AActor A; A.Boost(); Boost(null); }
    }
}
";
        let idx = build(&[("unique://res/mixns.as", SRC)]);
        let file = file_of("unique://res/mixns.as");
        // 显式接收者：A.Boost() → mixin（AActor 上无 Boost 成员；第 2 次 = 使用点）
        let r = resolve_at(&idx, file, nth(SRC, "Boost(", 2)).unwrap();
        assert_eq!(r.level, LEVEL_MIXIN);
        assert_eq!(first_name(&idx, &r), "Boost");
        // Lib 内类的方法体：ns 链含 Lib → 隐式 this 的 Boost 命中
        let r = resolve_at(&idx, file, off(SRC, "Boost(null")).unwrap();
        assert_eq!(r.level, LEVEL_MIXIN);
    }

    // ------------------------------------------------------------------
    // super / 命名空间 / 类型 / 表达式定型子集
    // ------------------------------------------------------------------

    #[test]
    fn super_resolves_to_direct_base() {
        const SRC: &str = "\
class A { void BeginPlay() {} }
class B : A
{
    void BeginPlay() { Super::BeginPlay(); }
}
";
        let idx = build(&[("unique://res/super.as", SRC)]);
        let file = file_of("unique://res/super.as");
        // Super → 直接父类 A
        let r = resolve_at(&idx, file, off(SRC, "Super::")).unwrap();
        assert_eq!(r.level, LEVEL_THIS_SUPER);
        assert_eq!(first_name(&idx, &r), "A");
        // Super::BeginPlay → A 的成员（"BeginPlay();" 仅使用点——声明是 "BeginPlay() {"）
        let r = resolve_at(&idx, file, off(SRC, "BeginPlay();")).unwrap();
        assert_eq!(r.level, LEVEL_THIS_SUPER);
        assert_eq!(first_name(&idx, &r), "BeginPlay");
    }

    #[test]
    fn namespace_chain_and_static_class() {
        const SRC: &str = "\
namespace FVector
{
    float64 ZeroVector;
    namespace Inner { float64 Epsilon; }
}
void F()
{
    float64 A = FVector::ZeroVector;
    float64 B = FVector::Inner::Epsilon;
    UClass C = AActor::StaticClass();
}
class AActor {}
";
        let idx = build(&[("unique://res/ns.as", SRC)]);
        let file = file_of("unique://res/ns.as");

        let r = resolve_at(&idx, file, nth(SRC, "ZeroVector", 2)).unwrap();
        assert_eq!(r.level, LEVEL_NAMESPACE);
        assert_eq!(first_name(&idx, &r), "ZeroVector");
        // 合成 StaticClass（M3b）：class 同名 namespace 聚合命中
        let r = resolve_at(&idx, file, off(SRC, "StaticClass")).unwrap();
        assert_eq!(r.level, LEVEL_NAMESPACE);
        assert_eq!(first_name(&idx, &r), "StaticClass");
        // 嵌套 namespace 首段
        let r = resolve_at(&idx, file, off(SRC, "Inner::")).unwrap();
        assert_eq!(first_name(&idx, &r), "Inner");
    }

    #[test]
    fn enum_scoped_value_access() {
        const SRC: &str = "\
enum EColor { Red, Green }
void F()
{
    EColor C = EColor::Red;
    EColor D = Red;
}
";
        let idx = build(&[("unique://res/enum.as", SRC)]);
        let file = file_of("unique://res/enum.as");
        let r = resolve_at(&idx, file, nth(SRC, "Red", 2)).unwrap();
        assert_eq!(r.level, LEVEL_GLOBAL);
        assert_eq!(first_name(&idx, &r), "Red");
        // 裸值不可用（asEP_REQUIRE_ENUM_SCOPE=1；"Red" 第 3 次 = 裸使用点）
        assert!(resolve_at(&idx, file, nth(SRC, "Red", 3)).is_none());
    }

    #[test]
    fn type_use_and_ctor_call() {
        const SRC: &str = "\
struct FVector { float X; }
void F()
{
    FVector V = FVector(1.0, 2.0, 3.0);
    float A = V.X;
}
";
        let idx = build(&[("unique://res/ty.as", SRC)]);
        let file = file_of("unique://res/ty.as");
        // 类型位置
        let r = resolve_at(&idx, file, off(SRC, "FVector V")).unwrap();
        assert_eq!(r.level, LEVEL_GLOBAL);
        assert_eq!(first_name(&idx, &r), "FVector");
        // 构造调用 callee → 类型本身
        let r = resolve_at(&idx, file, off(SRC, "FVector(1.0")).unwrap();
        assert_eq!(first_name(&idx, &r), "FVector");
        // 成员链：V.X（局部 V 的声明类型 → FVector 成员；"X;" 第 2 次 = 使用点）
        let r = resolve_at(&idx, file, nth(SRC, "X;", 2)).unwrap();
        assert_eq!(r.level, LEVEL_MEMBER);
        assert_eq!(first_name(&idx, &r), "X");
    }

    #[test]
    fn chained_member_typing() {
        // expr_type 最小子集：this → 方法返回 → 再成员
        const SRC: &str = "\
struct FVector { float X; }
struct FTransform { FVector GetLocation() { FVector V; return V; } }
class A
{
    FTransform GetActorTransform() { FTransform T; return T; }
    void M()
    {
        float L = this.GetActorTransform().GetLocation().X;
    }
}
";
        let idx = build(&[("unique://res/chain2.as", SRC)]);
        let file = file_of("unique://res/chain2.as");
        let r = resolve_at(&idx, file, nth(SRC, "GetLocation(", 2)).unwrap();
        assert_eq!(r.level, LEVEL_MEMBER);
        assert_eq!(first_name(&idx, &r), "GetLocation");
        let r = resolve_at(&idx, file, nth(SRC, "X;", 2)).unwrap();
        assert_eq!(r.level, LEVEL_MEMBER);
        assert_eq!(first_name(&idx, &r), "X");
    }

    #[test]
    fn local_function_module_isolation() {
        // local 函数：仅声明所在模块可见（模块信息缺失时不过滤——服务端带模块时
        // 同文件必然同模块；跨模块过滤的完整用例随 M4 workspace 侧落地）
        const SRC: &str = "\
local void Helper() {}
void User() { Helper(); }
";
        let idx = build(&[("unique://res/local.as", SRC)]);
        let file = file_of("unique://res/local.as");
        let r = resolve_at(&idx, file, off(SRC, "Helper();")).unwrap();
        assert_eq!(r.level, LEVEL_GLOBAL);
        assert_eq!(first_name(&idx, &r), "Helper");
        assert!(idx
            .def(match r.targets[0] {
                Target::Def(d) => d,
                _ => unreachable!(),
            })
            .flags
            .contains(DefFlags::LOCAL));
    }

    #[test]
    fn delegate_expanded_member_resolution() {
        const SRC: &str = "\
delegate void FOnHit(int Damage);
class A
{
    FOnHit OnHit;
    void M() { OnHit.Execute(5); }
}
";
        let idx = build(&[("unique://res/dlg.as", SRC)]);
        let file = file_of("unique://res/dlg.as");
        let r = resolve_at(&idx, file, off(SRC, "Execute")).unwrap();
        assert_eq!(r.level, LEVEL_MEMBER);
        assert_eq!(first_name(&idx, &r), "Execute");
        // 定义落回委托声明（D10 origin 由 server 侧回落；此处直接命中合成成员）
    }

    #[test]
    fn builtin_type_use_resolves() {
        const SRC: &str = "\
void F()
{
    float X = 1.0;
    int Y = 2;
}
";
        let idx = build(&[("unique://res/builtin.as", SRC)]);
        let file = file_of("unique://res/builtin.as");
        // float 归一化（默认 float_is_float64=true → float64）
        let r = resolve_at(&idx, file, off(SRC, "float X")).unwrap();
        assert_eq!(first_name(&idx, &r), "float64");
        let r = resolve_at(&idx, file, off(SRC, "int Y")).unwrap();
        assert_eq!(first_name(&idx, &r), "int");
    }

    #[test]
    fn plain_identifier_in_global_function_has_no_this() {
        // 全局函数体内 this 不存在（§4.5 第 0 级约束）——用局部使用点验证
        const SRC: &str = "\
class A { int Field; }
void G() { int X = 1; int Y = X; }
";
        let idx = build(&[("unique://res/nothis.as", SRC)]);
        let file = file_of("unique://res/nothis.as");
        let r = resolve_at(&idx, file, off(SRC, "X;")).unwrap();
        assert_eq!(r.level, LEVEL_LOCAL);
        assert_eq!(first_name(&idx, &r), "X");
    }

    #[test]
    fn for_each_loop_variable_resolves_to_local() {
        // M4 修正：M3 的 "range_for_statement" 是不存在的节点 kind（死分支），
        // range-for 迭代变量此前不入局部帧。正确 kind 是 for_each_statement。
        const SRC: &str = "\
int[] Items;
void F()
{
    for (int Elem : Items) { int A = Elem; }
}
";
        let idx = build(&[("unique://res/foreach.as", SRC)]);
        let file = file_of("unique://res/foreach.as");
        // 循环体内的 Elem（第 2 次出现）→ 局部
        let r = resolve_at(&idx, file, nth(SRC, "Elem", 2)).unwrap();
        assert_eq!(r.level, LEVEL_LOCAL);
        assert_eq!(first_name(&idx, &r), "Elem");
        // 迭代变量声明自身 → DECL_SELF
        let r = resolve_at(&idx, file, off(SRC, "Elem :")).unwrap();
        assert_eq!(r.level, LEVEL_DECL_SELF);
        // range 表达式里的 Items（迭代变量声明点之前/之外）→ 全局
        let r = resolve_at(&idx, file, nth(SRC, "Items", 2)).unwrap();
        assert_eq!(r.level, LEVEL_GLOBAL);
        assert_eq!(first_name(&idx, &r), "Items");
    }
}
