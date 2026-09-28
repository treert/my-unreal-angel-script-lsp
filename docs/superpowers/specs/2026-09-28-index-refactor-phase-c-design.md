# 索引重构 Phase C 设计：eager 派生表删除 + 继承走链现算

> 版本：v3（2026-09-28，brainstorming 三轮收敛的终版）
>
> **定位**：`docs/index-architecture.md`（v1.3）§11 步骤 4 / §8 生死清单最后三个
> ⏳ 项的实施设计。Phase A（D37）、Phase B（D38）已完成。
>
> **与 v1 稿的根本差异**：v1 假设是"三块 eager 预计算改按需 + Mutex 缓存容器"；
> brainstorming 中对消费者面的逐点核实推翻了该前提——其中两块是**死代码**，
> 第三块（继承链）**一跳可推导、不值得缓存**。终版是"纯删除 + 走链现算"，
> 不引入任何新并发机制。

## 1. 关键发现（本设计的依据）

Phase B 收官时 `Workspace::build` 尾段仍 eager 预计算三块派生数据，逐一核实：

### 1.1 `closures`（继承祖先链）——活跃，但一跳可推导

7 个消费站点分两形态：

| 形态 | 站点 | 实际需要 |
|---|---|---|
| 取直接基类 | `resolve.rs` ×3（this/super：`closures.get(&t).first()`） | 单跳——`chain[0]` 恰为 `resolve_base_class` 的返回值，现状是绕道缓存取一跳 |
| 成员搜索空间 | `resolve.rs` ×2 + `completion.rs` ×2 | 整条链 |

链由 `RawDecl.bases`（基名）+ agg 一次名字查找**完全推导**：单继承、AActor 链深
~10 ≈ 10 次哈希，µs 级；references 全语料验证 ~2.5 万次解析也只几十 ms。缓存
节省的是 10 次哈希，付出的是 Mutex 域 + 失效时序 + prime 对账口径三件管理成本
——负收益。**零缓存还白赚正确性收益**：reindex 后链立即按新 agg 现算，无
"清空→重建"中间态。

### 1.2 `resolve_decl_types` → `resolved`（3.2 万条声明类型预解析）——死代码

`expr.rs` 定型主干是 `syn_type_base`（拼写 → 基类 DeclRef，直接查 agg），全程
不产 TypeId。`resolved` 在 expr.rs 的唯一消费点（:574 回落）**静态证伪为死路径**：

- 回落触发条件：`def_decl_syn` 为 None ⟺ `Variable{ty:None}`（该 match arm 只
  处理 Field/GlobalVar/VirtualProperty/AssetDecl，extra 只可能是 Variable 形态）
- `resolved` 填充条件：`Variable{ty:Some}`
- 两者不相交 ⟹ 回落永远 miss

D38 所记"expr 的 resolved 回落路径仍消费"系未经证实的假设。

### 1.3 TypeId 体系（TypeTable + resolve_syn）——平行世界残留

死路径删除后，TypeId 体系的完整消费者 = `render_type`（仅单测/调试）+ as-cli
统计行 + 单测。**活跃查询路径消费为零**。

