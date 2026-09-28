# 索引重构 Phase B 实施计划（L3 后端切换：DefId → DeclRef，旧 WorkspaceIndex 删除）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** 把 L3 查询层（resolve/expr/overload/expand/references/completion/hover/signature/inlay/search/types）从旧 `WorkspaceIndex`（全局 DefId arena）切换到新架构（`Workspace`：FileEntry + Aggregation + DeclRef），删除 `index.rs` 与 `uses.rs`，as-lsp / as-cli 同步移植。**Phase D 的 references 查询期核心提前并入本计划 Task 1**（在旧后端上先验证），避免把 use-site/ref_index 移植到新后端（死代码移植）。

**Architecture:** Task 1 在旧后端上实现并验证查询期 references（字符串扫 + 逐点解析验证，ref-stats 语料 A/B 对账）——零架构切换纯增量。Task 2-4 是一次原子切换（L3 十文件互相调用，无法逐文件换后端），靠 163 单测 + 语料对账兜底。Task 5 移植宿主（as-lsp / as-cli）并删除旧代码。

**Tech Stack:** Rust cargo workspace、tree-sitter 0.25、rayon。

## Global Constraints

- 单测 `.as` 用例内置源码字符串（D1）；`intern_file` 路径加 `unique://` 前缀
- 每 Task 结束 `cargo test --workspace` 全绿；本计划基线 **163 个**
- 提交信息中文、首行 ≤ 72 字符
- master 串行，一 Task 一 commit

## 关键裁决（本计划新增，实施时落入代码注释；Task 5 同步设计稿 v1.3 + 决策记录 D38）

| # | 裁决 | 理由 |
|---|---|---|
| B1 | **references 查询期核心在旧后端先行**（Task 1）：`find_word_occurrences` + `find_references_query` 直接对 `WorkspaceIndex` 实现，ref-stats A/B 对账通过后才进入切换 | 等价性先证后切；uses/ref_index 永不移植 |
| B2 | **builtin = 合成 FileSummary**：`intern_file("<as-core:builtin>")` 伪文件（进程级一次）+ 15 个 SYNTHETIC RawDecl（局部 id 0..14 恒定），随 `Workspace::build` 进 files + agg | `lookup("int8")` 与普通名字同一代码路径；DefId 0..15 恒为内建的旧性质以 DeclRef 形态保留 |
| B3 | **合成 namespace 机制消失**：`namespaces_named` 改为「kind == Namespace **或** is_type_like（class/struct）」；`origin_fallback` 的 namespace→class 归一删除（新架构类 DeclRef 直接兼任 namespace，两处使用点产生同一 DeclRef，天然归一） | 旧机制是 arena 时代「类与 namespace 需两个 DefId」的补丁；语义等价（§2.2.1 推论 1 语境择一不变） |
| B4 | **delegate/event 展开（Execute/Bind 等）与 StaticClass 改查询期**：`synthetic_members(r: DeclRef) -> Vec<SyntheticMember>`（从 RawDecl 的 tags/params 现推，引擎模板取证见 expand.rs 头注释）；resolve 第 3/5 级与 completion/hover/search 的成员枚举处 ∪ synthetic_members | expand.rs 的 `expand_all` 预计算依赖 arena 注入，无移植路径；查询期是设计稿 §7 既定方向，提前到 B |
| B5 | **closures / resolved 声明类型 Phase B 保持 eager**（`Workspace::build` 尾段一遍算，HashMap<DeclRef, …>） | 行为零变化优先；Phase C 再按需化 |
| B6 | **resolve 的局部变量（LocalDecl/CST walk）Phase B 不动**：SemCtx::from_ancestors 查询期走树的既有实现保留，scope_tree 的消费留 Phase E | B 的改动面已到极限；scope_tree 等价性由 A3 单测钉死，切换是纯替换无语义风险 |
| B7 | **`members` 查询**：FileEntry 增加 `by_parent: HashMap<u32, Vec<u32>>`（decls 提取后一遍建），`Workspace::members(DeclRef) -> Vec<DeclRef>`（跨文件 namespace 聚合 = agg.main 过滤 kind） | 旧 members 表的 per-file 部分；namespace 成员跨文件聚合本就走 main |

