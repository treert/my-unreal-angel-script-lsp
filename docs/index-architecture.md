# 索引架构（per-file summary + 薄聚合层 + 按需定型）

> 版本 v1.4（2026-09-28）
>
> **定位**：索引层的**重构设计稿**。现行架构（三阶段流水线、全局扁平符号 arena、
> use-site 预收集）的替代方案。驱动因素：冷启动性能（414 `.d.as` ~1s 仍偏慢且
> 扩展性差）、单文件更新的 O(全局) 失效成本、以及一批为扁平 arena 擦屁股的
> 机制（D29 指纹、幽灵过滤、`use_cache` 失效链）。
>
> 参考实现：`ai-mylua-lsp`（DocumentSummary / WorkspaceAggregation / 查询期
> references 三件套）。AS 的语言约束（无 lambda、声明即类型）使本方案比 mylua
> **更简单**，不需要 mylua 的多数权衡。

---

## 0. TL;DR

| 维度 | 现行（三阶段 + 全局 arena） | 新（本方案） |
|---|---|---|
| 启动 | parse(并行) + decl 提取(**顺序** 490ms) + uses 收集(300ms) + finish | parse + summary 提取（**全程并行**）+ 一遍名字聚合 |
| 单文件更新 | remove 全表扫 O(全局 main) + 全量重建闭包/mixin + D29 指纹 diff | 换掉该文件的 summary，重聚合其贡献 O(该文件) |
| use-site / 引用倒排 | 启动期预收集（D5） | **查询期**字符串搜 + 逐点解析验证（mylua 同构，D5 翻案） |
| 继承闭包 / mixin 倒排 / 声明类型预解析 | finish() 预计算 | 按需走链 + 进程级缓存 |
| 符号存储 | 全局扁平 `SymbolTable` arena（无文件边界） | per-file summary + 聚合层倒排（`名字 → [(文件, 锚点)]`） |
| 内存 | source + tree + uses + 11 万 DefData 全摊平 | source + tree + per-file summary（uses 删除） |

实测基线（414 `.d.as` / 10MB，release）：优化后索引构建 ~1072ms，其中
decl 提取 490ms（顺序）、uses 300ms——本方案两段都消失。

---

## 1. 语言前提（为什么 AS 比通用方案更简单）

全部取证见 [`../as-docs/`](../as-docs/README.md)，此处只列架构推论：

1. **无 lambda / funcdef / 闭包**（`设计取舍 §2.3`、`对比 §5.2`：funcdef 是
   死 token，lambda 唯一出口断言必败）——作用域模型退化为纯块作用域，
   无捕获、无逃逸、无函数值。mylua 最复杂的 TypeFact / table_shape /
   推断机器一概不需要。
2. **声明即类型**：每个变量 / 形参 / 返回值都带显式类型语法（`SynType`，
   语法层名字）。「推导」只有 `auto`（局部变量 + range-for，BNF DATATYPE
   限定），其余全是「显式名字沿链传播」。
3. **mixin 极简**：mixin 函数（两种声明形式）+ 首参类型名挂靠 + 实例上
   像成员一样调用。无 mixin class（paper feature）、无冲突消解。
4. **模板只在 `.d.as`**：类型形参只来自 `.d.as` 模板定义体；`.as` 侧
   永远是实参（`设计取舍 §2.5.1`）。
5. **`.d.as` 必然合法**（导出器生成），语法错误只可能出现在 `.as`。

**推论**：per-file summary 的边界天然干净——提取期产出的一切都可以只含
「名字」（Sym），跨文件链接（DefId 级引用）全部推迟到聚合/查询期。

---

## 2. 三层模型

```
┌────────────────────────────────────────────────────────────┐
│ L1 每文件 summary（FileSummary）                            │
│    parse(tree-sitter) + 声明提取 + 局部定型表               │
│    纯函数：(source, 配置) → FileSummary                     │
│    全程并行（rayon），无锁、无全局状态                       │
├────────────────────────────────────────────────────────────┤
│ L2 聚合层（Aggregation）                                    │
│    名字 → [(文件, 锚点)] 倒排 + 优先级排序 + mixin 名字倒排   │
│    单临界区一遍扫描全部 summary 构建（mylua build_initial）  │
├────────────────────────────────────────────────────────────┤
│ L3 查询层（按需定型）                                       │
│    查找链 0-6 级 + 表达式定型 + 成员链沿走 + 进程级缓存      │
│    继承闭包 / mixin 注入 / 合成成员 / auto 全部现算          │
└────────────────────────────────────────────────────────────┘
```

