//! 每文件摘要（LSP实现规划 Phase A / index-architecture.md §3，D37）。
//!
//! **纯函数**：`(tree, source, kind, module, cfg) → FileSummary`——不依赖
//! 任何全局状态（intern 只增表除外），全程可并行（rayon）。声明提取逻辑
//! **逐行平移自 `index.rs` 的 extract_decl 家族**（Phase A 双轨：旧侧原样
//! 服务查询，本模块做等价对账；Phase B 切换后旧侧删除）。
//!
//! 与旧 `WorkspaceIndex` 的结构差异（设计稿 §3.1）：
//! - `parent` 是**文件内局部 id**（u32），不是全局 DefId——文件重索引不
//!   影响其他文件；
//! - `DefExtra::TypeDecl` 的 bases / template_params **上提为 RawDecl 字段**
//!   （跨文件链接的原料，聚合层/查询期消费）；
//! - `origin` 不迁移（合成成员是查询期概念，Phase C）；
//! - 内建注入（inject_builtins）不迁移（L3 职责）；
//! - `scope_tree`：函数体内局部（Task 3 接真数据，本版恒为空树）。

use std::collections::HashMap;

use as_syntax::tree_sitter::Node;

use crate::decl_tags::{parse_comment_texts, SemanticTag, TagKind, TagValue};
use crate::config::IndexConfig;
use crate::expr::{is_auto_type, number_base_name};
use crate::id::Sym;
use crate::index::FileKind;
use crate::intern::intern_sym;
use crate::range::TextRange;
use crate::scope::{LocalDecl, LocalKind, Scope, ScopeTree};
use crate::symbol::{BaseRef, DefFlags, DefKind, ParamDecl};
use crate::syntax::{self, DeclCtx};
use crate::types::SynType;

/// 每文件摘要：声明表 + 文件内名字倒排 + 局部作用域树 + 文件头 tag。
#[derive(Debug, Clone)]
pub struct FileSummary {
    pub kind: FileKind,
    pub module: Option<Sym>,
    /// `.d.as` 文件头 `@group`（hover 归属显示，§2.2.1 推论 2）
    pub group: Option<String>,
    /// 文件头 `@cache_format`——识别但不消费（D21）
    pub cache_format: Option<u32>,
    /// 声明表：局部 id = 下标，文件内稳定（重索引不扰动其他文件）
    pub decls: Vec<RawDecl>,
    /// 名字 → 局部 id 倒排（含重载组，源码序）
    pub by_name: HashMap<Sym, Vec<u32>>,
    /// 局部作用域树（函数体内；本任务恒为空树，建树在后续任务接入）
    pub scope_tree: ScopeTree,
}

/// 一条声明（≈ `DefData` 去跨文件字段）。
#[derive(Debug, Clone)]
pub struct RawDecl {
    pub name: Sym,
    pub kind: DefKind,
    /// 名字 token（definition/rename/hover 锚点）
    pub name_span: TextRange,
    /// 整个声明
    pub full_span: TextRange,
    /// 文件内局部 id（类成员 → 类；namespace 成员 → namespace）
    pub parent: Option<u32>,
    /// 继承基名（class）——只记名字，不解析（跨文件链接留给聚合/查询期）
    pub bases: Vec<BaseRef>,
    /// 模板形参（`.d.as` 模板声明头）
    pub template_params: Vec<Sym>,
    pub extra: RawExtra,
    pub flags: DefFlags,
    pub doc: Option<Box<str>>,
    pub tags: Vec<SemanticTag>,
}

/// kind 特化数据（原 `DefExtra`，TypeDecl 内容上提为 RawDecl 字段）。
#[derive(Debug, Clone)]
pub enum RawExtra {
    None,
    /// 函数/方法/构造/析构/delegate/event：返回类型（语法层）+ 形参列表
    Callable {
        return_type: Option<SynType>,
        params: Vec<ParamDecl>,
    },
    /// 字段/全局变量/asset/虚属性：声明类型（语法层）
    Variable { ty: Option<SynType> },
    /// enum 成员：`= Expr` 原文
    EnumValue { value: Option<Box<str>> },
}