## 机械替换规则（Task 3 全员遵守，写完自查）

| 旧 | 新 |
|---|---|
| `DefId`（API 层） | `DeclRef` |
| `idx.def(id)` | `ws.decl(&r) -> &RawDecl` |
| `d.file` | `r.file` |
| `d.parent`（Option<DefId>） | `decl.parent.map(\|p\| DeclRef { file: r.file, local: p })` |
| `idx.main.get(&name)` | `ws.lookup(name) -> &[DeclRef]`（agg.main） |
| `idx.members.get(&p)` | `ws.members(&p) -> Vec<DeclRef>` |
| `idx.closures.get(&c)` | `ws.closures.get(&c)`（同形，键值换 DeclRef） |
| `idx.files.get(&f)` | `ws.files.get(&f)`（FileEntry：source/tree/lines/summary） |
| `Target::Def(id)` / `RefTarget::Def(id)` | `Target::Def(r)` / `RefTarget::Def(r)` |
| `idx.config` | `ws.config` |
| `idx.types`（TypeTable） | `ws.types`（TypeKind::Named.def 换 DeclRef） |

---

### Task 1: 查询期 references（旧后端上实现 + ref-stats A/B 对账）

**Files:**
- Modify: `lsp/crates/as-core/src/references.rs`（新增查询期实现，旧实现原样）
- Modify: `lsp/crates/as-cli/src/main.rs`（`--ref-stats` 增加 A/B 对账输出）

**Interfaces（Task 3/4 依赖）:**
```rust
/// 词边界搜索（大小写敏感）：源文本中 word 的全部出现位置（字节偏移）。
/// 边界 = [A-Za-z0-9_] 外。命中含注释/字符串/声明名——由调用方解析验证滤掉。
pub fn find_word_occurrences(src: &str, word: &str) -> Vec<u32>;

/// 查询期 references（B1：先对旧 WorkspaceIndex 实现，Task 3 平移到 Workspace）。
/// 语义 = 旧 find_references（§4.6 匹配语义：站点解析集合 ∩ 查询集合 ≠ ∅ 即命中）。
pub fn find_references_query(idx: &WorkspaceIndex, targets: &[RefTarget]) -> Vec<(FileId, TextRange)>;
```
实现要点：`for (file, snap) in &idx.files` → `for off in find_word_occurrences(&snap.source, name)` → `snap.tree.root_node().descendant_for_byte_range(off, off+len)` → 节点 kind == "identifier" 才继续 → `resolve::resolve_at_node` → 目标匹配（复用 `match_uses` 的逐站点匹配核心，抽出 `use_matches(targets, resolution) -> bool`）。局部变量目标只扫声明所在文件。**不建倒排、不收 UseSite**。

- [x] **Step 1: 写失败单测**——`find_word_occurrences`（词边界/大小写/多次命中/误中防御 `Health` vs `GetHealthTime`）+ `find_references_query` 与旧 `find_references` 在内置用例（重载组、namespace、mixin、f-string 插值段）上结果相等
- [x] **Step 2: 确认失败 → 实现 → 单测绿**
- [x] **Step 3: as-cli ref-stats 加 A/B 列**：`old (倒排): N hits | query (字符串扫): M hits | match`，不等 → 退出码非 0
- [x] **Step 4: 语料验收**：`cargo build --release -p as-cli && .\target\release\as-cli.exe dump-index d:\DTmp\test-my-as-lsp\AS-Cache --ref-stats`——两法命中数一致（旧基线 68763 站点 99.4% 解析），记录查询期耗时
- [x] **Step 5: `cargo test --workspace` 全绿 → Commit** `索引重构 B1：references 查询期内核（旧后端先行，ref-stats A/B 对账零差异）`

### Task 2: Workspace 核心（新查询容器，与旧并行存在）

**Files:**
- Create: `lsp/crates/as-core/src/workspace.rs`（as-core 内，与 as-lsp 的 workspace.rs 不同 crate 不冲突）
- Modify: `lib.rs`（`pub mod workspace;` + re-export）

