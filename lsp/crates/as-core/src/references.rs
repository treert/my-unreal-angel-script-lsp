//! references 内核（LSP实现规划 §4.1 Phase 3 / 架构设计 §4.6 / D5）。
//!
//! 数据流：引用倒排（`WorkspaceIndex::ref_index`）给候选文件集 → 逐文件解析
//! UseSite（`resolve_at` 逐站点驱动——Phase 3 惰性，as-lsp 侧有按文件缓存）
//! → 匹配。
//!
//! **匹配语义（架构设计 §4.6 / M4 定案）**：
//! - 站点解析集合 ∩ 查询集合 ≠ ∅ 即命中——无法消歧的调用点对组内每个重载
//!   都算引用（不可排除）；
//! - 查询点本身消歧失败（重载组）⇒ 调用方对组内全部成员取并集
//!   （**消歧失败报全部重载**）；
//! - **DefId 匹配不做 origin 归一**（D10 的回落只影响 definition/hover 的
//!   落点与声明位置的展示）：合成成员（delegate 的 Execute / StaticClass）
//!   是独立名字，按各自 DefId 独立 references。**唯一例外是 class 的合成
//!   同名 namespace**——它就是类名本身，归一到类（`AActor::` 限定段计入
//!   类引用，见 [`origin_fallback`]）；
//! - 局部变量无 DefId，按 `(file, 声明 span)` 身份匹配（SemCtx 逐站点独立
//!   解析，遮蔽天然正确），引用天然限声明所在文件；
//! - 内建 primitive（合成 DefId、无源码文件）不可 references/rename——
//!   调用方提前返回空。

use std::collections::BTreeSet;

use crate::id::{DefId, FileId, Sym};
use crate::index::WorkspaceIndex;
use crate::range::TextRange;
use crate::resolve::{self, Target};
use crate::symbol::{DefFlags, DefKind};

/// 归一化引用目标。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RefTarget {
    Def(DefId),
    /// 局部变量 / 形参：语法层声明（索引不收函数体），按声明 span 唯一。
    Local { file: FileId, span: TextRange },
}

/// 一个使用点的解析结果（目标集合——重载组原样保留，消歧语义见上）。
#[derive(Clone, Debug)]
pub struct UseResolution {
    pub span: TextRange,
    pub targets: Vec<RefTarget>,
}

/// class 合成同名 namespace → class 本身（唯一做 origin 归一的形态）。
/// delegate/event 展开成员（Execute 等）与 StaticClass 是独立名字，不归一。
pub fn origin_fallback(idx: &WorkspaceIndex, id: DefId) -> DefId {
    let d = idx.def(id);
    if d.kind == DefKind::Namespace && d.flags.contains(DefFlags::SYNTHETIC) {
        if let Some(origin) = d.origin {
            return origin;
        }
    }
    id
}

/// 单文件全部 UseSite 的解析（纯函数；as-lsp 侧有按文件缓存，D5）。
/// 节点定位走 tree-sitter 原生 `descendant_for_byte_range`（O(log n)）+
/// `resolve_at_node`（parent 链上溯）——不做从根下潜的重遍历。
pub fn resolve_file_uses(idx: &WorkspaceIndex, file: FileId) -> Vec<UseResolution> {
    let Some(snap) = idx.files.get(&file) else { return Vec::new() };
    let root = snap.tree.root_node();
    let mut out = Vec::with_capacity(snap.uses.len());
    for site in &snap.uses {
        let ident = root.descendant_for_byte_range(
            site.span.start as usize,
            site.span.end as usize,
        );
        let res = match ident {
            Some(n) => resolve::resolve_at_node(idx, file, &snap.source, n),
            // 防御：提取与解析同树，正常必然命中；未命中按字节偏移重试
            None => resolve::resolve_at(idx, file, site.span.start),
        };
        let Some(res) = res else { continue };
        let mut targets = Vec::with_capacity(res.targets.len());
        for t in res.targets {
            match t {
                Target::Def(id) => targets.push(RefTarget::Def(origin_fallback(idx, id))),
                Target::Local(l) => targets.push(RefTarget::Local { file, span: l.name_span }),
            }
        }
        if targets.is_empty() {
            continue;
        }
        out.push(UseResolution { span: site.span, targets });
    }
    out
}

