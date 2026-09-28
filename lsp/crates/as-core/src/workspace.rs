//! Workspace 查询容器（index-architecture.md Phase B / D37）：L3 的数据后端，
//! 取代旧 `WorkspaceIndex`（全局 DefId arena）。
//!
//! 组成（三层装配）：
//! - `files: HashMap<FileId, FileEntry>`——per-file（source + tree + lines +
//!   summary + by_parent），A 阶段已验证的并行产物；
//! - `agg: Aggregation`——薄聚合层（名字 → DeclRef 倒排）；
//! - 继承走链现算（Phase C / C6，零缓存）：`base_class` / `ancestor_chain` /
//!   `cyclic_classes`——链由 bases 名 + agg 一跳完全推导；
//! - `resolved` / `types`（声明类型归一化 + TypeTable）Phase B 保持 eager，
//!   Phase C Task 4 随 TypeId 体系退役（C7）删除。
//!
//! 与旧架构的语义对应（Phase B 裁决，见计划 B1-B7）：
//! - **B2 builtin**：合成 FileSummary（`<as-core:builtin>` 伪文件，进程级
//!   一次）——15 个 primitive SYNTHETIC RawDecl，`lookup("int8")` 与普通
//!   名字同一代码路径；
//! - **B3 合成 namespace 消失**：`namespaces_named` = kind == Namespace ∪
//!   is_type_like（class/struct 直接兼任 namespace——旧架构要两个 DefId +
//!   origin_fallback 归一，新架构同一 DeclRef 天然归一）；
//! - **B4 合成成员查询期**：`synthetic_members`（delegate/event 成员集 +
//!   StaticClass）现推，不预存——expand.rs 的 arena 注入无移植路径；
//! - **B7 members**：by_parent（per-file）+ 跨文件 namespace 聚合（agg.main
//!   过滤）。

use std::collections::HashMap;

use rayon::prelude::*;

use crate::aggregation::{Aggregation, DeclRef};
use crate::config::IndexConfig;
use crate::id::{FileId, Sym};
use crate::intern::{intern_file, intern_sym, sym_str};
use crate::range::{LineIndex, TextRange};
use crate::summary::{extract_summary, FileSummary, RawDecl, RawExtra};
use crate::symbol::{DefFlags, DefKind, ParamDecl};
use crate::types::{RefKind, SynType};

/// 文件类别（Phase 0 的实质产出之一：两类失效粒度与允许构造不同，D19）。
/// （Phase B 从 index.rs 挪入——index.rs 删除后本模块是 FileInput 的家。）
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
    /// 模块名（引擎 `FilenameToModuleName`，相对收集根计算——Phase 0 产出，
    /// 规划 §4：决定 `local` 符号的可见域）。`.d.as` 侧可不给（无 local 函数）。
    pub module: Option<Sym>,
}

/// 引擎 `FilenameToModuleName`：去 `.as` / `.d.as` 扩展、`/` / `\` → `.`。
/// 输入应是相对模块根的路径（根的确定是 Phase 0 的事，本函数不感知）。
pub fn filename_to_module_name(rel_path: &str) -> String {
    let stem = rel_path
        .strip_suffix(".d.as")
        .or_else(|| rel_path.strip_suffix(".as"))
        .unwrap_or(rel_path);
    stem.chars()
        .map(|c| if c == '/' || c == '\\' { '.' } else { c })
        .collect()
}

/// builtin 伪文件路径（`intern_file` 一次；FileId 进程级复用）。
pub const BUILTIN_FILE_PATH: &str = "<as-core:builtin>";

/// primitive 全集（D25：不含 `float`——裸 float 按配置归一化）。
const BUILTIN_PRIMITIVES: &[&str] = &[
    "void", "bool",
    "int8", "int16", "int", "int32", "int64",
    "uint8", "uint16", "uint", "uint32", "uint64",
    "float32", "float64", "double",
];

/// 走链深度上限（防环与病态深度；环另有显式检测）。C6：零缓存走链的
/// 安全阀，对齐旧 MAX_CLOSURE_DEPTH（B5 eager 时代的闭包构建上限）。
const MAX_CHAIN_DEPTH: usize = 256;

/// per-file 查询入口（旧 FileSnapshot 的继任）。
pub struct FileEntry {
    pub kind: FileKind,
    pub source: String,
    pub tree: as_syntax::tree_sitter::Tree,
    pub lines: LineIndex,
    pub summary: FileSummary,
    /// 局部 id → 子声明局部 id（源码序；B7）
    pub by_parent: HashMap<u32, Vec<u32>>,
}