/// 提取入口（纯函数）。
pub fn extract_summary(
    tree: &as_syntax::tree_sitter::Tree,
    source: &str,
    kind: FileKind,
    module: Option<Sym>,
    cfg: &IndexConfig,
) -> FileSummary {
    let mut b = SummaryBuilder { decls: Vec::new(), by_name: HashMap::new() };

    // 文件头 tag（平移自 add_file；`.d.as` 固定 4 行；@cache_format 不消费——D21）
    let root = tree.root_node();
    let header = syntax::leading_comment_texts(root, source);
    let header_block = parse_comment_texts(&header);
    let group = header_block.tag(TagKind::Group).and_then(|t| match &t.value {
        TagValue::Text(s) => Some(s.clone()),
        _ => None,
    });
    let cache_format = header_block.tag(TagKind::CacheFormat).and_then(|t| match &t.value {
        TagValue::Int(n) => Some(*n),
        _ => None,
    });

    for (_field, child) in syntax::children_with_fields(root) {
        b.extract_decl(child, source, None, DeclCtx::Global);
    }

    // 局部作用域树：独立一趟（与声明提取正交——函数体局部不进 decls）
    let scopes = build_scopes(root, source, cfg);

    FileSummary {
        kind,
        module,
        group,
        cache_format,
        decls: b.decls,
        by_name: b.by_name,
        scope_tree: ScopeTree::from_scopes(scopes),
    }
}

// ---------------------------------------------------------------------------
// 语法层剥壳（聚合层 mixin 名字倒排用，§5.3——不经类型表）
// ---------------------------------------------------------------------------

/// `SynType` 逐层剥壳取基名（`FVector&` / `const FVector&in` / `T[]` /
/// 模板 → 各自的名字；`A::B` 取尾段）。Auto / Wildcard / Qualified 空段
/// → None。
pub fn syn_base_name(t: &SynType) -> Option<Sym> {
    match t {
        SynType::Named(n, _)
        | SynType::Primitive(n, _)
        | SynType::Template { name: n, .. } => Some(*n),
        SynType::Ref(inner, _)
        | SynType::Const(inner)
        | SynType::Array(inner)
        | SynType::UnresolvedObject(inner) => syn_base_name(inner),
        SynType::Qualified(segs) => segs.last().map(|(n, _)| *n),
        SynType::Auto | SynType::Wildcard => None,
    }
}

// ---------------------------------------------------------------------------
// scope_tree 构建 + auto 局部预推导（index-architecture §3.3）
// ---------------------------------------------------------------------------

/// 遍历全树找带体的函数/构造/析构声明，为每个建一棵 scope 子树（AS 无嵌套
/// 函数 ⇒ 各子树独立，根 scope span = 声明整体、含形参）。
fn build_scopes(root: Node<'_>, src: &str, cfg: &IndexConfig) -> Vec<Scope> {
    let mut w = ScopeWalker { src, cfg, scopes: Vec::new() };
    w.walk_root(root);
    w.scopes
}

struct ScopeWalker<'a> {
    src: &'a str,
    cfg: &'a IndexConfig,
    scopes: Vec<Scope>,
}

impl<'a> ScopeWalker<'a> {
    /// 递归找函数体起点（namespace / class 体内均可）。
    fn walk_root(&mut self, node: Node<'_>) {
        match node.kind() {
            "function_declaration" | "constructor_declaration" | "destructor_declaration" => {
                if let Some(body) = node.child_by_field_name("body") {
                    // 根 scope：span = 声明整体（形参在 body 外，必须被覆盖）
                    let decls = syntax::param_decls(node, self.src)
                        .into_iter()
                        .map(|p| LocalDecl {
                            name: p.name,
                            kind: LocalKind::Param,
                            name_span: p.span,
                            ty: p.ty,
                        })
                        .collect();
                    let idx = self.scopes.len() as u32;
                    self.scopes.push(Scope { parent: None, span: syntax::span(node), decls });
                    self.walk_stmts(body, idx);
                    return; // body 已走完，不再整体下潜
                }
            }
            _ => {}
        }
        for (_f, child) in syntax::children_with_fields(node) {
            self.walk_root(child);
        }
    }

