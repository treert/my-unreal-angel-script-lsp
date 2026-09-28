//! Workspace 查询容器（index-architecture.md Phase B / D37）：L3 的数据后端，
//! 取代旧 `WorkspaceIndex`（全局 DefId arena）。
//!
//! 组成（三层装配）：
//! - `files: HashMap<FileId, FileEntry>`——per-file（source + tree + lines +
//!   summary + by_parent），A 阶段已验证的并行产物；
//! - `agg: Aggregation`——薄聚合层（名字 → DeclRef 倒排）；
//! - eager 派生表（Phase B 保持预计算，Phase C 按需化）：`closures`（class
//!   继承闭包）、`resolved`（声明类型归一化）、`types`（TypeTable）。
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
use crate::id::{FileId, Sym, TypeId};
use crate::index::FileInput;
use crate::intern::{intern_file, intern_sym, sym_str};
use crate::range::{LineIndex, TextRange};
use crate::summary::{extract_summary, FileSummary, RawDecl, RawExtra};
use crate::symbol::{BaseRef, DefFlags, DefKind, ParamDecl};
use crate::types::{RefKind, SynType, TypeKind, TypeTable};

/// builtin 伪文件路径（`intern_file` 一次；FileId 进程级复用）。
pub const BUILTIN_FILE_PATH: &str = "<as-core:builtin>";

/// primitive 全集（D25：不含 `float`——裸 float 按配置归一化）。
const BUILTIN_PRIMITIVES: &[&str] = &[
    "void", "bool",
    "int8", "int16", "int", "int32", "int64",
    "uint8", "uint16", "uint", "uint32", "uint64",
    "float32", "float64", "double",
];

/// 继承闭包深度上限（防环与病态深度；环另有显式检测）。
const MAX_CLOSURE_DEPTH: usize = 256;

/// per-file 查询入口（旧 FileSnapshot 的继任）。
pub struct FileEntry {
    pub kind: crate::index::FileKind,
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
    /// 锚点复用源头声明（hover/definition 落点）
    pub name_span: TextRange,
}

/// L3 数据后端。
pub struct Workspace {
    pub config: IndexConfig,
    pub files: HashMap<FileId, FileEntry>,
    pub agg: Aggregation,
    /// class 继承闭包（Phase B eager；struct 不建——D16）
    pub closures: HashMap<DeclRef, Vec<DeclRef>>,
    pub cycle_classes: Vec<DeclRef>,
    /// 声明类型归一化表 + TypeTable（Task 3 与 types.rs 的 TypeKind::Named
    /// 切 DeclRef 一起落地——消费者 expr.rs 同批切换，孤立切换无意义）
    pub resolved: HashMap<DeclRef, crate::id::TypeId>,
    pub types: TypeTable,
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
                kind: crate::index::FileKind::Decl,
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
        let t_agg = t0.elapsed();