/// 查询期合成成员（B4：expand 的轻量继任——不进 decls/by_name/agg，
/// 消费方 = resolve 成员枚举 / completion / hover / search）。
#[derive(Clone, Debug)]
pub struct SyntheticMember {
    pub name: Sym,
    pub kind: DefKind,
    pub flags: DefFlags,
    /// 签名（Callable 形态；StaticClass 是零参函数）
    pub return_type: Option<SynType>,
    pub params: Vec<ParamDecl>,
    /// 锚点 = 源头声明的名字 token（hover/definition 落点）
    pub name_span: TextRange,
    /// 源头声明（D10 origin 的 DeclRef 形态——definition/rename 语义锚）
    pub origin: DeclRef,
}

/// L3 数据后端。
pub struct Workspace {
    pub config: IndexConfig,
    pub files: HashMap<FileId, FileEntry>,
    pub agg: Aggregation,
}

impl Workspace {
    /// 冷启动：并行 parse + summary → 聚合 → builtin → 派生表。
    pub fn build(config: IndexConfig, inputs: Vec<FileInput>) -> Workspace {
        let t0 = std::time::Instant::now();
        // 并行段（A 阶段验证路径）
        let parsed: Vec<(FileInput, as_syntax::tree_sitter::Tree, LineIndex, FileSummary)> =
            inputs
                .into_par_iter()
                .map(|input| {
                    let tree = as_syntax::parse(&input.source, None);
                    let lines = LineIndex::new(&input.source);
                    let summary =
                        extract_summary(&tree, &input.source, input.kind, input.module, &config);
                    (input, tree, lines, summary)
                })
                .collect();
        let t_parse = t0.elapsed();

        // 串行装配段
        let mut files: HashMap<FileId, FileEntry> = HashMap::with_capacity(parsed.len() + 1);
        for (input, tree, lines, summary) in parsed {
            let by_parent = build_by_parent(&summary);
            files.insert(
                input.file,
                FileEntry {
                    kind: input.kind,
                    source: input.source,
                    tree,
                    lines,
                    summary,
                    by_parent,
                },
            );
        }
        // builtin 合成（B2）：与真实文件同构，agg 一并吸收
        let builtin = builtin_summary();
        let builtin_file = intern_file(BUILTIN_FILE_PATH, u32::MAX);
        files.insert(
            builtin_file,
            FileEntry {
                kind: FileKind::Decl,
                source: String::new(),
                tree: as_syntax::parse("", None),
                lines: LineIndex::new(""),
                by_parent: build_by_parent(&builtin),
                summary: builtin,
            },
        );
        let agg = Aggregation::build(
            &files.iter().map(|(f, e)| (*f, &e.summary)).collect::<HashMap<FileId, &FileSummary>>(),
        );

        let ws = Workspace {
            config,
            files,
            agg,
        };
        crate::as_log!(
            "workspace: built {} files | parse+summary {:?} | aggregation {:?}",
            ws.files.len(),
            t_parse,
            t0.elapsed() - t_parse,
        );
        ws
    }

    // -----------------------------------------------------------------------
    // 查询原语（机械替换表的落点）
    // -----------------------------------------------------------------------

    pub fn decl(&self, r: &DeclRef) -> &RawDecl {
        &self.files[&r.file].summary.decls[r.local as usize]
    }

    /// 名字 → 声明锚点（含 builtin；重载组原样保留，§4.2 序）。
    pub fn lookup(&self, name: Sym) -> &[DeclRef] {
        self.agg.main.get(&name).map_or(&[], |v| v)
    }

    /// 类型声明查找（is_type_like；内建 SYNTHETIC 排除）。
    pub fn lookup_type_def(&self, name: Sym) -> Option<DeclRef> {
        self.lookup(name)
            .iter()
            .copied()
            .find(|&r| {
                let d = self.decl(&r);
                d.kind.is_type_like() && !d.flags.contains(DefFlags::SYNTHETIC)
            })
    }

    /// 容器的成员：per-file by_parent + 跨文件 namespace 聚合 + 合成成员。
    /// （消费方按需 ∪ `synthetic_members`。）
    pub fn members(&self, parent: &DeclRef) -> Vec<DeclRef> {
        // namespace：跨文件聚合（同名 namespace 的全部声明都是其成员语境）
        if self.decl(parent).kind == DefKind::Namespace {
            let name = self.decl(parent).name;
            let mut out: Vec<DeclRef> = self
                .lookup(name)
                .iter()
                .copied()
                .filter(|&r| self.decl(&r).kind == DefKind::Namespace)
                .flat_map(|ns| {
                    let e = &self.files[&ns.file];
                    e.by_parent
                        .get(&ns.local)
                        .map_or(&[][..], |v| v.as_slice())
                        .iter()
                        .map(|&local| DeclRef { file: ns.file, local })
                        .collect::<Vec<_>>()
                })
                .collect();
            out.sort_by_key(|r| (r.file, r.local));
            return out;
        }
        let e = &self.files[&parent.file];
        e.by_parent
            .get(&parent.local)
            .map_or(Vec::new(), |v| {
                v.iter().map(|&local| DeclRef { file: parent.file, local }).collect()
            })
    }