/// 内建 / 已摘除声明的目标（声明文件无快照）。调用方对这类查询返回空。
pub fn is_unreferenced_target(idx: &WorkspaceIndex, t: &RefTarget) -> bool {
    match t {
        RefTarget::Def(id) => idx.files.get(&idx.def(*id).file).is_none(),
        RefTarget::Local { .. } => false,
    }
}

/// 引用倒排给出的候选文件集（∪ 声明文件；过滤已摘除快照）。升序稳定。
pub fn candidate_files(idx: &WorkspaceIndex, targets: &[RefTarget]) -> Vec<FileId> {
    let mut set: BTreeSet<FileId> = BTreeSet::new();
    for t in targets {
        match *t {
            RefTarget::Def(id) => {
                let d = idx.def(id);
                if idx.files.get(&d.file).is_none() {
                    continue; // 内建（无快照）
                }
                set.insert(d.file);
                if let Some(fs) = idx.ref_index.get(&d.name) {
                    set.extend(fs.iter().copied());
                }
            }
            RefTarget::Local { file, .. } => {
                set.insert(file);
            }
        }
    }
    set.into_iter().filter(|&f| idx.files.contains_key(&f)).collect()
}

/// 站点匹配：解析集合 ∩ 查询集合 ≠ ∅。
pub fn match_uses(targets: &[RefTarget], resolved: &[UseResolution]) -> Vec<TextRange> {
    resolved
        .iter()
        .filter(|u| u.targets.iter().any(|t| targets.contains(t)))
        .map(|u| u.span)
        .collect()
}

/// rename 的**严格匹配**（比 references 严）：只有「解析结果唯一且等于目标」
/// 的站点才可改写——歧义站点（重载组）可能属于其它重载，改写会误伤
/// （宁缺毋假，D14 同族；M4 定案）。
pub fn match_uses_strict(targets: &[RefTarget], resolved: &[UseResolution]) -> Vec<TextRange> {
    resolved
        .iter()
        .filter(|u| u.targets.len() == 1 && targets.contains(&u.targets[0]))
        .map(|u| u.span)
        .collect()
}

