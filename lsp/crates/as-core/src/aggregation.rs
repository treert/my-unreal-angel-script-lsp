//! 薄聚合层（LSP实现规划 Phase A / index-architecture.md §4，D37）。
//!
//! 职责边界（刻意做薄）：**只回答「名字在哪个文件的哪个位置」**——
//! `名字 → [(文件, 局部 id)]` 倒排。不存签名、不存类型语义、不做任何
//! 语义判断（那些在 L3 拿 DeclRef 回源文件 summary 现取）。
//!
//! 候选排序（§4.2，错误排序 = `class Foo : UObject` 解析到错误基类）：
//! 根优先级（脚本根 < `.d.as` 根，root_index 升序）→ FileId → 局部 id
//! （= 源码序）。构建时一次排定。
//!
//! 增量（§4.3）：贡献倒排（`ContributionKey`）记录每个文件动过哪些桶
//! ——单文件替换 = 摘旧贡献 O(旧声明数) + 扫新贡献 O(新声明数)，其余
//! 文件条目不动。取代旧架构的全表 `remove_file_defs` 扫描与 D29 指纹。

use std::collections::{BTreeSet, HashMap};

use crate::id::{FileId, Sym};
use crate::intern::file_meta;
use crate::summary::{syn_base_name, FileSummary, RawExtra};
use crate::symbol::DefFlags;

/// 跨文件声明锚点（DefId 的替代，Phase A 产出 / Phase B 消费）：
/// 文件内局部 id 在文件重索引时**不扰动其他文件**。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DeclRef {
    pub file: FileId,
    pub local: u32,
}

/// 一个文件对聚合层的贡献记录（增量更新的钥匙）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ContributionKey {
    /// `main` 桶（名字，局部 id）
    Main(Sym, u32),
    /// `mixin_by_name` 桶（首参基名，局部 id）
    Mixin(Sym, u32),
    /// `module_members`（模块名）
    Module(Sym),
}

/// 聚合层：两张名字倒排 + 模块成员表 + 贡献倒排（私有）。
#[derive(Debug, Default)]
pub struct Aggregation {
    /// 名字 → 声明锚点（跨文件）。重载组原样保留（消歧语义不变），
    /// 桶内按 §4.2 排序规则稳定有序。
    pub main: HashMap<Sym, Vec<DeclRef>>,
    /// mixin 名字倒排（§5.3，取代旧 DefId 倒排 D23）：首参基名 → mixin
    /// 声明锚点。**未解析的目标也是有效键**（`mixin_pending` 概念删除——
    /// 名字键天然容错，没人查的键不占语义）。
    pub mixin_by_name: HashMap<Sym, Vec<DeclRef>>,
    /// 模块名 → 成员文件（`local` 函数可见域过滤）；映射取
    /// [`Aggregation::module_file`]（root 优先序最优者）。
    module_members: HashMap<Sym, BTreeSet<FileId>>,
    /// FileId → 该文件动过的桶（增量钥匙）
    contributions: HashMap<FileId, Vec<ContributionKey>>,
}

/// 排序键：root 优先级（脚本根 < decl 根）→ FileId → 局部 id。
fn sort_key(r: &DeclRef) -> (u32, FileId, u32) {
    let root = file_meta(r.file).map_or(0, |m| m.root_index);
    (root, r.file, r.local)
}

impl Aggregation {
    /// 冷启动：全部 summary 就绪后一遍扫描（mylua `build_initial` 同款——
    /// 消除批序依赖）。O(总声明数)。
    pub fn build(files: &HashMap<FileId, FileSummary>) -> Aggregation {
        let mut agg = Aggregation::default();
        for (file, s) in files {
            agg.absorb(*file, s);
        }
        // 单遍吸收后统一排序（确定性：排序键与插入序无关）
        for bucket in agg.main.values_mut().chain(agg.mixin_by_name.values_mut()) {
            bucket.sort_by_key(|r| sort_key(r));
        }
        agg
    }

    /// 单文件替换（didChange / watched-files 变更）：摘旧贡献 → 扫新贡献
    /// → 重排受影响桶。O(新旧声明数)，不触碰其他文件的条目。
    pub fn replace_file(&mut self, file: FileId, old: &FileSummary, new: &FileSummary) {
        self.retract(file, old);
        self.absorb(file, new);
        self.resort_touched(file);
    }

