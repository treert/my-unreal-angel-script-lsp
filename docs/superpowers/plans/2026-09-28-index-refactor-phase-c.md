# 索引重构 Phase C 实施计划（eager 派生表删除 + 继承走链现算）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 spec `2026-09-28-index-refactor-phase-c-design.md`（v3）：删除 `Workspace::build` 尾段的 eager 派生表（继承闭包、resolved 声明类型预解析、TypeTable），继承链改查询期走链现算（零缓存），TypeId 体系整体退役。行为零变化。

**Architecture:** 纯删除 + 走链，**不引入任何新并发机制**（无 Mutex/无缓存容器——删完死代码后全库查询路径纯只读）。Task 2 先建走链接口并用「新旧对照等价测试」钉死，Task 3/4 分别删两块 eager，Task 5 语料验收 + 文档同步（设计稿 v1.4 + 决策记录 D39）。

**Tech Stack:** Rust（cargo workspace：as-core / as-lsp / as-cli）、tree-sitter 0.25、rayon。

## Global Constraints

- 单测 `.as` 用例**一律内置于源码字符串字面量**，禁止读 `tests/` 等外部文件（AGENTS.md 硬性规则 / D1）
- `intern_file` 路径一律加 `unique://` 前缀（并行测试防串）
- 每 Task 结束 `cargo test --workspace` 全绿；本计划基线 **162 个**（退休须有继任用例或机制消失佐证，语义断言不得静默删除）
- 提交信息中文、首行 ≤ 72 字符；master 串行，一 Task 一 commit
- as-lsp crate 预期**零改动**（已核实 TypeId/TypeTable/closures 零引用——Task 5 用 `git diff` 证实）
- 语料路径 `d:\DTmp\test-my-as-lsp\AS-Cache`（414 `.d.as`，与 Phase A/B 验收同源）

## 关键裁决（引自 spec，C1-C7）

| # | 裁决 |
|---|---|
| C1 | 范围不含 Phase D/E |
| C2 | 无缓存容器：DerivedCaches / Mutex / interior mutability 整体不引入 |
| C3 | 三旗标不回归即验收；expr 死回落先计数器取证（预期 0 命中，数字进提交信息） |
| C4 | `cycle_classes` 删字段，留 `cyclic_classes()` 函数 |
| C5 | 命名改祖先链语义：`base_class` / `ancestor_chain`（原 `closures` 是图论闭包命名，与 AS 无函数闭包易混淆） |
| C6 | 继承零缓存：单跳 = `base_class`（现 `resolve_base_class` 更名）；整链 = `ancestor_chain` 走链现算 |
| C7 | TypeId 体系退役：TypeTable / TypeKind / TypeId / `resolve_syn` / `resolved` / `render_type` / `named_base` / expr.rs 死回落全删；`types.rs` 只留 `SynType` / `RefKind`；D17/G7 随 D39 记翻案 |

## 机械替换规则（Task 3 全员遵守，写完自查）

| 旧 | 新 |
|---|---|
| `ws.closures.get(&t).and_then(\|c\| c.first().copied())` | `ws.base_class(&t)` |
| `let mut space = vec![def]; if let Some(chain) = ws.closures.get(&def) { space.extend(chain.iter().copied()); }` | `let mut space = vec![def]; space.extend(ws.ancestor_chain(&def));` |
| `let mut chain = vec![recv]; if let Some(cl) = ws.closures.get(&recv) { chain.extend(cl.iter().copied()); }` | `let mut chain = vec![recv]; chain.extend(ws.ancestor_chain(&recv));` |
| `ws.resolve_base_class(&r)` | `ws.base_class(&r)` |
| `ws.resolved.get(&def)` / `ws.resolved[&x]` | （随 C7 删除，无继任调用——见 Task 4） |

---

### Task 1: 基线取证（无代码提交）

**Files:**
- 临时改 `lsp/crates/as-core/src/expr.rs`（:571-575 死回落处加计数器，取证后**立即还原**，不提交）

**目的：** C3 的证据链——①三旗标 + 派生统计行的 Phase B 基线数字（写进 Task 3/4/5 提交信息做对照）；②expr 死回落 0 命中的语料取证。

- [ ] **Step 1: 跑 B 基线并记录全部输出行**