    /// 是否存在真实 `.d.as` 声明文件（builtin 伪文件不算——诊断 AS0902 的
    /// 「decl 计数」口径与旧架构对齐）。
    pub fn has_decl_files(&self) -> bool {
        let builtin = intern_file(BUILTIN_FILE_PATH, u32::MAX);
        self.files.iter().any(|(&f, e)| f != builtin && e.kind == FileKind::Decl)
    }

    /// 同名 namespace 聚合（B3：Namespace ∪ class/struct——类直接兼任
    /// namespace，取代旧合成 namespace + origin_fallback 归一）。
    pub fn namespaces_named(&self, name: Sym) -> Vec<DeclRef> {
        self.lookup(name)
            .iter()
            .copied()
            .filter(|&r| {
                let d = self.decl(&r);
                d.kind == DefKind::Namespace || d.kind.is_type_decl()
            })
            .collect()
    }

    /// 查询期合成成员（B4，expand.rs 预计算的继任）：
    /// - delegate（单播）：N() / N(const N&) / opAssign / Execute /
    ///   ExecuteIfBound / BindUFunction / 绑定构造
    /// - event（多播）：公共集 + Broadcast / AddUFunction
    /// - class：StaticClass()（struct 无 UClass，不合成）
    ///
    /// 引擎侧真值（原 expand.rs 头注释取证，文件随 Phase B 删除）：
    /// - delegate/event 的成员集来自预处理器 `ProcessDelegates` 的生成模板
    ///   （[ENGINE] AngelscriptPreprocessor.cpp:534-695，逐行核对）；
    /// - `C.StaticClass()` 是绑定层给每个 **UClass** 注册的命名空间全局函数
    ///   （Bind_BlueprintType.cpp:661-680，`UClass StaticClass()`，经
    ///   `PreviousBindPassScriptFunctionAsFirstParam` 把类作为隐藏首参——脚本侧
    ///   签名就是零参）。struct 不绑定（无 UClass）。
    ///   同处的 `__StaticType_<TypeName>` 全局变量是 `__` 前缀内部符号，
    ///   「类名直接作值」由 resolve 层的语境判定处理，不在此合成；
    /// - `_Inner` 字段（`__` 前缀）不展开——架构设计 §4.4 成员表未列，
    ///   展开它只会污染补全。
    pub fn synthetic_members(&self, r: &DeclRef) -> Vec<SyntheticMember> {
        let d = self.decl(r);
        let span = d.name_span;
        let mut out = Vec::new();
        match d.kind {
            DefKind::Delegate | DefKind::Event => {
                let (name, rt, params) = match &d.extra {
                    RawExtra::Callable { return_type, params } => {
                        (d.name, return_type.clone(), params.clone())
                    }
                    _ => (d.name, None, Vec::new()),
                };
                let t_self = SynType::Named(name, span);
                let mk = |mname: &str, kind, flags, rt: Option<SynType>, params: Vec<ParamDecl>| {
                    SyntheticMember {
                        name: intern_sym(mname),
                        kind,
                        flags: flags | DefFlags::SYNTHETIC,
                        return_type: rt,
                        params,
                        name_span: span,
                        origin: *r,
                    }
                };
                let param = |pname: &str, ty: SynType| ParamDecl {
                    name: intern_sym(pname),
                    span,
                    ty: Some(ty),
                    flags: DefFlags::NONE,
                };
                // 公共集
                out.push(mk(
                    &sym_str(name),
                    DefKind::Constructor,
                    DefFlags::NONE,
                    None,
                    vec![],
                ));
                out.push(mk(
                    &sym_str(name),
                    DefKind::Constructor,
                    DefFlags::NONE,
                    None,
                    vec![param("Other", t_const_ref_in(t_self.clone()))],
                ));
                out.push(mk(
                    "opAssign",
                    DefKind::Operator,
                    DefFlags::NONE,
                    Some(SynType::Ref(Box::new(t_self.clone()), RefKind::Plain)),
                    vec![param("Other", t_const_ref_in(t_self.clone()))],
                ));
                if d.kind == DefKind::Event {
                    out.push(mk("Broadcast", DefKind::Method, DefFlags::CONST, rt.clone(), params.clone()));
                    out.push(mk(
                        "AddUFunction",
                        DefKind::Method,
                        DefFlags::NONE,
                        None,
                        vec![
                            param(
                                "Object",
                                SynType::Const(Box::new(SynType::Named(uobject(), span))),
                            ),
                            param("FunctionName", t_const_ref_in(SynType::Named(fname(), span))),
                        ],
                    ));
                } else {
                    out.push(mk("Execute", DefKind::Method, DefFlags::CONST, rt.clone(), params.clone()));
                    out.push(mk("ExecuteIfBound", DefKind::Method, DefFlags::CONST, rt.clone(), params.clone()));
                    out.push(mk(
                        "BindUFunction",
                        DefKind::Method,
                        DefFlags::NONE,
                        None,
                        vec![
                            param("Object", SynType::Named(uobject(), span)),
                            param("BindFunctionName", t_const_ref_in(SynType::Named(fname(), span))),
                        ],
                    ));
                    out.push(mk(
                        &sym_str(name),
                        DefKind::Constructor,
                        DefFlags::NONE,
                        None,
                        vec![
                            param("Object", SynType::Named(uobject(), span)),
                            param("BindFunctionName", t_const_ref_in(SynType::Named(fname(), span))),
                        ],
                    ));
                }
            }
            DefKind::Class => {
                out.push(SyntheticMember {
                    name: intern_sym("StaticClass"),
                    kind: DefKind::Method,
                    flags: DefFlags::SYNTHETIC,
                    return_type: Some(SynType::Named(intern_sym("UClass"), span)),
                    params: vec![],
                    name_span: span,
                    origin: *r,
                });
            }
            _ => {}
        }
        out
    }