各层职责边界：

- **L1 知道**：本文件声明了什么（含签名、doc、tags、基类**名字**）、
  局部变量及其可本地推导的类型；
  **不知道**：任何其他文件的存在。
- **L2 知道**：每个名字在哪些文件的哪个位置；
  **不知道**：签名、类型语义、继承关系。
- **L3 知道**：一切（按需查询 L1/L2）；带缓存，缓存失效粒度 = 文件。

---

## 3. L1 — FileSummary

### 3.1 结构（设计稿形态，字段名以实现为准）

```rust
pub struct FileSummary {
    pub kind: FileKind,                    // Script / Decl
    pub module: Option<Sym>,               // 引擎 FilenameToModuleName 口径
    pub group: Option<String>,             // .d.as 文件头 @group
    pub cache_format: Option<u32>,         // 识别不消费（D21）
    /// 声明表（文件内局部 id 0..n 稳定）
    pub decls: Vec<RawDecl>,
    /// 名字 → 局部 id 倒排（本文件内查找，含重载组）
    pub by_name: HashMap<Sym, Vec<u32>>,
    /// 局部作用域树（§3.3）：函数体内的局部/形参/迭代变量 + 预推导类型
    pub scope_tree: ScopeTree,
}

pub struct RawDecl {
    pub name: Sym,
    pub kind: DefKind,
    pub name_span: TextRange,              // 锚点（rename/definition 用）
    pub full_span: TextRange,
    pub parent: Option<u32>,               // 文件内局部 id（类成员 → 类）
    /// 基类名（class）——只记名字，不解析（§5.2）
    pub bases: Vec<BaseRef>,
    /// 签名（SynType，语法层名字）
    pub extra: RawExtra,                   // Callable/Variable/EnumValue…
    pub flags: DefFlags,                   // MIXIN/LOCAL/SYNTHETIC… 全保留
    pub doc: Option<Box<str>>,
    pub tags: Vec<SemanticTag>,
}
```

要点：

- `RawDecl` ≈ 现有 `DefData` 去掉跨文件字段（`file: FileId`、
  `origin: DefId`→见 §7 合成成员、`parent: DefId`→局部 id）。
- **局部 id 在文件内稳定且永不跨文件**——这是「文件重索引不影响其他文件」
  的根基。
- `by_name` 是文件内的 `main` 微缩版，L3 查询成员/本地符号先查它。

### 3.2 与 FileSnapshot 的关系

`FileSnapshot`（source + tree + lines）**保留**——hover/补全/诊断的任意点
查询需要整棵 CST。summary 是它的伴生提取物，二者一起换入换出：

```rust
pub struct FileEntry {           // 替代 FileSnapshot
    pub source: String,
    pub tree: Tree,
    pub lines: LineIndex,
    pub summary: FileSummary,
}
```

**不再有**：`uses: Vec<UseSite>`（→ §6 查询期）、`errors`（已删，诊断期
按需 `verify_tree`，`has_error` 剪枝）。

### 3.3 scope_tree（局部作用域树 + 局部定型）

**FileSummary 必含 scope_tree**（mylua `scope.rs` 同构，定案——曾考虑
扁平局部表，否决：遮蔽语义下每次查询都要现场重建树形关系，不如提取期
一次建好）。结构（设计稿形态）：

```rust
pub struct ScopeTree {
    /// 作用域 arena：块 / 函数体 / for-range 体各一节点，parent 指针
    scopes: Vec<Scope>,
}

pub struct Scope {
    pub parent: Option<u32>,
    pub span: TextRange,                // 块 span（查询点包含判定）
    pub decls: Vec<LocalDecl>,
}

pub struct LocalDecl {
    pub name: Sym,
    pub kind: LocalKind,                // Var / Param / IterVar（AS 无嵌套函数，无 LocalFn）
    pub name_span: TextRange,           // 声明锚点（rename/definition）
    /// 预推导类型（§3.3.1）；推不出 = None（查询期补）
    pub ty: Option<SynType>,
}
```