```powershell
cargo build --release -p as-cli
.\target\release\as-cli.exe dump-index d:\DTmp\test-my-as-lsp\AS-Cache --new-arch --resolve-stats --ref-stats
```

把以下各行数字抄录到工作笔记（后续提交信息引用）：
- `inheritance: class closures {N}, cycles {M}, ...`（Task 3 对照：classes 数 = N、cycles = M）
- `types: interned {X}, variable decl types resolved {Y}/{Z}`（Task 4 退休佐证）
- `---- new-arch ...` / `resolve-stats ... hit ... %` / `ref-stats ... hits`（三旗标基线：decls 101134 / 94.9% / 25505）

- [ ] **Step 2: 死回落计数器取证**——expr.rs `def_expr_ty` 的 None 分支临时加：

```rust
                None => {
                    eprintln!("DEAD-FALLBACK-HIT: {}", sym_str(ws.decl(&def).name));
                    let &t = ws.resolved.get(&def)?;
                    Some(ExprTy { base: named_base(ws, t)?, syn: None })
                }
```

重跑 Step 1 命令 → **预期 stderr 零输出**（死回落 0 命中）。若出现任何命中：**停止实施**，回报用户（C7 的前提被推翻）。

- [ ] **Step 3: 还原计数器**，`git diff` 确认 expr.rs 无改动。

---

### Task 2: 走链查询接口（TDD + 新旧对照等价测试）

**Files:**
- Modify: `lsp/crates/as-core/src/workspace.rs`
- Modify: `lsp/crates/as-cli/src/main.rs`（`resolve_base_class` → `base_class` 更名的唯一外部调用点 :416）

**Interfaces（Task 3/4 依赖，签名一字不差）:**
```rust
impl Workspace {
    /// 单跳：class → 直接基类（simple 基名 → Class 声明；struct/未知名 = None）
    pub fn base_class(&self, class: &DeclRef) -> Option<DeclRef>;
    /// 祖先链（近者在前，不含 self）；空链 = 无基类/unresolved/struct（D16）
    pub fn ancestor_chain(&self, class: &DeclRef) -> Vec<DeclRef>;
    /// 全扫环检测（诊断素材 / as-cli 统计）
    pub fn cyclic_classes(&self) -> Vec<DeclRef>;
}
```

- [ ] **Step 1: 写失败单测**（workspace.rs `mod tests` 追加）

```rust
#[test]
fn ancestor_chain_matches_eager_closures() {
    // 过渡等价性（C6）：对全部 class 断言 ancestor_chain == closures 表值。
    // Task 3 删 closures 字段后本用例随之删除（语义用例接棒）
    const SRC: &str = "\
class ABase {}
class AMid : ABase {}
class ALeaf : AMid {}
class Bad1 : Bad2 {}
class Bad2 : Bad1 {}
class Orphan : TMissing {}
struct S : ABase {}
";
    let ws = ws_build(&[("unique://wseq/mix.as", SRC)]);
    let mut checked = 0;
    for (&file, e) in &ws.files {
        for (i, d) in e.summary.decls.iter().enumerate() {
            if d.kind != DefKind::Class || d.flags.contains(DefFlags::SYNTHETIC) {
                continue;
            }
            let c = DeclRef { file, local: i as u32 };
            assert_eq!(ws.ancestor_chain(&c), ws.closures[&c], "链等价: {}", sym_str(d.name));
            checked += 1;
        }
    }
    assert!(checked >= 6);
}

#[test]
fn cyclic_classes_matches_old() {
    // 过渡等价性：与 eager cycle_classes 对照（Task 3 随字段删除而删除）
    const SRC: &str = "class A : B {}\nclass B : A {}\nclass C : A {}\n";
    let ws = ws_build(&[("unique://wcyc/m.as", SRC)]);
    let mut old = ws.cycle_classes.clone();
    let mut new = ws.cyclic_classes();
    old.sort();
    new.sort();
    assert_eq!(new, old);
    assert_eq!(new.len(), 2);
}
```

- [ ] **Step 2: `cargo test -p as-core ancestor_chain cyclic` → 编译失败**（方法不存在）
- [ ] **Step 3: 实现**——workspace.rs：

  ① 常量更名：`MAX_CLOSURE_DEPTH` → `MAX_CHAIN_DEPTH`（值不变 256，注释改「走链深度上限（防环与病态深度）」）。
  ② `resolve_base_class` 更名 `base_class`（`&mut` 无涉、逻辑逐行不动），方法注释改「单跳：class → 直接基类（simple 基名；struct/未知名 = None）」；`build_closures` 内调用点跟随更名。
  ③ 新增两方法：

