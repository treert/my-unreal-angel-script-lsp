# 索引重构 Phase E 设计：scope_tree 消费 + 定向失效退役（收官）

> 版本：v1（2026-09-28，brainstorming 两项裁决收敛）
>
> **定位**：`docs/index-architecture.md`（v1.5）§11 步骤 6 的**最后半边**——
> B6 延后的 scope_tree 消费 + D38 预留「定向失效消费方」的处置。前置
> Phase A/B/C/D（D37-D40）已完成。**本 Phase 落地后索引重构全表收官。**

## 1. 裁决

| # | 裁决 | 理由 |
|---|---|---|
| E1 | **SemCtx 局部源切 scope_tree**：`at_byte` 的局部收集（形参/块/for 三臂 + `collect_block_locals`/`collect_declarators`，~150 行 CST walk）改为查 `summary.scope_tree`；`SemCtx.locals` 接口与形态（`Vec<resolve::LocalDecl>`）零变化——resolve 三查找点 / completion / expr / hover 无感 | 双轨局部收集逻辑归一（单一真值 = 提取期树）；省每查询的块内 children 扫描；树已建好且 Phase A 测试齐 |
| E2 | **ty 急切回填（边入栈边定型）**：树有 ty 直接用；`ty=None` 的局部经 `descendant_for_byte_range(name_span)` 回找节点——Var 走 `variable_declarator`（auto + 初始化式 + 不在自身初始化式内 → `expr::expr_type`），IterVar 走 `for_each_statement`（`expr::for_each_element`）；失败保留声明类型（Auto）——与现状「宁缺毋假」口径逐字一致 | 成本与现状同量级（现状也是建语境期全量定型），单一代码路径；先行局部在定型瞬间可见（`Vec A; auto S = A + B;`）——回填顺序 = 收集顺序 |
| E3 | **新原语 `locals_chain`**：查询点链上全部可见局部，outermost → innermost、scope 内声明序、**不去重**（SemCtx 语义：同名重复入栈，查找端 `.rev()` 决定遮蔽；completion「同名都给」依赖）；`locals_visible` **删除**（生产无消费方——设计时预留给 completion，实际 completion 走 SemCtx） | 顺序语义与现有收集序逐位对齐（形参 → 外块 → 内块、块内声明序）；死代码不保留（C7 同族） |
| E4 | **定向失效退役**（D41）：D38 预留「reindex 声明面 diff 的定向失效消费方」时的服务对象（`use_cache` 跨文件解析缓存）已被 Phase B 删除、L3 Phase C 零缓存、Phase D 已用 surface 变化做级联触发（非结构编辑只诊断热文件 = 粗粒度定向天然达成）。正式退役，**触发条件**：语料到万级文件、全量 drain 实测超阈值时再评估「全量→受影响集」收窄 | 「全量→受影响集」需要反向闭包计算，复杂度与全量重算同量级；441 语料全量 drain 是 ms 级，零收益 |

## 2. 终态结构

```
SemCtx::at_byte(ws, file, src, byte, node)
  ├─ ns/type/fn 链：保持现有 parent 上溯（与局部无关，不动）
  └─ locals：
       for l in ws.files[file].summary.scope_tree.locals_chain(byte):
           ty = l.ty.clone().or_else(|| 回填：回找节点 + expr 定型)
           ctx.locals.push(LocalDecl { name, kind: 映射(l.kind), name_span, ty })
       // kind 映射：Param→Param；Var/IterVar→LocalVar（现状口径）
       // full_span：局部无生产消费方——回填 name_span 占位（见 §4 风险 3）
```

## 3. 删除清单

- `resolve.rs`：`collect_block_locals` / `collect_declarators`；`at_byte` 的
  `"block"` / `"for_statement"` / `"for_each_statement"` 收集臂（形参数据改来自树）；
  函数声明臂只留 `fn_def` 判定；
- `scope.rs`：`locals_visible`（+其单测），由 `locals_chain` 接棒。

## 4. 风险

| # | 风险 | 缓解 |
|---|---|---|
| 1 | 树与 CST walk 语义有细微差（classic for / 形参帧 / 可见性口径） | Phase A 建树即按 SemCtx 口径（scope.rs 模块头明载）；等价用例钉死 + 176 测试 + 三旗标兜底（resolve-stats 对局部解析断裂极敏感） |
| 2 | 回填顺序错误致 auto 定型看不到先行声明 | `locals_chain` 顺序 = 现有收集序逐位对齐（外→内、声明序）；等价单测 `auto S = A + B` |
| 3 | `full_span` 疑似无消费方 | 实现期 grep 证实；确无消费则改占位（name_span），有消费则按声明 span 回找填真值 |
| 4 | 树对 ERROR 区（中途编辑）的覆盖差于 CST walk | 树同样从 CST 提取（同一语法），错误恢复形态一致；smoke 的修复-再查询段覆盖 |

## 5. 测试与验收

- **新增**：`locals_chain` 顺序/可见性/不去重；SemCtx 树切换等价用例（遮蔽 / 形参 /
  for-range / classic for / auto 跨文件回填 `auto X = Func()` / 迭代变量元素类型 /
  初始化式内自引用跳过）；
- **基线**：`cargo test --workspace` 176 + 新增全绿；三旗标语料验收不回归
  （resolve-stats 94.9% / ref-stats 25505 / decls 101322）；smoke + e2e-m4 复跑；
- **文档**：D41、设计稿 v1.6（§11 全表 ✅ + 变更记录）、D38「Phase E 接消费方」
  表述修正、future-work 对账。

## 6. 后续（不在本批）

- future-work 语义近似（模板实例化 / mixin shadow / rename 保护）、P5 诊断规则、
  VSCode 体感验收——独立批次；
- 万级语料时的诊断级联收窄（E4 触发条件）。