    /// 成员名查找（per-file + 合成；namespace 走聚合）。返回真成员优先。
    pub fn members_named<'a>(
        &'a self,
        parents: &[DeclRef],
        name: Sym,
        pred: impl Fn(&RawDecl) -> bool + Copy,
    ) -> Vec<DeclRef> {
        let mut out = Vec::new();
        for p in parents {
            for r in self.members(p) {
                let d = self.decl(&r);
                if d.name == name && pred(d) {
                    out.push(r);
                }
            }
        }
        out
    }

    /// 合成成员名查找（synthetic_members 的过滤变体）。
    pub fn synthetic_named(&self, parents: &[DeclRef], name: Sym) -> Vec<SyntheticMember> {
        let mut out = Vec::new();
        for p in parents {
            for m in self.synthetic_members(p) {
                if m.name == name {
                    out.push(m);
                }
            }
        }
        out
    }

    // -----------------------------------------------------------------------
    // 增量
    // -----------------------------------------------------------------------

    /// 单文件重索引（模块名沿用）。返回 = 声明面是否变化（D29 继任：
    /// 新旧 summary 的 (name, kind, parent 名) 集 diff——消费方据此失效
    /// 派生缓存；Phase E 接 as-lsp）。
    pub fn reindex_file(&mut self, file: FileId, kind: FileKind, source: String) -> bool {
        let module = self.files.get(&file).and_then(|e| e.summary.module);
        self.reindex_file_full(file, kind, module, source)
    }

    /// 全量重索引入口（module 显式传入——新增/改名后模块名随新路径重算）。
    pub fn reindex_file_full(
        &mut self,
        file: FileId,
        kind: FileKind,
        module: Option<Sym>,
        source: String,
    ) -> bool {
        let old_summary = self.files.get(&file).map(|e| e.summary.clone());
        let old_surface = old_summary.as_ref().map(|s| decl_surface(s));

        let tree = as_syntax::parse(&source, None);
        let lines = LineIndex::new(&source);
        let summary = extract_summary(&tree, &source, kind, module, &self.config);
        let by_parent = build_by_parent(&summary);
        let entry = FileEntry { kind, source, tree, lines, summary, by_parent };

        match &old_summary {
            Some(old) => {
                self.agg.replace_file(file, old, &entry.summary);
                self.files.insert(file, entry);
            }
            None => {
                self.agg.replace_file(file, &crate::summary::FileSummary::default_for(kind), &entry.summary);
                self.files.insert(file, entry);
            }
        }
        let new_surface = decl_surface(&self.files[&file].summary);
        old_surface.map_or(true, |old| old != new_surface)
    }

    /// 删除文件：贡献摘除 + 墓碑（D18 的 FileId 层语义不变）。
    pub fn remove_file(&mut self, file: FileId) {
        if let Some(entry) = self.files.remove(&file) {
            self.agg.remove_file(file, &entry.summary);
            crate::intern::tombstone_file(file);
        }
    }

    // -----------------------------------------------------------------------
    // 继承走链（Phase C / C6：零缓存，查询期现算）
    // -----------------------------------------------------------------------

    /// 单跳：class → 直接基类（simple 基名 → Class 声明；struct / 未知名 =
    /// None）。原 `resolve_base_class` 更名（C5：命名改祖先链语义）。
    pub fn base_class(&self, class: &DeclRef) -> Option<DeclRef> {
        let d = self.decl(class);
        let base = d.bases.iter().find(|b| b.simple)?;
        self.lookup(base.name)
            .iter()
            .copied()
            .find(|&r| {
                let bd = self.decl(&r);
                bd.kind == DefKind::Class && !bd.flags.contains(DefFlags::SYNTHETIC)
            })
    }

    /// 祖先链（近者在前，不含 self）：逐级 base_class 走链，环在首次重复处
    /// 截断 + 深度上限。空链 = 无基类 / unresolved base / struct（D16：struct
    /// 不走链）。
    /// C6：零缓存——链由 bases 名 + agg 一跳完全推导（AActor 深度 ~10 ≈
    /// 10 次哈希，µs 级），reindex 后立即按新 agg 现算，无「清空→重建」
    /// 中间态。
    pub fn ancestor_chain(&self, class: &DeclRef) -> Vec<DeclRef> {
        let mut chain = Vec::new();
        let mut visited: Vec<DeclRef> = vec![*class];
        let mut cur = *class;
        for _ in 0..MAX_CHAIN_DEPTH {
            let Some(base) = self.base_class(&cur) else { break };
            if visited.contains(&base) {
                break; // 环：在首次重复处截断（对齐旧 build_closures 语义）
            }
            visited.push(base);
            chain.push(base);
            cur = base;
        }
        chain
    }

    /// 全扫环检测（诊断素材 / as-cli 统计；C4——旧 `cycle_classes` 字段的
    /// 查询期继任）：走链中 path 上重复段即环成员，去重收集。
    pub fn cyclic_classes(&self) -> Vec<DeclRef> {
        let mut out: Vec<DeclRef> = Vec::new();
        for (&file, e) in &self.files {
            for (i, d) in e.summary.decls.iter().enumerate() {
                if d.kind != DefKind::Class || d.flags.contains(DefFlags::SYNTHETIC) {
                    continue;
                }
                let class = DeclRef { file, local: i as u32 };
                let mut path = vec![class];
                let mut cur = class;
                for _ in 0..MAX_CHAIN_DEPTH {
                    let Some(base) = self.base_class(&cur) else { break };
                    if let Some(pos) = path.iter().position(|&d| d == base) {
                        for &d in &path[pos..] {
                            if !out.contains(&d) {
                                out.push(d);
                            }
                        }
                        break;
                    }
                    path.push(base);
                    cur = base;
                }
            }
        }
        out
    }

    /// 文件 → 模块名（`local` 函数可见域过滤；模块归属随 FileSummary）。
    pub fn module_of(&self, file: FileId) -> Option<Sym> {
        self.files.get(&file).and_then(|e| e.summary.module)
    }

    /// 声明的父声明（文件内局部 id → DeclRef）。
    pub fn parent_of(&self, r: &DeclRef) -> Option<DeclRef> {
        self.decl(r).parent.map(|p| DeclRef { file: r.file, local: p })
    }
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

