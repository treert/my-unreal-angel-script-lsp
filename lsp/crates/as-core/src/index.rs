//! WorkspaceIndex：三阶段流水线的数据产物与构建入口（LSP实现规划 §4）。
//!
//! M1 落地 **Phase 1 + Phase 2**（冷启动构建）：
//! - Phase 1 = 全量 parse（rayon 并行，不设轻扫描——D4）+ 声明提取 → 主索引；
//! - Phase 2 = 成员表 / class 继承闭包（struct 不建）/ 类型归一化（字段与
//!   全局变量的声明类型 → TypeId，`float` 归一化由 `IndexConfig` 决定）。
//! Phase 3（UseSite / 表达式定型）是查询期的事，M3+ 落地。
//!
//! as-core 无 IO：输入是「已读好的字符串 + FileId」（规划 §2）。
//! 文件类别（`.as` / `.d.as`）由调用方在 FileInput 里声明（Phase 0 产出）。
//!
//! 模块归属表（FilenameToModuleName → local 可见域）随 M3 落地——M1 无消费方。

use std::collections::HashMap;

use rayon::prelude::*;

use as_syntax::tree_sitter::Node;
use as_syntax::SyntaxError;

use crate::decl_tags::{parse_comment_texts, TagKind, TagValue};
use crate::id::{DefId, FileId, Sym, TypeId};
use crate::intern::{intern_file, intern_sym, sym_str};
use crate::range::{LineIndex, TextRange};
use crate::symbol::{DefData, DefExtra, DefFlags, DefKind, SymbolTable};
use crate::syntax::{self, DeclCtx};
use crate::types::{SynType, TypeKind, TypeTable};

/// 索引构建输入参数（规划 §3.3）。改动该配置 ⇒ 视同全量重建（§5.3）。
#[derive(Clone, Copy, Debug)]
pub struct IndexConfig {
    /// 对应引擎 `bScriptFloatIsFloat64`（默认 true）：裸 `float` 归一化到
    /// `float64`（true）还是 `float32`（false）。架构设计 §2.5 / §5。
    pub float_is_float64: bool,
}

impl Default for IndexConfig {
    fn default() -> Self {
        IndexConfig { float_is_float64: true }
    }
}

/// 文件类别（Phase 0 的实质产出之一：两类失效粒度与允许构造不同，D19）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FileKind {
    /// 用户脚本
    Script,
    /// UE 导出声明（`Saved/AS-Cache` / `TypeDecls`）
    Decl,
}

impl FileKind {
    pub fn label(self) -> &'static str {
        match self {
            FileKind::Script => "script",
            FileKind::Decl => "decl",
        }
    }
}

pub struct FileInput {
    pub file: FileId,
    pub kind: FileKind,
    pub source: String,
}

/// 每文件的持久快照（CST + 行首表是 Phase 2 产物，规划 §4）。
pub struct FileSnapshot {
    pub kind: FileKind,
    pub source: String,
    pub tree: as_syntax::tree_sitter::Tree,
    pub lines: LineIndex,
    pub errors: Vec<SyntaxError>,
    /// `.d.as` 文件头 `@group`（hover 归属显示的完整包路径，§2.2.1 推论 2）
    pub group: Option<String>,
    /// 文件头 `@cache_format`——识别但不消费（D21）
    pub cache_format: Option<u32>,
}

/// 内建基础类型的注册路径（合成 DefId 的 file 字段；SYNTHETIC 永不进遍历统计）。
const BUILTIN_FILE_PATH: &str = "<as-core:builtin>";

/// primitive_type 全集（grammar Part 3.1）。**不含 `float`**——裸 `float`
/// 按 `IndexConfig` 归一化到 `float64` / `float32`，从不落到名为 `float`
/// 的 DefId（架构设计 §2.5）。
const BUILTIN_PRIMITIVES: &[&str] = &[
    "void", "bool",
    "int8", "int16", "int", "int32", "int64",
    "uint8", "uint16", "uint", "uint32", "uint64",
    "float32", "float64", "double",
];

/// 继承闭包深度上限（防环与病态深度；环另有显式检测）。
const MAX_CLOSURE_DEPTH: usize = 256;

