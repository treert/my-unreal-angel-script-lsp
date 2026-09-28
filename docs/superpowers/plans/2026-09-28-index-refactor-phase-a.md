# 索引重构 Phase A 实施计划（summary + 聚合层，双轨对照）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 `docs/index-architecture.md`（v1.1）实施切分的第 1-2 步：`FileSummary`（含 scope_tree）+ `Aggregation` 薄聚合层，与旧 `WorkspaceIndex` **双轨并行**，等价性由单测与语料对账双重钉死——LSP 行为零变化。

**Architecture:** 新建 `summary.rs`（纯函数提取）与 `aggregation.rs`（名字倒排 + 贡献倒排）两个 as-core 模块，旧 `index.rs` 原样服务查询；as-cli 增加 `--new-arch` 双轨对账模式。Phase A 结束时新架构代码全部就位且被语料验证，但未接线。

**Tech Stack:** Rust（cargo workspace，crates：as-core / as-cli）、tree-sitter 0.25、rayon。

## Global Constraints

- Rust 单测 `.as` 用例**一律内置于源码字符串字面量**，禁止读 `tests/` 等外部文件（AGENTS.md 硬性规则 / D1）
- 每任务结束 `cargo test --workspace` 必须全绿（当前基线：151 个）
- 提交信息用中文、风格与仓库既有提交一致（首行 ≤ 72 字符概要 + 空行 + 正文）
- 本 Phase **不改动** as-lsp crate 与 `WorkspaceIndex` 的任何行为——双轨，旧路径原样
- 路径唯一性约定：单测内 `intern_file` 的路径一律加 `unique://` 前缀（进程级全局注册表，并行测试防串）

## 总路线图（Phase A-E，本计划只细化 A）

| Phase | 对应设计稿 §11 | 内容 | 计划 |
|---|---|---|---|
| **A（本计划）** | 步骤 1+2 | summary + scope_tree + 聚合层（双轨对照验收） | 下文 Task 1-5 |
| B | 步骤 3 | L3 后端切换：DefId → DeclRef，旧 `WorkspaceIndex` 删除 | A 验收后另写计划 |
| C | 步骤 4 | 按需化：继承链走链 / mixin 查询 / 合成成员 / TypeTable | B 后另写 |
| D | 步骤 5 | references/rename 查询期化，删 uses/ref_index/use_cache/D29 | C 后另写 |
| E | 步骤 6+7 | as-lsp 增量更新 + 诊断调度队列 + 文档同步 | D 后另写 |

---

### Task 1: ScopeTree 数据结构与查询原语

**Files:**
- Create: `lsp/crates/as-core/src/scope.rs`
- Modify: `lsp/crates/as-core/src/lib.rs`（`pub mod scope;` + re-export）

**Interfaces（后续任务依赖，签名必须一字不差）:**
```rust
pub struct ScopeTree { /* scopes: Vec<Scope>，字段私有 */ }
pub struct Scope { pub parent: Option<u32>, pub span: TextRange, pub decls: Vec<LocalDecl> }
pub struct LocalDecl {
    pub name: Sym,
    pub kind: LocalKind,           // Var | Param | IterVar（AS 无嵌套函数，无 LocalFn——设计稿 v1.1 的 LocalKind::LocalFn 是笔误，Phase A 验收时同步改设计稿）
    pub name_span: TextRange,      // 声明锚点
    pub ty: Option<SynType>,       // 预推导类型；推不出 = None
}
impl ScopeTree {
    pub fn resolve_local(&self, byte: u32, name: Sym) -> Option<&LocalDecl>;
    pub fn locals_visible(&self, byte: u32) -> Vec<&LocalDecl>;
}
```
可见性规则（与现行 `resolve.rs` 的 `span.start <= byte` 一致）：候选 scope = 最内层包含 byte 的块沿 parent 链上溯；scope 内仅 `name_span.start <= byte` 的声明可见；同链最近者优先（遮蔽）。

- [ ] **Step 1: 写失败的单测**（`scope.rs` 内 `#[cfg(test)] mod tests`，用例内置）