```rust
    /// 祖先链（近者在前，不含 self）：逐级 base_class 走链，visited 环截断 +
    /// 深度上限。空链 = 无基类 / unresolved base / struct（D16：struct 不走链）。
    /// C6：零缓存——链由 bases 名 + agg 一跳完全推导，reindex 后立即反映。
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

    /// 全扫环检测（诊断素材 / as-cli 统计；C4——旧 cycle_classes 字段的
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
```

  ④ as-cli `main.rs:416`：`ws.resolve_base_class(&r)` → `ws.base_class(&r)`。

- [ ] **Step 4: `cargo test --workspace` 全绿**（162 + 2 新增；旧 closures 测试原样通过——eager 表未动）
- [ ] **Step 5: Commit** — `git add -A` → `索引重构 C2：走链查询接口 base_class/ancestor_chain/cyclic_classes（零缓存，新旧对照等价钉死）`

---

### Task 3: 消费点切换 + eager 继承表删除

**Files:**
- Modify: `lsp/crates/as-core/src/resolve.rs`（5 处）
- Modify: `lsp/crates/as-core/src/completion.rs`（2 处）
- Modify: `lsp/crates/as-core/src/workspace.rs`（删字段/函数/调用 + 测试改写）
- Modify: `lsp/crates/as-cli/src/main.rs`（inheritance 统计行走链现算）

- [ ] **Step 1: resolve.rs 五处替换**（按机械替换表；逐处 before → after）

  ① :429-432（`Super::` 首段）：
```rust
// before
                return ctx
                    .type_def
                    .and_then(|t| ws.closures.get(&t).and_then(|c| c.first().copied()))
                    .map(|b| Resolution { targets: vec![Target::Def(b)], level: LEVEL_THIS_SUPER });
// after
                return ctx
                    .type_def
                    .and_then(|t| ws.base_class(&t))
                    .map(|b| Resolution { targets: vec![Target::Def(b)], level: LEVEL_THIS_SUPER });
```

  ② :663-665（resolve_scoped_last 的 Super）：
```rust
// before
        let base = ctx
            .type_def
            .and_then(|t| ws.closures.get(&t).and_then(|c| c.first().copied()))?;
// after
        let base = ctx.type_def.and_then(|t| ws.base_class(&t))?;
```

  ③ :791-793（resolve_plain 的 super）——同 ② 形态替换。

  ④ :938-944（member_search_space）：
```rust
// before
        DefKind::Class => {
            let mut space = vec![def];
            if let Some(chain) = ws.closures.get(&def) {
                space.extend(chain.iter().copied());
            }
            space
        }
// after
        DefKind::Class => {
            let mut space = vec![def];
            space.extend(ws.ancestor_chain(&def));
            space
        }
```

  ⑤ :1000-1003（mixin_candidates）：
```rust
// before
    let mut chain = vec![recv];
    if let Some(cl) = ws.closures.get(&recv) {
        chain.extend(cl.iter().copied());
    }
// after
    let mut chain = vec![recv];
    chain.extend(ws.ancestor_chain(&recv));
```

- [ ] **Step 2: completion.rs 两处替换**——① `extend_mixin_candidates` :439-442（同 ⑤ 形态）；② `scoped_candidates` :476-478（同 ① 的 `.and_then(|t| ws.base_class(&t))` 形态）

- [ ] **Step 3: workspace.rs 删除 eager 继承**

  - 删字段 `closures` / `cycle_classes`（struct 定义 + `build()` 初始化两行）
  - 删函数 `build_closures` 整体
  - `build()`：删 `ws.build_closures();` 一行；`resolve_decl_types` 调用**保留**（Task 4 删）；as_log 行改 `"workspace: built {} files | parse+summary {:?} | aggregation {:?}"`（derived 段消失）
  - `reindex_file_full`：两个分支各删 `self.closures.clear(); self.cycle_classes.clear(); ... self.build_closures();`（`resolved.retain` 与 `resolve_decl_types_in` 保留到 Task 4）
  - `remove_file`：删 `self.closures.clear(); self.cycle_classes.clear(); self.build_closures();` 三行
  - 模块头注释 :8-9 的「eager 派生表」描述改为「继承走链现算（C6，零缓存）；resolved/types eager 待 Task 4 删除」