pub struct WorkspaceIndex {
    pub config: IndexConfig,
    pub symbols: SymbolTable,
    /// 主索引（全局名表）：name → 同名声明（按 DefId 升序 = 注册序）
    pub main: HashMap<Sym, Vec<DefId>>,
    /// 成员表：parent → 子声明（源码序）
    pub members: HashMap<DefId, Vec<DefId>>,
    /// class 继承闭包：class → 祖先链（近者在前）。**struct 不建**（架构设计 §4.5 第 2 级）
    pub closures: HashMap<DefId, Vec<DefId>>,
    /// 检出继承环的 class（坏源码容错：报出来不 panic，规划 §4 Phase 2）
    pub cycle_classes: Vec<DefId>,
    pub types: TypeTable,
    /// 字段/全局变量/asset 的声明类型解析结果（M1 子集；方法签名 M3）
    pub resolved: HashMap<DefId, TypeId>,
    pub files: HashMap<FileId, FileSnapshot>,
}

impl WorkspaceIndex {
    /// Phase 1 + Phase 2：并行全量 parse，顺序提取声明，建表。
    pub fn build(config: IndexConfig, inputs: Vec<FileInput>) -> WorkspaceIndex {
        // Phase 1：rayon 并行 parse（441 文件量级秒级，不设轻扫描——D4）
        let mut parsed: Vec<(FileInput, as_syntax::tree_sitter::Tree, Vec<SyntaxError>, LineIndex)> =
            inputs
                .into_par_iter()
                .map(|input| {
                    let tree = as_syntax::parse(&input.source, None);
                    let errors = as_syntax::verify_tree(&tree);
                    let lines = LineIndex::new(&input.source);
                    (input, tree, errors, lines)
                })
                .collect();
        parsed.sort_by_key(|(input, ..)| input.file);

        let mut idx = WorkspaceIndex::new(config);
        for (input, tree, errors, lines) in parsed {
            idx.add_file(input, tree, errors, lines);
        }
        idx.finish();
        idx
    }

    fn new(config: IndexConfig) -> Self {
        let mut idx = WorkspaceIndex {
            config,
            symbols: SymbolTable::new(),
            main: HashMap::new(),
            members: HashMap::new(),
            closures: HashMap::new(),
            cycle_classes: Vec::new(),
            types: TypeTable::new(),
            resolved: HashMap::new(),
            files: HashMap::new(),
        };
        idx.inject_builtins();
        idx
    }

    /// 内建基础类型 → 合成 DefId（D25）。注入在一切文件之前 ⇒ DefId 0..15
    /// 恒为内建，主索引可稳定命中。
    fn inject_builtins(&mut self) {
        let file = intern_file(BUILTIN_FILE_PATH, 0);
        for name in BUILTIN_PRIMITIVES {
            let sym = intern_sym(name);
            let id = self.symbols.push(DefData {
                name: sym,
                kind: DefKind::Struct,
                file,
                name_span: TextRange::new(0, 0),
                full_span: TextRange::new(0, 0),
                parent: None,
                origin: None,
                flags: DefFlags::SYNTHETIC,
                extra: DefExtra::None,
                doc: None,
                tags: Vec::new(),
            });
            self.main.entry(sym).or_default().push(id);
        }
    }

    fn add_file(
        &mut self,
        input: FileInput,
        tree: as_syntax::tree_sitter::Tree,
        errors: Vec<SyntaxError>,
        lines: LineIndex,
    ) {
        let file = input.file;
        let src = &input.source;

        // 文件头 tag（.d.as 固定 4 行；@cache_format 识别不消费——D21）
        let header = syntax::leading_comment_texts(tree.root_node(), src);
        let header_block = parse_comment_texts(&header);
        let group = header_block.tag(TagKind::Group).and_then(|t| match &t.value {
            TagValue::Text(s) => Some(s.clone()),
            _ => None,
        });
        let cache_format = header_block.tag(TagKind::CacheFormat).and_then(|t| match &t.value {
            TagValue::Int(n) => Some(*n),
            _ => None,
        });

        // Phase 2a：声明提取（顺序遍历，parent 先于 children 压栈）
        let cursor_root = tree.root_node();
        for (_field, child) in syntax::children_with_fields(cursor_root) {
            self.extract_decl(child, src, file, None, DeclCtx::Global);
        }

        self.files.insert(
            file,
            FileSnapshot {
                kind: input.kind,
                source: input.source,
                tree,
                lines,
                errors,
                group,
                cache_format,
            },
        );
    }