```rust
// 手工构造树（提取逻辑在 Task 3，本任务只测数据结构 + 原语）
fn tree() -> ScopeTree {
    ScopeTree::from_scopes(vec![
        Scope { parent: None, span: TextRange::new(0, 100), decls: vec![
            LocalDecl { name: intern_sym("A"), kind: LocalKind::Var,
                        name_span: TextRange::new(5, 6), ty: None },
        ]},
        Scope { parent: Some(0), span: TextRange::new(40, 80), decls: vec![
            LocalDecl { name: intern_sym("A"), kind: LocalKind::Var,
                        name_span: TextRange::new(50, 51), ty: None },  // 内层遮蔽
        ]},
    ])
}

#[test]
fn shadowing_inner_wins() {
    let t = tree();
    let d = t.resolve_local(60, intern_sym("A")).unwrap();
    assert_eq!(d.name_span, TextRange::new(50, 51), "内层遮蔽外层");
}

#[test]
fn visibility_starts_at_decl() {
    let t = tree();
    assert!(t.resolve_local(10, intern_sym("A")).is_some(), "外层声明 5 已可见");
    // byte=60 在内层块，但内层声明在 50，60>50 可见；45 在内层块但声明未到 → 命中外层
    let d = t.resolve_local(45, intern_sym("A")).unwrap();
    assert_eq!(d.name_span, TextRange::new(5, 6), "声明点之前回落外层");
}

#[test]
fn locals_visible_unions_chain() {
    let t = tree();
    let mut names: Vec<&str> = t.locals_visible(60)
        .into_iter().map(|d| sym_str(d.name)).collect();
    names.sort();
    assert_eq!(names, vec!["A"], "同名遮蔽只留最近者");
}

#[test]
fn resolve_outside_any_scope_is_none() {
    let t = tree();
    assert!(t.resolve_local(150, intern_sym("A")).is_none(), "树外查询");
}
```
补充：`from_scopes(Vec<Scope>) -> ScopeTree` 构造器（Task 3 的提取器经它落树）。

- [ ] **Step 2: 跑测试确认失败** — `cargo test -p as-core scope` → 编译错误（模块不存在）
- [ ] **Step 3: 实现** — `ScopeTree { scopes: Vec<Scope> }`；`resolve_local`：二分/线性找最内层包含 byte 的 scope（取 span 最窄者），沿 parent 链每层取 `name_span.start <= byte` 的**最后一个**同名声明，命中即返回；`locals_visible`：同链收集，同名只留最近。纯数据结构，无 IO、无全局状态。
- [ ] **Step 4: `cargo test -p as-core scope` 全绿**
- [ ] **Step 5: Commit** — `git add lsp/crates/as-core/src/scope.rs lsp/crates/as-core/src/lib.rs` → `索引重构 A1：ScopeTree 数据结构与 resolve_local/locals_visible 原语`

---

### Task 2: RawDecl / FileSummary 类型 + extract_summary 声明提取（平移）

**Files:**
- Create: `lsp/crates/as-core/src/summary.rs`
- Modify: `lsp/crates/as-core/src/lib.rs`（`pub mod summary;` + re-export `FileSummary / RawDecl / extract_summary`）