**查询原语**（L3 各功能共用）：`resolve_local(tree, byte, name)`——
定位最内层包含 byte 的 scope，沿 parent 链上溯找最近声明（遮蔽天然
正确）；`locals_visible(tree, byte)`——completion 候选用。

**AS 简化**（相对 mylua）：无 lambda / 闭包逃逸 → 树只反映语法块嵌套，
无捕获拷贝、无 upvalue 记账；`local` 函数的**模块级**可见性不在这棵树
（那是 L2 的 `module_files` 职责），树里只有函数体内的局部。

#### 3.3.1 summary 期可做的局部推导

建树同趟完成。推导产物一律是 `SynType`（名字），**不是** DeclRef；
推不出的保留 `None`（宁缺毋假，语义与现状一致）。

| 模式 | 结果 |
|---|---|
| `Cast<T>(x)` | `Named(T)` —— 名字直接抄，不解（文法侧 `'Cast'` 是匿名 token，type 字段直接是 T） |
| `auto X = OtherLocal` | 复制已定型局部的名字（同趟顺序扫描） |
| 字面量 | `1`→int、`1.5`→float（按配置归一）、`n"…"`→FName、f-string/字符串→FString（D25） |

**实现期裁决（Phase A 落地时定，与初稿两处差异）**：
- **构造调用不做**——`auto X = Ident(args)` 无法与函数调用语法区分，
  宁缺毋假（宁少推不推错）；
- **显式声明类型保留裸 `float`**（归一化是 L3 消费期的事）；字面量无
  源码类型名，直接给规范名（float64/float32）——两层各自一致。

**推不出的**（跨文件传播，如 `auto X = Actor.GetComponent()`）：保留
`None`，L3 查询期补。**预推导是加速器，不是真值源**——命中率高低不影响
正确性。

### 3.4 并行与确定性

`(source, config) → FileSummary` 是纯函数 → rayon `par_iter` 全程并行。
重载组顺序 = 文件内源码序（局部 id 升序），确定性由聚合层排序保证（§4.2）。

---

## 4. L2 — 聚合层

### 4.1 结构

```rust
pub struct Aggregation {
    /// 名字 → 声明锚点（跨文件）。重载组原样保留（消歧语义不变）
    pub main: HashMap<Sym, Vec<DeclRef>>,
    /// mixin 名字倒排：首参类型名 → mixin 声明锚点
    pub mixin_by_name: HashMap<Sym, Vec<DeclRef>>,
    /// 模块名 → 成员文件集（local 函数可见域过滤）；映射经
    /// `module_file(Sym) -> Option<FileId>` 访问器取（root 优先序最优，
    /// 实现期定：成员表而非单值映射——接替者在删除时自动重选）
    module_members: HashMap<Sym, BTreeSet<FileId>>,
    /// per-file 贡献倒排（增量更新的钥匙，mylua uri_to_paths 同款）
    contributions: HashMap<FileId, Vec<ContributionKey>>,
}

pub struct DeclRef {
    pub file: FileId,
    pub local: u32,        // 文件内局部 id
}
```

**它只是「名字在哪个文件的哪个位置」**——不存签名、不存类型、不做任何
语义判断。签名/类型在 L3 拿 DeclRef 回源文件 summary 现取。

### 4.2 候选排序（继承自架构设计 §4.2，显式化）

同名字多候选的排序规则，聚合层构建时一次排定：

1. 根优先级：脚本根 > `.d.as` 根（`root_index` 升序）；
2. 同根内按 FileId、再按局部 id 升序（= 源码序）。

排序错误直接导致 `class Foo : UObject` 解析到错误基类——单测钉死。

### 4.3 构建与增量

- **冷启动**：全部 summary 就绪后单临界区一遍扫描（mylua `build_initial`
  同款——消除批序依赖）；O(总声明数)。