fn build_by_parent(s: &FileSummary) -> HashMap<u32, Vec<u32>> {
    let mut out: HashMap<u32, Vec<u32>> = HashMap::new();
    for (i, d) in s.decls.iter().enumerate() {
        if let Some(p) = d.parent {
            out.entry(p).or_default().push(i as u32);
        }
    }
    out
}

/// builtin 合成 summary（B2）：15 个 primitive SYNTHETIC RawDecl。
fn builtin_summary() -> FileSummary {
    let mut decls = Vec::with_capacity(BUILTIN_PRIMITIVES.len());
    let mut by_name = HashMap::new();
    for (i, name) in BUILTIN_PRIMITIVES.iter().enumerate() {
        let sym = intern_sym(name);
        decls.push(RawDecl {
            name: sym,
            kind: DefKind::Struct,
            name_span: TextRange::new(0, 0),
            full_span: TextRange::new(0, 0),
            parent: None,
            bases: Vec::new(),
            template_params: Vec::new(),
            extra: RawExtra::None,
            flags: DefFlags::SYNTHETIC,
            doc: None,
            tags: Vec::new(),
        });
        by_name.entry(sym).or_insert_with(|| Vec::new()).push(i as u32);
    }
    FileSummary {
        kind: FileKind::Decl,
        module: None,
        group: None,
        cache_format: None,
        decls,
        by_name,
        scope_tree: crate::scope::ScopeTree::empty(),
    }
}

fn decl_surface(s: &FileSummary) -> Vec<(Sym, DefKind, Option<Sym>)> {
    let mut out: Vec<(Sym, DefKind, Option<Sym>)> = s
        .decls
        .iter()
        .map(|d| {
            let parent_name = d.parent.map(|p| s.decls[p as usize].name);
            (d.name, d.kind, parent_name)
        })
        .collect();
    out.sort();
    out
}

fn t_const_ref_in(t: SynType) -> SynType {
    SynType::Ref(Box::new(SynType::Const(Box::new(t))), RefKind::In)
}

fn uobject() -> Sym {
    intern_sym("UObject")
}

fn fname() -> Sym {
    intern_sym("FName")
}

impl FileSummary {
    /// 空 summary（remove 场景的占位；不进聚合）。
    fn default_for(kind: FileKind) -> FileSummary {
        FileSummary {
            kind,
            module: None,
            group: None,
            cache_format: None,
            decls: Vec::new(),
            by_name: HashMap::new(),
            scope_tree: crate::scope::ScopeTree::empty(),
        }
    }
}