**Interfaces:**
```rust
pub fn extract_summary(
    tree: &as_syntax::tree_sitter::Tree,
    source: &str,
    kind: FileKind,
    module: Option<Sym>,
    cfg: &IndexConfig,
) -> FileSummary;

pub struct FileSummary {
    pub kind: FileKind,
    pub module: Option<Sym>,
    pub group: Option<String>,
    pub cache_format: Option<u32>,
    pub decls: Vec<RawDecl>,                  // 局部 id = 下标，文件内稳定
    pub by_name: HashMap<Sym, Vec<u32>>,      // 名字 → 局部 id（含重载组，源码序）
    pub scope_tree: ScopeTree,                // Task 3 填充；本任务恒为空树
}
pub struct RawDecl {                          // ≈ DefData 去跨文件字段
    pub name: Sym,
    pub kind: DefKind,
    pub name_span: TextRange,
    pub full_span: TextRange,
    pub parent: Option<u32>,                  // 文件内局部 id（替代 DefId）
    pub bases: Vec<BaseRef>,                  // 原 DefExtra::TypeDecl.bases，提为字段
    pub template_params: Vec<Sym>,            // 原 DefExtra::TypeDecl.template_params
    pub extra: RawExtra,                      // Callable/Variable/EnumValue/None（TypeDecl 内容上提）
    pub flags: DefFlags,
    pub doc: Option<Box<str>>,
    pub tags: Vec<SemanticTag>,
}
pub enum RawExtra {
    None,
    Callable { return_type: Option<SynType>, params: Vec<ParamDecl> },
    Variable { ty: Option<SynType> },
    EnumValue { value: Option<Box<str>> },
}
```
**Consumes:** `syntax::{classify_decl, scan_flags, parse_syn_type, decl_name_node, class_bases, template_params, doc_comment_texts, leading_comment_texts, children_with_fields, text, span}`、`decl_tags::parse_comment_texts`——全部既有，不改。`DefKind/DefFlags/ParamDecl/BaseRef/SemanticTag/SynType/TextRange` 直接复用。
**Produces:** Task 3/4/5 依赖 `FileSummary.decls/by_name` 与 `extract_summary` 签名。

平移来源：`index.rs` 的 `extract_decl` 家族（`extract_type_decl`/`extract_enum`/`extract_namespace`/`extract_callable_decl`/`extract_asset`/`extract_virtual_property`/`extract_function`/`extract_variable`，约 index.rs:330-650）。逐个改写为**无 `&mut self` 的 builder 模式**：`SummaryBuilder { decls, by_name, scope_scopes }`，`push_decl(...) -> u32`（替代 `push_def`——不写 main/members，只 append decls + by_name）。`origin` 字段不迁移（合成成员是 Phase C 查询期概念）。内建注入（inject_builtins）不迁移（L3 职责）。

- [ ] **Step 1: 写等价性失败单测**（summary.rs 内置用例；核心断言：新旧架构对同一输入产出**相同的声明集合**）

```rust
/// 等价性骨架：同一批内置源码，旧 WorkspaceIndex 与新 summary 对照。
/// 覆盖形态：class 继承 + 成员（字段/方法/构造）、enum、namespace、
/// delegate、函数重载组、全局变量、mixin 两种形式、.d.as 文件头 tag。
const SRC_MIX: &str = "\
// @group /Script/Core
// @cache_format 2
namespace NS { void NFunc() {} }
class AActor2 : UObject2 { int Health; void Tick(float DT) {} AActor2() {} }
struct FVector2 { float X; }
enum EColor { Red, Green = 2, Blue }
delegate void FOnHit2(int Damage);
int GlobalCounter = 0;
void Overload(int A) {}
void Overload(float B) {}
mixin void Heal2(AActor2 A) {}
void AlsoMixin2(const FVector2&in V) mixin {}
UPROPERTY() float Tagged;
";

#[test]
fn summary_matches_old_index() {
    let path = "unique://summix/a.d.as";
    let tree = as_syntax::parse(SRC_MIX, None);
    let summary = extract_summary(&tree, SRC_MIX, FileKind::Decl, None, &IndexConfig::default());

    let inputs = vec![FileInput {
        file: intern_file(path, 0), kind: FileKind::Decl, module: None,
        source: SRC_MIX.to_string(),
    }];
    let old = WorkspaceIndex::build(IndexConfig::default(), inputs);

    // ① 声明总数一致（SYNTHETIC 除外——旧侧 delegate 展开与内建不计入对照）
    let old_real: Vec<_> = old.symbols.iter()
        .filter(|(_, d)| !d.flags.contains(DefFlags::SYNTHETIC) && d.file.as_raw() != u32::MAX)
        .collect();
    assert_eq!(summary.decls.len(), old_real.len(),
        "非合成声明数：new {} vs old {}", summary.decls.len(), old_real.len());

    // ② (name, kind) 多重集一致
    let key = |name: Sym, kind: DefKind| (sym_str(name).to_string(), kind.label());
    let mut new_keys: Vec<_> = summary.decls.iter().map(|d| key(d.name, d.kind)).collect();
    let mut old_keys: Vec<_> = old_real.iter().map(|(_, d)| key(d.name, d.kind)).collect();
    new_keys.sort(); old_keys.sort();
    assert_eq!(new_keys, old_keys);

    // ③ 同名声明的 name_span 一致
    let find = |k: &(String, &str)| old_real.iter()
        .find(|(_, d)| (sym_str(d.name), d.kind.label()) == *k).unwrap().1.name_span;
    for d in &summary.decls {
        let k = key(d.name, d.kind);
        assert_eq!(d.name_span, find(&k), "span 不一致: {k:?}");
    }

    // ④ by_name：重载组源码序
    let ov = summary.by_name.get(&intern_sym("Overload")).unwrap();
    assert_eq!(ov.len(), 2, "重载组保留");

    // ⑤ 文件头 tag
    assert_eq!(summary.group.as_deref(), Some("/Script/Core"));
    assert_eq!(summary.cache_format, Some(2));
}
```
（实现时按需再拆 3-4 个小用例：`.as` 侧形态 / mixin flags 等价 / parent 链等价——`summary.decls[i].parent` 指向的父声明 name 与旧 `DefData.parent` 解引用后一致。）