    /// 语句级遍历：block / for-range 建新 scope，variable_declaration 记
    /// 局部（auto 走预推导），其余语句下潜（表达式内无 block，安全）。
    fn walk_stmts(&mut self, node: Node<'_>, scope: u32) {
        match node.kind() {
            "block" => {
                let idx = self.scopes.len() as u32;
                self.scopes.push(Scope {
                    parent: Some(scope),
                    span: syntax::span(node),
                    decls: Vec::new(),
                });
                for (_f, child) in syntax::children_with_fields(node) {
                    self.walk_stmts(child, idx);
                }
            }
            "for_each_statement" => {
                // 迭代变量 scope = 整个 for 语句（E 只在语句内可见，不泄漏外层）
                let declared = node
                    .child_by_field_name("type")
                    .and_then(|t| syntax::parse_syn_type(t, self.src));
                let mut decls = Vec::new();
                if let Some(n) = node.child_by_field_name("name") {
                    // auto 迭代变量：元素类型需查 range 表达式（跨文件）——保留 None
                    let ty = match &declared {
                        Some(t) if !is_auto_type(t) => declared.clone(),
                        _ => None,
                    };
                    decls.push(LocalDecl {
                        name: intern_sym(syntax::text(n, self.src)),
                        kind: LocalKind::IterVar,
                        name_span: syntax::span(n),
                        ty,
                    });
                }
                let idx = self.scopes.len() as u32;
                self.scopes.push(Scope { parent: Some(scope), span: syntax::span(node), decls });
                for (_f, child) in syntax::children_with_fields(node) {
                    self.walk_stmts(child, idx);
                }
            }
            "variable_declaration" => {
                let declared = node
                    .child_by_field_name("type")
                    .and_then(|t| syntax::parse_syn_type(t, self.src));
                for (_f, child) in syntax::children_with_fields(node) {
                    if child.kind() != "variable_declarator" {
                        continue;
                    }
                    let Some(name_node) = child.child_by_field_name("name") else { continue };
                    let value = child.child_by_field_name("value");
                    let ty = self.infer_var_ty(&declared, value, scope);
                    self.scopes[scope as usize].decls.push(LocalDecl {
                        name: intern_sym(syntax::text(name_node, self.src)),
                        kind: LocalKind::Var,
                        name_span: syntax::span(name_node),
                        ty,
                    });
                }
                // 初始化式是表达式（无 block / 无嵌套函数），不再下潜
            }
            _ => {
                for (_f, child) in syntax::children_with_fields(node) {
                    self.walk_stmts(child, scope);
                }
            }
        }
    }

    /// 局部变量定型：显式类型原样保留（含裸 `float`——归一化是 L3 消费期的
    /// 事，与字面量路径不同：字面量无源码类型名，直接给规范名）；
    /// `auto`（含 `auto&`）→ 初始化式预推导；推不出 = None（宁缺毋假）。
    fn infer_var_ty(
        &self,
        declared: &Option<SynType>,
        value: Option<Node<'_>>,
        scope: u32,
    ) -> Option<SynType> {
        let declared = declared.as_ref()?;
        if !is_auto_type(declared) {
            return Some(declared.clone());
        }
        self.infer_expr(value?, scope)
    }

    /// summary 期预推导（零全局依赖，产物一律 SynType 名字——加速器不是
    /// 真值源，L3 查询期可推翻重算）：
    /// - `Cast<T>(x)`：type 字段直接是 T（`'Cast'` 是匿名 token）
    /// - 字面量：数字后缀/进制与 D25 同规则（number_base_name）、
    ///   `"…"`/f-string/heredoc → FString、`n"…"` → FName、bool → bool
    /// - 裸标识符：复制同作用域链上已定型局部（顺序扫描同趟可见）
    /// - **不做**构造调用（`auto X = Ident(args)` 无法与函数调用语法区分，
    ///   宁缺毋假）；跨文件传播（成员链/调用返回）留 L3
    fn infer_expr(&self, node: Node<'_>, scope: u32) -> Option<SynType> {
        match node.kind() {
            "cast_expression" => node
                .child_by_field_name("type")
                .and_then(|t| syntax::parse_syn_type(t, self.src)),
            "parenthesized_expression" => {
                let inner = syntax::children_with_fields(node)
                    .into_iter()
                    .find(|(_, c)| c.is_named())
                    .map(|(_, c)| c)?;
                self.infer_expr(inner, scope)
            }
            "number" => {
                let n = number_base_name(syntax::text(node, self.src), self.cfg.float_is_float64);
                Some(SynType::Primitive(intern_sym(n), syntax::span(node)))
            }
            "string_literal" | "heredoc_string" | "format_string" => {
                Some(SynType::Named(intern_sym("FString"), syntax::span(node)))
            }
            "name_literal" => Some(SynType::Named(intern_sym("FName"), syntax::span(node))),
            "boolean_literal" => {
                Some(SynType::Primitive(intern_sym("bool"), syntax::span(node)))
            }
            "identifier" => {
                let name = intern_sym(syntax::text(node, self.src));
                resolve_in_chain(&self.scopes, scope, node.start_byte() as u32, name)
                    .and_then(|d| d.ty.clone())
            }
            _ => None,
        }
    }
}