**Interfaces（Task 3/4/5 依赖）:**
```rust
pub struct FileEntry {            // 旧 FileSnapshot 的继任
    pub kind: FileKind,
    pub source: String,
    pub tree: as_syntax::tree_sitter::Tree,
    pub lines: LineIndex,
    pub summary: FileSummary,
    /// 局部 id → 子声明局部 id（B7，decls 后一遍建）
    pub by_parent: HashMap<u32, Vec<u32>>,
}

pub struct Workspace {
    pub config: IndexConfig,
    pub files: HashMap<FileId, FileEntry>,
    pub agg: Aggregation,
    /// Phase B eager（B5）；Phase C 按需化
    pub closures: HashMap<DeclRef, Vec<DeclRef>>,
    pub cycle_classes: Vec<DeclRef>,
    pub resolved: HashMap<DeclRef, TypeId>,
    pub types: TypeTable,
}
impl Workspace {
    pub fn build(config: IndexConfig, inputs: Vec<FileInput>) -> Workspace;
    pub fn decl(&self, r: &DeclRef) -> &RawDecl;
    pub fn lookup(&self, name: Sym) -> &[DeclRef];
    pub fn members(&self, parent: &DeclRef) -> Vec<DeclRef>;
    pub fn namespaces_named(&self, name: Sym) -> Vec<DeclRef>;   // B3：Namespace ∪ is_type_like
    pub fn synthetic_members(&self, r: &DeclRef) -> Vec<SyntheticMember>;  // B4
    pub fn reindex_file(&mut self, file: FileId, kind: FileKind, source: String) -> bool;  // 声明面变化（D29 的继任：新旧 summary 的 (name,kind,parent) 集 diff）
    pub fn reindex_file_full(&mut self, file: FileId, kind: FileKind, module: Option<Sym>, source: String);
    pub fn remove_file(&mut self, file: FileId);
}
```
构建管线：rayon 并行 `parse + extract_summary`（A 阶段已验证 ~530ms 路径）→ 串行 Aggregation::build + builtin 合成（B2）+ closures/resolved（B5，逻辑平移自 index.rs `build_closures`/`resolve_decl_types`，DefId→DeclRef 机械替换）。`synthetic_members` 逻辑平移自 expand.rs（StaticClass + delegate/event 成员集），产出轻量 `SyntheticMember { name, kind, params 渲染, origin: DeclRef }`。

- [x] **Step 1: 写失败单测**——builtin lookup（"int8"/"float64" 命中且 SYNTHETIC）、closures（AActor 链 + 环截断 + unresolved base 容错，对齐旧 m1/m3 用例）、members/by_parent、namespaces_named 的 B3 语义（class 亦命中）、reindex 增量（换 summary 后 agg/closures 正确、其他文件 DeclRef 不变）
- [x] **Step 2-4: 实现 → 绿 → Commit** `索引重构 B2：Workspace 查询容器（FileEntry + Aggregation + eager closures/resolved + 查询期合成成员）`

### Task 3: L3 原子切换（十文件 + types.rs，最大的一步）

**Files（全部 Modify）:** `resolve.rs` `expr.rs` `overload.rs` `completion.rs` `hover.rs` `signature.rs` `inlay.rs` `references.rs` `search.rs` `types.rs`
**Delete:** `index.rs` `expand.rs`（逻辑进 workspace.rs synthetic_members）

步骤：按机械替换表逐文件移植；`WorkspaceIndex` → `Workspace`、`DefId` → `DeclRef`；`namespaces_named` 走 B3 新语义（删 origin_fallback）；`completion`/`hover`/`signature`/`inlay`/`search` 的成员/类型枚举处 ∪ `synthetic_members`；references 的 `find_references`/`candidate_files`/`match_uses`/`resolve_file_uses` 删除（Task 1 的 `find_references_query` 平移后更名 `find_references`）；各文件测试的 `build()` 助手换 `Workspace::build`、断言里 DefId 比较 → DeclRef/名字比较。**顺序**：types → resolve → expr/overload → references → completion/hover/signature/inlay/search（编译依赖序），但**一个 commit 交付**（中间态编译不过）。