成因：TypeId 是 M1 时代按规划 §3.3 预铺的（"归一化 intern、相等即同一、
instantiate 随 M3 落地"），但实际实现走通了另一条路——定型 = `ExprTy { base:
DeclRef, syn: SynType }`，模板替换在 SynType 上做（`template_map`/`subst_syn`），
float 归一在 `syn_type_base` 按 config 现判。整条活跃管线是 **DeclRef + SynType
双件套**，TypeId 从未接线。

## 2. 裁决（C1-C7）

| # | 裁决 | 理由 |
|---|---|---|
| C1 | **范围不含 Phase D/E**（诊断调度、scope_tree 消费、定向失效另批） | 各自独立可验收，混批原子性差；D 依赖本期之后的成本模型 |
| C2 | **无缓存容器**：DerivedCaches / Mutex / interior mutability 整体不引入 | 删完死代码后，全库无任何查询路径需要 `&self` 写入（1.2/1.3 + §3 排查） |
| C3 | **三旗标不回归即验收**（resolve-stats 94.9% / ref-stats 25505 / --new-arch 101134）+ expr 死回落先加计数器跑语料取证（预期 0 命中，数字进提交信息） | 被删路径三旗标均不触碰——"死"的第三重验证；证据文化 |
| C4 | **`cycle_classes` 删字段，留 `cyclic_classes()` 函数** | 唯一运行时消费方是 as-cli 统计；§5.2 环是查询结果的一部分，函数形态供 as-cli + Phase D 诊断素材 |
| C5 | **命名：`closures` → 祖先链语义**（`ancestor_chain` / `base_class` / `ancestor_chains`） | 旧名是图论"传递闭包"运算命名，与 AS 无函数闭包（语言前提 §1.1）易混淆；实际含义是查询类继承体系——单继承链非树 |
| C6 | **继承零缓存**：单跳 = 现有 `resolve_base_class`（更名 `base_class`）；整链 = `ancestor_chain` 走链现算（visited 环截断 + `MAX_CHAIN_DEPTH`） | 见 §1.1 |
| C7 | **TypeId 体系整体退役**：TypeTable / TypeKind / TypeId / `resolve_syn` / `resolved` / `render_type` / `named_base` / expr.rs 死回落全部删除；`types.rs` 只留活跃的 `SynType` / `RefKind` | 见 §1.2/§1.3；D17/G7 随 D39 记翻案（实现路线已用 SynType/DeclRef 替代，M3 若需类型身份再按当时需求重设计）。不做"留作 M3 备件"——不保留死代码 |

## 3. 终态架构

```rust
pub struct Workspace {
    pub config: IndexConfig,
    pub files: HashMap<FileId, FileEntry>,
    pub agg: Aggregation,
    // 无 derived 字段——全库查询路径纯只读
}
```

新增查询接口（全 `&self` 纯计算，无缓存无锁）：

| 方法 | 语义 |
|---|---|
| `base_class(&self, class: &DeclRef) -> Option<DeclRef>` | 单跳：基名 → Class 声明（现 `resolve_base_class` 更名，逻辑不变） |
| `ancestor_chain(&self, class: &DeclRef) -> Vec<DeclRef>` | 整链：逐级 `base_class` 走链，visited 环截断 + 深度上限；不含 self、近者在前；空链 = 无基类/unresolved base/struct（D16 不走此路） |
| `cyclic_classes(&self) -> Vec<DeclRef>` | 全扫环检测（诊断素材；语义对齐旧 `cycle_classes` 的 path 重复段去重集合） |

不变量：`ancestor_chain(c)[0] == base_class(c)`（与旧 `closures` 语义逐位对齐，
环/旁支/unresolved 单测平移钉死）。

## 4. 删除清单

**workspace.rs**：字段 `closures` / `cycle_classes` / `resolved` / `types`；
函数 `build_closures` / `resolve_decl_types` / `resolve_decl_types_in` /
`resolve_syn` / `render_type`；`build()` 尾段两调用 + derived 分段计时。
**expr.rs**：:571-575 死回落分支 + `named_base`。
**types.rs**：`TypeTable` / `TypeKind` / `intern` 及其单测；保留 `SynType` /
`RefKind`（活跃——summary/expr/synthetic_members 消费）。
**id.rs**：`TypeId`（若无其他引用）。
**as-cli**：`types: interned X / resolved Y/Z` 统计行；`inheritance` 行改走链
现算（classes N / cycles M / unresolved base——后者现有循环已算，保留）。
**reindex/remove 路径**：`closures.clear()` / `resolved.retain(...)` 等派生表
维护代码全部消失（reindex 只剩 agg 贡献替换 + 声明面 diff 返回值）。

## 5. 消费点改造（机械）

- `resolve.rs` ×3 this/super：`ws.closures.get(&t).and_then(c.first())` →
  `ws.base_class(&t)`
- `resolve.rs` ×2 + `completion.rs` ×2 成员搜索空间：`ws.ancestor_chain(&recv)`
- expr.rs 死回落：随 C7 删除（先计数器取证）
- 单测：workspace.rs closures/resolved 相关断言改走链等价（语义断言不动）；
  TypeTable 3 条单测随机制退休；162 基线相应增减记入 D39

## 6. 测试与验收

1. 死路径取证：expr 回落处加命中计数器（`eprintln!` 或原子计数），跑 414
   语料 `dump-index --resolve-stats`，预期 0 命中，数字进提交信息
2. `cargo test --workspace` 全绿（语义断言零变化，仅接口形态跟随）
3. 三旗标语料验收：resolve-stats ≥ 94.9%、ref-stats 25505 逐位一致、
   --new-arch decls 101134 一致
4. 收益记录：rebuild 日志 derived 段消失（B 基线数字进提交信息）；as-cli
   走链统计行（classes/cycles/unresolved）与 B 基线 closures/cycles 语义对齐

## 7. 风险

| # | 风险 | 缓解 |
|---|---|---|
| 1 | 死路径证明有漏（某形态 Variable{ty} 为 None 但 resolved 有值） | C3 计数器语料取证 + 单测静态形态覆盖；三旗标兜底 |
| 2 | 走链与 eager 值偏差（环/截断语义） | 不变量单测 + 三旗标（resolve-stats 对链错误极敏感——成员解析断裂会拉低命中率） |
| 3 | M3 需要类型身份系统 | D39 翻案记录；git 历史可考；M3 按当期需求重设计优于维护平行世界 |
| 4 | TypeId 删除遗漏引用 | 编译器兜底（`cargo check --workspace` 收敛清单） |

## 8. 后续（不在本批）

- Phase D：诊断调度队列（mylua 式），依赖本期成本模型
- Phase E：scope_tree 消费（B6 延后）、reindex 声明面 diff 定向失效消费方、
  文档同步（设计稿 v1.4 + 决策记录 D39 于 Phase C 落地时追加）