    // -----------------------------------------------------------------------
    // 声明提取
    // -----------------------------------------------------------------------

    fn extract_decl(
        &mut self,
        node: Node<'_>,
        src: &str,
        file: FileId,
        parent: Option<DefId>,
        ctx: DeclCtx,
    ) {
        let Some(kind) = syntax::classify_decl(node, src, ctx) else {
            return; // comment / empty_declaration / access_declaration / default_statement / ERROR
        };
        match kind {
            DefKind::Class | DefKind::Struct => {
                self.extract_type_decl(node, src, file, parent, kind)
            }
            DefKind::Enum => self.extract_enum(node, src, file, parent),
            DefKind::Namespace => self.extract_namespace(node, src, file, parent),
            DefKind::Delegate | DefKind::Event => {
                self.extract_callable_decl(node, src, file, parent, kind)
            }
            DefKind::Constructor | DefKind::Destructor => {
                self.extract_callable_decl(node, src, file, parent, kind)
            }
            DefKind::AssetDecl => self.extract_asset(node, src, file, parent),
            DefKind::VirtualProperty => self.extract_virtual_property(node, src, file, parent),
            DefKind::Function | DefKind::Method | DefKind::Operator => {
                self.extract_function(node, src, file, parent, ctx, kind)
            }
            DefKind::GlobalVar | DefKind::Field => {
                self.extract_variable(node, src, file, parent, ctx, kind)
            }
            _ => {}
        }
    }

    fn push_def(
        &mut self,
        node: Node<'_>,
        name_node: Node<'_>,
        src: &str,
        file: FileId,
        kind: DefKind,
        parent: Option<DefId>,
        flags: DefFlags,
        extra: DefExtra,
        doc: Option<String>,
        tags: Vec<crate::decl_tags::SemanticTag>,
    ) -> DefId {
        let name = intern_sym(syntax::text(name_node, src));
        let id = self.symbols.push(DefData {
            name,
            kind,
            file,
            name_span: syntax::span(name_node),
            full_span: syntax::span(node),
            parent,
            origin: None,
            flags,
            extra,
            doc: doc.filter(|d| !d.is_empty()).map(String::into_boxed_str),
            tags,
        });
        self.main.entry(name).or_default().push(id);
        if let Some(p) = parent {
            self.members.entry(p).or_default().push(id);
        }
        id
    }