- **单文件更新**（didChange / watched-files）：
  1. 重提取该文件 summary（并行池外的单文件，毫秒级）；
  2. 沿 `contributions[file]` 摘除旧贡献（O(该文件声明数)）；
  3. 扫入新贡献；
  4. 失效 L3 缓存中与该文件相关的条目（§5.4）。
  **不再有**：全表 `remove_file_defs` 扫描、全量闭包/mixin 重建、
  D29 声明面指纹（其职责被「贡献倒排 + 缓存失效」天然取代）。

---

## 5. L3 — 按需定型（查询层）

### 5.1 查找链不变

`resolve.rs` 的 0-6 级查找链（local → member → mixin → global → …）、
重载消歧（D28）、表达式定型管线（D14、D31-D33）**逻辑全部保留**，只换
数据后端：`idx.def(id)` / `idx.main` 查询改为 DeclRef → 源 summary 取数。

### 5.2 继承走链现算（C6：零缓存）

`AActor` 的祖先链不再预计算（`closures` 表删除）：

```
walk_bases(class):
    chain = [class]
    for base_name in summary(class).bases:
        target = L2.lookup_type(base_name)        // 名字查聚合层
        if target is None: 记 unresolved（诊断素材）, continue
        if target in chain: 环检测（visited 集）, continue
        chain.push(target); 递归
```

- **零缓存（Phase C 实现期裁决 C6，否决 v1.0 的链缓存方案）**：链由
  `bases` 名 + agg 一跳**完全推导**（单继承链非树；AActor 深度 ~10 ≈
  10 次哈希，µs 级）——缓存节省的是重复哈希，付出的是 Mutex 域 + 失效
  时序 + prime 对账口径三件管理成本，负收益。接口 = `base_class`（单跳）
  / `ancestor_chain`（整链，环在首次重复处截断）/ `cyclic_classes()`
  （全扫环检测，诊断素材）。
- 无缓存还白赚正确性收益：reindex 后链立即按新 agg 现算，不存在
  「清空→重建」中间态。
- 环检测从 `cycle_classes` 预计算改为走链时 visited 集合；环的存在不再
  是索引期错误，而是查询结果的一部分（诊断可后置消费）。

### 5.3 mixin 简化

```
查询 X.SetVectorToZero()（X: FVector）:
  1. summary(FVector) 成员查 by_name：miss
  2. L2.mixin_by_name["FVector"]：命中 → mixin 声明
  3. class 目标：沿 §5.2 链逐级重复 1-2
```

**消失的机制**：`mixin_index: HashMap<DefId, Vec<DefId>>`（DefId 键倒排）、
`mixin_pending` 待定桶（名字键下解析失败 = 没人查的键，天然容错）、
`resolve_syn` + `named_base_of` 索引期剥壳（语法层直接取 base name）、
`live_def_ids` 幽灵过滤（per-file 替换即消失）。

首参剥壳（`FVector&` / `const FVector&in`）在 `SynType` 语法层完成
（`Ref/Const/Array` 包装逐层剥到 base name），不经过类型表。

### 5.4 缓存与失效（Phase C 后：L3 无缓存）

v1.0 原案设想的「进程级缓存容器 + 统一失效」被实现期证伪（C2/C6/C7）：
祖先链一跳可推导不值得缓存、TypeId 体系是死代码、合成成员本就现推——
**L3 全部现算，不存在任何缓存**，「失效」问题整体消失：

| v1.0 设想 | 实际（v1.4） |
|---|---|
| 祖先链缓存（文件更新全清） | 现算（§5.2 零缓存） |
| 合成成员缓存 | 本就现推（§7，B4 落地即查询期） |
| auto 定型 / UseSite 解析缓存 | 不存在（全部现算，见 §6） |

**不再有** as-lsp 侧 `use_cache` + D29 失效链——该机制整体删除。

### 5.5 诊断分层（显式化既有事实）

| 层 | 依赖 | 时机 |
|---|---|---|
| 语法诊断（AS0903） | 仅该文件 CST（`has_error` 剪枝 O(1)） | 任意时刻 |
| 语义诊断（AS0902、未来 AS04xx/AS02xx） | 聚合层就位 | Ready 后，mylua 式队列：修改文件 > 打开文件 > 其余，300ms 防抖 |