- [ ] **Step 2: `cargo test -p as-core summary` 失败**（模块未建）
- [ ] **Step 3: 实现** — `SummaryBuilder` + 八个 `extract_*` 平移。每个 extract_* 的树遍历、flags、doc/tags 分流逻辑**逐行照搬**，只改落库三行（push_def → push_decl）。`RawExtra::TypeDecl` 拆进 `RawDecl.bases/template_params` 两个字段。
- [ ] **Step 4: `cargo test -p as-core` 全绿**（既有 151 + 新增）
- [ ] **Step 5: Commit** — `索引重构 A2：extract_summary 声明提取平移（RawDecl 局部 id，等价单测钉死）`

---

### Task 3: scope_tree 建树 + 局部预推导

**Files:**
- Modify: `lsp/crates/as-core/src/summary.rs`（SummaryBuilder 增加函数体扫描；`FileSummary.scope_tree` 接真数据）

**Interfaces:**
- Consumes: Task 1 的 `ScopeTree::from_scopes`、`LocalDecl/LocalKind`
- Produces: `FileSummary.scope_tree` 有真实内容；Task 5 对账含 scope 数

推导规则（设计稿 §3.3.1，产物一律 `SynType`）：

| 模式 | ty |
|---|---|
| 形参 / 显式类型局部 | `parse_syn_type` 的语法层类型（原样，含名字） |
| `auto X = Cast<T>(...)` | `Named(T)` |
| `auto X = Ident(...)`（构造调用，Ident 是类型形态的裸标识符） | `Named(Ident)` |
| `auto X = OtherLocal` | 复制 OtherLocal 已推导 ty |
| `auto X = <literal>` | `1`→int；`1.5`→按 cfg 归一的 float；`n".."`→FName；f-string→FString（`expr.rs` 既有字面量规则可参考，但**不 import expr.rs**——它依赖 WorkspaceIndex；字面量判定在 summary.rs 内联实现，约 20 行） |
| 其余（跨文件传播） | `None` |

- [ ] **Step 1: 写失败单测**（内置用例）

