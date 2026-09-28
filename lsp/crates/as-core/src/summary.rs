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
use crate::id::Sym;
use crate::index::FileKind;
use crate::intern::intern_sym;
use crate::range::TextRange;
use crate::scope::ScopeTree;
use crate::symbol::{BaseRef, DefFlags, DefKind, ParamDecl};
use crate::syntax::{self, DeclCtx};
use crate::types::SynType;

/// 每文件摘要：声明表 + 文件内名字倒排 + 局部作用域树 + 文件头 tag。
#[derive(Debug)]
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
#[derive(Debug)]
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
#[derive(Debug)]
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
    _cfg: &IndexConfig,
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

    FileSummary {
        kind,
        module,
        group,
        cache_format,
        decls: b.decls,
        by_name: b.by_name,
        scope_tree: ScopeTree::empty(),
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
        // scope_tree 本任务恒为空树
        assert!(s.scope_tree.locals_visible(0).is_empty());
    }
}