- [ ] **Step 4: 测试改写**（语义断言全部保留，仅接口跟随）

  删除 Task 2 的两条过渡等价用例（`ancestor_chain_matches_eager_closures` / `cyclic_classes_matches_old`——字段已删，使命完成，语义由下列用例接棒）。

  `struct_writes_base_but_no_closure` → `struct_writes_base_but_no_chain`：
```rust
    #[test]
    fn struct_writes_base_but_no_chain() {
        // D16：struct 即使写了基类也不走链
        const SRC: &str = "struct S : T {}\nstruct T { int X; }\nclass K : J {}\nclass J {}\n";
        let ws = ws_build(&[("unique://wsnc/mix.as", SRC)]);
        let s = ws.lookup_type_def(intern_sym("S")).unwrap();
        assert!(ws.ancestor_chain(&s).is_empty());
        let k = ws.lookup_type_def(intern_sym("K")).unwrap();
        assert_eq!(ws.ancestor_chain(&k).len(), 1, "class 正常走链");
    }
```

  `closures_and_cycles_match_old_semantics` → `chain_and_cycles_semantics`（断言逐条平移，`ws.closures.get` → `ws.ancestor_chain`、`ws.cycle_classes` → `ws.cyclic_classes()`）：
```rust
    #[test]
    fn chain_and_cycles_semantics() {
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
        let bad1 = ws.lookup_type_def(intern_sym("Bad1")).unwrap();
        let bad2 = ws.lookup_type_def(intern_sym("Bad2")).unwrap();
        let cycles = ws.cyclic_classes();
        assert!(cycles.contains(&bad1) && cycles.contains(&bad2));
        assert_eq!(ws.ancestor_chain(&bad1).len(), 1, "链在首次重复处截断");
        let orphan = ws.lookup_type_def(intern_sym("Orphan")).unwrap();
        assert!(ws.ancestor_chain(&orphan).is_empty(), "unresolved base 不 panic");
    }
```

  `cycle_does_not_affect_side_branch` 同法平移（`ws.cycle_classes.len() == 2` → `ws.cyclic_classes().len() == 2`；`ws.closures[&c]` → `ws.ancestor_chain(&c)`）。

  新增「reindex 后走链立即反映」（C6 的正确性收益断言）：
```rust
    #[test]
    fn reindex_reflects_base_change_immediately() {
        // C6：无缓存 ⇒ reindex 后链立即按新 agg 现算，无清空重建中间态
        const V1: &str = "class Mid : Top1 {}\nclass Top1 {}\nclass Top2 {}\n";
        const V2: &str = "class Mid : Top2 {}\nclass Top1 {}\nclass Top2 {}\n";
        let path = "unique://wsrb/m.as";
        let mut ws = ws_build(&[(path, V1)]);
        let mid = ws.lookup_type_def(intern_sym("Mid")).unwrap();
        let names = |ws: &Workspace, c: &DeclRef| -> Vec<String> {
            ws.ancestor_chain(c).iter().map(|r| sym_str(ws.decl(r).name).to_string()).collect()
        };
        assert_eq!(names(&ws, &mid), vec!["Top1".to_string()]);
        let f = intern_file(path, 0);
        let _ = ws.reindex_file(f, FileKind::Script, V2.to_string());
        let mid2 = ws.lookup_type_def(intern_sym("Mid")).unwrap();
        assert_eq!(names(&ws, &mid2), vec!["Top2".to_string()]);
    }
```

- [ ] **Step 5: as-cli inheritance 统计行**——`dump-index` 的 :408-426 循环扩为走链现算（unresolved_bases 循环保留原样，classes_with_base 判定不动）：

```rust
    let mut classes_total = 0usize;
    let mut chain_links = 0usize;
    for (&file, entry) in ws.files.iter() {
        for (i, d) in entry.summary.decls.iter().enumerate() {
            if d.kind != DefKind::Class || d.flags.contains(DefFlags::SYNTHETIC) {
                continue;
            }
            classes_total += 1;
            chain_links += ws.ancestor_chain(&DeclRef { file, local: i as u32 }).len();
        }
    }
    let cycles = ws.cyclic_classes().len();
    println!(
        "inheritance: classes {classes_total}, chain links {chain_links}, cycles {cycles}, classes-with-base {classes_with_base} (unresolved base {unresolved_bases})"
    );
```