```rust
const SRC_SCOPE: &str = "\
void F(float DT)
{
    int A = 1;
    {
        int A = 2;                 // 遮蔽
        auto B = Cast<FVector2>(A);
    }
    auto C = A;
    auto D = 1.5;
    for (auto E : Items) { Use(E); }
}
";

#[test]
fn scope_tree_built_with_inference() {
    let tree = as_syntax::parse(SRC_SCOPE, None);
    let s = extract_summary(&tree, SRC_SCOPE, FileKind::Script, None, &IndexConfig::default());
    // 形参
    let dt = s.scope_tree.resolve_local(30, intern_sym("DT")).unwrap();
    assert!(matches!(dt.kind, LocalKind::Param));
    // 遮蔽：内层块的 A
    let a = s.scope_tree.resolve_local(80, intern_sym("A")).unwrap();
    assert_eq!(sym_str(a.name), "A");
    // Cast 推导
    let b = s.scope_tree.resolve_local(120, intern_sym("B")).unwrap();
    let SynType::Named(n, _) = b.ty.as_ref().unwrap() else { panic!() };
    assert_eq!(sym_str(*n), "FVector2");
    // 复制推导：C 的 ty = A 的显式 int
    let c = s.scope_tree.resolve_local(160, intern_sym("C")).unwrap();
    assert!(matches!(c.ty.as_ref(), Some(SynType::Primitive(..))));
    // 字面量：float 归一（默认 float64）
    let d = s.scope_tree.resolve_local(200, intern_sym("D")).unwrap();
    let SynType::Primitive(p, _) = d.ty.as_ref().unwrap() else { panic!() };
    assert_eq!(sym_str(*p), "float64");
    // range-for 迭代变量：跨文件，保留 None
    let e = s.scope_tree.resolve_local(260, intern_sym("E")).unwrap();
    assert!(matches!(e.kind, LocalKind::IterVar));
    assert!(e.ty.is_none(), "Items 类型未知 ⇒ 宁缺毋假");
}
```

- [ ] **Step 2: 测试失败**（scope_tree 为空树，resolve_local 恒 None）
- [ ] **Step 3: 实现** — `extract_function` 平移时对 `body` 字段做单趟 DFS：进入 block/for-range 体压栈 scope，遇到 `parameter`/`variable_declaration`/`for_each_statement` 记 `LocalDecl`（auto 走推导表）；树随 summary 一起由 `from_scapes`…（`from_scopes`）落成。**只处理函数体**——类成员/全局不进 scope_tree（它们在 decls 里）。
- [ ] **Step 4: `cargo test -p as-core` 全绿**
- [ ] **Step 5: Commit** — `索引重构 A3：scope_tree 建树 + auto 局部预推导（Cast/构造/复制/字面量）`

---

### Task 4: DeclRef + Aggregation 构建与增量

**Files:**
- Create: `lsp/crates/as-core/src/aggregation.rs`
- Modify: `lsp/crates/as-core/src/lib.rs`（`pub mod aggregation;` + re-export `DeclRef / Aggregation`）

**Interfaces:**
```rust
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DeclRef { pub file: FileId, pub local: u32 }

pub struct Aggregation {
    pub main: HashMap<Sym, Vec<DeclRef>>,
    pub mixin_by_name: HashMap<Sym, Vec<DeclRef>>,
    pub module_files: HashMap<Sym, FileId>,
    contributions: HashMap<FileId, Vec<ContributionKey>>,   // 增量钥匙（私有）
}
impl Aggregation {
    /// 冷启动：一遍扫描全部 summary（排序键 = (root_index, FileId, local)，
    /// root_index 从 intern::file_meta 取）
    pub fn build(files: &HashMap<FileId, FileSummary>) -> Aggregation;
    /// 单文件替换：摘旧贡献 → 扫新贡献。O(新旧声明数)
    pub fn replace_file(&mut self, file: FileId, old: &FileSummary, new: &FileSummary);
    /// 单文件删除
    pub fn remove_file(&mut self, file: FileId, old: &FileSummary);
}
```
`ContributionKey`：该文件对三张表的贡献记录（main 的 `(Sym, local)`、mixin 的 `(Sym, local)`、module 的 `Sym`）——设计稿 §4.3，mylua `uri_to_paths` 同款。