    /// 声明前 doc 块 → tag/doc 分流（D15）+ flag 镜像。
    fn doc_and_tags(&self, node: Node<'_>, src: &str) -> (Option<String>, Vec<crate::decl_tags::SemanticTag>, DefFlags) {
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

    fn extract_type_decl(
        &mut self,
        node: Node<'_>,
        src: &str,
        file: FileId,
        parent: Option<DefId>,
        kind: DefKind,
    ) {
        let Some(name_node) = syntax::decl_name_node(node) else { return };
        let bases = syntax::class_bases(node, src);
        let tparams = syntax::template_params(node, src);
        let (doc, tags, flags) = self.doc_and_tags(node, src);
        let id = self.push_def(
            node,
            name_node,
            src,
            file,
            kind,
            parent,
            flags,
            DefExtra::TypeDecl { bases, template_params: tparams },
            doc,
            tags,
        );
        if let Some(body) = node.child_by_field_name("body") {
            for (_field, child) in syntax::children_with_fields(body) {
                self.extract_decl(child, src, file, Some(id), DeclCtx::TypeBody);
            }
        }
    }

    fn extract_enum(&mut self, node: Node<'_>, src: &str, file: FileId, parent: Option<DefId>) {
        let Some(name_node) = syntax::decl_name_node(node) else { return };
        let (doc, tags, flags) = self.doc_and_tags(node, src);
        let id = self.push_def(
            node,
            name_node,
            src,
            file,
            DefKind::Enum,
            parent,
            flags,
            DefExtra::None,
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
            self.push_def(
                child,
                ename,
                src,
                file,
                DefKind::EnumValue,
                Some(id),
                eflags,
                DefExtra::EnumValue { value },
                edoc,
                etags,
            );
        }
    }

    fn extract_namespace(&mut self, node: Node<'_>, src: &str, file: FileId, parent: Option<DefId>) {
        let Some(name_node) = syntax::decl_name_node(node) else { return };
        let (doc, tags, flags) = self.doc_and_tags(node, src);
        let id = self.push_def(
            node,
            name_node,
            src,
            file,
            DefKind::Namespace,
            parent,
            flags,
            DefExtra::None,
            doc,
            tags,
        );
        if let Some(body) = node.child_by_field_name("body") {
            for (_field, child) in syntax::children_with_fields(body) {
                self.extract_decl(child, src, file, Some(id), DeclCtx::Global);
            }
        }
    }

    fn extract_callable_decl(
        &mut self,
        node: Node<'_>,
        src: &str,
        file: FileId,
        parent: Option<DefId>,
        kind: DefKind,
    ) {
        // delegate / event / constructor / destructor：body 无关紧要（.d.as 无体）
        let Some(name_node) = syntax::decl_name_node(node) else { return };
        let (doc, tags, flags) = self.doc_and_tags(node, src);
        let return_type = node
            .child_by_field_name("type")
            .and_then(|t| syntax::parse_syn_type(t, src));
        let param_count = syntax::param_count(node, src);
        self.push_def(
            node,
            name_node,
            src,
            file,
            kind,
            parent,
            flags,
            DefExtra::Callable { return_type, param_count },
            doc,
            tags,
        );
    }

    fn extract_function(
        &mut self,
        node: Node<'_>,
        src: &str,
        file: FileId,
        parent: Option<DefId>,
        ctx: DeclCtx,
        kind: DefKind,
    ) {
        let _ = ctx;
        let Some(name_node) = syntax::decl_name_node(node) else { return };
        let (doc, tags, flags) = self.doc_and_tags(node, src);
        let return_type = node
            .child_by_field_name("type")
            .and_then(|t| syntax::parse_syn_type(t, src));
        let param_count = syntax::param_count(node, src);
        self.push_def(
            node,
            name_node,
            src,
            file,
            kind,
            parent,
            flags,
            DefExtra::Callable { return_type, param_count },
            doc,
            tags,
        );
    }

    fn extract_variable(
        &mut self,
        node: Node<'_>,
        src: &str,
        file: FileId,
        parent: Option<DefId>,
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
            self.push_def(
                node,
                name_node,
                src,
                file,
                kind,
                parent,
                flags,
                DefExtra::Variable { ty: ty.clone() },
                doc,
                tags,
            );
        }
    }

    fn extract_asset(&mut self, node: Node<'_>, src: &str, file: FileId, parent: Option<DefId>) {
        let Some(name_node) = syntax::decl_name_node(node) else { return };
        let (doc, tags, flags) = self.doc_and_tags(node, src);
        let ty = node
            .child_by_field_name("type")
            .and_then(|t| syntax::parse_syn_type(t, src));
        self.push_def(
            node,
            name_node,
            src,
            file,
            DefKind::AssetDecl,
            parent,
            flags,
            DefExtra::Variable { ty },
            doc,
            tags,
        );
    }

    fn extract_virtual_property(
        &mut self,
        node: Node<'_>,
        src: &str,
        file: FileId,
        parent: Option<DefId>,
    ) {
        let Some(name_node) = syntax::decl_name_node(node) else { return };
        let (doc, tags, flags) = self.doc_and_tags(node, src);
        let ty = node
            .child_by_field_name("type")
            .and_then(|t| syntax::parse_syn_type(t, src));
        self.push_def(
            node,
            name_node,
            src,
            file,
            DefKind::VirtualProperty,
            parent,
            flags,
            DefExtra::Variable { ty },
            doc,
            tags,
        );
    }

    // -----------------------------------------------------------------------
    // Phase 2 收尾：闭包 + 类型归一化
    // -----------------------------------------------------------------------

    fn finish(&mut self) {
        self.build_closures();
        self.resolve_decl_types();
    }