    /// 单文件删除（watched-files 删除 / 改名的前半）：贡献清零，
    /// 其余文件不受影响。
    pub fn remove_file(&mut self, file: FileId, old: &FileSummary) {
        self.retract(file, old);
    }

    /// 模块映射：成员中 root 优先序最优者（同 root 取 FileId 小者，确定性）。
    pub fn module_file(&self, module: Sym) -> Option<FileId> {
        let members = self.module_members.get(&module)?;
        members
            .iter()
            .copied()
            .min_by_key(|&f| (file_meta(f).map_or(0, |m| m.root_index), f))
    }

    /// 扫入一个文件的全部贡献（私有；build / replace_file 共用）。
    fn absorb(&mut self, file: FileId, s: &FileSummary) {
        let mut keys = Vec::new();
        for (local, d) in s.decls.iter().enumerate() {
            let local = local as u32;
            self.main.entry(d.name).or_default().push(DeclRef { file, local });
            keys.push(ContributionKey::Main(d.name, local));
            // mixin 倒排（名字键）：首参基名，剥壳在语法层（§5.3——
            // 不经类型表，`FVector&` / `const FVector&in` 逐层剥到名字）
            if d.flags.contains(DefFlags::MIXIN) {
                if let RawExtra::Callable { params, .. } = &d.extra {
                    if let Some(base) =
                        params.first().and_then(|p| p.ty.as_ref()).and_then(syn_base_name)
                    {
                        self.mixin_by_name.entry(base).or_default().push(DeclRef { file, local });
                        keys.push(ContributionKey::Mixin(base, local));
                    }
                }
            }
        }
        if let Some(m) = s.module {
            self.module_members.entry(m).or_default().insert(file);
            keys.push(ContributionKey::Module(m));
        }
        self.contributions.entry(file).or_default().extend(keys);
    }

    /// 摘除一个文件的全部贡献（私有；main/mixin 按 FileId 全摘——
    /// 贡献清单只用于定位桶，条目过滤按文件，幂等且与其他文件无关）。
    fn retract(&mut self, file: FileId, old: &FileSummary) {
        let _ = old; // 摘除按 FileId 过滤，不需要旧内容
        let Some(keys) = self.contributions.remove(&file) else { return };
        for key in keys {
            match key {
                ContributionKey::Main(sym, _) => {
                    if let Some(bucket) = self.main.get_mut(&sym) {
                        bucket.retain(|r| r.file != file);
                        if bucket.is_empty() {
                            self.main.remove(&sym);
                        }
                    }
                }
                ContributionKey::Mixin(sym, _) => {
                    if let Some(bucket) = self.mixin_by_name.get_mut(&sym) {
                        bucket.retain(|r| r.file != file);
                        if bucket.is_empty() {
                            self.mixin_by_name.remove(&sym);
                        }
                    }
                }
                ContributionKey::Module(sym) => {
                    if let Some(members) = self.module_members.get_mut(&sym) {
                        members.remove(&file);
                        if members.is_empty() {
                            self.module_members.remove(&sym);
                        }
                    }
                }
            }
        }
    }