/// 作用域链解析（建树过程中的复制推导用——树在建到一半，直接在 Vec 上走，
/// 语义与 `ScopeTree::resolve_local` 一致：链上最近者胜、声明点可见）。
fn resolve_in_chain<'s>(
    scopes: &'s [Scope],
    cur: u32,
    byte: u32,
    name: Sym,
) -> Option<&'s LocalDecl> {
    let mut i = cur;
    loop {
        let s = &scopes[i as usize];
        let mut hit: Option<&LocalDecl> = None;
        for d in &s.decls {
            if d.name == name && d.name_span.start <= byte {
                hit = Some(d);
            }
        }
        if let Some(d) = hit {
            return Some(d);
        }
        i = s.parent?;
    }
}

/// 提取器（builder 模式，无全局状态）。
struct SummaryBuilder {
    decls: Vec<RawDecl>,
    by_name: HashMap<Sym, Vec<u32>>,
}

impl SummaryBuilder {
    /// `push_def` 的平移：append decls + by_name（不写 main/members）。
    #[allow(clippy::too_many_arguments)]
    fn push_decl(
        &mut self,
        node: Node<'_>,
        name_node: Node<'_>,
        src: &str,
        kind: DefKind,
        parent: Option<u32>,
        flags: DefFlags,
        bases: Vec<BaseRef>,
        template_params: Vec<Sym>,
        extra: RawExtra,
        doc: Option<String>,
        tags: Vec<SemanticTag>,
    ) -> u32 {
        let name = intern_sym(syntax::text(name_node, src));
        let id = self.decls.len() as u32;
        self.decls.push(RawDecl {
            name,
            kind,
            name_span: syntax::span(name_node),
            full_span: syntax::span(node),
            parent,
            bases,
            template_params,
            extra,
            flags,
            doc: doc.filter(|d| !d.is_empty()).map(String::into_boxed_str),
            tags,
        });
        self.by_name.entry(name).or_default().push(id);
        id
    }

    /// 声明前 doc 块 → tag/doc 分流（D15）+ flag 镜像（平移自 index.rs）。
    fn doc_and_tags(
        &self,
        node: Node<'_>,
        src: &str,
    ) -> (Option<String>, Vec<SemanticTag>, DefFlags) {
        let texts = syntax::doc_comment_texts(node, src);
        let block = parse_comment_texts(&texts);
        let mut flags = syntax::scan_flags(node, src);
        for tag in &block.tags {
            match tag.kind {
                TagKind::Editable => flags |= DefFlags::EDITABLE,
                TagKind::NotProperty => flags |= DefFlags::NOT_PROPERTY,
                TagKind::NotCallable => flags |= DefFlags::NOT_CALLABLE,
                _ => {}
            }
        }
        let doc = if block.doc.is_empty() { None } else { Some(block.doc) };
        (doc, block.tags, flags)
    }