    /// class 继承闭包（struct 不建——`.d.as` 的 struct 无父类且 C++ 继承已
    /// 展平，架构设计 §4.5 第 2 级）。环检测：路径上重复出现即在首次重复处
    /// 截断，环成员记入 `cycle_classes`（报出来，不 panic）。
    fn build_closures(&mut self) {
        let classes: Vec<DefId> = self
            .symbols
            .iter()
            .filter(|(_, d)| d.kind == DefKind::Class && !d.flags.contains(DefFlags::SYNTHETIC))
            .map(|(id, _)| id)
            .collect();
        let mut cyclic: Vec<DefId> = Vec::new();
        for class in classes {
            let mut chain: Vec<DefId> = Vec::new();
            let mut path: Vec<DefId> = vec![class];
            let mut cur = class;
            for _ in 0..MAX_CLOSURE_DEPTH {
                let Some(base) = self.resolve_base_class(cur) else { break };
                if let Some(pos) = path.iter().position(|&d| d == base) {
                    for &d in &path[pos..] {
                        if !cyclic.contains(&d) {
                            cyclic.push(d);
                        }
                    }
                    break; // 环：链在首次重复处截断（不把重复基类再入链）
                }
                path.push(base);
                chain.push(base);
                cur = base;
            }
            self.closures.insert(class, chain);
        }
        self.cycle_classes = cyclic;
    }

    /// 基名 → 主索引中的 Class 声明（首个命中）。非 simple 基名（模板/
    /// qualified）与查不到的名字返回 None（闭包到此为止，宁缺毋假）。
    pub fn resolve_base_class(&self, def: DefId) -> Option<DefId> {
        let d = self.symbols.get(def);
        let DefExtra::TypeDecl { bases, .. } = &d.extra else { return None };
        let base = bases.iter().find(|b| b.simple)?;
        self.main
            .get(&base.name)?
            .iter()
            .copied()
            .find(|&id| self.symbols.get(id).kind == DefKind::Class)
    }

    /// 字段/全局变量/asset 的声明类型 → 归一化 TypeId。
    /// 解析失败（未知名/qualified/模板实参未解析）不报错、不入表（宁缺毋假）。
    fn resolve_decl_types(&mut self) {
        let jobs: Vec<(DefId, SynType)> = self
            .symbols
            .iter()
            .filter(|(_, d)| {
                matches!(
                    d.kind,
                    DefKind::Field | DefKind::GlobalVar | DefKind::AssetDecl | DefKind::VirtualProperty
                )
            })
            .filter_map(|(id, d)| {
                let DefExtra::Variable { ty: Some(t) } = &d.extra else { return None };
                Some((id, t.clone()))
            })
            .collect();
        for (def, syn) in jobs {
            if let Some(t) = self.resolve_syn(&syn) {
                self.resolved.insert(def, t);
            }
        }
    }

    /// 语法层类型 → 归一化 TypeId。名字解析走主索引（类型声明 + 内建）。
    pub fn resolve_syn(&mut self, syn: &SynType) -> Option<TypeId> {
        match syn {
            SynType::Primitive(name, _) => {
                // 裸 float：按配置归一化（架构设计 §2.5，D25）
                let target = if sym_str(*name) == "float" {
                    if self.config.float_is_float64 { "float64" } else { "float32" }
                } else {
                    sym_str(*name)
                };
                let def = self
                    .main
                    .get(&intern_sym(target))?
                    .iter()
                    .copied()
                    .find(|&id| self.symbols.get(id).flags.contains(DefFlags::SYNTHETIC))?;
                Some(self.types.intern(TypeKind::Named { def, args: vec![] }))
            }
            SynType::Named(name, _) => {
                let def = self.lookup_type_def(*name)?;
                Some(self.types.intern(TypeKind::Named { def, args: vec![] }))
            }
            SynType::Template { name, args, .. } => {
                let def = self.lookup_type_def(*name)?;
                let mut resolved_args = Vec::with_capacity(args.len());
                for a in args {
                    resolved_args.push(self.resolve_syn(a)?);
                }
                Some(self.types.intern(TypeKind::Named { def, args: resolved_args }))
            }
            SynType::Qualified(_) => None, // M1 不解析（M3 查找链第 5 级的活）
            SynType::Array(inner) => {
                let t = self.resolve_syn(inner)?;
                Some(self.types.intern(TypeKind::Array(t)))
            }
            SynType::Const(inner) => {
                let t = self.resolve_syn(inner)?;
                Some(self.types.intern(TypeKind::Const(t)))
            }
            SynType::Ref(inner, k) => {
                let t = self.resolve_syn(inner)?;
                Some(self.types.intern(TypeKind::Ref(t, *k)))
            }
            SynType::UnresolvedObject(inner) => self.resolve_syn(inner), // D8：按基类型
            SynType::Auto => Some(self.types.intern(TypeKind::Auto)),
            SynType::Wildcard => Some(self.types.intern(TypeKind::Wildcard)),
        }
    }