mixin 名字提取：`RawDecl.flags` 含 `MIXIN` 且 `extra == Callable` → `syn_base_name(params[0].ty)`；新增 `pub fn syn_base_name(t: &SynType) -> Option<Sym>`（Ref/Const/Array 逐层剥到 Named/Primitive 的名字——设计稿 §5.3，放 summary.rs 导出）。

- [ ] **Step 1: 写失败单测**

```rust
#[test]
fn aggregation_orders_and_inverts() {
    // 根优先级：脚本根(0) < decl 根(1)；同根按 FileId、local 升序
    let script = intern_file("unique://agg/script.as", 0);   // root 0
    let decl_a = intern_file("unique://agg/a.d.as", 1);      // root 1
    let decl_b = intern_file("unique://agg/b.d.as", 1);      // root 1
    // 三个文件都声明 FVector3；脚本版必须排首位
    let mk = |src: &str| extract_summary(&as_syntax::parse(src, None), src,
        FileKind::Script, None, &IndexConfig::default());
    let mut files = HashMap::new();
    files.insert(script, mk("struct FVector3 {}\nvoid F() {}\n"));
    files.insert(decl_a, mk("struct FVector3 {}\n"));
    files.insert(decl_b, mk("struct FVector3 {}\n"));
    let agg = Aggregation::build(&files);
    let hits = agg.main.get(&intern_sym("FVector3")).unwrap();
    assert_eq!(hits[0].file, script, "脚本根优先");
    assert!(hits[1].file < hits[2].file, "同根按 FileId 稳定序");
    assert_eq!(agg.main.get(&intern_sym("F")).unwrap().len(), 1);
}

#[test]
fn mixin_inverted_by_name() {
    // mixin 首参剥壳：const FVector&in → FVector；未解析类型也进表（名字键天然容错）
    let src = "mixin void Heal4(FVector4& V) {}\nmixin void Odd(TMissing M) {}\n";
    let f = intern_file("unique://aggm/m.as", 0);
    let s = extract_summary(&as_syntax::parse(src, None), src, FileKind::Script, None,
        &IndexConfig::default());
    let mut files = HashMap::new();
    files.insert(f, s);
    let agg = Aggregation::build(&files);
    assert_eq!(agg.mixin_by_name.get(&intern_sym("FVector4")).unwrap().len(), 1);
    assert_eq!(agg.mixin_by_name.get(&intern_sym("TMissing")).unwrap().len(), 1,
        "未解析目标也是有效键（mixin_pending 概念删除）");
}

#[test]
fn replace_and_remove_are_incremental() {
    // build → replace_file（F 删、G 增）→ main 精确反映；其他文件条目不动
    // …（构造两文件、替换其一、断言 main/module_files/contributions 一致性，
    //     再 remove_file 断言贡献清零且其余文件不受影响——实现时补全，模式同上）
}
```

- [ ] **Step 2: 测试失败**
- [ ] **Step 3: 实现** — build：遍历 `files`，对每个 summary 的 decls 生成贡献并按排序键入桶（排序在收集后统一 `sort_by_key`，保证确定性）；replace/remove：沿 contributions 摘除 → 重扫。`module_files` 用 summary.module 填充（同模块多文件取 root_index 最小者）。
- [ ] **Step 4: `cargo test -p as-core` 全绿**
- [ ] **Step 5: Commit** — `索引重构 A4：Aggregation 薄聚合层（名字/mixin/module 倒排 + 贡献增量）`

---

### Task 5: as-cli 双轨对账 + 计时验收

**Files:**
- Modify: `lsp/crates/as-cli/src/main.rs`（DumpIndex 增 `--new-arch` flag）
- Modify: `lsp/crates/as-core/src/index.rs`（**仅** `as_log!` 计时两行——不触碰逻辑）

**Interfaces:** `dump-index [--new-arch]`：旧路径照跑；flag 开启时**同时**构建新架构（rayon 并行 parse + extract_summary + Aggregation::build），打印两边耗时与对账行，任何计数不一致 → 退出码非 0。