/// 纯函数版 references（单测与 as-cli 用；as-lsp 侧走缓存分批，语义相同）。
pub fn find_references(idx: &WorkspaceIndex, targets: &[RefTarget]) -> Vec<(FileId, TextRange)> {
    let mut out = Vec::new();
    for file in candidate_files(idx, targets) {
        let resolved = resolve_file_uses(idx, file);
        for span in match_uses(targets, &resolved) {
            out.push((file, span));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 查询期 references（D5 翻案 / index-architecture §6，Phase B1：
// 先对旧 WorkspaceIndex 实现——B3 平移到 Workspace 后倒排路径整体删除）
// ---------------------------------------------------------------------------

/// 词边界搜索（大小写敏感）：`word` 在 `src` 的全部出现起始字节偏移。
/// 边界 = `[A-Za-z0-9_]` 之外（防 `Health` 误中 `GetHealthTime`）。
/// 命中含注释 / 字符串 / 声明名——由调用方逐点解析验证滤掉（超集过滤）。
pub fn find_word_occurrences(src: &str, word: &str) -> Vec<u32> {
    let bytes = src.as_bytes();
    let w = word.as_bytes();
    debug_assert!(!w.is_empty());
    let is_id = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + w.len() <= bytes.len() {
        if &bytes[i..i + w.len()] == w {
            let left_ok = i == 0 || !is_id(bytes[i - 1]);
            let right_end = i + w.len();
            let right_ok = right_end >= bytes.len() || !is_id(bytes[right_end]);
            if left_ok && right_ok {
                out.push(i as u32);
            }
        }
        // 无匹配处快进：找下一个候选首字节
        i += 1;
    }
    out
}

/// 查询目标的名字集合（Def 组名字 ∪ 局部变量名字——局部名字从声明锚点
/// 的 CST 节点取回）。返回 `(Sym, 源文本)` 对，去重。
fn query_names(idx: &WorkspaceIndex, targets: &[RefTarget]) -> Vec<(Sym, &'static str)> {
    let mut out: Vec<(Sym, &'static str)> = Vec::new();
    for t in targets {
        match *t {
            RefTarget::Def(id) => {
                let name = idx.def(id).name;
                let s = crate::intern::sym_str(name);
                if !out.iter().any(|(n, _)| *n == name) {
                    out.push((name, s));
                }
            }
            RefTarget::Local { file, span } => {
                // 局部名字：声明锚点处的 CST 节点文本
                if let Some(snap) = idx.files.get(&file) {
                    if let Some(node) = snap
                        .tree
                        .root_node()
                        .descendant_for_byte_range(span.start as usize, span.end as usize)
                    {
                        let s = node.utf8_text(snap.source.as_bytes()).unwrap_or("");
                        let name = crate::intern::intern_sym(s);
                        if !out.iter().any(|(n, _)| *n == name) {
                            out.push((name, crate::intern::sym_str(name)));
                        }
                    }
                }
            }
        }
    }
    out
}

/// 查询期 references（字符串搜 + 逐点解析验证，mylua 同构）。
/// 语义与 [`find_references`]（倒排路径）完全一致——站点解析集合 ∩ 查询
/// 集合 ≠ ∅ 即命中；`strict`（rename）= 解析唯一且等于目标。
pub fn find_references_query(
    idx: &WorkspaceIndex,
    targets: &[RefTarget],
    strict: bool,
) -> Vec<(FileId, TextRange)> {
    let names = query_names(idx, targets);
    if names.is_empty() {
        return Vec::new();
    }
    let has_def_target = targets.iter().any(|t| matches!(t, RefTarget::Def(_)));
    // 文件级 rayon 并行（设计稿 §6.2 预留）：文件间完全独立，resolve 只读。
    // 串行实测 10MB 语料 ~700ms/查询（逐 occurrence 全链解析），并行后
    // 进入可接受区间；文件内仍串行（保持命中序 = 源码序）。
    use rayon::prelude::*;
    let mut out: Vec<(FileId, TextRange)> = idx
        .files
        .par_iter()
        .flat_map_iter(|(&file, snap)| {
            // 局部变量目标：引用天然限声明所在文件（Def 目标才全库扫）
            if !has_def_target {
                let declared_here = targets.iter().any(|t| match *t {
                    RefTarget::Local { file: f, .. } => f == file,
                    RefTarget::Def(_) => false,
                });
                if !declared_here {
                    return Vec::new();
                }
            }
            let root = snap.tree.root_node();
            let mut hits: Vec<u32> = Vec::new();
            for (_, word) in &names {
                hits.extend(find_word_occurrences(&snap.source, word));
            }
            hits.sort_unstable();
            hits.dedup();
            let mut file_hits = Vec::new();
            for off in hits {
                let Some(node) = root.descendant_for_byte_range(off as usize, off as usize + 1)
                else {
                    continue;
                };
                // 命中必须是 identifier 节点本身（注释 / 字符串 / 类型 token 滤掉）
                if node.kind() != "identifier" || node.start_byte() as u32 != off {
                    continue;
                }
                let Some(res) = resolve::resolve_at_node(idx, file, &snap.source, node) else {
                    continue;
                };
                // 声明名位点不计引用（旧路径 UseSite 提取排除声明名——
                // `is_decl_name`；此处按解析级数等价排除）
                if res.level == crate::resolve::LEVEL_DECL_SELF {
                    continue;
                }
                let mut matched = false;
                for t in &res.targets {
                    let rt = match t {
                        Target::Def(id) => RefTarget::Def(origin_fallback(idx, *id)),
                        Target::Local(l) => RefTarget::Local { file, span: l.name_span },
                    };
                    if targets.contains(&rt) && (!strict || res.targets.len() == 1) {
                        matched = true;
                        break;
                    }
                }
                if matched {
                    file_hits
                        .push((file, crate::range::TextRange::new(off, node.end_byte() as u32)));
                }
            }
            file_hits
        })
        .collect::<Vec<_>>();
    out.sort_by_key(|(f, s)| (*f, s.start, s.end));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IndexConfig;
    use crate::index::{FileInput, FileKind};
    use crate::intern::{intern_file, intern_sym, sym_str};
    use crate::resolve::{resolve_at, Target, LEVEL_DECL_SELF, LEVEL_MIXIN};
    use crate::symbol::DefExtra;
    use crate::types::SynType;

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

    /// 光标处的查询目标（Def 组或 Local）。
    fn query_at(idx: &WorkspaceIndex, file: FileId, _src: &str, byte: u32) -> Vec<RefTarget> {
        let r = resolve_at(idx, file, byte).expect("应命中");
        r.targets
            .iter()
            .map(|t| match t {
                Target::Def(id) => RefTarget::Def(origin_fallback(idx, *id)),
                Target::Local(l) => RefTarget::Local { file, span: l.name_span },
            })
            .collect()
    }

    // ------------------------------------------------------------------
    // 查询期内核（B1）：词边界搜索 + 与倒排路径的等价性
    // ------------------------------------------------------------------

    #[test]
    fn word_occurrences_respect_boundaries() {
        let src = "Health GetHealthTime HealthX MyHealth Health";
        let hits = find_word_occurrences(src, "Health");
        // 只命中独立词：首尾两处；前缀/后缀/中缀全部排除
        assert_eq!(hits, vec![0, 38], "词边界过滤（源串长 44，尾词起始 38）");
        assert_eq!(find_word_occurrences("health Health", "Health"), vec![7], "大小写敏感");
        assert!(find_word_occurrences("abc", "abcd").is_empty(), "超长不命中");
    }

    #[test]
    fn query_time_matches_inverted_index() {
        // 覆盖：跨文件 / 重载组（消歧失败报全部）/ 局部变量（限本文件）/
        // 声明名不计 / f-string 插值段
        const A: &str = "\
int Counter = 0;
void Target2() {}
void OverloadFn(int A) {}
void OverloadFn(float B) {}
void F()
{
    int Local = 1;
    int X = Local + Counter;
    Print(f\"{Local}\");
    OverloadFn(X);
    OverloadFn(1.5);
}
";
        const B: &str = "\
void G()
{
    int Local = 9;   // 另一文件的同名局部——不串
    Target2();
    OverloadFn(1);
}
";
        let idx = build(&[("unique://qtime/a.as", A), ("unique://qtime/b.as", B)]);
        let fa = file_of("unique://qtime/a.as");
        let queries: Vec<(u32, &str)> = vec![
            (off(A, "Counter = 0"), "全局变量（声明名锚点查询）"),
            (off(A, "Target2() {}"), "跨文件函数"),
            (nth(A, "OverloadFn(", 2), "重载组"),
            (off(A, "Local + Counter"), "局部变量"),
        ];
        for (byte, what) in queries {
            let targets = query_at(&idx, fa, A, byte);
            let old = find_references(&idx, &targets);
            let new = find_references_query(&idx, &targets, false);
            assert_eq!(old, new, "两法结果不等（{what}）: old {old:?} vs new {new:?}");
        }
        // 局部变量：另一文件同名局部不得串
        let targets = query_at(&idx, fa, A, off(A, "Local + Counter"));
        let hits = find_references_query(&idx, &targets, false);
        assert!(hits.iter().all(|(f, _)| *f == fa), "局部限本文件");
        // 声明名不计：Counter 全部引用 = 使用点 1 处（声明除外）
        let targets = query_at(&idx, fa, A, off(A, "Counter = 0"));
        let hits = find_references_query(&idx, &targets, false);
        assert_eq!(hits.len(), 1, "声明位点不计引用");
    }

    // ------------------------------------------------------------------
    // 基础：跨文件引用 / 局部变量 / mixin / enum 值
    // ------------------------------------------------------------------

    #[test]
    fn cross_file_function_references() {
        const A: &str = "void Target() {}\nvoid Other() { Target(); Target(); }\n";
        const B: &str = "void User() { Target(); }\n";
        let idx = build(&[("unique://ref/a.as", A), ("unique://ref/b.as", B)]);
        let targets = query_at(&idx, file_of("unique://ref/a.as"), A, off(A, "Target() {}"));
        let refs = find_references(&idx, &targets);
        assert_eq!(refs.len(), 3, "a.as 两处 + b.as 一处");
        let in_a = refs.iter().filter(|(f, _)| *f == file_of("unique://ref/a.as")).count();
        let in_b = refs.iter().filter(|(f, _)| *f == file_of("unique://ref/b.as")).count();
        assert_eq!((in_a, in_b), (2, 1));
        // 声明名本身不计入（UseSite 排除）
        let decl_start = off(A, "Target() {}");
        assert!(refs.iter().all(|(_, s)| s.start != decl_start));
    }

    #[test]
    fn local_references_with_shadowing() {
        const SRC: &str = "\
void F()
{
    int X = 1;
    {
        float X = 2.0;
        int A = X;
    }
    int B = X;
}
";
        let idx = build(&[("unique://ref/shadow.as", SRC)]);
        let file = file_of("unique://ref/shadow.as");
        // 内层 X（float）声明：LEVEL_DECL_SELF + Local
        let r = resolve_at(&idx, file, off(SRC, "X = 2.0")).unwrap();
        assert_eq!(r.level, LEVEL_DECL_SELF);
        let inner = query_at(&idx, file, SRC, off(SRC, "X = 2.0"));
        let refs = find_references(&idx, &inner);
        assert_eq!(refs.len(), 1, "只命中内层使用点（int A = X）");
        // 外层 X（int）声明
        let outer = query_at(&idx, file, SRC, off(SRC, "X = 1"));
        let refs = find_references(&idx, &outer);
        assert_eq!(refs.len(), 1, "只命中外层使用点（int B = X）——遮蔽不误报");
    }

    #[test]
    fn mixin_call_site_references() {
        const SRC: &str = "\
class AActor {}
mixin void Heal(AActor Target, float Amount) {}
class C : AActor
{
    void M() { Heal(1.0); }
}
";
        let idx = build(&[("unique://ref/mixin.as", SRC)]);
        let file = file_of("unique://ref/mixin.as");
        let r = resolve_at(&idx, file, off(SRC, "Heal(1.0)")).unwrap();
        assert_eq!(r.level, LEVEL_MIXIN, "mixin 命中");
        let targets = query_at(&idx, file, SRC, off(SRC, "Heal(1.0)"));
        let refs = find_references(&idx, &targets);
        assert_eq!(refs.len(), 1, "调用点计入 mixin 声明的 references");
    }

    #[test]
    fn enum_value_references() {
        const SRC: &str = "\
enum EColor { Red, Green }
void F()
{
    EColor A = EColor::Red;
    EColor B = EColor::Green;
}
";
        let idx = build(&[("unique://ref/enum.as", SRC)]);
        let file = file_of("unique://ref/enum.as");
        let red = query_at(&idx, file, SRC, off(SRC, "Red"));
        let refs = find_references(&idx, &red);
        assert_eq!(refs.len(), 1, "EColor::Red 的使用点");
        assert_eq!(refs[0].1.start, nth(SRC, "Red", 2), "落在使用点（EColor::Red）");
        let green = query_at(&idx, file, SRC, off(SRC, "Green"));
        assert_eq!(find_references(&idx, &green).len(), 1);
    }

    // ------------------------------------------------------------------
    // 重载消歧：成功 / 失败双路径（规划 §9 M4 验收）
    // ------------------------------------------------------------------

    #[test]
    fn overload_disambiguation_by_arg_type() {
        const SRC: &str = "\
struct FString {}
void F(int A) {}
void F(FString S) {}
void Calls()
{
    F(1);
    F(\"x\");
    F(Unknown());
}
";
        let idx = build(&[("unique://ref/ovl.as", SRC)]);
        let file = file_of("unique://ref/ovl.as");

        let param0_is = |id: DefId, name: &str| match &idx.def(id).extra {
            DefExtra::Callable { params, .. } => match &params[0].ty {
                Some(SynType::Primitive(n, _)) => sym_str(*n) == name,
                Some(SynType::Named(n, _)) => sym_str(*n) == name,
                _ => false,
            },
            _ => false,
        };

        // 成功路径 ①：字面量实参类型消歧
        let r = resolve_at(&idx, file, off(SRC, "F(1);")).unwrap();
        assert_eq!(r.targets.len(), 1, "int 实参唯一命中");
        let Target::Def(int_overload) = r.targets[0] else { panic!() };
        assert!(param0_is(int_overload, "int"));
        let r = resolve_at(&idx, file, off(SRC, "F(\"x\");")).unwrap();
        assert_eq!(r.targets.len(), 1, "字符串实参唯一命中");
        let Target::Def(str_overload) = r.targets[0] else { panic!() };
        assert!(param0_is(str_overload, "FString"));

        // 成功路径 ②：arity 消歧
        let r = resolve_at(&idx, file, off(SRC, "F(Unknown());")).unwrap();
        assert_eq!(r.targets.len(), 2, "不可定型实参 ⇒ 消歧失败，保留全部重载");

        // references 侧：消歧成功的站点归各自重载；**不可消歧的站点
        // （F(Unknown())）对组内每个重载都算引用**（架构设计 §4.6——
        // 不可排除）⇒ int = F(1) + F(Unknown())，FString = F("x") + F(Unknown())
        let int_refs = find_references(&idx, &[RefTarget::Def(int_overload)]);
        assert_eq!(int_refs.len(), 2, "F(1) + F(Unknown())");
        let str_refs = find_references(&idx, &[RefTarget::Def(str_overload)]);
        assert_eq!(str_refs.len(), 2, "F(\"x\") + F(Unknown())");

        // 消歧失败 ⇒ 报全部重载（查询组 = 两个重载的并集，与单查询取并同）
        let both = find_references(
            &idx,
            &[RefTarget::Def(int_overload), RefTarget::Def(str_overload)],
        );
        assert_eq!(both.len(), 3, "F(1) + F(\"x\") + F(Unknown()) 全部计入");
    }

    #[test]
    fn overload_disambiguation_by_arity() {
        const SRC: &str = "\
void F2(int A) {}
void F2(int A, int B) {}
void C()
{
    F2(1);
    F2(1, 2);
}
";
        let idx = build(&[("unique://ref/ar.as", SRC)]);
        let file = file_of("unique://ref/ar.as");
        let r = resolve_at(&idx, file, off(SRC, "F2(1);")).unwrap();
        assert_eq!(r.targets.len(), 1, "单实参 ⇒ 一参重载");
        let Target::Def(one) = r.targets[0] else { panic!() };
        let r = resolve_at(&idx, file, off(SRC, "F2(1, 2);")).unwrap();
        assert_eq!(r.targets.len(), 1, "双实参 ⇒ 两参重载");
        let Target::Def(two) = r.targets[0] else { panic!() };
        assert_ne!(one, two);
        assert_eq!(find_references(&idx, &[RefTarget::Def(one)]).len(), 1);
        assert_eq!(find_references(&idx, &[RefTarget::Def(two)]).len(), 1);
    }

    // ------------------------------------------------------------------
    // 合成符号的引用语义（D27 取舍）
    // ------------------------------------------------------------------

    #[test]
    fn delegate_members_are_independent_reference_targets() {
        const SRC: &str = "\
delegate void FOnHit(int Damage);
class A
{
    FOnHit OnHit;
    void M() { OnHit.Execute(5); OnHit.Execute(6); }
    void N() { FOnHit H; }
}
";
        let idx = build(&[("unique://ref/dlg.as", SRC)]);
        let file = file_of("unique://ref/dlg.as");

        // Execute（合成成员）独立 references
        let exec = query_at(&idx, file, SRC, off(SRC, "Execute"));
        let exec_refs = find_references(&idx, &exec);
        assert_eq!(exec_refs.len(), 2, "两个 Execute 调用点");

        // 委托声明 FOnHit 的 references = 类型使用点，不含 Execute 站点
        let decl = query_at(&idx, file, SRC, off(SRC, "FOnHit(int"));
        let decl_refs = find_references(&idx, &decl);
        assert_eq!(decl_refs.len(), 2, "FOnHit OnHit; + FOnHit H;（Execute 不并入）");
    }

    #[test]
    fn class_references_include_qualifier_segment() {
        const SRC: &str = "\
class AActor {}
void F() { UClass C = AActor::StaticClass(); }
";
        let idx = build(&[("unique://ref/cls.as", SRC)]);
        let file = file_of("unique://ref/cls.as");
        // 查询：类声明本身
        let class = query_at(&idx, file, SRC, off(SRC, "AActor {}"));
        // 站点：AActor:: 限定段（第 2 次出现）经合成 namespace 归一到类
        let refs = find_references(&idx, &class);
        assert_eq!(refs.len(), 1, "AActor:: 限定段计入类引用（origin_fallback）");
        assert_eq!(refs[0].1.start, nth(SRC, "AActor", 2));
    }

    // ------------------------------------------------------------------
    // 文件增删一致性（规划 §5.3 / §9 M4 验收）
    // ------------------------------------------------------------------

    #[test]
    fn remove_file_then_revive_keeps_consistency() {
        const LIB: &str = "class CLib { int Field; }\n";
        const USER: &str = "void F() { CLib C; }\n";
        let lib_path = "unique://reflife/lib.as";
        let user_file = file_of("unique://reflife/user.as");
        let mut idx = build(&[(lib_path, LIB), ("unique://reflife/user.as", USER)]);
        let lib_file = file_of(lib_path);

        let cls = query_at(&idx, lib_file, LIB, off(LIB, "CLib"));
        assert_eq!(find_references(&idx, &cls).len(), 1, "user 的类型使用点");

        // 删除：defs 摘除 + 幽灵符号不得残留
        idx.remove_file(lib_file);
        assert!(idx.lookup_type_def(intern_sym("CLib")).is_none(), "声明不可达");
        assert!(idx.files.get(&lib_file).is_none(), "快照摘除");
        assert!(
            !idx.ref_index.get(&intern_sym("CLib")).map_or(false, |s| s.contains(&lib_file)),
            "引用倒排摘除该文件的贡献"
        );
        assert!(
            resolve_at(&idx, user_file, off(USER, "CLib C")).is_none(),
            "使用点随之失效"
        );

        // 复活（同路径 ⇒ 同 FileId，墓碑翻回）：声明与 references 恢复
        idx.reindex_file_full(lib_file, FileKind::Script, None, LIB.to_string());
        assert!(idx.lookup_type_def(intern_sym("CLib")).is_some());
        let cls2 = query_at(&idx, user_file, USER, off(USER, "CLib C"));
        assert_eq!(find_references(&idx, &cls2).len(), 1);
    }

    // ------------------------------------------------------------------
    // 指纹：decl_surface（D29，as-lsp 缓存联动失效的判定）
    // ------------------------------------------------------------------

    #[test]
    fn for_each_loop_var_references() {
        const SRC: &str = "\
int[] Items;
void F()
{
    for (int Elem : Items) { int A = Elem; }
    for (int Elem : Items) { int B = Elem; }
}
";
        let idx = build(&[("unique://ref/fe.as", SRC)]);
        let file = file_of("unique://ref/fe.as");
        // 第一个迭代变量的引用：第 1 个循环体的 Elem 使用点
        let decl1 = query_at(&idx, file, SRC, off(SRC, "Elem :"));
        let refs = find_references(&idx, &decl1);
        assert_eq!(refs.len(), 1, "只命中本循环体的使用点");
        assert_eq!(refs[0].1.start, nth(SRC, "Elem", 2));
        // 第二个循环：同名迭代变量是独立声明
        let decl2 = query_at(&idx, file, SRC, nth(SRC, "Elem :", 2));
        let refs2 = find_references(&idx, &decl2);
        assert_eq!(refs2.len(), 1);
        assert_eq!(refs2[0].1.start, nth(SRC, "Elem", 4));
    }
}