Loading 期语义请求返回空的设计与本分层自洽（auto 跨文件传播推不出 =
索引未就位的正确行为）。

---

## 6. references / rename 查询期化（D5 翻案）

### 6.1 流程（mylua references.rs 同构）

```
find_references(targets):
  names = 目标名字集合（重载组若干）
  for file in 全部文件（可 rayon 并行化，预留）:
      for occurrence in find_word_occurrences(source(file), name):
          node = tree(file).descendant_for_byte_range(occurrence)
          if node 不是 identifier: continue          // 注释/字符串命中滤掉
          resolution = resolve_at_node(node)          // 既有查找链
          if resolution ∩ targets ≠ ∅: 命中
```

- `find_word_occurrences`：词边界搜索 `[A-Za-z_][A-Za-z0-9_]*`
  （防 `Health` 误中 `GetHealthTime`），区分大小写。
- 局部变量（`RefTarget::Local`）：只扫声明所在文件。
- f-string 插值段天然覆盖（源文本就在那）。
- 匹配语义（§4.6：站点解析集合 ∩ 查询集合 ≠ ∅ 即命中、消歧失败报全部
  重载、namespace → class 归一）**不变**——只是候选枚举从倒排表换成
  字符串搜索。

### 6.2 成本

- 10MB 语料：memmem 级，全库扫几 ms，无感。
- 200MB（2w 文件量级）：100-200ms——mylua 实测可接受；预留 rayon
  并行化（`files` HashMap 先收集 Vec 再 `par_iter`）。
- **删除**：`FileSnapshot.uses`（`Core.d.as` 数十万站点 ≈ 10MB+ 内存）、
  `ref_index`（~10 万条 BTreeMap）、`collect_use_sites` / `UseSite` /
  `UseRole`（role 无运行时消费者——已核实）、`match_uses` 两个变体、
  as-lsp `use_cache`。

### 6.3 documentHighlight

同链路，只扫当前文件——天然更快。

---

## 7. 合成成员（expand 改查询期）

`expand.rs`（delegate/event 成员集展开 + StaticClass 合成）改为查询期函数：

```
synthetic_members(decl_ref):
    match kind:
        Delegate/Event → [Execute/Bind/… 按 tag 形态现推]
        Class → [StaticClass]
        _ → []
```

- 不预存展开产物；L3 查成员时 `summary 成员 ∪ synthetic_members(类)`。
- **workspaceSymbol 影响**：`query_symbols` 失去「全量已展开」的便利。
  方案：搜索时对候选类型实时展开一次；量大（万级类型）再引入轻量全局
  缓存（失效 = 全清，重建 = 遍历 summary 调 synthetic_members）。
  实现期量化后定。
- `origin` 字段（D10 归一）保留语义：合成成员的查询期构造物携带源
  DeclRef，references 的 namespace → class 归一（§4.6 例外条款）继续成立。

---

## 8. 旧机制生死清单（逐条裁决）

> **Phase B 落地状态（v1.3，D38）**：下表「状态」列为 2026-09-28 实际执行
> 结果。与 v1.0 原案的两处偏差：closures / resolve_decl_types / TypeTable
> 的按需化**延后到 Phase C**（B5：行为零变化优先）；D29 指纹**收缩为
> reindex 返回值接口**而非整体删除（Phase E 消费，见 D38 处置修正）。
>
> **Phase C 落地状态（v1.4，D39）**：最后三个 ⏳ 项收官——且结论比原案
> 更激进：不是「按需 + 缓存」而是**删除与零缓存**（继承链一跳可推导 C6；
> resolved/TypeTable 是从未接线的平行世界残留 C7）。v1.0 设想的「进程级
> 缓存容器 + 统一失效」随之整体作废（§5.4）。