    fn extract_decl(&mut self, node: Node<'_>, src: &str, parent: Option<u32>, ctx: DeclCtx) {
        let Some(kind) = syntax::classify_decl(node, src, ctx) else {
            return; // comment / empty_declaration / access_declaration / default_statement / ERROR
        };
        match kind {
            DefKind::Class | DefKind::Struct => {
                self.extract_type_decl(node, src, parent, kind)
            }
            DefKind::Enum => self.extract_enum(node, src, parent),
            DefKind::Namespace => self.extract_namespace(node, src, parent),
            DefKind::Delegate | DefKind::Event => {
                self.extract_callable_decl(node, src, parent, kind)
            }
            DefKind::Constructor | DefKind::Destructor => {
                self.extract_callable_decl(node, src, parent, kind)
            }
            DefKind::AssetDecl => self.extract_asset(node, src, parent),
            DefKind::VirtualProperty => self.extract_virtual_property(node, src, parent),
            DefKind::Function | DefKind::Method | DefKind::Operator => {
                self.extract_function(node, src, parent, ctx, kind)
            }
            DefKind::GlobalVar | DefKind::Field => {
                self.extract_variable(node, src, parent, ctx, kind)
            }
            _ => {}
        }
    }

    fn extract_type_decl(
        &mut self,
        node: Node<'_>,
        src: &str,
        parent: Option<u32>,
        kind: DefKind,
    ) {
        let Some(name_node) = syntax::decl_name_node(node) else { return };
        let bases = syntax::class_bases(node, src);
        let tparams = syntax::template_params(node, src);
        let (doc, tags, flags) = self.doc_and_tags(node, src);
        let id = self.push_decl(
            node,
            name_node,
            src,
            kind,
            parent,
            flags,
            bases,
            tparams,
            RawExtra::None,
            doc,
            tags,
        );
        if let Some(body) = node.child_by_field_name("body") {
            for (_field, child) in syntax::children_with_fields(body) {
                self.extract_decl(child, src, Some(id), DeclCtx::TypeBody);
            }
        }
    }

    fn extract_enum(&mut self, node: Node<'_>, src: &str, parent: Option<u32>) {
        let Some(name_node) = syntax::decl_name_node(node) else { return };
        let (doc, tags, flags) = self.doc_and_tags(node, src);
        let id = self.push_decl(
            node,
            name_node,
            src,
            DefKind::Enum,
            parent,
            flags,
            Vec::new(),
            Vec::new(),
            RawExtra::None,
            doc,
            tags,
        );
        let Some(body) = node.child_by_field_name("body") else { return };
        for (_field, child) in syntax::children_with_fields(body) {
            if child.kind() != "enumerator" {
                continue;
            }
            let Some(ename) = child.child_by_field_name("name") else { continue };
            let value = child
                .child_by_field_name("value")
                .map(|v| syntax::text(v, src).to_string().into_boxed_str());
            let (edoc, etags, eflags) = self.doc_and_tags(child, src);
            self.push_decl(
                child,
                ename,
                src,
                DefKind::EnumValue,
                Some(id),
                eflags,
                Vec::new(),
                Vec::new(),
                RawExtra::EnumValue { value },
                edoc,
                etags,
            );
        }
    }

    fn extract_namespace(&mut self, node: Node<'_>, src: &str, parent: Option<u32>) {
        let Some(name_node) = syntax::decl_name_node(node) else { return };
        let (doc, tags, flags) = self.doc_and_tags(node, src);
        let id = self.push_decl(
            node,
            name_node,
            src,
            DefKind::Namespace,
            parent,
            flags,
            Vec::new(),
            Vec::new(),
            RawExtra::None,
            doc,
            tags,
        );
        if let Some(body) = node.child_by_field_name("body") {
            for (_field, child) in syntax::children_with_fields(body) {
                self.extract_decl(child, src, Some(id), DeclCtx::Global);
            }
        }
    }

    fn extract_callable_decl(
        &mut self,
        node: Node<'_>,
        src: &str,
        parent: Option<u32>,
        kind: DefKind,
    ) {
        // delegate / event / constructor / destructor：body 无关紧要（.d.as 无体）
        let Some(name_node) = syntax::decl_name_node(node) else { return };
        let (doc, tags, flags) = self.doc_and_tags(node, src);
        let return_type = node
            .child_by_field_name("type")
            .and_then(|t| syntax::parse_syn_type(t, src));
        let params = syntax::param_decls(node, src);
        self.push_decl(
            node,
            name_node,
            src,
            kind,
            parent,
            flags,
            Vec::new(),
            Vec::new(),
            RawExtra::Callable { return_type, params },
            doc,
            tags,
        );
    }