- [ ] **Step 1: 手工验收（无新单测——本任务是接线与验收）**
- [ ] **Step 2: 实现** — collect_inputs 复用；新侧：`inputs.par_iter().map(|i| (i.file, extract_summary(&parse(&i.source), ...))).collect::<HashMap<_,_>>()` + `Aggregation::build`；对账三行：
  - `decls total: new 111881 vs old 111881`（新侧 = sum(decls.len())，旧侧 = 非合成 symbol 数）
  - `main keys: new N vs old N`（名字集合相等断言）
  - `timing: old 1072ms | new XXXms`（as_log 落 fileLog 同款格式）
- [ ] **Step 3: 语料验收** — `cargo build --release -p as-cli` 后：
  ```powershell
  .\target\release\as-cli.exe dump-index d:\DTmp\test-my-as-lsp\AS-Cache --new-arch
  ```
  预期：计数全部一致、退出码 0；新架构耗时**显著低于**旧路径（旧 1072ms 中 Phase 2a 顺序提取 490ms 在新侧被并行化）。
- [ ] **Step 4: `cargo test --workspace` 全绿**（151 + Phase A 新增）
- [ ] **Step 5: 设计稿小修** — `docs/index-architecture.md` v1.1 的 `LocalKind::LocalFn` 笔误订正（Task 1 注记），变更记录加 v1.2 行
- [ ] **Step 6: Commit** — `索引重构 A5：as-cli --new-arch 双轨对账（414 语料计数一致 + 并行提速验收）`

---

## Phase A 验收标准（整体）

1. `cargo test --workspace` 全绿（151 基线 + Phase A 新增 ≈ 15-20 个）
2. `dump-index --new-arch` 对 414 `.d.as` 语料：声明数 / 名字集合零差异，退出码 0
3. 新架构构建耗时 < 旧架构（记录具体数字进提交信息，供 Phase B-E 基线）
4. as-lsp crate 零改动（`git diff --stat` 证实）
5. LSP 端到端行为不变（`tools/test-extension.ps1 -Release` 冒烟，非必须——A 不接线）

## Phase B-E 展望（各自开工前另写详细计划）

- **B（L3 后端切换）**：`DeclRef` 全面替换 `DefId`（resolve/expr/completion/hover/inlay/outline/tokens/signature ≈ 8 文件）；`fetch(DeclRef) -> &RawDecl` 访问器先行；旧 `WorkspaceIndex` 连同 `finish/closures/expand/resolved` 删除；验收 = 既有单测全绿 + resolve-stats 94.9% 不回归
- **C（按需化）**：继承链走链（§5.2）、mixin 查询（§5.3）、合成成员查询期（§7）、TypeTable 查询期填充
- **D（references 查询期化）**：`find_word_occurrences` + 逐点解析验证；删 uses/ref_index/use_cache/D29；验收 = ref-stats 99.4% 不回归
- **E（as-lsp + 文档）**：贡献倒排接入 didChange/watched-files 增量、诊断调度队列（mylua 式）、`架构设计.md` §4.2 改写、`LSP实现规划.md` §4-§6 同步

## Self-Review 记录

- **Spec 覆盖**：设计稿 §11 步骤 1（=Task 2+3）+ 步骤 2（=Task 4）+ 对账验收（=Task 5）；§3.3 scope_tree（=Task 1+3）；§4 聚合层含增量（=Task 4）。步骤 3-7 归 Phase B-E 计划。✅
- **占位符扫描**：Task 4 Step 1 的 `replace_and_remove_are_incremental` 用例给了模式说明但未展开完整代码——已注明"实现时补全，模式同上"，验收点明确；其余任务代码完整。⚠️ 可接受（用例模式与前两个用例同构，展开反而冗余）
- **类型一致性**：`extract_summary` 签名在 Task 2 定义、Task 3/4/5 引用一致；`DeclRef { file, local }` 在 Task 4 定义、Phase B 依赖；`ScopeTree::from_scopes` Task 1 产、Task 3 用；`syn_base_name` Task 4 定义于 summary.rs（Task 3 不需要它——auto 推导只取 Cast 的类型节点直接 parse_syn_type）✅