（与 Task 1 基线对照：`classes_total` == 旧 `class closures {N}`、`cycles` == 旧 `cycles {M}`——空链也计数的口径对齐。）

- [ ] **Step 6: `cargo test --workspace` 全绿 + 语料单项验收**：`cargo build --release -p as-cli && .\target\release\as-cli.exe dump-index d:\DTmp\test-my-as-lsp\AS-Cache --resolve-stats`——resolve-stats ≥ 94.9%（链错误会拉低命中率，这是走链等价的端到端信号）
- [ ] **Step 7: Commit** — `索引重构 C3：继承链改查询期走链现算（删 closures/cycle_classes，消费点七处切换）`

---

### Task 4: TypeId 体系退役

**Files:**
- Modify: `lsp/crates/as-core/src/expr.rs`（死回落删除 + `named_base` 删除 + 新测试 2 条）
- Modify: `lsp/crates/as-core/src/workspace.rs`（删 resolved/types + 三个函数 + 测试修剪）
- Modify: `lsp/crates/as-core/src/types.rs`（删 TypeKind/TypeTable，只留 SynType/RefKind）
- Modify: `lsp/crates/as-core/src/id.rs`（删 TypeId）
- Modify: `lsp/crates/as-core/src/lib.rs`（re-export 修剪）
- Modify: `lsp/crates/as-cli/src/main.rs`（删 types 统计行）

- [ ] **Step 1: expr.rs 死回落删除 + 继任测试**（先写测试再删——本步测试与删除同 commit）

  ① `def_expr_ty` 的 match 臂（:568-577）：
```rust
// before
        DefKind::Field | DefKind::GlobalVar | DefKind::VirtualProperty | DefKind::AssetDecl => {
            match def_decl_syn(ws, def) {
                Some(syn) => Some(ExprTy { base: syn_type_base(ws, &syn)?, syn: Some(syn) }),
                None => {
                    // 声明类型缺失时回落归一化表（resolved）
                    let &t = ws.resolved.get(&def)?;
                    Some(ExprTy { base: named_base(ws, t)?, syn: None })
                }
            }
        }
// after（C7：回落经 Task 1 取证 0 命中，死路径删除）
        DefKind::Field | DefKind::GlobalVar | DefKind::VirtualProperty | DefKind::AssetDecl => {
            let syn = def_decl_syn(ws, def)?;
            Some(ExprTy { base: syn_type_base(ws, &syn)?, syn: Some(syn) })
        }
```

  ② 删 `named_base` 函数（:692-701 附近）；imports：`use crate::id::{Sym, TypeId};` → `use crate::id::Sym;`，`use crate::types::{SynType, TypeKind};` → `use crate::types::SynType;`。

  ③ `mod tests` 追加两条继任测试（原 workspace.rs 两条 resolved 断言用例的活跃路径继任）：
```rust
    #[test]
    fn syn_type_base_float_normalizes_by_config() {
        // 原 workspace.rs float_dual_config_normalization 的继任（C7）：裸 float
        // 归一化的活跃消费点自本期起唯一存在于 syn_type_base
        let mk = |f64cfg: bool| {
            Workspace::build(
                IndexConfig { float_is_float64: f64cfg },
                vec![FileInput {
                    file: intern_file("unique://exprflt/f.as", 0),
                    kind: FileKind::Script,
                    source: String::new(),
                    module: None,
                }],
            )
        };
        let syn = SynType::Primitive(intern_sym("float"), TextRange::new(0, 0));
        for (cfg, expect) in [(true, "float64"), (false, "float32")] {
            let ws = mk(cfg);
            let base = syn_type_base(&ws, &syn).expect("内建 float 必命中");
            assert_eq!(sym_str(ws.decl(&base).name), expect);
        }
    }

    #[test]
    fn syn_type_base_template_targets_container() {
        // 原 workspace.rs template_field_type_resolution 的继任（C7）：模板
        // 使用位的成员查找落点 = 模板本体；实参替换（template_map/subst_syn）
        // 由 expr.rs 既有模板用例覆盖
        const SRC: &str = "struct TArray<T> { }\nstruct FVector { }\n";
        let ws = build(&[("unique://exprtpl/t.as", SRC)]);
        let syn = SynType::Template {
            name: intern_sym("TArray"),
            name_span: TextRange::new(0, 0),
            args: vec![SynType::Named(intern_sym("FVector"), TextRange::new(0, 0))],
        };
        let base = syn_type_base(&ws, &syn).unwrap();
        assert_eq!(sym_str(ws.decl(&base).name), "TArray");
    }
```