| 旧机制 | 去向 | 理由 | 状态（v1.3） |
|---|---|---|---|
| D4（不设轻扫描，全量 parse） | **保留** | parse 仍是全量（并行），summary 提取并入同一并行段 | ✅ 落地（parse+summary 同一 rayon 段，~530ms） |
| D5（Phase 2 记录 use-site，Phase 3 惰性解析） | **翻案 → §6** | 查询期字符串搜 + 现解析；每次查询工作量反而下降 | ✅ 生效（uses.rs / ref_index / use_cache 删除；ref-stats 25505 逐位一致） |
| D18（append-only arena + 墓碑） | **收缩** | FileId 注册表照旧（路径 → id 复用不变）；DefId arena 整体消失，墓碑只剩 FileId 层 | ✅ 落地（DefId 类型本身删除） |
| D23（mixin DefId 倒排，键不预展开） | **翻案 → §5.3** | 名字倒排 + 查询期剥壳，更简单 | ✅ 生效（`mixin_by_name` + `mixin_pending` 删除） |
| D25（内建 primitive 合成 DefId） | **保留，改形式** | 内建类型表改常量映射（名字 → 固定合成锚点），不进 per-file arena | ✅ 落地改 B2 形态（builtin 伪文件 + SYNTHETIC RawDecl，同一代码路径） |
| D29（声明面指纹） | **删除** | 职责被贡献倒排 + 缓存失效取代 | 🔶 收缩（返回值保留为增量接口，Phase E 消费——D38 修正） |
| `closures` / `cycle_classes` | **删除** → §5.2 | 按需走链 + visited 环检测 | ✅ Phase C 落地（C5/C6：`base_class`/`ancestor_chain` 走链现算**零缓存**——链缓存方案被否决；`cycle_classes` 字段删、`cyclic_classes()` 函数留作诊断素材） |
| `resolve_decl_types`（3.2 万条预解析） | **删除** | 查询期从 SynType 现查聚合层（hover 本来就走查找链） | ✅ Phase C 删除（C7：唯一消费点 expr 回落经 441 文件语料取证 **0 命中**——触发条件与填充条件不相交，D38 所记「expr 回落仍消费」系未证实假设） |
| `TypeTable`（类型规范 intern） | **保留，填充改查询期** → **退役** | ~~Sym 级进程 intern~~ 活跃定型管线是 DeclRef + SynType 双件套（expr 的 `syn_type_base`/`template_map`/`subst_syn`），TypeId 从未接线 | ✅ Phase C 退役（C7/D39：D17/G7 翻案；`types.rs` 只留 SynType/RefKind，M3 若需类型身份按当期需求重设计） |
| `modules` 表 | **收缩** | per-file 自带模块名；聚合层只留 `Sym → FileId` 小表 | ✅ 落地（`Workspace::module_of` 走 summary；聚合层 module_members 选举） |
| `live_def_ids` 幽灵过滤 | **删除** | per-file 替换即消失 | ✅ 落地（无 arena 即无幽灵） |
| verify_tree 启动期收集 | **已删**（前序提交） | 诊断期 `has_error` 剪枝 | ✅ 维持 |

---

## 9. DefId 过渡策略

**方案 X 先行**（本次重构）：L1/L2/L3 全按 DeclRef（`(FileId, u32)`）实现，
`resolve.rs` / `completion.rs` / `hover.rs` 等消费点的 `DefId` 替换为
DeclRef + 统一的 `fetch(DeclRef) → &RawDecl` 访问器。改动面大但一次到位，
不留双轨。

**不采用的方案 Y**（全局 arena 保留 + per-file 倒排）：改动小但保留
append-only 内存单调涨与重映射复杂度——旧设计不迁就（用户决策，2026-09-28）。

---

## 10. 与 mylua 的对照表（实现时参考哪个模块）

| 本方案 | mylua 对应 | 差异 |
|---|---|---|
| FileSummary | DocumentSummary | AS 有显式类型签名；无 table_shape / TypeFact |
| Aggregation.main + contributions | GlobalShard + uri_to_paths | AS 扁平名字空间（无 Lua 的 `a.b.c` 树形贡献） |
| 查询期 references | references.rs find_word_occurrences + verify | verify 逻辑 = 既有查找链，不用重写 |
| 诊断调度 | diagnostic_scheduler.rs | 优先级队列 + 300ms 防抖，可直接移植结构 |
| scope_tree | scope.rs | AS 无 lambda：树只反映语法块嵌套，无捕获/upvalue 记账；`resolve_local` / `locals_visible` 两原语即覆盖全部消费方 |

---