impl Default for FileSummary {
    fn default() -> Self {
        Self::default_for(FileKind::Script)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 用例源码全部内置（AGENTS.md 硬性规则 / D1）。

    fn ws_build(srcs: &[(&str, &str)]) -> Workspace {
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

    #[test]
    fn struct_writes_base_but_no_chain() {
        // 原 index.rs m1_acceptance_struct_no_closure（D16：struct 即使写了
        // 基类也不走链）
        const SRC: &str = "struct S : T {}\nstruct T { int X; }\nclass K : J {}\nclass J {}\n";
        let ws = ws_build(&[("unique://wsnc/mix.as", SRC)]);
        let s = ws.lookup_type_def(intern_sym("S")).unwrap();
        assert!(ws.ancestor_chain(&s).is_empty(), "struct 即使写了基类也不走链");
        let k = ws.lookup_type_def(intern_sym("K")).unwrap();
        assert_eq!(ws.ancestor_chain(&k).len(), 1, "class 正常走链");
    }

    #[test]
    fn mixin_reverse_index_and_reindex_rebuild() {
        // 原 index.rs m3_mixin_reverse_index + m3_reindex_rebuilds_mixin_index
        //（Phase B：mixin_pending 概念删除——名字键天然容错，未解析类型也是
        // 有效键；agg.mixin_by_name 增量维护）
        const A: &str = "\
struct FVector {}
mixin void Heal(FVector& V) {}
void AlsoMixin(const FVector&in V) mixin {}
mixin void Unresolvable(TMissing M) {}
mixin void NoParam() {}
";
        const V1: &str = "class T {}\n";
        const V2: &str = "class T {}\nmixin void M(T X) {}\n";
        let path = "unique://wsmix/r.as";
        let mut ws = ws_build(&[("unique://wsmix/a.as", A), (path, V1)]);
        let f = intern_file(path, 0);

        // 前置 / 后置两种声明形式都进倒排（首参剥壳：FVector& / const&in）
        let hits = ws.agg.mixin_by_name.get(&intern_sym("FVector")).unwrap();
        assert_eq!(hits.len(), 2);
        // 未解析基名也是有效键（名字键天然容错）
        assert!(ws.agg.mixin_by_name.get(&intern_sym("TMissing")).is_some());

        // 重索引后新增 mixin 进倒排
        let _ = ws.reindex_file(f, FileKind::Script, V2.to_string());
        let t = ws.lookup_type_def(intern_sym("T")).unwrap();
        let hits = ws.agg.mixin_by_name.get(&ws.decl(&t).name).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(sym_str(ws.decl(&hits[0]).name), "M");
    }

    #[test]
    fn delegate_synthetic_member_set_details() {
        // 原 expand.rs delegate_expansion_member_set（成员集细节：ctor×3 /
        // origin / Execute 的 const + 形参克隆 / 拷贝构造形参形态）
        const SRC: &str = "delegate void FMyDelegate(int X, float Y);\n";
        let ws = ws_build(&[("unique://wsdmd/del.as", SRC)]);
        let d = ws.lookup_type_def(intern_sym("FMyDelegate")).unwrap();
        let ms = ws.synthetic_members(&d);
        // 单播：ctor×3 + opAssign + Execute + ExecuteIfBound + BindUFunction
        let names: Vec<&str> = ms.iter().map(|m| sym_str(m.name)).collect();
        assert_eq!(ms.len(), 7, "成员集：{names:?}");
        for m in &ms {
            assert_eq!(m.origin, d, "origin 回落源头声明（D10）");
            assert!(m.flags.contains(DefFlags::SYNTHETIC));
        }
        // Execute 的形参从委托声明克隆 + const 方法
        let exec = ms.iter().find(|m| sym_str(m.name) == "Execute").unwrap();
        assert!(exec.flags.contains(DefFlags::CONST));
        assert_eq!(exec.params.len(), 2);
        assert_eq!(sym_str(exec.params[0].name), "X");
        // 拷贝构造 + 绑定构造（带参 ctor×2）
        let ctors: Vec<_> = ms.iter().filter(|m| m.kind == DefKind::Constructor).collect();
        assert_eq!(ctors.len(), 3);
        assert_eq!(ctors.iter().filter(|m| !m.params.is_empty()).count(), 2, "拷贝构造 + 绑定构造");
    }

    #[test]
    fn builtin_primitives_synthetic() {
        // 原 index.rs builtin_primitives_synthetic：15 个内建 SYNTHETIC、不进
        // 真实文件声明统计（resolved 断言随 C7 退役——变量定型由 expr 的
        // syn_type_base 用例覆盖）
        const SRC: &str = "int A;\nvoid F(bool B) {}\n";
        let ws = ws_build(&[("unique://wsb2/b.as", SRC)]);
        let builtin = intern_file(BUILTIN_FILE_PATH, u32::MAX);
        let b = &ws.files[&builtin];
        assert_eq!(b.summary.decls.len(), BUILTIN_PRIMITIVES.len());
        assert!(b.summary.decls.iter().all(|d| d.flags.contains(DefFlags::SYNTHETIC)));
    }

    #[test]
    fn module_names() {
        // 平移自 index.rs（引擎 FilenameToModuleName 语义）
        assert_eq!(filename_to_module_name("MyDir/MyFile.as"), "MyDir.MyFile");
        assert_eq!(filename_to_module_name("X.d.as"), "X");
        assert_eq!(filename_to_module_name("a\\b\\c.as"), "a.b.c");
        assert_eq!(filename_to_module_name("plain.as"), "plain");
    }

    #[test]
    fn builtin_lookup_via_normal_path() {
        let ws = ws_build(&[("unique://wsb/a.as", "int X = 1;\n")]);
        // B2：builtin 与普通名字同一代码路径，SYNTHETIC 标记保留
        let hits = ws.lookup(intern_sym("int8"));
        assert_eq!(hits.len(), 1, "内建命中且唯一");
        let d = ws.decl(&hits[0]);
        assert!(d.flags.contains(DefFlags::SYNTHETIC));
        assert_eq!(d.kind, DefKind::Struct);
        // 类型查找排除内建（lookup_type_def 不返回 SYNTHETIC）
        assert!(ws.lookup_type_def(intern_sym("int8")).is_none());
    }

    #[test]
    fn chain_and_cycles_semantics() {
        // 原 index.rs 继承语义用例平移（closures → ancestor_chain 现算）
        const SRC: &str = "\
class ABase {}
class AMid : ABase {}
class ALeaf : AMid {}
class Bad1 : Bad2 {}
class Bad2 : Bad1 {}
class Orphan : TMissing {}
";
        let ws = ws_build(&[("unique://wsc/a.as", SRC)]);
        let leaf = ws.lookup_type_def(intern_sym("ALeaf")).unwrap();
        let names: Vec<&str> =
            ws.ancestor_chain(&leaf).iter().map(|r| sym_str(ws.decl(r).name)).collect();
        assert_eq!(names, vec!["AMid", "ABase"], "近者在前");
        // 环检测：Bad1/Bad2 入环集合，链在首次重复处截断
        let bad1 = ws.lookup_type_def(intern_sym("Bad1")).unwrap();
        let bad2 = ws.lookup_type_def(intern_sym("Bad2")).unwrap();
        let cycles = ws.cyclic_classes();
        assert!(cycles.contains(&bad1) && cycles.contains(&bad2));
        assert_eq!(ws.ancestor_chain(&bad1).len(), 1);
        // unresolved base：链为空但不 panic
        let orphan = ws.lookup_type_def(intern_sym("Orphan")).unwrap();
        assert!(ws.ancestor_chain(&orphan).is_empty());
    }

    #[test]
    fn cycle_does_not_affect_side_branch() {
        // 原 index.rs m1_acceptance_inheritance_cycle 的旁支断言：
        // 环不影响旁支（C : A，A/B 成环，C 的链 = [A, B]）
        const SRC: &str = "class A : B {}\nclass B : A {}\nclass C : A {}\n";
        let ws = ws_build(&[("unique://wscyc/side.as", SRC)]);
        assert_eq!(ws.cyclic_classes().len(), 2, "A 与 B 应被标记为环");
        let c = ws.lookup_type_def(intern_sym("C")).unwrap();
        let names: Vec<&str> =
            ws.ancestor_chain(&c).iter().map(|r| sym_str(ws.decl(r).name)).collect();
        assert_eq!(names, vec!["A", "B"], "环不影响旁支");
    }

    #[test]
    fn reindex_reflects_base_change_immediately() {
        // C6：无缓存 ⇒ reindex 后链立即按新 agg 现算，无「清空→重建」中间态
        const V1: &str = "class Mid : Top1 {}\nclass Top1 {}\nclass Top2 {}\n";
        const V2: &str = "class Mid : Top2 {}\nclass Top1 {}\nclass Top2 {}\n";
        let path = "unique://wsrb/m.as";
        let mut ws = ws_build(&[(path, V1)]);
        let names = |ws: &Workspace, c: &DeclRef| -> Vec<String> {
            ws.ancestor_chain(c)
                .iter()
                .map(|r| sym_str(ws.decl(r).name).to_string())
                .collect()
        };
        let mid = ws.lookup_type_def(intern_sym("Mid")).unwrap();
        assert_eq!(names(&ws, &mid), vec!["Top1".to_string()]);
        let f = intern_file(path, 0);
        let _ = ws.reindex_file(f, FileKind::Script, V2.to_string());
        let mid2 = ws.lookup_type_def(intern_sym("Mid")).unwrap();
        assert_eq!(names(&ws, &mid2), vec!["Top2".to_string()]);
    }

    #[test]
    fn namespaces_named_includes_classes() {
        // B3：class 直接兼任 namespace（合成 namespace 机制消失）
        const SRC: &str = "class FVector {}\nnamespace FVector { float ZeroVector; }\n";
        let ws = ws_build(&[("unique://wsn/a.as", SRC)]);
        let nss = ws.namespaces_named(intern_sym("FVector"));
        assert_eq!(nss.len(), 2, "class + 真实 namespace 都命中");
    }

    #[test]
    fn synthetic_members_on_demand() {
        const SRC: &str = "\
delegate void FOnHit2(int Damage);
event void FOnMulticast2(float X);
class C2 {}
struct S2 {}
";
        let ws = ws_build(&[("unique://wss/a.as", SRC)]);
        let d = ws.lookup_type_def(intern_sym("FOnHit2")).unwrap();
        let ms = ws.synthetic_members(&d);
        let names: Vec<&str> = ms.iter().map(|m| sym_str(m.name)).collect();
        assert!(names.contains(&"Execute"), "单播 Execute");
        assert!(names.contains(&"ExecuteIfBound"));
        assert!(names.contains(&"BindUFunction"));
        assert_eq!(
            names.iter().filter(|n| **n == "FOnHit2").count(),
            3,
            "默认构造 + 拷贝构造 + 绑定构造（引擎 ProcessDelegates 模板全集）"
        );
        assert!(names.contains(&"opAssign"));

        let e = ws.lookup_type_def(intern_sym("FOnMulticast2")).unwrap();
        let ms = ws.synthetic_members(&e);
        let names: Vec<&str> = ms.iter().map(|m| sym_str(m.name)).collect();
        assert!(names.contains(&"Broadcast"), "多播 Broadcast");
        assert!(!names.contains(&"Execute"), "多播无 Execute");

        let c = ws.lookup_type_def(intern_sym("C2")).unwrap();
        let ms = ws.synthetic_members(&c);
        assert_eq!(ms.len(), 1);
        assert_eq!(sym_str(ms[0].name), "StaticClass");

        let s = ws.lookup_type_def(intern_sym("S2")).unwrap();
        assert!(ws.synthetic_members(&s).is_empty(), "struct 无 UClass");
    }

    #[test]
    fn members_and_reindex_incremental() {
        const V1: &str = "class T2 { int A; }\nvoid F2() {}\n";
        const V2: &str = "class T2 { int A; int B; }\nvoid G2() {}\n";
        let f = intern_file("unique://wsm/a.as", 0);
        let mut ws = ws_build(&[("unique://wsm/a.as", V1)]);
        let _ = &ws;

        let t2 = ws.lookup_type_def(intern_sym("T2")).unwrap();
        assert_eq!(ws.members(&t2).len(), 1, "成员 A");
        assert!(ws.lookup(intern_sym("F2")).len() == 1);

        // 声明面变化（F2 → G2）
        let changed = ws.reindex_file(f, FileKind::Script, V2.to_string());
        assert!(changed, "F2→G2 是声明面变化");
        assert!(ws.lookup(intern_sym("F2")).is_empty());
        assert_eq!(ws.lookup(intern_sym("G2")).len(), 1);
        let t2b = ws.lookup_type_def(intern_sym("T2")).unwrap();
        assert_eq!(t2b, t2, "DeclRef 跨重索引稳定（局部 id 不变）");
        assert_eq!(ws.members(&t2b).len(), 2, "B 加入成员表");

        // 纯函数体改动：声明面不变
        const V3: &str = "class T2 { int A; int B; }\nvoid G2() { int Local = 1; }\n";
        let changed = ws.reindex_file(f, FileKind::Script, V3.to_string());
        assert!(!changed, "函数体局部不是声明面");

        // remove：贡献清零
        ws.remove_file(f);
        assert!(ws.lookup(intern_sym("T2")).is_empty());
        assert!(ws.lookup(intern_sym("G2")).is_empty());
    }

    #[test]
    fn decl_counts() {
        // 声明计数（resolved 断言随 C7 退役——归一化语义由 expr 的
        // syn_type_base 继任用例覆盖；跨架构对账由 as-cli --new-arch 承担）
        const SRC: &str = "\
class AActor2 : UObject2
{
    float Health;
    void Tick(float DT) {}
}
enum EColor { Red, Green }
void Overload(int A) {}
void Overload(float B) {}
int Counter = 0;
";
        let ws = ws_build(&[("unique://wsd/a.d.as", SRC)]);
        let f = intern_file("unique://wsd/a.d.as", 0);
        let real = ws.files[&f].summary.decls.len();
        assert_eq!(real, 9, "class + field + method + enum + 2 枚举值 + 2 重载 + 全局变量");
        // 非合成声明数 = 全部 decls（builtin 在独立伪文件）
        let new_real: usize = ws
            .files
            .values()
            .map(|e| e.summary.decls.len())
            .sum::<usize>()
            - builtin_summary().decls.len();
        assert_eq!(new_real, real);
    }
}