（测试 mod 需按既有 use 补齐：`FileInput` / `FileKind` / `Workspace` / `TextRange`——跟随 mod tests 既有 import 风格。）

- [ ] **Step 2: workspace.rs 删除 resolved/types 体系**

  - 删字段 `resolved` / `types` + `build()` 初始化两行 + `ws.resolve_decl_types();` 调用
  - 删函数：`resolve_decl_types` / `resolve_decl_types_in` / `resolve_syn` / `render_type`
  - `reindex_file_full` 两个分支：删 `self.resolved.retain(...)` 与 `self.resolve_decl_types_in(...)`；`remove_file`：删 `self.resolved.retain(...)`
  - imports：删 `crate::id::TypeId`、`crate::types::{... TypeKind, TypeTable}`（保留 `RefKind` / `SynType`——synthetic_members 仍用）；模块头注释同步（resolved/types 段删除，补 C7 说明一行）
  - 测试修剪：
    - `float_dual_config_normalization` **删除**（继任 = expr 新测试①）
    - `template_field_type_resolution` **删除**（继任 = expr 新测试②）
    - `builtin_primitives_synthetic` 修剪为：
```rust
    #[test]
    fn builtin_primitives_synthetic() {
        // 原 index.rs builtin_primitives_synthetic：15 内建 SYNTHETIC、不进
        // 真实文件统计（resolved 断言随 C7 退役——变量定型由 expr 的
        // syn_type_base 用例覆盖）
        const SRC: &str = "int A;\nvoid F(bool B) {}\n";
        let ws = ws_build(&[("unique://wsb2/b.as", SRC)]);
        let builtin = intern_file(BUILTIN_FILE_PATH, u32::MAX);
        let b = &ws.files[&builtin];
        assert_eq!(b.summary.decls.len(), BUILTIN_PRIMITIVES.len());
        assert!(b.summary.decls.iter().all(|d| d.flags.contains(DefFlags::SYNTHETIC)));
    }
```
    - `decl_counts_and_resolved_types` 修剪为 `decl_counts`：保留 9 条 decl 计数断言，删尾部 Health/Counter 的 resolved 断言两段（归一化语义由 expr 继任测试覆盖）

- [ ] **Step 3: types.rs / id.rs / lib.rs**

  - `types.rs`：删 `TypeKind` / `TypeTable` 及其全部实现与 `mod tests` 3 条用例（机制消失退休）；保留 `RefKind` / `SynType`；模块头注释改写为「语法层类型（CST 解析产物，名字是 Sym）——TypeId 归一化体系（D17/G7）随 Phase C/D39 退役，定型走 DeclRef + SynType 双件套（expr）」
  - `id.rs`：删 `define_u32_id!(TypeId ...)` 块 + 文档表里的 `TypeId` 行 + 头注释补一句「Phase C（D39）：TypeId 随 TypeTable 退役」
  - `lib.rs`：`pub use id::{FileId, Sym, TypeId};` → `pub use id::{FileId, Sym};`；`pub use types::{RefKind, SynType, TypeKind, TypeTable};` → `pub use types::{RefKind, SynType};`

- [ ] **Step 4: as-cli**——删 `types: interned ...` 统计行（:432-437 的 println + `type_jobs` 计算 :391-407 整段）；`build: ...` 行保留
- [ ] **Step 5: `cargo test --workspace` 全绿**（`cargo check --workspace` 错误清单收敛后再跑测试——TypeId 删除的漏网引用由编译器兜底）
- [ ] **Step 6: Commit** — `索引重构 C4：TypeId 体系退役（死回落 0 命中取证，types.rs 只留 SynType/RefKind，D17/G7 翻案见 D39）`

---

### Task 5: 收尾（三旗标验收 + 设计稿 v1.4 + D39 + 死代码清扫）

**Files:**
- Modify: `docs/index-architecture.md`（v1.4）
- Modify: `docs/实现决策记录.md`（追加 D39）
- Modify: `docs/README.md`（状态表）