## 11. 实施切分（每步独立可测、可提交）

| # | 内容 | 验收 |
|---|---|---|
| 1 | `FileSummary` / `RawDecl` / `ScopeTree` 类型 + `extract_summary` 纯函数（从 `extract_decl` 平移，输出改局部 id；scope_tree 建树同趟，参考 mylua scope.rs） | 单测：summary 内容等价断言（对照现有索引逐字段）；scope_tree 遮蔽 / for-range / 形参用例 |
| 2 | `Aggregation` + 冷启动单临界区构建 + 候选排序 | 单测：排序规则；对账 type_count/member_count 不回归 |
| 3 | L3 数据后端切换：DeclRef + fetch 访问器替换 DefId 直查（resolve/expr/completion/hover/inlay/outline/tokens/signature） | 全部既有单测绿（这是最大的一步，靠 138+ 单测兜底） |
| 4 | 闭包/mixin/合成成员/TypeTable 改按需 + 缓存 | 单测：走链等价（含环 / unresolved / struct mixin） |
| 5 | references/rename/documentHighlight 查询期化 + 删 uses/ref_index/use_cache/D29 | 单测：匹配语义等价；ref-stats 语料对账（99.4% 不回归） |
| 6 | as-lsp 侧：诊断调度队列（mylua 式）+ 增量更新路径（贡献倒排） | e2e：didChange 增量 / watched-files 生命周期 |
| 7 | 文档同步：`架构设计.md` §4、`LSP实现规划.md` §4-§6、决策记录追加 D37+ | 文档治理规则 3/5 |

步骤 3 与 5 可互换；步骤 1-2 无行为变化；步骤 3 后旧 `WorkspaceIndex`
即删。

> **Phase A/B 实际执行映射（v1.3，D38）**：步骤 1-2 = Phase A（A1-A5，
> 双轨对账零差异收官）；**步骤 5 的 references 查询期内核提前并入 Phase B
> Task 1**（旧后端上先 A/B 对账验证，避免 use-site 死代码移植）；步骤 3
> 与 步骤 5 的删除部分在 **Phase B Task 3 一次原子切换**（L3 十文件 +
> 删 index.rs/expand.rs，as-cli 临时同批挂载——as-lsp 依赖旧 API 使
> 「只动 as-core」会断编 workspace 测试）；步骤 5 的 as-lsp 侧 + uses.rs
> 删除 = **Task 4**；步骤 4（按需化）= Phase C（B5 裁决延后）；步骤 6
> = Phase E；步骤 7 = 本版 v1.3 + D38。验收基线：162 单测全绿
> （171 基线中 9 条随已删机制退休、存活断言全平移），语料三重对账
> 全一致（详见 D38）。
>
> **Phase C 实际执行映射（v1.4，D39）**：步骤 4 = Task 1-5（取证 →
> 走链接口（新旧对照等价测试钉死后删 eager）→ 消费点七处切换 →
> TypeId 体系退役 → 收尾）。与原案的偏差：**「按需 + 缓存」变「删除 +
> 零缓存」**（C6/C7，消费者面逐点核实推翻 B5 时的假设）；验收 = 三旗标
> 不回归（resolve-stats 1028/1083=94.9% 精确一致、ref-stats 25505 hits
> 逐位一致、decls 101322）+ expr 死回落语料取证 0 命中 + 测试 160 全绿
> （退休账目见 D39）。as-lsp crate 零改动。

---

## 12. 风险与验证项

| # | 风险 | 缓解 |
|---|---|---|
| 1 | DefId → DeclRef 改动面大（几十处消费点） | 切分步骤 3 独立成步；fetch 访问器先行；138 单测 + 语料对账兜底 |
| 2 | 重载组顺序语义变化（注册序 → 文件序+源码序） | §4.2 排序规则单测；消歧单测全量过 |
| 3 | workspaceSymbol 实时展开性能 | §7；实现期量化，超阈值再加缓存 |
| 4 | references 全库扫在超大工作区的延迟 | 预留 rayon；先按 mylua 实测接受 |
| 5 | 双源 `.d.as`（多副本）同名冲突排序 | 沿用 root_index 优先级（§4.2）；多副本消解仍属 D24 搁置项 |