    fn extract_function(
        &mut self,
        node: Node<'_>,
        src: &str,
        parent: Option<u32>,
        _ctx: DeclCtx,
        kind: DefKind,
    ) {
        let Some(name_node) = syntax::decl_name_node(node) else { return };
        let (doc, tags, flags) = self.doc_and_tags(node, src);
        let return_type = node
            .child_by_field_name("type")
            .and_then(|t| syntax::parse_syn_type(t, src));
        let params = syntax::param_decls(node, src);
        self.push_decl(
            node,
            name_node,
            src,
            kind,
            parent,
            flags,
            Vec::new(),
            Vec::new(),
            RawExtra::Callable { return_type, params },
            doc,
            tags,
        );
    }

    fn extract_variable(
        &mut self,
        node: Node<'_>,
        src: &str,
        parent: Option<u32>,
        _ctx: DeclCtx,
        kind: DefKind,
    ) {
        let ty = node
            .child_by_field_name("type")
            .and_then(|t| syntax::parse_syn_type(t, src));
        let (doc, tags, flags) = self.doc_and_tags(node, src);
        let mut first = true;
        for (_field, child) in syntax::children_with_fields(node) {
            if child.kind() != "variable_declarator" {
                continue;
            }
            let Some(name_node) = child.child_by_field_name("name") else { continue };
            let (doc, tags) = if first { (doc.clone(), tags.clone()) } else { (None, Vec::new()) };
            first = false;
            self.push_decl(
                node,
                name_node,
                src,
                kind,
                parent,
                flags,
                Vec::new(),
                Vec::new(),
                RawExtra::Variable { ty: ty.clone() },
                doc,
                tags,
            );
        }
    }

    fn extract_asset(&mut self, node: Node<'_>, src: &str, parent: Option<u32>) {
        let Some(name_node) = syntax::decl_name_node(node) else { return };
        let (doc, tags, flags) = self.doc_and_tags(node, src);
        let ty = node
            .child_by_field_name("type")
            .and_then(|t| syntax::parse_syn_type(t, src));
        self.push_decl(
            node,
            name_node,
            src,
            DefKind::AssetDecl,
            parent,
            flags,
            Vec::new(),
            Vec::new(),
            RawExtra::Variable { ty },
            doc,
            tags,
        );
    }