        let mut ws = Workspace {
            config,
            files,
            agg,
            closures: HashMap::new(),
            cycle_classes: Vec::new(),
            resolved: HashMap::new(),
            types: TypeTable::new(),
        };
        ws.build_closures();
        // resolve_decl_types：Task 3 与 types.rs 的 TypeKind::Named 切
        // DeclRef 一起落地（消费者 expr.rs 同批；孤立切换编译不过——
        // 旧 index.rs 仍持 DefId 版 TypeKind 共存期）
        crate::as_log!(
            "workspace: built {} files | parse+summary {:?} | aggregation {:?} | derived {:?}",
            ws.files.len(),
            t_parse,
            t_agg - t_parse,
            t0.elapsed() - t_agg,
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

    /// 查询期合成成员（B4，expand.rs 的继任；引擎取证见原文件头注释）：
    /// - delegate（单播）：N() / N(const N&) / opAssign / Execute /
    ///   ExecuteIfBound / BindUFunction / 绑定构造
    /// - event（多播）：公共集 + Broadcast / AddUFunction
    /// - class：StaticClass()（struct 无 UClass，不合成）
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
    pub fn reindex_file(&mut self, file: FileId, kind: crate::index::FileKind, source: String) -> bool {
        let module = self.files.get(&file).and_then(|e| e.summary.module);
        self.reindex_file_full(file, kind, module, source)
    }

    /// 全量重索引入口（module 显式传入——新增/改名后模块名随新路径重算）。
    pub fn reindex_file_full(
        &mut self,
        file: FileId,
        kind: crate::index::FileKind,
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
                // 摘除旧派生条目（closures 全量重建——量级毫秒）
                self.closures.clear();
                self.cycle_classes.clear();
                self.resolved.retain(|r, _| r.file != file);
                self.files.insert(file, entry);
                self.build_closures();
            }
            None => {
                self.agg.replace_file(file, &crate::summary::FileSummary::default_for(kind), &entry.summary);
                self.files.insert(file, entry);
                self.build_closures();
            }
        }
        let new_surface = decl_surface(&self.files[&file].summary);
        old_surface.map_or(true, |old| old != new_surface)
    }

    /// 删除文件：贡献摘除 + 派生重建 + 墓碑（D18 的 FileId 层语义不变）。
    pub fn remove_file(&mut self, file: FileId) {
        if let Some(entry) = self.files.remove(&file) {
            self.agg.remove_file(file, &entry.summary);
            self.closures.clear();
            self.cycle_classes.clear();
            self.resolved.retain(|r, _| r.file != file);
            self.build_closures();
            crate::intern::tombstone_file(file);
        }
    }

    // -----------------------------------------------------------------------
    // eager 派生（Phase B 保持预计算；Phase C 按需化）
    // -----------------------------------------------------------------------

    /// class 继承闭包（平移自 index.rs build_closures；struct 不建——D16）。
    fn build_closures(&mut self) {
        let classes: Vec<DeclRef> = self
            .files
            .iter()
            .flat_map(|(&file, e)| {
                e.summary
                    .decls
                    .iter()
                    .enumerate()
                    .filter(|(_, d)| {
                        d.kind == DefKind::Class && !d.flags.contains(DefFlags::SYNTHETIC)
                    })
                    .map(move |(i, _)| DeclRef { file, local: i as u32 })
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut cyclic: Vec<DeclRef> = Vec::new();
        for class in classes {
            let mut chain: Vec<DeclRef> = Vec::new();
            let mut path: Vec<DeclRef> = vec![class];
            let mut cur = class;
            for _ in 0..MAX_CLOSURE_DEPTH {
                let Some(base) = self.resolve_base_class(&cur) else { break };
                if let Some(pos) = path.iter().position(|&d| d == base) {
                    for &d in &path[pos..] {
                        if !cyclic.contains(&d) {
                            cyclic.push(d);
                        }
                    }
                    break;
                }
                path.push(base);
                chain.push(base);
                cur = base;
            }
            self.closures.insert(class, chain);
        }
        self.cycle_classes = cyclic;
    }

    /// 基名 → Class 声明（simple 形态才解析；查不到 = 链到此为止）。
    pub fn resolve_base_class(&self, def: &DeclRef) -> Option<DeclRef> {
        let d = self.decl(def);
        let base = d.bases.iter().find(|b| b.simple)?;
        self.lookup(base.name)
            .iter()
            .copied()
            .find(|&r| {
                let bd = self.decl(&r);
                bd.kind == DefKind::Class && !bd.flags.contains(DefFlags::SYNTHETIC)
            })
    }

    // 声明类型归一化 + `resolve_syn`：Task 3 与 types.rs 的
    // `TypeKind::Named.def` 切 DeclRef 一起落地（旧 index.rs 仍持
    // DefId 版 TypeKind 共存期，孤立切换编译不过；消费者 expr.rs 同批）。
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
        kind: crate::index::FileKind::Decl,
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
    fn default_for(kind: crate::index::FileKind) -> FileSummary {
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
        Self::default_for(crate::index::FileKind::Script)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{FileKind, WorkspaceIndex};

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
    fn closures_and_cycles_match_old_semantics() {
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
        let chain = ws.closures.get(&leaf).unwrap();
        let names: Vec<&str> = chain.iter().map(|r| sym_str(ws.decl(r).name)).collect();
        assert_eq!(names, vec!["AMid", "ABase"], "近者在前");
        // 环检测：Bad1/Bad2 入 cycle_classes，链在首次重复处截断
        let bad1 = ws.lookup_type_def(intern_sym("Bad1")).unwrap();
        let bad2 = ws.lookup_type_def(intern_sym("Bad2")).unwrap();
        assert!(ws.cycle_classes.contains(&bad1) && ws.cycle_classes.contains(&bad2));
        assert_eq!(ws.closures.get(&bad1).unwrap().len(), 1);
        // unresolved base：链为空但不 panic
        let orphan = ws.lookup_type_def(intern_sym("Orphan")).unwrap();
        assert!(ws.closures.get(&orphan).unwrap().is_empty());
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
    fn decl_counts_match_old_index() {
        // 与旧架构的声明对账（同源同配置）
        const SRC: &str = "\
class AActor2 : UObject2
{
    float Health;
    void Tick(float DT) {}
}
enum EColor { Red, Green }
void Overload(int A) {}
void Overload(float B) {}
";
        let ws = ws_build(&[("unique://wsd/a.d.as", SRC)]);
        let inputs = vec![FileInput {
            file: intern_file("unique://wsd/a.d.as", 0),
            kind: FileKind::Decl,
            source: SRC.to_string(),
            module: None,
        }];
        let old = WorkspaceIndex::build(IndexConfig::default(), inputs);
        let old_real = old
            .symbols
            .iter()
            .filter(|(_, d)| !d.flags.contains(DefFlags::SYNTHETIC))
            .count();
        // 新侧不含 builtin（15 个）与合成展开成员
        let new_real: usize = ws
            .files
            .values()
            .map(|e| e.summary.decls.len())
            .sum::<usize>()
            - builtin_summary().decls.len();
        assert_eq!(new_real, old_real, "非合成声明数一致");
    }
}