---

## 变更记录

| 版本 | 日期 | 内容 |
|---|---|---|
| v1.4 | 2026-09-28 | **Phase C 落地同步**（D39）：最后三个 ⏳ 项收官，结论比原案更激进——**删除与零缓存**而非「按需 + 缓存」。C5（命名改祖先链语义：`base_class`/`ancestor_chain`——`closures` 系图论闭包命名易与 AS 无函数闭包混淆）；C6（继承零缓存：链由 bases 名 + agg 一跳完全推导，原方案的链缓存被否决；reindex 后立即现算无中间态）；C7（TypeId 体系退役：expr resolved 回落经语料取证 0 命中系死路径、活跃定型管线是 DeclRef + SynType 双件套——TypeTable/TypeKind/TypeId/resolve_syn/render_type/named_base 全删，types.rs 只留 SynType/RefKind，**D17/G7 翻案**）；C2（无缓存容器：v1.0 设想的 Mutex 派生缓存整体作废，§5.2/§5.4 重写）。验收：160 单测全绿（162 基线：-2 退休有继任、-3 随 TypeTable 机制消失、+1 reindex 即时反映、+2 expr 继任）、三旗标精确一致（resolve-stats 1028/1083=94.9%、ref-stats 25505 hits 逐位、decls 101322）、expr 死回落 441 文件语料取证 0 命中、as-lsp 零改动。代码净删 ~470 行 |
| v1.3 | 2026-09-28 | **Phase B 落地同步**（D38）：L3 后端切换 DefId → DeclRef 完成——B1-B7 实施裁决落入 §8/§11（references 查询期唯一路径；builtin 合成伪文件（B2，agg.main +15 内建键为预期差）；类直接兼任 namespace（B3，origin_fallback/合成 namespace 删除）；合成成员查询期 synthetic_members + Target::Synthetic/RefTarget::Synthetic（B4，D10 origin 以 DeclRef 保留）；closures/resolved/TypeTable 保持 eager 到 Phase C（B5）；members = by_parent + namespace 聚合（B7））。§8 生死清单标注落地状态；§11 补 Phase A/B 执行映射（步骤 5 提前并入 B1、步骤 3+5 合并于 B3 原子切换）。旧 WorkspaceIndex/DefId/DefData/SymbolTable/index.rs/expand.rs/uses.rs 全删。验收：162 单测全绿、resolve-stats 94.9% 精确一致、ref-stats 25505 hits 逐位一致（~305ms）、--new-arch decls 101134 一致 |
| v1.2 | 2026-09-28 | **Phase A 落地同步**（实现期三处裁决）：① LocalKind 无 LocalFn（AS 无嵌套函数，v1.1 笔误）；② auto 预推导不做构造调用（`auto X = Ident(args)` 与函数调用无法语法区分，宁缺毋假）+ 显式声明保留裸 `float` / 字面量给规范名的双层口径（§3.3.1）；③ 聚合层模块映射改成员表 + `module_file` 访问器（删除时接替者自动重选，§4.1）。Phase A 验收：414 `.d.as` 双轨对账零差异（decls 101134 / main keys 53711），新架构 531ms vs 旧 956ms（-44%） |
| v1.1 | 2026-09-28 | **scope_tree 升级为 FileSummary 必含组件**（§3.3 重写）：否决「扁平局部表」倾向——遮蔽语义下每次查询都要现场重建树形关系（区间包含 + 深度比较），resolve 第 1 级 / completion 局部候选 / references 逐点验证 / inlay 全依赖它，不如提取期一次建好；结构定案（Scope arena + parent 链 + LocalDecl 含预推导类型）、两查询原语（`resolve_local` / `locals_visible`）、AS 简化点（无 lambda ⇒ 无捕获记账；`local` 函数模块级可见性归 L2）；风险 4（扁平表验证项）随之移除 |
| v1.0 | 2026-09-28 | 首版：三层模型（per-file summary / 薄聚合层 / 按需定型）；D5/D23/D29 翻案裁决；mixin 名字倒排简化；auto-only 推导边界与局部定型表；references 查询期化；DefId → DeclRef 一步到位；实施切分 7 步 |