    /// 名字 → 类型声明 DefId（class/struct/enum；同名 namespace 不算——
    /// §2.2.1 推论 1 的类型/命名空间双落点按语境择一是查找链的事）。
    pub fn lookup_type_def(&self, name: Sym) -> Option<DefId> {
        self.main
            .get(&name)?
            .iter()
            .copied()
            .find(|&id| self.symbols.get(id).kind.is_type_decl())
    }

    /// 类型渲染（dump/调试用）。
    pub fn render_type(&self, t: TypeId) -> String {
        match self.types.get(t) {
            TypeKind::Named { def, args } => {
                let name = sym_str(self.symbols.get(*def).name).to_string();
                if args.is_empty() {
                    name
                } else {
                    let inner: Vec<String> = args.iter().map(|&a| self.render_type(a)).collect();
                    format!("{name}<{}>", inner.join(", "))
                }
            }
            TypeKind::Array(inner) => format!("{}[]", self.render_type(*inner)),
            TypeKind::Const(inner) => format!("const {}", self.render_type(*inner)),
            TypeKind::Ref(inner, k) => format!("{}{}", self.render_type(*inner), k.label()),
            TypeKind::Param(i) => format!("$T{i}"),
            TypeKind::Wildcard => "?".into(),
            TypeKind::Auto => "auto".into(),
        }
    }