- [ ] **Step 1: 语料三旗标终验**：
```powershell
cargo build --release -p as-cli
.\target\release\as-cli.exe dump-index d:\DTmp\test-my-as-lsp\AS-Cache --new-arch --resolve-stats --ref-stats
```
对照 Task 1 基线：decls 101134 一致、resolve-stats ≥ 94.9%、ref-stats 25505 逐位一致；inheritance 行 `classes` == 旧 `class closures` 数、`cycles` 一致。任一不符 → 停下分析。

- [ ] **Step 2: 残留扫描**：`git grep -n "closures\|cycle_classes\|resolve_syn\|resolve_decl_types\|TypeTable\|TypeKind\|TypeId\|render_type\|named_base"`——允许命中：文档（本任务 Step 3 会更新）、`resolved_param0` 等测试助手（无关同名）、`ancestor` 相关新名。代码零残留。

- [ ] **Step 3: 文档同步**
  - `index-architecture.md` v1.4：§8 生死清单三行 ⏳ → ✅（closures/cycle_classes → 走链现算零缓存 C6；resolve_decl_types → 随死路径删除 C7；TypeTable → 退役 C7，D17/G7 翻案）；§5.2/§5.4 按「零缓存（原方案的链缓存被 C6 否决）」改写；§11 步骤 4 映射到 Phase C 实际执行（Task 1-4）；变更记录加 v1.4 行（含 Task 1 取证数字、测试增减账目）
  - `docs/实现决策记录.md` 追加 **D39**：Phase C 落地裁决（C1-C7 全文 + 死回落 0 命中取证数字 + D17/G7 翻案 + 测试基线变化 + 三旗标验收数字）；文末版本表加行
  - `docs/README.md` 状态表：索引层重构行更新为「Phase A/B/C 完成」并概述 C 的结论

- [ ] **Step 4: `cargo test --workspace` 全绿 + `git diff --stat lsp/crates/as-lsp` 空输出确认**（C：as-lsp 零改动）
- [ ] **Step 5: Commit** — `索引重构 C5：三旗标验收 + 设计稿 v1.4 + D39（Phase C 收官，按需定型名实相符）`

---

## Phase C 验收标准（整体）

1. `cargo test --workspace` 全绿（162 基线：+Task 2/3/4 净增改写，退休用例均有继任或机制消失佐证——账目记入 D39）
2. 语料三旗标：decls 101134 / resolve-stats ≥ 94.9% / ref-stats 25505 逐位一致；inheritance 统计与 B 基线口径对齐
3. expr 死回落语料取证 0 命中（Task 1，数字进提交信息）
4. `git grep` 死代码零残留（Task 5 Step 2 口径）
5. as-lsp crate 零改动
6. 文档同步：设计稿 v1.4 + 决策记录 D39 + docs/README

## Phase D/E 展望（不在本批，开工前另写计划）

- **D（诊断调度）**：mylua 式优先级队列 + 300ms 防抖（`cyclic_classes()` 等 / D39 退役的环检测素材在此成为诊断候选）
- **E（scope_tree 消费 + 增量失效）**：B6 延后的局部变量路径切换；reindex 声明面 diff 返回值的消费方（定向失效——本期后缓存已不存在，其形态待 E 重新定义）

## Self-Review

- **Spec 覆盖**：C1（范围）= 计划头 + Phase D/E 展望；C2（无容器）= 全计划无 Mutex；C3（取证+三旗标）= Task 1/3/5；C4 = Task 2③/Task 5 统计；C5 = Task 2 更名；C6 = Task 2/3；C7 = Task 4。✅
- **占位符扫描**：无 TBD/TODO；「同 ② 形态替换」「同法平移」均给出了被参照的完整代码块（② 本体）。⚠️ 可接受
- **类型一致性**：`base_class`/`ancestor_chain`/`cyclic_classes` 签名在 Task 2 定义、Task 3 消费一致；`syn_type_base` 测试用例引用 expr.rs 既有 `build` 测试助手（:905 已见同款用法）✅
- **风险最大点**：Task 4 的 TypeId 删除波及 id/types/lib 三处公共导出——编译器兜底（`cargo check` 收敛），as-lsp 零引用已预先核实；Task 3 的七处消费点切换靠 resolve-stats 端到端信号兜底