    /// 替换后重排受影响桶（增量插入可能破坏 §4.2 序，重排恢复确定性）。
    fn resort_touched(&mut self, file: FileId) {
        let Some(keys) = self.contributions.get(&file) else { return };
        let mut syms: Vec<Sym> = keys
            .iter()
            .filter_map(|k| match k {
                ContributionKey::Main(s, _) | ContributionKey::Mixin(s, _) => Some(*s),
                ContributionKey::Module(_) => None,
            })
            .collect();
        syms.sort();
        syms.dedup();
        for s in syms {
            if let Some(b) = self.main.get_mut(&s) {
                b.sort_by_key(|r| sort_key(r));
            }
            if let Some(b) = self.mixin_by_name.get_mut(&s) {
                b.sort_by_key(|r| sort_key(r));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IndexConfig;
    use crate::index::FileKind;
    use crate::intern::{intern_file, intern_sym};
    use crate::summary::extract_summary;

    // 用例源码全部内置（AGENTS.md 硬性规则 / D1）。

    fn mk_summary(src: &str, module: Option<&str>) -> FileSummary {
        let tree = as_syntax::parse(src, None);
        let module = module.map(intern_sym);
        extract_summary(&tree, src, FileKind::Script, module, &IndexConfig::default())
    }

    #[test]
    fn aggregation_orders_and_inverts() {
        // 根优先级：脚本根(0) < decl 根(1)；同根按 FileId、local 升序
        let script = intern_file("unique://agg/script.as", 0);
        let decl_a = intern_file("unique://agg/a.d.as", 1);
        let decl_b = intern_file("unique://agg/b.d.as", 1);
        let mut files = HashMap::new();
        files.insert(script, mk_summary("struct FVector3 {}\nvoid F() {}\n", None));
        files.insert(decl_a, mk_summary("struct FVector3 {}\n", None));
        files.insert(decl_b, mk_summary("struct FVector3 {}\n", None));

        let agg = Aggregation::build(&files);
        let hits = agg.main.get(&intern_sym("FVector3")).unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].file, script, "脚本根优先");
        assert!(hits[1].file < hits[2].file, "同根按 FileId 稳定序");
        assert_eq!(agg.main.get(&intern_sym("F")).unwrap().len(), 1);
    }

    #[test]
    fn mixin_inverted_by_name() {
        // 首参剥壳：const FVector&in → FVector；未解析类型也是有效键
        //（mixin_pending 概念删除——名字键天然容错）
        let src = "\
struct FVector4 {}
mixin void Heal4(FVector4& V) {}
mixin void Odd(TMissing M) {}
void AlsoMixin4(const FVector4&in V) mixin {}
";
        let f = intern_file("unique://aggm/m.as", 0);
        let mut files = HashMap::new();
        files.insert(f, mk_summary(src, None));
        let agg = Aggregation::build(&files);
        let hits = agg.mixin_by_name.get(&intern_sym("FVector4")).unwrap();
        assert_eq!(hits.len(), 2, "前置与后置两种声明形式都进倒排");
        assert_eq!(agg.mixin_by_name.get(&intern_sym("TMissing")).unwrap().len(), 1);
    }

    #[test]
    fn replace_and_remove_are_incremental() {
        let f1 = intern_file("unique://aggi/1.as", 0);
        let f2 = intern_file("unique://aggi/2.as", 0);
        let v1 = mk_summary("void F() {}\nvoid Keep() {}\n", None);
        let v2 = mk_summary("void G() {}\nvoid Keep() {}\n", None);
        let other = mk_summary("void F() {}\nvoid Keep() {}\n", None);

        let mut files = HashMap::new();
        files.insert(f1, v1.clone());
        files.insert(f2, other);
        let mut agg = Aggregation::build(&files);

        // f1: F 删、G 增、Keep 不变；f2 的同名条目不受影响
        agg.replace_file(f1, &v1, &v2);
        let f_hits = agg.main.get(&intern_sym("F")).unwrap();
        assert_eq!(f_hits.len(), 1, "只摘 f1 的 F，f2 的保留");
        assert_eq!(f_hits[0].file, f2);
        let keep = agg.main.get(&intern_sym("Keep")).unwrap();
        assert_eq!(keep.len(), 2, "两边 Keep 都在");
        let g = agg.main.get(&intern_sym("G")).unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].file, f1);

        // remove：f1 贡献清零，f2 不受影响
        agg.remove_file(f1, &v2);
        assert_eq!(agg.main.get(&intern_sym("Keep")).unwrap().len(), 1);
        assert!(agg.main.get(&intern_sym("G")).is_none());
        assert_eq!(agg.main.get(&intern_sym("F")).unwrap().len(), 1, "f2 的 F 仍在");
    }

    #[test]
    fn module_mapping_prefers_lower_root_and_re_elects() {
        let script = intern_file("unique://aggmod/s.as", 0);
        let decl = intern_file("unique://aggmod/d.d.as", 1);
        let mut files = HashMap::new();
        files.insert(decl, mk_summary("void F() {}\n", Some("MyMod")));
        files.insert(script, mk_summary("void F() {}\n", Some("MyMod")));
        let mut agg = Aggregation::build(&files);
        assert_eq!(
            agg.module_file(intern_sym("MyMod")),
            Some(script),
            "脚本根优先"
        );

        // 摘除脚本侧 → 接替者自动落到 decl 侧（模块成员表重选）
        let script_summary = files.remove(&script).unwrap();
        agg.remove_file(script, &script_summary);
        assert_eq!(agg.module_file(intern_sym("MyMod")), Some(decl), "接替者重选");
    }
}