- [x] **Step 1: 移植（按上述顺序；每文件完成后 `cargo check -p as-core` 看剩余错误清单收敛）**
- [x] **Step 2: `cargo test -p as-core` 全绿**（预期测试改动集中在助手函数与断言形态，语义断言不动——有失败即语义回归，停下分析不得改断言迁就）
- [x] **Step 3: as-cli `dump-index`/`resolve-stats`/`ref-stats` 临时改挂 Workspace（--new-arch 变为唯一路径，flag 保留为 no-op 兼容）→ 语料验收**：resolve-stats ≥ 94.9% 不回归、ref-stats 与 Task 1 基线一致、`--new-arch` 对账行仍 OK
- [x] **Step 4: `cargo test --workspace` → Commit** `索引重构 B3：L3 后端切换 DefId → DeclRef（十文件 + 删 index.rs/expand.rs）`

### Task 4: as-lsp 移植（WorkspaceState 持有 Workspace）

**Files（Modify）:** `as-lsp/src/workspace.rs` `main.rs`；`as-cli/src/main.rs`（正式化）
**Delete:** as-lsp 侧 `use_cache`/`cached_uses`/D29 调用链（references 已不走它）；as-core `uses.rs`、`lib.rs` 的 UseSite/UseRole 导出

- [x] **Step 1:** `WorkspaceState.index: RwLock<Option<WorkspaceIndex>>` → `Option<Workspace>`；`build_index` 产出新管线；`reindex`/`add_file`/`remove_file` 转 `Workspace` 增量（`use_cache` 字段与 `cached_uses` 整体删除；`publish_and_replay` 简化）
- [x] **Step 2:** `main.rs` 的 references/rename/documentHighlight 改调 `find_references_query`（锁内同步、无缓存）；hover/definition/completion 等路由的 `ws.with` 闭包内类型跟改
- [x] **Step 3:** `cargo test --workspace` 全绿 + `cargo build --release -p as-lsp`
- [x] **Step 4: e2e 冒烟**：`tools\test-extension.ps1 -Release`，看日志 rebuild 时间 + VSCode 手测 hover/references/rename 三件
- [x] **Step 5: Commit** `索引重构 B4：as-lsp 切换 Workspace + 删 use_cache/D29 链 + uses.rs`

### Task 5: 收尾（设计稿 v1.3 + D38 + 死代码清扫）

- [x] **Step 1:** `grep -r "WorkspaceIndex\|DefId\|ref_index\|UseSite"` 确认零残留（id.rs 的 DefId 类型本身删除；`git grep decl_surface` 零命中）
- [x] **Step 2:** 设计稿 v1.3：§8 生死清单标记完成态、§11 步骤 3/5 合并说明、B1-B7 裁决落入正文；决策记录追加 D38（Phase B 落地裁决，含「D5/D23/D29 翻案生效」）；docs/README 状态表更新
- [x] **Step 3:** `cargo test --workspace` 全绿 → Commit `索引重构 B5：死代码清扫 + 文档同步（Phase B 收官）`

## 验收标准（Phase B 整体）

1. `cargo test --workspace` 全绿（163 基线，允许测试形态改动、不允许语义断言删除）
2. 语料：resolve-stats ≥ 94.9%、ref-stats 与 B1 基线一致、声明对账 OK
3. `git grep -n "WorkspaceIndex\|decl_surface\|use_cache"` 零命中
4. e2e 冒烟：rebuild 日志时间 ≤ 600ms（旧 956ms 路径删除后）；hover/references/rename 手测正常

## Phase C-E 展望（不变，见 A 计划尾节；B6 注：scope_tree 消费在 E）

## Self-Review

- **覆盖**：设计稿 §11 步骤 3（Task 2/3）+ 步骤 5 的核心（Task 1，提前并入）+ 宿主移植（Task 4）；步骤 4（按需化）= Phase C、步骤 6/7 = Phase E。✅
- **风险最大点**：Task 3 原子切换——缓解 = 机械替换表先行写死、语义断言不得改、语料三重对账；B3（合成 namespace）是唯一语义重解释点，已有 m3/m4 单测覆盖（namespace 成员查找/引用归一），切换失败会立刻暴露
- **类型一致性**：`find_references_query` Task 1 产出（&WorkspaceIndex 签名）→ Task 3 平移换签名（&Workspace）——计划已注明；`SyntheticMember` Task 2 定义 → Task 3 四个消费文件使用 ✅
