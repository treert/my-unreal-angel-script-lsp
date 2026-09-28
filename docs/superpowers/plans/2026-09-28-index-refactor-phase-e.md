# 索引重构 Phase E 实施计划（scope_tree 消费 + 定向失效退役）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 spec `2026-09-28-index-refactor-phase-e-design.md`（v1）：SemCtx 局部源切 scope_tree（E1-E3），定向失效退役记 D41（E4）。索引重构全表收官。

**Architecture:** 只动 as-core `scope.rs`（新原语 `locals_chain`）与 `resolve.rs`（`at_byte` 重写 + 删收集家族）；ty 急切回填补齐 auto/迭代变量跨文件类型（复用 expr 既有管线）。

**Tech Stack:** Rust（cargo workspace）、tree-sitter 0.25。

## Global Constraints

- 单测 `.as` 用例**一律内置于源码字符串字面量**（AGENTS.md 硬性规则 / D1）
- `intern_file`/`intern_sym` 路径与名字用 `unique://` 前缀（并行测试防串）
- 每 Task 结束 `cargo test --workspace` 全绿；基线 **176**（语义断言不得静默删除）
- 提交信息中文、首行 ≤ 72 字符；一 Task 一 commit
- 语料验收：resolve-stats 94.9%（1028/1083）/ ref-stats 25505 / decls 101322 不回归；smoke + e2e-m4 复跑
- spec 裁决 E1-E4 为唯一依据（`docs/superpowers/specs/2026-09-28-index-refactor-phase-e-design.md`）

## 关键签名（Task 1 内部对齐）

```rust
// scope.rs（新增；locals_visible 删除）
impl ScopeTree {
    pub fn locals_chain(&self, byte: u32) -> Vec<&LocalDecl>;
}
// resolve.rs（SemCtx::at_byte 内部；私有）
fn backfill_local_ty(ws: &Workspace, ctx: &SemCtx, src: &str, root: Node<'_>, d: &LocalDecl) -> Option<SynType>;
```

---

### Task 1: scope_tree 消费（SemCtx 切换 + ty 回填 + 删收集家族）

**Files:**
- Modify: `lsp/crates/as-core/src/scope.rs`（`locals_chain` + 删 `locals_visible` 及其单测）
- Modify: `lsp/crates/as-core/src/resolve.rs`（`at_byte` 重写 + 删 `collect_block_locals`/`collect_declarators` + 新增回填函数 + 等价单测）

- [ ] **Step 1: 写失败单测**
  - `scope.rs`：`locals_chain` 顺序用例（outermost→innermost、scope 内声明序、不去重——同名两个都返回、可见性过滤）。
  - `resolve.rs` 测试模块：树切换等价用例（SemCtx 不可直接构造——经 `resolve_at` 端到端断言）：遮蔽（内层赢）、形参解析、for-range 迭代变量、classic for 初始化声明、`auto X = Func()` 跨文件回填（hover/局部定型层面）、初始化式内自引用跳过。
- [ ] **Step 2: 跑测试确认失败**（`cargo test -p as-core`，预期编译错误/断言失败）
- [ ] **Step 3: 实现**
  - `scope.rs`：`locals_chain`（innermost 沿 parent 收集到根、scope 内正序 + `name_span.start <= byte` 过滤、**不去重**、整体反转 = 外→内）；删 `locals_visible` + 其单测（`resolve_outside_any_scope_is_none` 的 `locals_visible` 半句保留改 `locals_chain`）。
  - `resolve.rs`：
    - `at_byte`：ns/type/fn 三臂不动（`for_each_statement`/`for_statement`/`block` 收集臂删除；函数声明臂删形参收集只留 `fn_def`）；局部改为：
      ```rust
      let mut root = node; while let Some(p) = root.parent() { root = p; }
      for l in ws.files[&file].summary.scope_tree.locals_chain(byte) {
          let ty = l.ty.clone().or_else(|| backfill_local_ty(ws, &ctx, src, root, l));
          ctx.locals.push(LocalDecl {
              name: l.name,
              kind: match l.kind { LocalKind::Param => DefKind::Param, _ => DefKind::LocalVar },
              name_span: l.name_span,
              full_span: l.name_span, // 无消费方（spec 风险 3 已证实）
              ty,
          });
      }
      ```
    - `backfill_local_ty`：`root.descendant_for_byte_range(name_span)` 回找 → 上溯判形：
      - 祖先有 `for_each_statement`（且 name 在其 name 字段）→ `expr::for_each_element(ws, ctx, src, fe_node)` → `syn 或 syn_of_base`，失败回落声明类型（Auto）；
      - 祖先有 `variable_declarator` → 声明类型 auto + 有 value + 不在 value 内 → `expr::expr_type(ws, ctx, src, init)` → 同上映射；否则解析声明类型；
      - 找不到/失败 → `None` 之上层取声明类型或 Auto（宁缺毋假）。
- [ ] **Step 4: `cargo test -p as-core` + `cargo test --workspace` 全绿**
- [ ] **Step 5: Commit**——`as-core：SemCtx 局部源切 scope_tree + ty 急切回填（Phase E Task 1）`

### Task 2: 语料验收 + 文档同步（D41 + 设计稿 v1.6 收官）

**Files:**
- Modify: `docs/实现决策记录.md`（D41 + 索引表 + 变更记录 v1.11）
- Modify: `docs/index-architecture.md`（v1.6：§11 全表 ✅ + §3.3 消费标注 + 变更记录；D38「Phase E 接消费方」表述修正）
- Modify: `docs/future-work.md`（对账，若有相关项）

- [ ] **Step 1: 语料三旗标 + smoke + e2e-m4 复跑**（命令与数字同 Phase D Task 4；不一致即停）
- [ ] **Step 2: 文档四项编辑**（D41 内容按 spec E1-E4；设计稿 §11 步骤 6 标注「全表收官 ✅」；D38 条目「Phase E 接消费方」改为「D41 退役裁决」）
- [ ] **Step 3: `cargo test --workspace` 终验 + Commit**——`Phase E 文档同步：D41 + 设计稿 v1.6（索引重构收官，三旗标一致）`
