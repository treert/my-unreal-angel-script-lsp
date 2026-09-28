# 索引重构 Phase C 设计：eager 派生表按需化

> 日期：2026-09-28
>
> **定位**：`docs/index-architecture.md`（v1.3）§11 步骤 4 / §8 生死清单最后三个 ⏳ 项
> 的实施设计。Phase A（D37，summary + 聚合层）、Phase B（D38，DeclRef 切换 +
> references 查询期化）已完成；Phase C 收掉 B5 裁决延后的三块 eager 预计算。

## 1. 背景与目的

Phase B 收官时 `Workspace::build` 尾段仍 eager 预计算三块派生数据：

| eager 残留 | 内容 | 规模 | 实际查询命中率 |
|---|---|---|---|
| `closures` | 全部 class 的继承祖先链 | 414 `.d.as` 全部 class | 个位数百分比（hover/补全/定义碰到的类） |
| `resolve_decl_types` → `resolved` | 全部字段/全局变量声明类型归一化 | ~3.2 万条 | 同上 |
| `TypeTable` 填充 | 上述解析的 intern 副产物 | 随 resolved | 同上 |

即：冷启动、配置变更全量重建、单文件 reindex 都为 **100% 声明付 100% 派生计算
成本，而实际查询只碰少数**。Phase C 改为「查询时现算 + 进程内缓存」（设计稿
§5.2/§5.4 原案）——**把索引期付账改成查询期按账付**，语义零变化，只是计算
时机后移 + 缓存。这是新架构三层（per-file summary + 薄聚合 + **按需定型**）
的收口，为 Phase D（诊断调度，其成本模型依赖按需查询）铺路。

收益指标：rebuild 日志中 derived 串行段耗时 → 0（B 基线数字写入提交信息）。

## 2. 裁决摘要（本设计定案）

| # | 裁决 | 理由 |
|---|---|---|
| C1 | **范围 = 仅按需化**：Phase D（诊断调度）、Phase E（scope_tree 消费 + reindex 声明面 diff 定向失效）不并入 | C 已含并发模型改造 + as-cli 对账适配；D 依赖 C 之后的成本模型；E 是又一轮 L3 机械替换。三者独立可验收，混批原子性差 |
| C2 | **单容器 `Mutex<DerivedCaches>`**（ancestor_chains + resolved + TypeTable 同容器），查询方法全 `&self`，锁外计算 + 短锁查插 | tower-lsp 并发读锁下 RefCell 有 double-borrow panic 风险（排除）；`&mut` 传染要改 L3 十文件签名（排除，且与「旧快照可服务」哲学冲突）；单容器与 §5.4「全部缓存进一个容器、失效规则统一」原文对齐 |
| C3 | **as-cli 对账 = prime 后逐位对账**：dump-index 统计前遍历全部 class 调 `ancestor_chain`、全部变量调 `resolved_type`，再报数 | 等价性钉到与 Phase B 基线逐位一致（closures/cycles/types/resolved 四计数）；prime 成本只花在 dump 命令，LSP 冷启动路径不含（这正是 Phase C 的收益）；三旗标（resolve-stats 94.9% / ref-stats 25505 / --new-arch 101134）照常不回归 |
| C4 | **`cycle_classes` 字段删除，留按需 `cyclic_classes()` 函数** | 唯一运行时消费方是 as-cli 统计；§5.2 环的存在改为查询结果的一部分，函数形态供 as-cli + Phase D 诊断素材消费 |
| C5 | **命名修正：`closures` → 祖先链语义**（`ancestor_chain` / `ancestor_chains`） | 旧名是图论「传递闭包」的运算命名，与 AS 无函数闭包（语言前提 §1.1）易混淆；实际含义是**查询并缓存类继承体系**——AS class 单继承（链非树），逐级 `resolve_base_class` 走到头 |

## 3. 架构

### 3.1 数据结构

```rust
pub struct Workspace {
    pub config: IndexConfig,
    pub files: HashMap<FileId, FileEntry>,
    pub agg: Aggregation,
    /// 按需派生缓存（Phase C / C2）：锁外计算、短锁查插，全清失效
    derived: Mutex<DerivedCaches>,          // 私有，经查询方法访问
}

struct DerivedCaches {
    /// class → 祖先链（近者在前；空链也缓存——prime 计数与 B 基线同口径）
    ancestor_chains: HashMap<DeclRef, Vec<DeclRef>>,
    resolved: HashMap<DeclRef, TypeId>,
    types: TypeTable,
}
```

原 `pub closures / cycle_classes / resolved / types` 四字段删除，全部改经查询方法。

### 3.2 查询接口（全 `&self`）