    pub fn def(&self, id: DefId) -> &DefData {
        self.symbols.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intern::intern_file;

    fn build(srcs: &[(&str, &str)], config: IndexConfig) -> WorkspaceIndex {
        let inputs = srcs
            .iter()
            .map(|(path, src)| FileInput {
                file: intern_file(path, 0),
                kind: if path.ends_with(".d.as") { FileKind::Decl } else { FileKind::Script },
                source: (*src).to_string(),
            })
            .collect();
        WorkspaceIndex::build(config, inputs)
    }

    /// 用例源码全部内置（AGENTS.md 硬性规则 / D1）。

    #[test]
    fn m1_acceptance_float_dual_config() {
        // 验收 ④：floatIsFloat64 两种取值下 FVector.X 分别定型为 float64 / float32
        const SRC: &str = "struct FVector { float X; float32 Y; }\n";
        for (config, expect) in [(true, "float64"), (false, "float32")] {
            let idx = build(&[("FVector.d.as", SRC)], IndexConfig { float_is_float64: config });
            let (x, y) = {
                let fvector = idx.lookup_type_def(intern_sym("FVector")).unwrap();
                let members = idx.members.get(&fvector).unwrap();
                assert_eq!(members.len(), 2);
                (members[0], members[1])
            };
            let tx = idx.resolved.get(&x).expect("X must resolve");
            let ty = idx.resolved.get(&y).expect("Y must resolve");
            let render_x = idx.render_type(*tx);
            let render_y = idx.render_type(*ty);
            assert_eq!(render_x, expect, "裸 float 应归一化为 {expect}");
            assert_eq!(render_y, "float32", "显式 float32 不受配置影响");
        }
    }

    #[test]
    fn m1_acceptance_inheritance_cycle() {
        // 验收 ③a：继承环——不 panic，闭包有限，环成员被检出
        const SRC: &str = "class A : B {}\nclass B : A {}\nclass C : A {}\n";
        let idx = build(&[("cycle.as", SRC)], IndexConfig::default());
        assert_eq!(idx.cycle_classes.len(), 2, "A 与 B 应被标记为环");
        let a = idx.lookup_type_def(intern_sym("A")).unwrap();
        let c = idx.lookup_type_def(intern_sym("C")).unwrap();
        let closure_a = idx.closures.get(&a).unwrap();
        assert!(closure_a.len() <= 2, "环路径在首次重复处截断");
        let closure_c = idx.closures.get(&c).unwrap();
        assert_eq!(closure_c.len(), 2, "C 的闭包 = [A, B]（环不影响旁支）");
    }

    #[test]
    fn m1_acceptance_struct_no_closure() {
        // 验收 ③b：struct 不建闭包（架构设计 §4.5 第 2 级）
        const SRC: &str = "struct S : T {}\nstruct T { int X; }\nclass K : J {}\nclass J {}\n";
        let idx = build(&[("mix.as", SRC)], IndexConfig::default());
        let s = idx.lookup_type_def(intern_sym("S")).unwrap();
        assert!(!idx.closures.contains_key(&s), "struct 即使写了基类也不建闭包");
        let k = idx.lookup_type_def(intern_sym("K")).unwrap();
        assert_eq!(idx.closures.get(&k).unwrap().len(), 1, "class 正常建闭包");
    }

    #[test]
    fn m1_acceptance_four_zero_corpus_tags() {
        // 验收 ②：4 个语料零出现 tag 的内置单测（架构设计 §8 风险 8）
        const SRC: &str = "\
// @keywords MoveTo;Teleport
// @defaultsOnly
void SetX(int X);

// @template_inherit_specializations
// @template_covariant
struct TBox<T> { T Value; }

// @templateSpecialization
class TArray<FVector> { void SortByLength(); }

// @outputTypeIndex abc
int Bad();

// @meta Key=Value
// @editable
// @notCallable
float Field;
";
        let idx = build(&[("tags.d.as", SRC)], IndexConfig::default());
        let find = |name: &str| {
            idx.main
                .get(&intern_sym(name))
                .unwrap()
                .iter()
                .copied()
                .next()
                .unwrap()
        };
        let set_x = find("SetX");
        let d = idx.def(set_x);
        assert!(d.tags.iter().any(|t| t.kind == TagKind::Keywords));
        assert!(d.tags.iter().any(|t| t.kind == TagKind::DefaultsOnly));

        let tbox = idx.lookup_type_def(intern_sym("TBox")).unwrap();
        let d = idx.def(tbox);
        assert!(d.tags.iter().any(|t| t.kind == TagKind::TemplateInheritSpecializations));
        assert!(d.tags.iter().any(|t| t.kind == TagKind::TemplateCovariant));
        match &d.extra {
            DefExtra::TypeDecl { template_params, .. } => {
                assert_eq!(template_params.len(), 1);
                assert_eq!(sym_str(template_params[0]), "T");
            }
            _ => panic!("TBox 应有 TypeDecl extra"),
        }

        let tarray = idx.lookup_type_def(intern_sym("TArray")).unwrap();
        assert!(idx.def(tarray).tags.iter().any(|t| t.kind == TagKind::TemplateSpecialization));

        // value 解析失败 → tag 丢弃（§2.4.4 规则 4）
        let bad = find("Bad");
        assert!(!idx.def(bad).tags.iter().any(|t| t.kind == TagKind::OutputTypeIndex));

        // doxygen 噪声与 doc 分流 + flag 镜像
        let field = find("Field");
        let d = idx.def(field);
        assert!(d.flags.contains(DefFlags::EDITABLE));
        assert!(d.flags.contains(DefFlags::NOT_CALLABLE));
        assert!(d.tags.iter().any(|t| t.kind == TagKind::Meta));
    }

    #[test]
    fn doc_tags_split_on_members() {
        // 架构设计 §2.4.4 的实测样例形态：doc 在前、tag 在后紧邻出现
        const SRC: &str = "\
class A
{
    // Returns the location.
    // @see bGenerateOverlapEvents, ...
    // @editable
    EActorUpdateOverlapsMethod UpdateOverlapsMethodDuringLevelStreaming;
}
";
        let idx = build(&[("A.d.as", SRC)], IndexConfig::default());
        let a = idx.lookup_type_def(intern_sym("A")).unwrap();
        let field = idx.members.get(&a).unwrap()[0];
        let d = idx.def(field);
        assert!(d.flags.contains(DefFlags::EDITABLE));
        let doc = d.doc.as_deref().unwrap();
        assert!(doc.contains("Returns the location."));
        assert!(doc.contains("@see bGenerateOverlapEvents, ..."), "doxygen tag 留在 doc 正文");
    }

    #[test]
    fn declaration_counting_sanity() {
        const SRC: &str = "\
class A : UObject { int X; void F() {} A() {} }
struct B { float Y; }
enum E { A, B = 2 }
namespace Math { float64 Sqrt(float64 V); }
void G() {}
int V;
mixin void M(A Target, float Amount) {}
local void L() {}
asset Icon of Texture2D;
";
        let idx = build(&[("all.as", SRC)], IndexConfig::default());
        let count = |k: DefKind| {
            idx.symbols
                .iter()
                .filter(|(_, d)| d.kind == k && !d.flags.contains(DefFlags::SYNTHETIC))
                .count()
        };
        assert_eq!(count(DefKind::Class), 1);
        assert_eq!(count(DefKind::Struct), 1);
        assert_eq!(count(DefKind::Enum), 1);
        assert_eq!(count(DefKind::EnumValue), 2);
        assert_eq!(count(DefKind::Namespace), 1);
        assert_eq!(count(DefKind::Field), 2);
        assert_eq!(count(DefKind::Method), 1);
        assert_eq!(count(DefKind::Constructor), 1);
        assert_eq!(count(DefKind::Function), 4); // G + M + L + Math::Sqrt（mixin/local 也是函数）
        assert_eq!(count(DefKind::GlobalVar), 1);
        assert_eq!(count(DefKind::AssetDecl), 1);

        // mixin / local flag
        let m = idx.main.get(&intern_sym("M")).unwrap()[0];
        assert!(idx.def(m).flags.contains(DefFlags::MIXIN));
        let l = idx.main.get(&intern_sym("L")).unwrap()[0];
        assert!(idx.def(l).flags.contains(DefFlags::LOCAL));

        // namespace 内函数归属
        let sqrt = idx.main.get(&intern_sym("Sqrt")).unwrap()[0];
        assert_eq!(idx.def(sqrt).parent, Some(idx.main.get(&intern_sym("Math")).unwrap()[0]));

        // 模板字段类型解析：T 未解析 → 不入 resolved（宁缺毋假）
        // （TBox 用例已在 four_zero_corpus_tags 覆盖，这里查 B.Y）
        let b = idx.lookup_type_def(intern_sym("B")).unwrap();
        let y = idx.members.get(&b).unwrap()[0];
        assert!(idx.resolved.contains_key(&y));
    }

    #[test]
    fn template_field_type_resolution() {
        // 模板实例使用位：TArray<FVector> 字段解析为 Named{def=TArray, args=[Named FVector]}
        const SRC: &str = "struct TArray<T> { }\nstruct FVector { }\nstruct Holder { TArray<FVector> Arr; }\n";
        let idx = build(&[("tpl.d.as", SRC)], IndexConfig::default());
        let holder = idx.lookup_type_def(intern_sym("Holder")).unwrap();
        let arr = idx.members.get(&holder).unwrap()[0];
        let t = idx.resolved.get(&arr).expect("TArray<FVector> 应可解析");
        match idx.types.get(*t) {
            TypeKind::Named { def, args } => {
                assert_eq!(sym_str(idx.def(*def).name), "TArray");
                assert_eq!(args.len(), 1);
                assert_eq!(idx.render_type(args[0]), "FVector");
            }
            other => panic!("应为 Named，实际 {other:?}"),
        }
    }

    #[test]
    fn builtin_primitives_synthetic() {
        const SRC: &str = "int A;\nvoid F(bool B) {}\n";
        let idx = build(&[("b.as", SRC)], IndexConfig::default());
        let a = idx.main.get(&intern_sym("A")).unwrap()[0];
        let t = idx.resolved.get(&a).unwrap();
        assert_eq!(idx.render_type(*t), "int");
        // 内建是 SYNTHETIC，不进任何声明统计
        let synthetic_count = idx
            .symbols
            .iter()
            .filter(|(_, d)| d.flags.contains(DefFlags::SYNTHETIC))
            .count();
        assert_eq!(synthetic_count, BUILTIN_PRIMITIVES.len());
    }
}