    fn extract_virtual_property(&mut self, node: Node<'_>, src: &str, parent: Option<u32>) {
        let Some(name_node) = syntax::decl_name_node(node) else { return };
        let (doc, tags, flags) = self.doc_and_tags(node, src);
        let ty = node
            .child_by_field_name("type")
            .and_then(|t| syntax::parse_syn_type(t, src));
        self.push_decl(
            node,
            name_node,
            src,
            DefKind::VirtualProperty,
            parent,
            flags,
            Vec::new(),
            Vec::new(),
            RawExtra::Variable { ty },
            doc,
            tags,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{FileInput, WorkspaceIndex};
    use crate::intern::{intern_file, sym_str};

    // 用例源码全部内置（AGENTS.md 硬性规则 / D1）。
    // 等价性骨架：同一批内置源码，旧 WorkspaceIndex 与新 summary 对照。

    /// 覆盖形态：class 继承 + 成员（字段/方法/构造 + UPROPERTY tag）、
    /// enum（含赋值）、namespace、delegate、函数重载组、全局变量、
    /// mixin 两种形式、`.d.as` 文件头 tag。
    const SRC_MIX: &str = "\
// @group /Script/Core
// @cache_format 2
namespace NS { void NFunc() {} }
class AActor2 : UObject2
{
    UPROPERTY()
    float Health;
    void Tick(float DT) {}
    AActor2() {}
}
struct FVector2 { float X; }
enum EColor { Red, Green = 2, Blue }
delegate void FOnHit2(int Damage);
int GlobalCounter = 0;
void Overload(int A) {}
void Overload(float B) {}
mixin void Heal2(AActor2 A) {}
void AlsoMixin2(const FVector2&in V) mixin {}
";

    #[test]
    fn summary_matches_old_index() {
        let path = "unique://summix/a.d.as";
        let tree = as_syntax::parse(SRC_MIX, None);
        let summary = extract_summary(
            &tree,
            SRC_MIX,
            FileKind::Decl,
            None,
            &IndexConfig::default(),
        );

        let inputs = vec![FileInput {
            file: intern_file(path, 0),
            kind: FileKind::Decl,
            module: None,
            source: SRC_MIX.to_string(),
        }];
        let old = WorkspaceIndex::build(IndexConfig::default(), inputs);

        // ① 非合成声明数一致（SYNTHETIC = 内建 + delegate 展开 + namespace 合成，
        //    均为旧侧索引期产物，新侧不迁移）
        let old_real: Vec<_> = old
            .symbols
            .iter()
            .filter(|(_, d)| !d.flags.contains(DefFlags::SYNTHETIC))
            .collect();
        assert_eq!(
            summary.decls.len(),
            old_real.len(),
            "非合成声明数：new {} vs old {}",
            summary.decls.len(),
            old_real.len()
        );

        // ② + ③ (name, kind, name_span) 多重集一致——重载组同 (name, kind)
        // 不可逐个 find（恒命中第一个），排序后成对对照
        let key = |name: Sym, kind: DefKind, span: TextRange| {
            (sym_str(name).to_string(), kind.label(), span.start, span.end)
        };
        let mut new_keys: Vec<_> =
            summary.decls.iter().map(|d| key(d.name, d.kind, d.name_span)).collect();
        let mut old_keys: Vec<_> = old_real
            .iter()
            .map(|(_, d)| key(d.name, d.kind, d.name_span))
            .collect();
        new_keys.sort();
        old_keys.sort();
        assert_eq!(new_keys, old_keys, "(name, kind, span) 多重集");

        // ④ parent 链等价：类成员的 parent 指向类声明（文件内局部 id）
        let actor = *summary.by_name.get(&intern_sym("AActor2")).unwrap().first().unwrap();
        let tick = *summary.by_name.get(&intern_sym("Tick")).unwrap().first().unwrap();
        assert_eq!(summary.decls[tick as usize].parent, Some(actor), "成员 parent = 类");

        // ⑤ by_name：重载组源码序保留
        let ov = summary.by_name.get(&intern_sym("Overload")).unwrap();
        assert_eq!(ov.len(), 2, "重载组保留");
        assert!(ov[0] < ov[1], "源码序");

        // ⑥ mixin 两种形式 flag 等价
        for name in ["Heal2", "AlsoMixin2"] {
            let id = *summary.by_name.get(&intern_sym(name)).unwrap().first().unwrap();
            assert!(
                summary.decls[id as usize].flags.contains(DefFlags::MIXIN),
                "{name} 应带 MIXIN flag"
            );
        }

        // ⑦ 文件头 tag
        assert_eq!(summary.group.as_deref(), Some("/Script/Core"));
        assert_eq!(summary.cache_format, Some(2));

        // ⑧ bases 只记名字（TypeDecl 上提字段）
        assert_eq!(summary.decls[actor as usize].bases.len(), 1);
        assert_eq!(sym_str(summary.decls[actor as usize].bases[0].name), "UObject2");
    }

    #[test]
    fn as_script_forms_and_enum_values() {
        // `.as` 侧形态：全局函数 + 局部不进 decls + enum 值原文
        const SRC: &str = "\
int TopLevel = 1;
void F()
{
    int Local = 2;   // 函数体局部不进 decls（进 scope_tree，后续任务）
}
enum E { A, B = 5 }
";
        let tree = as_syntax::parse(SRC, None);
        let s = extract_summary(&tree, SRC, FileKind::Script, None, &IndexConfig::default());
        let mut names: Vec<&str> =
            s.decls.iter().map(|d| sym_str(d.name)).collect();
        names.sort();
        assert_eq!(names, vec!["A", "B", "E", "F", "TopLevel"], "局部不进 decls");
        // enum 值原文
        let b = *s.by_name.get(&intern_sym("B")).unwrap().first().unwrap();
        match &s.decls[b as usize].extra {
            RawExtra::EnumValue { value } => {
                assert_eq!(value.as_deref(), Some("5"));
            }
            _ => panic!("B 应是 EnumValue"),
        }
    }

    /// scope_tree 建树 + auto 预推导（index-architecture §3.3.1 四类可解）。
    /// 字节偏移用 find 定位（不硬编码，防源码微调脆断）。
    #[test]
    fn scope_tree_built_with_inference() {
        const SRC: &str = "\
void F(float DT)
{
    int A = 1;
    {
        int A = 2;
        auto B = Cast<FVector2>(A);
    }
    auto C = A;
    auto D = 1.5;
    auto S = n\"Foo\";
    for (auto E : Items) { Use(E); }
    for (int I : Items) { Use(I); }
}
";
        let tree = as_syntax::parse(SRC, None);
        let s = extract_summary(&tree, SRC, FileKind::Script, None, &IndexConfig::default());
        let after = |pat: &str| (SRC.find(pat).unwrap() + pat.len()) as u32;

        // 形参：显式类型原样（裸 float 不归一化——归一化是 L3 消费期的事）
        let dt = s.scope_tree.resolve_local(after("int A = 1;"), intern_sym("DT")).unwrap();
        assert!(matches!(dt.kind, LocalKind::Param));
        match dt.ty.as_ref() {
            Some(SynType::Primitive(p, _)) => assert_eq!(sym_str(*p), "float"),
            other => panic!("DT 应是 Primitive(float)，实际 {other:?}"),
        }

        // 遮蔽：内层块的 A（声明 2）
        let a = s.scope_tree.resolve_local(after("auto B ="), intern_sym("A")).unwrap();
        assert_eq!(a.name_span, TextRange::new(
            SRC.find("int A = 2").unwrap() as u32 + 4,
            SRC.find("int A = 2").unwrap() as u32 + 5,
        ));

        // Cast 推导：type 字段直接是 T
        let b = s.scope_tree.resolve_local(after("auto B ="), intern_sym("B")).unwrap();
        match b.ty.as_ref() {
            Some(SynType::Named(n, _)) => assert_eq!(sym_str(*n), "FVector2"),
            other => panic!("B 应是 Named(FVector2)，实际 {other:?}"),
        }

        // 复制推导：C 复制外层 A 的显式 int
        let c = s.scope_tree.resolve_local(after("auto D ="), intern_sym("C")).unwrap();
        match c.ty.as_ref() {
            Some(SynType::Primitive(p, _)) => assert_eq!(sym_str(*p), "int"),
            other => panic!("C 应是 Primitive(int)，实际 {other:?}"),
        }

        // 字面量：float 按 cfg 归一（默认 float64——字面量无源码类型名，
        // 直接给规范名，与显式声明保留裸名不同）
        let d = s.scope_tree.resolve_local(after("auto S ="), intern_sym("D")).unwrap();
        match d.ty.as_ref() {
            Some(SynType::Primitive(p, _)) => assert_eq!(sym_str(*p), "float64"),
            other => panic!("D 应是 Primitive(float64)，实际 {other:?}"),
        }

        // n"..." → FName
        let ns = s.scope_tree.resolve_local(after("for (auto E"), intern_sym("S")).unwrap();
        match ns.ty.as_ref() {
            Some(SynType::Named(n, _)) => assert_eq!(sym_str(*n), "FName"),
            other => panic!("S 应是 Named(FName)，实际 {other:?}"),
        }

        // range-for：auto 迭代变量跨文件（元素类型）→ None（宁缺毋假）；
        // 显式类型迭代变量 → 原样
        let e = s.scope_tree.resolve_local(after("for (auto E"), intern_sym("E")).unwrap();
        assert!(matches!(e.kind, LocalKind::IterVar));
        assert!(e.ty.is_none(), "Items 元素类型未知 ⇒ None");
        let i = s.scope_tree.resolve_local(after("for (int I"), intern_sym("I")).unwrap();
        match i.ty.as_ref() {
            Some(SynType::Primitive(p, _)) => assert_eq!(sym_str(*p), "int"),
            other => panic!("I 应是 Primitive(int)，实际 {other:?}"),
        }

        // 构造调用不做（与函数调用无法语法区分）——auto G = H(...) 推不出
        // （间接由 E 的 None 断言覆盖同类语义；此处不另设用例）

        // 局部不进 decls（与 as_script_forms 用例同一不变量，此处复验）
        assert!(s.by_name.get(&intern_sym("A")).is_none(), "函数体局部不进 decls");
    }
}