| 方法 | 语义 |
|---|---|
| `ancestor_chain(&self, class: &DeclRef) -> Vec<DeclRef>` | 缓存命中 clone 返回；miss 走链（`resolve_base_class` 逐级 + visited 环截断 + `MAX_CLOSURE_DEPTH`）→ 插入（**空链也插**，含 unresolved base / 无基类 / struct 不走此路——D16） |
| `resolved_type(&self, def: &DeclRef) -> Option<TypeId>` | `Variable{ty:Some}` → `resolve_syn` → 缓存；其余 None |
| `resolve_syn(&self, syn: &SynType) -> Option<TypeId>` | 签名 `&mut self`→`&self`；intern 经 `intern_locked`（短锁）；外部无调用方，签名变化无涟漪 |
| `type_kind(&self, t: TypeId) -> TypeKind` | clone 替代 `ws.types.get(t)` 直读（消费点：`expr.rs` `named_base`、`render_type`、单测） |
| `cyclic_classes(&self) -> Vec<DeclRef>` | 按需全扫环检测（walk 全部 class，visited 检出 path 上重复者；语义对齐旧 `cycle_classes` 的 path[pos..] 去重集合） |
| `derived_stats(&self) -> (usize, usize, usize)` | ancestor_chains / resolved / types 三计数（锁内快照，prime 后对账用） |

### 3.3 锁纪律（无重入 = 无死锁）

- **锁外计算、短锁查/插**：`ancestor_chain` 用 `resolve_base_class`（无锁）；
  `resolve_syn` 用 `lookup`/`decl`/`lookup_type_def`（无锁）+ `intern_locked`
  （短锁）——递归（模板实参/Array/Const/Ref）各层短锁交替，**无嵌套**。
- 每个新查询方法在实现时注释列出锁触点，review 钉死。
- as-lsp 读锁（`with(|ws| &Workspace)`）下并发安全：std `Mutex`，不跨 await 持锁
  的既有约束不变。

### 3.4 失效（§5.4 统一规则）

`build()` 尾段 / `reindex_file` / `reindex_file_full` / `remove_file`：
`derived.lock()` + `clear()`（types 一并清）。TypeId 是 TypeTable 内下标，全清后
旧 id 作废——L3 无持久化 TypeId 的场景（expr 查询内即算即用；`resolved` 缓存
与 types 同容器，自洽）。

`build_closures` / `resolve_decl_types` / `resolve_decl_types_in` 三个函数整体
删除；`build` 的 as_log derived 分段计时随之消失（或改报 0/省略）。

## 4. 消费点改造（机械）

- `resolve.rs` 5 处 + `completion.rs` 2 处：`ws.closures.get(&x)` →
  `ws.ancestor_chain(&x)`（`.first().copied()` / `extend` 等用法不变）
- `expr.rs` 回落（原 :573）：`ws.resolved.get(&def)?` → `ws.resolved_type(&def)?`
  ——**语义原样保留**
- as-cli `dump-index`：统计段前 prime（遍历全部 class 调 `ancestor_chain`、全部
  变量调 `resolved_type`、`cyclic_classes()`），四计数与 B 基线逐位对账；
  `--resolve-stats` / `--ref-stats` **不加 prime**，消费点自动走按需路径（这
  本身就是验证）

## 5. 实施期验证项（非裁决）

- **resolved 回落疑似死路径**：`expr.rs` 的回落只在 `def_decl_syn`（`Variable{ty}`
  / `Callable{return_type}`）为 None 时触发，而 `resolved` 只在 `Variable{ty:Some}`
  时填——两者不相交，推断该回落永不命中。本期保持语义不变（换 `resolved_type`
  调用即可），用 dump-index / debug 手段取证；若证实死路径，记入 Phase E 清理
  候选，**不在 C 删**（行为零变化优先，同 B5 精神）。
- **TypeId 持久化排查**：实施时 grep L3 层确认无跨请求持久化 TypeId 的形态。

## 6. 测试与验收

- workspace.rs 8 个 closures/resolved 相关单测平移到新接口（`ws.closures.get`
  → `ws.ancestor_chain`、`ws.resolved[&x]` → `ws.resolved_type(&x).unwrap()`），
  **语义断言不动**。
- 新增：「reindex 后缓存失效」用例（查询 → 改基类 reindex → 再查询链更新）、
  「模板实参递归 resolve_syn 锁纪律」冒烟（既有用例覆盖）。
- 语料验收：`dump-index`（prime）四计数与 B 基线逐位一致 + 三旗标不回归；
  rebuild 日志 derived 段耗时 → 0（写入提交信息）。
- `cargo test --workspace` 162 全绿。

## 7. 风险

| # | 风险 | 缓解 |
|---|---|---|
| 1 | 锁重入死锁 | §3.3 无重入纪律 + 每方法锁触点注释；模板递归单测 |
| 2 | TypeId 全清后失效 | §5 排查项；缓存同容器自洽 |
| 3 | 走链结果与 eager 值偏差（环/截断语义） | prime 对账钉死（B 基线数字进验收）；环/旁支/unresolved 单测平移 |
| 4 | 并发查询性能（Mutex 争用） | 查询路径锁持有 = 哈希查/插，微秒级；LSP 单客户端场景无感 |

## 8. 后续（不在本批）

- Phase D：诊断调度队列（mylua 式优先级 + 300ms 防抖），依赖本期成本模型
- Phase E：scope_tree 消费（B6 延后）、reindex 声明面 diff 的定向失效、文档
  同步（设计稿 v1.4 + 决策记录 D39 于 Phase C 落地时追加）
