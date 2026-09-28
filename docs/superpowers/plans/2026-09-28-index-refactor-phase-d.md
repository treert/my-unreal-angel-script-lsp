# 索引重构 Phase D 实施计划（语义诊断调度队列）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 spec `2026-09-28-index-refactor-phase-d-design.md`（v1）：把 M6 的「发布即推」升级为 mylua 式诊断调度队列（热文件 300ms 防抖 + 结构变化全量重诊断 + 打开文件优先），接入 AS0907 继承环诊断，D36 的「无防抖」随 D40 记翻案。

**Architecture:** 调度器是**纯数据结构**（as-lsp 新模块，无 ws/docs 依赖，scope 收集全在 consumer 的 `drain`）；as-core 增量扩展（As0907 / `cycle_diags` / `filter_suppressions` 抽出 / `decl_surface` 扩含 bases）；main.rs 原子切换发布路径（删 `publish_file`/`publish_all_open`，diag_tx 语义改为 `request_full`）。

**Tech Stack:** Rust（cargo workspace：as-core / as-lsp）、tree-sitter 0.25、tokio（补 `time` feature）、tower-lsp-server。

## Global Constraints

- 单测 `.as` 用例**一律内置于源码字符串字面量**，禁止读 `tests/` 等外部文件（AGENTS.md 硬性规则 / D1）
- `intern_file` 路径一律加 `unique://` 前缀（并行测试防串）
- 每 Task 结束 `cargo test --workspace` 全绿；本计划基线 **160 个**（退休须有继任用例，语义断言不得静默删除）
- 提交信息中文、首行 ≤ 72 字符；master 串行，一 Task 一 commit
- 验收语料 `d:\DTmp\test-my-as-lsp` **全目录**（441 文件，含 Script-Examples 的 27 个 `.as`）；三旗标基线：resolve-stats 94.9%（1028/1083）/ ref-stats 25505 / decls 101322；`inheritance:` 行 cycles 预期 0
- 锁序约定不变：docs → index 读（跨锁不交叉持锁）

## 关键裁决（引自 spec，P1-P10）

| # | 裁决 |
|---|---|
| P1 | D36 全量翻案：300ms 防抖、单发布通道（覆盖式全量发布） |
| P2 | `cyclic_classes()` 本期接消费方：AS0907（base 名 span，Error，Decl 文件成员跳过） |
| P3 | 固定 Full：队列全集 = 索引内全部 Script 文件（`.d.as` 剔除），无配置 |
| P4 | 级联 = 结构变化 → 全量重诊断；无 last_published、不做只推变化 |
| P5 | 插队 = 热文件表 pop 优先（seq 降序）；didOpen/didChange 都标热 |
| P6 | `decl_surface` 扩含 bases（改基类必须算结构变化） |
| P7 | `ensure_file_fresh` / `add_file` 的 surface 布尔上供 |
| P8 | diag_tx 通道保留，语义改为 `request_full`（吃掉 `publish_all_open`） |
| P9 | AS0902 发布面从「已打开 Script 文档」扩到「全部 Script 文件」 |
| P10 | 调度器纯数据结构，scope 收集全在 consumer |

---

### Task 1: as-core 诊断内核扩展（AS0907 + cycle_diags + filter_suppressions + decl_surface 扩 bases）

**Files:**
- Modify: `lsp/crates/as-core/src/diag.rs`
- Modify: `lsp/crates/as-core/src/workspace.rs`

**Interfaces（Task 3 依赖，签名一字不差）:**
```rust
// diag.rs
pub enum DiagCode { ..., DiagCode::As0907 }
pub const AS0907_MESSAGE: &str;
pub fn filter_suppressions(diags: Vec<Diag>, text: &str) -> Vec<Diag>;
// workspace.rs impl Workspace
pub fn cycle_diags(&self) -> HashMap<FileId, Vec<Diag>>;
// workspace.rs fn decl_surface（私有，行为变化：元组扩含 bases）
```

- [ ] **Step 1: 写失败单测**（diag.rs `mod tests` 追加 + 既有 2 条扩展）

diag.rs 测试追加：

```rust
#[test]
fn diag_code_as0907_display_and_parse() {
    assert_eq!(DiagCode::As0907.to_string(), "AS0907");
    assert_eq!(DiagCode::parse("AS0907"), Some(DiagCode::As0907));
    assert_eq!(DiagCode::parse("AS0908"), None); // 未登记
}
```

workspace.rs 测试追加（`ws_build` 按路径后缀推断 kind——`.d.as` → Decl）：

```rust
#[test]
fn cycle_diags_reports_members_at_base_span() {
    // 双文件互继承 → 两侧各一条 AS0907，range 落在 base 名字区间
    let ws = ws_build(&[
        ("unique://cyc/a.as", "class A : B {}\n"),
        ("unique://cyc/b.as", "class B : A {}\n"),
    ]);
    let diags = ws.cycle_diags();
    let a = intern_file("unique://cyc/a.as", 0);
    let b = intern_file("unique://cyc/b.as", 0);
    assert_eq!(diags.len(), 2, "两个文件各一条");
    let da = &diags[&a];
    assert_eq!(da.len(), 1);
    assert_eq!(da[0].code, crate::diag::DiagCode::As0907);
    assert_eq!(da[0].severity, crate::diag::DiagSeverity::Error);
    // range 精确落在 base 名（"class A : B {}" 里的 "B"）
    let src = "class A : B {}\n";
    assert_eq!(&src[da[0].range.start as usize..da[0].range.end as usize], "B");
    let db = &diags[&b];
    assert_eq!(db.len(), 1);
    let src = "class B : A {}\n";
    assert_eq!(&src[db[0].range.start as usize..db[0].range.end as usize], "A");
    // 无环文件不在表里
    let ws2 = ws_build(&[("unique://cyc/clean.as", "class C {}\n")]);
    assert!(ws2.cycle_diags().is_empty());
}

#[test]
fn cycle_diags_skips_decl_file_members() {
    // Decl 文件（.d.as）里的环成员跳过——导出器不产环，腐坏导出不在诊断面
    let ws = ws_build(&[("unique://cyc/x.d.as", "class A : B {}\nclass B : A {}\n")]);
    assert!(ws.cycle_diags().is_empty());
}

#[test]
fn as0907_suppressible_via_ignore() {
    // 同套抑制语法：base 行同行尾注释 / next-line 均可抑制 AS0907
    let src_a = "class A : B {} // as-ignore: AS0907\n";
    let ws = ws_build(&[
        ("unique://sup/a.as", src_a),
        ("unique://sup/b.as", "class B : A {}\n"),
    ]);
    let a = intern_file("unique://sup/a.as", 0);
    let diags = crate::diag::filter_suppressions(ws.cycle_diags().remove(&a).unwrap(), src_a);
    assert!(diags.is_empty(), "同行 as-ignore 应抑制 AS0907");
}

#[test]
fn reindex_base_change_is_surface_change() {
    // P6：改基类（name/kind/parent 不变）必须算结构变化；纯局部内容改不算
    let mut ws = ws_build(&[("unique://surf/a.as", "class A : B {}\nclass B {}\n")]);
    let file = intern_file("unique://surf/a.as", 0);
    assert!(
        ws.reindex_file(file, FileKind::Script, "class A : C {}\nclass B {}\n".to_string()),
        "改基类应报 surface 变化"
    );
    assert!(
        !ws.reindex_file(file, FileKind::Script, "class A : C {}\nclass B {}\n".to_string()),
        "内容未变不应报"
    );
}
```

- [ ] **Step 2: 跑测试确认失败**

```powershell
cargo test -p as-core cycle_diags reindex_base_change
```
预期：编译失败（`As0907` / `cycle_diags` / `filter_suppressions` 不存在）。

- [ ] **Step 3: 实现 diag.rs 扩展**

`DiagCode` 枚举追加（登记位注释同步：下一个可用改 `AS0908`）：

```rust
/// `AS0907`：继承环（`Workspace::cycle_diags` 检出的环成员类，
/// range = base 名字区间——Phase D / D40）。
As0907,
```

`parse` 加 arm：`"AS0907" => Some(DiagCode::As0907)`；`Display` 加：`DiagCode::As0907 => "AS0907"`。

措辞常量（`AS0903_MESSAGE` 后追加）：

```rust
/// AS0907 的措辞（Phase D / D40）。
pub const AS0907_MESSAGE: &str = "该类的继承链成环（cyclic inheritance），引擎侧无法编译。";
```

抑制过滤抽出（`script_diags` 尾段平移为独立函数，`script_diags` 改为调用它）：

```rust
/// 行级抑制过滤（`parse_suppressions` + 诊断 range 起始行命中即丢弃）。
/// `script_diags` 与 AS0907（`cycle_diags` 的产物）共用同一套抑制语义。
pub fn filter_suppressions(mut diags: Vec<Diag>, text: &str) -> Vec<Diag> {
    let sups = parse_suppressions(text);
    if sups.is_empty() {
        return diags;
    }
    let lines = LineIndex::new(text);
    diags.retain(|d| {
        let line = lines.line_of(d.range.start) as u32;
        !sups.iter().any(|s| {
            s.line == line && s.codes.as_ref().map_or(true, |c| c.contains(&d.code))
        })
    });
    diags
}
```

`script_diags` 尾段改为：

```rust
    out.sort_by_key(|d| d.range.start);
    filter_suppressions(out, text)
}
```

- [ ] **Step 4: 实现 workspace.rs 扩展**

`impl Workspace` 追加（`cyclic_classes` 之后；文件头补 `use crate::diag::Diag;`）：

```rust
/// AS0907 继承环诊断（Phase D / P2）：`cyclic_classes()` 每个环成员类在
/// 其 base 名字区间（`bases[0].span`）产一条 Error。
/// **Decl 文件成员跳过**——导出器不产环，腐坏导出不在诊断面（spec P2）。
/// 输出未经抑制过滤——消费方（as-lsp）与 script_diags 共用
/// `diag::filter_suppressions` 统一滤。
pub fn cycle_diags(&self) -> HashMap<FileId, Vec<Diag>> {
    use crate::diag::{DiagCode, DiagSeverity, AS0907_MESSAGE};
    let mut out: HashMap<FileId, Vec<Diag>> = HashMap::new();
    for r in self.cyclic_classes() {
        if self.files[&r.file].kind == FileKind::Decl {
            continue;
        }
        if let Some(base) = self.decl(&r).bases.first() {
            out.entry(r.file).or_default().push(Diag {
                code: DiagCode::As0907,
                range: base.span,
                severity: DiagSeverity::Error,
                message: AS0907_MESSAGE.to_string(),
            });
        }
    }
    out
}
```

`decl_surface` 扩含 bases（P6）：

```rust
fn decl_surface(s: &FileSummary) -> Vec<(Sym, DefKind, Option<Sym>, Vec<Sym>)> {
    let mut out = s
        .decls
        .iter()
        .map(|d| {
            let parent_name = d.parent.map(|p| s.decls[p as usize].name);
            let mut bases: Vec<Sym> = d.bases.iter().map(|b| b.name).collect();
            bases.sort();
            (d.name, d.kind, parent_name, bases)
        })
        .collect();
    out.sort();
    out
}
```

- [ ] **Step 5: 跑测试确认通过 + 全 workspace**

```powershell
cargo test -p as-core
cargo test --workspace
```
预期：全绿（160 基线 + 本任务新增 4 条 = 164）。

- [ ] **Step 6: Commit**

```powershell
git add lsp/crates/as-core/src/diag.rs lsp/crates/as-core/src/workspace.rs
git commit -m "as-core：AS0907 继承环诊断 + decl_surface 扩含 bases（Phase D Task 1）"
```

---

### Task 2: as-lsp 调度器模块（纯数据结构，mylua 同构薄化）

**Files:**
- Create: `lsp/crates/as-lsp/src/diagnostic_scheduler.rs`
- Modify: `lsp/crates/as-lsp/src/main.rs`（只加 `mod diagnostic_scheduler;` 一行）
- Modify: `lsp/crates/as-lsp/Cargo.toml`（tokio features 补 `"time"`）

**Interfaces（Task 3 依赖，签名一字不差）:**
```rust
pub const DIAGNOSTIC_DEBOUNCE_MS: u64 = 300;
pub struct DiagnosticScheduler;
impl DiagnosticScheduler {
    pub fn new() -> Arc<Self>;
    pub fn schedule(self: &Arc<Self>, file: FileId, full: bool);
    pub fn request_full(&self);
    pub fn pop(&self) -> Option<FileId>;
    pub async fn notified(&self);
    pub fn take_full_flag(&self) -> bool;
    pub fn set_full_queue(&self, files: Vec<FileId>);
    pub fn invalidate(&self, file: FileId);
    pub fn schedule_now_for_test(&self, file: FileId, full: bool);
}
```

- [ ] **Step 1: Cargo.toml 补 time feature**

```toml
tokio = { version = "1.53.1", features = ["io-util", "io-std", "macros", "rt-multi-thread", "sync", "time"] }
```

main.rs 模块声明区加：`mod diagnostic_scheduler;`

- [ ] **Step 2: 写失败单测**（新文件 `#[cfg(test)] mod tests`）

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use as_core::intern::intern_file;

    fn id(n: i32) -> FileId {
        intern_file(&format!("unique://sched/{n}.as"), 0)
    }

    #[test]
    fn pop_prefers_newest_hot_then_queue() {
        let s = DiagnosticScheduler::new();
        s.schedule_now_for_test(id(1), false);
        s.schedule_now_for_test(id(2), false);
        s.set_full_queue(vec![id(3), id(4)]);
        assert_eq!(s.pop(), Some(id(2)), "热文件新者优先");
        assert_eq!(s.pop(), Some(id(1)));
        assert_eq!(s.pop(), Some(id(3)), "热表空后按队列");
        assert_eq!(s.pop(), Some(id(4)));
        assert_eq!(s.pop(), None);
    }

    #[test]
    fn schedule_same_file_dedups_single_pop() {
        let s = DiagnosticScheduler::new();
        s.schedule_now_for_test(id(1), false);
        s.schedule_now_for_test(id(1), false);
        assert_eq!(s.pop(), Some(id(1)));
        assert_eq!(s.pop(), None);
    }

    #[test]
    fn hot_survives_full_queue_rebuild_until_popped() {
        // mylua modified_entry_survives_rebuild_until_popped 的对应
        let s = DiagnosticScheduler::new();
        s.schedule_now_for_test(id(2), false);
        s.set_full_queue(vec![id(1), id(2), id(3)]);
        s.set_full_queue(vec![id(1), id(3)]); // 重建丢掉队列里的 id2
        assert_eq!(s.pop(), Some(id(2)), "热标记不随重建消失");
        assert_eq!(s.pop(), Some(id(1)));
        assert_eq!(s.pop(), Some(id(3)));
        assert_eq!(s.pop(), None);
    }

    #[test]
    fn set_full_queue_excludes_hot_no_double_pop() {
        let s = DiagnosticScheduler::new();
        s.schedule_now_for_test(id(2), false);
        s.set_full_queue(vec![id(1), id(2), id(3)]); // set_full_queue 内部剔热
        assert_eq!(s.pop(), Some(id(2)));
        assert_eq!(s.pop(), Some(id(1)));
        assert_eq!(s.pop(), Some(id(3)));
        assert_eq!(s.pop(), None, "热 pop 时同步摘队列，无双发");
    }

    #[test]
    fn take_full_flag_consumed_once() {
        let s = DiagnosticScheduler::new();
        s.request_full();
        assert!(s.take_full_flag());
        assert!(!s.take_full_flag());
    }

    #[test]
    fn invalidate_clears_hot_and_queue() {
        let s = DiagnosticScheduler::new();
        s.schedule_now_for_test(id(1), false);
        s.set_full_queue(vec![id(1), id(2)]);
        s.invalidate(id(1));
        assert_eq!(s.pop(), Some(id(2)), "id1 从热表与队列同时消失");
        assert_eq!(s.pop(), None);
    }

    #[tokio::test]
    async fn schedule_debounces_with_gen_collapse() {
        let s = DiagnosticScheduler::new();
        s.schedule(id(1), false);
        tokio::time::sleep(Duration::from_millis(50)).await;
        s.schedule(id(2), false);
        tokio::time::sleep(Duration::from_millis(50)).await;
        s.schedule(id(1), false);
        assert_eq!(s.pop(), None, "防抖窗内不应有产出");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(s.pop(), Some(id(1)));
        assert_eq!(s.pop(), Some(id(2)));
        assert_eq!(s.pop(), None);
    }

    #[tokio::test]
    async fn notify_wakes_consumer() {
        let s = DiagnosticScheduler::new();
        let s2 = s.clone();
        let handle = tokio::spawn(async move {
            loop {
                if s2.pop().is_some() {
                    return;
                }
                s2.notified().await;
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        s.request_full(); // 立即 notify（bypass 防抖）
        tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("consumer 应在 500ms 内被唤醒")
            .expect("任务应正常结束");
    }
}
```

- [ ] **Step 3: 跑测试确认失败**

```powershell
cargo test -p as-lsp diagnostic_scheduler
```
预期：编译失败（模块未实现）。

- [ ] **Step 4: 实现模块**

```rust
//! DiagnosticScheduler——语义诊断调度（Phase D / D40，mylua 同构薄化）。
//!
//! - **热文件表**（didOpen/didChange 标记，seq 新者优先，pop 前一直保留）：
//!   drain 中途的新事件即刻插队——`pop` 每次先看热表（P5）；
//! - **全量队列**只在结构变化（surface_changed）或快照发布（`request_full`）
//!   时由收集方（consumer 的 drain）喂入：打开文件在前，其余 FileId 升序（P3）；
//! - **300ms 防抖**（gen 合并，仅最新 gen 的 timer 有效）；
//! - 本结构是**纯数据**（P10）：不持有 ws/docs，scope 收集全在 as-lsp `diag::drain`。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use as_core::id::FileId;

pub const DIAGNOSTIC_DEBOUNCE_MS: u64 = 300;

struct Inner {
    /// 未 pop 的热文件；值 = seq（新者优先）。
    hot: HashMap<FileId, u64>,
    seq: u64,
    /// 全量队列（排序由收集方排好：打开在前 → FileId 升序）。
    queue: VecDeque<FileId>,
    queued: HashSet<FileId>,
    /// 防抖合并：仅最新 gen 的 timer 有效（mylua 同款）。
    gen: u64,
    /// 本轮防抖到期后需全量。
    pending_full: bool,
}

pub struct DiagnosticScheduler {
    inner: Mutex<Inner>,
    notify: Notify,
}

impl DiagnosticScheduler {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                hot: HashMap::new(),
                seq: 0,
                queue: VecDeque::new(),
                queued: HashSet::new(),
                gen: 0,
                pending_full: false,
            }),
            notify: Notify::new(),
        })
    }

    /// didOpen / didChange / watch 事件：标记热文件 + 防抖。
    /// `full = true`（watch 侧已检出 surface 变化）时，本轮防抖到期后
    /// 收集方应做全量。
    pub fn schedule(self: &Arc<Self>, file: FileId, full: bool) {
        let gen = self.record(file, full);
        self.spawn_debounce(gen);
    }

    /// 快照发布（publish_and_replay → diag_tx 转发任务）：bypass 防抖，
    /// 立即唤醒 consumer 做全量（吃掉旧 publish_all_open 语义，P8）。
    pub fn request_full(&self) {
        self.inner.lock().unwrap().pending_full = true;
        self.notify.notify_one();
    }

    /// 取下一个待诊断文件：热文件（seq 降序）→ 全量队列。
    /// 热 pop 时同步摘队列项（queued 移除 → 队列残项由 stale 跳过），防双发。
    pub fn pop(&self) -> Option<FileId> {
        let mut inner = self.inner.lock().unwrap();
        if let Some((&file, _)) = inner.hot.iter().max_by_key(|(_, &seq)| seq) {
            inner.hot.remove(&file);
            inner.queued.remove(&file);
            return Some(file);
        }
        while let Some(file) = inner.queue.pop_front() {
            if inner.queued.remove(&file) {
                return Some(file);
            }
        }
        None
    }

    /// 等下一次防抖到期 / request_full 唤醒（consumer 任务调用）。
    pub async fn notified(&self) {
        self.notify.notified().await;
    }

    /// drain 头消费（一次性）：本轮是否需要全量收集。
    pub fn take_full_flag(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        std::mem::take(&mut inner.pending_full)
    }

    /// 喂入全量队列（收集方已排好序：打开在前 → FileId 升序）。
    /// 热表成员剔除（pop 热优先，不重复入队）。
    pub fn set_full_queue(&self, files: Vec<FileId>) {
        let mut inner = self.inner.lock().unwrap();
        let files: Vec<FileId> =
            files.into_iter().filter(|f| !inner.hot.contains_key(f)).collect();
        inner.queued = files.iter().copied().collect();
        inner.queue = files.into();
    }

    /// 文件删除 / 关闭：清理调度状态（热 + 队列；队列残项由 stale 跳过）。
    pub fn invalidate(&self, file: FileId) {
        let mut inner = self.inner.lock().unwrap();
        inner.hot.remove(&file);
        inner.queued.remove(&file);
    }

    fn record(&self, file: FileId, full: bool) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        inner.seq += 1;
        inner.hot.insert(file, inner.seq);
        inner.pending_full |= full;
        inner.gen += 1;
        inner.gen
    }

    fn spawn_debounce(self: &Arc<Self>, gen: u64) {
        let sched = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(DIAGNOSTIC_DEBOUNCE_MS)).await;
            let current = sched.inner.lock().unwrap().gen;
            if current == gen {
                sched.notify.notify_one();
            }
        });
    }

    /// 测试入口：防抖旁路（直接标记，不 spawn timer）。
    pub fn schedule_now_for_test(&self, file: FileId, full: bool) {
        self.record(file, full);
    }
}
```

- [ ] **Step 5: 跑测试确认通过 + 全 workspace**

```powershell
cargo test -p as-lsp diagnostic_scheduler
cargo test --workspace
```
预期：全绿（164 + 本任务 8 条 = 172）。

- [ ] **Step 6: Commit**

```powershell
git add lsp/crates/as-lsp/src/diagnostic_scheduler.rs lsp/crates/as-lsp/src/main.rs lsp/crates/as-lsp/Cargo.toml
git commit -m "as-lsp：DiagnosticScheduler 调度器模块（Phase D Task 2，mylua 同构薄化）"
```

---

### Task 3: 集成——surface 布尔上供 + diag.rs 重构 + main.rs 原子切换

**Files:**
- Modify: `lsp/crates/as-lsp/src/workspace.rs`（`ensure_file_fresh`/`add_file`/`reindex` 返回 bool）
- Modify: `lsp/crates/as-lsp/src/diag.rs`（删 `publish_file`/`publish_all_open`，新增 `DrainFacts`/`file_ls_diags`/`drain`）
- Modify: `lsp/crates/as-lsp/src/main.rs`（Backend 加 `sched` 字段、consumer 任务、handlers 改调度）

**Interfaces:**
- Consumes: Task 1 的 `cycle_diags`/`filter_suppressions`；Task 2 的 `DiagnosticScheduler` 全套 API
- Produces: `diag::drain(client, docs, ws, sched)`（consumer 调用）；`Backend.sched: Arc<DiagnosticScheduler>`

- [ ] **Step 1: 写失败单测**

workspace.rs（as-lsp）`mod tests` 追加（现有 tests 若无则新建——该文件当前无测试模块，追加）：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::docs::DocStore;
    use as_core::intern::intern_file;

    // 单测用例内置于源码（D1）。

    #[test]
    fn ensure_file_fresh_reports_surface_change() {
        let ws = WorkspaceState::new();
        let file = intern_file("unique://fresh/a.as", 0);
        let inputs = vec![as_core::FileInput {
            file,
            kind: FileKind::Script,
            module: None,
            source: "class A : B {}\nclass B {}\n".to_string(),
        }];
        let idx = as_core::workspace::Workspace::build(as_core::IndexConfig::default(), inputs);
        ws.publish_and_replay(idx, &Mutex::new(DocStore::new()));
        // 打开（version 1，基类 B → C：P6 的结构变化）
        let mut store = DocStore::new();
        store.open("unique://fresh/a.as", 1, "class A : C {}\nclass B {}\n".to_string());
        let docs = Mutex::new(store);
        assert!(ws.ensure_file_fresh(file, &docs), "改基类应报 surface 变化");
        assert!(!ws.ensure_file_fresh(file, &docs), "版本未变不再重索引");
    }
}
```

diag.rs 测试：既有 2 条（`doc_ls_diags_maps_code_and_severity` / `doc_ls_diags_no_as0902_when_decls_present`）改用新 API 平移（见 Step 3 签名），另追加：

```rust
#[test]
fn file_ls_diags_merges_cycle_and_suppresses() {
    // 未打开文件路径（索引 FileEntry）：环诊断并入 + as-ignore 抑制
    let src_a = "class A : B {} // as-ignore: AS0907\n";
    let ws = as_core::workspace::Workspace::build(
        as_core::IndexConfig::default(),
        vec![
            as_core::FileInput {
                file: as_core::intern::intern_file("unique://lspdiag/cyc_a.as", 0),
                kind: as_core::FileKind::Script,
                module: None,
                source: src_a.to_string(),
            },
            as_core::FileInput {
                file: as_core::intern::intern_file("unique://lspdiag/cyc_b.as", 0),
                kind: as_core::FileKind::Script,
                module: None,
                source: "class B : A {}\n".to_string(),
            },
        ],
    );
    let facts = DrainFacts {
        decl_missing: false,
        cycle_diags: ws.cycle_diags(),
    };
    let a = as_core::intern::intern_file("unique://lspdiag/cyc_a.as", 0);
    let e = ws.files.get(&a).unwrap();
    let diags = file_ls_diags(&e.tree, &e.source, &e.lines, true, &facts, a);
    assert!(diags.is_empty(), "AS0907 被同行 as-ignore 抑制，且无其他诊断");
    // 去掉抑制注释 → AS0907 出现
    let ws2 = /* 同上但 src_a 无注释，略——见 Step 3 完整测试代码 */;
    let _ = ws2;
}
```

（`file_ls_diags` 形态见 Step 3；测试里第二个分支直接再 build 一个无注释的 ws 断言 `diags.len() == 1 && code == "AS0907"`。）

- [ ] **Step 2: 跑测试确认失败**

```powershell
cargo test -p as-lsp
```
预期：编译失败（`DrainFacts`/`file_ls_diags` 不存在、`ensure_file_fresh` 返回值不符）。

- [ ] **Step 3: 实现**

**workspace.rs（as-lsp）**——三处签名与返回值：

```rust
/// 单文件保鲜（语义请求前）：
/// ① didClose 后回落磁盘重读（§5.1）；② overlay 领先 → 惰性重索引。
/// 返回 = 本次是否发生声明面（含 bases）变化（Phase D / P7）。
pub fn ensure_file_fresh(&self, file: FileId, docs: &Mutex<DocStore>) -> bool {
    if !self.is_ready() {
        return false;
    }
    let mut changed = false;
    if self.stale.lock().unwrap().remove(&file) {
        if let Some(path) = file_path(file) {
            if let Ok(text) = std::fs::read_to_string(path) {
                changed |= self.reindex(file, kind_of_path(path), text);
                self.indexed_versions.lock().unwrap().remove(&file);
            }
        }
    }
    let overlay = {
        let store = docs.lock().unwrap();
        store.get(file).map(|d| (d.version, d.text.clone()))
    };
    if let Some((v, text)) = overlay {
        if self.indexed_versions.lock().unwrap().get(&file) != Some(&v) {
            if let Some(path) = file_path(file) {
                let kind = kind_of_path(path);
                changed |= self.reindex(file, kind, text);
                self.indexed_versions.lock().unwrap().insert(file, v);
            }
        }
    }
    changed
}

fn reindex(&self, file: FileId, kind: FileKind, text: String) -> bool {
    let surface_changed = {
        let mut idx = self.index.write().unwrap();
        match idx.as_mut() {
            Some(i) => i.reindex_file(file, kind, text),
            None => false,
        }
    };
    if surface_changed {
        as_log!("reindex: decl surface changed (query-time references need no invalidation)");
    }
    surface_changed
}

/// watched-files 新增 / 改名：单文件入索引。返回 = 声明面是否变化（P7）。
pub fn add_file(&self, file: FileId, kind: FileKind, module: Option<Sym>, text: String) -> bool {
    let bytes = text.len();
    let changed = {
        let mut idx = self.index.write().unwrap();
        match idx.as_mut() {
            Some(i) => i.reindex_file_full(file, kind, module, text),
            None => false,
        }
    };
    as_log!("add_file: indexed {bytes} bytes (kind={kind:?}, surface changed={changed})");
    changed
}
```

**diag.rs 重构**（删 `publish_file`/`publish_all_open`/`doc_ls_diags`，新文件头 + 三个函数 + 测试平移）：

```rust
//! 诊断发布管道（Phase D / D40：调度队列消费侧）。
//!
//! 时序（spec §2）：
//! - didOpen / didChange → 调度器标热 + 300ms 防抖 → [`drain`]；
//! - 结构变化（surface_changed，含改基类）或快照发布（`request_full`）
//!   → 全量队列（打开文件在前，P3/P4）；
//! - didClose → 立即推空数组清空（D36 行为保留）+ invalidate；
//! - Loading 期不 schedule（调用方 is_ready 门控），快照发布的 request_full 兜底。
//!
//! 规则本体与抑制过滤在 `as_core::diag`（纯函数）；本层只做 UTF-16 换算
//! 与协议映射（§3.2.1：换算只在本层发生）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tower_lsp_server::ls_types::{self as ls, *};
use tower_lsp_server::Client;

use as_core::diag::{filter_suppressions, script_diags, Diag, DiagSeverity};
use as_core::id::FileId;
use as_core::{as_syntax, FileKind, LineIndex};

use crate::diagnostic_scheduler::DiagnosticScheduler;
use crate::docs::DocStore;
use crate::workspace::{kind_of_path, WorkspaceState};

/// 一轮 drain 的工作区事实（每 drain 算一次，ms 级）。
pub struct DrainFacts {
    /// AS0902：索引中无真实 `.d.as`（builtin 伪文件不算，B2 口径）。
    pub decl_missing: bool,
    /// AS0907：文件 → 环诊断（未经抑制过滤，`file_ls_diags` 统一滤）。
    pub cycle_diags: HashMap<FileId, Vec<Diag>>,
}

/// per-file 统一入口（打开/未打开同构，P9）：
/// AS0903 ∪ AS0902（`decl_missing && Script`）∪ AS0907，经抑制过滤。
fn file_ls_diags(
    tree: &as_syntax::tree_sitter::Tree,
    text: &str,
    lines: &LineIndex,
    is_script: bool,
    facts: &DrainFacts,
    file: FileId,
) -> Vec<ls::Diagnostic> {
    let mut diags = script_diags(tree, text, is_script && facts.decl_missing);
    if let Some(cs) = facts.cycle_diags.get(&file) {
        diags.extend(cs.iter().cloned());
    }
    diags.sort_by_key(|d| d.range.start);
    // AS0907 不经 script_diags 内部过滤，统一再滤（对已滤部分幂等）
    let diags = filter_suppressions(diags, text);
    diags
        .into_iter()
        .map(|d| {
            let (sl, sc) = lines.line_col_utf16(text, d.range.start);
            let (el, ec) = lines.line_col_utf16(text, d.range.end);
            Diagnostic {
                range: Range::new(Position::new(sl, sc), Position::new(el, ec)),
                severity: Some(match d.severity {
                    DiagSeverity::Error => DiagnosticSeverity::ERROR,
                    DiagSeverity::Warning => DiagnosticSeverity::WARNING,
                    DiagSeverity::Information => DiagnosticSeverity::INFORMATION,
                    DiagSeverity::Hint => DiagnosticSeverity::HINT,
                }),
                code: Some(NumberOrString::String(d.code.to_string())),
                code_description: None,
                source: Some("my-as-lsp".to_string()),
                message: d.message,
                tags: None,
                related_information: None,
                data: None,
            }
        })
        .collect()
}

/// 消费一轮调度队列（consumer 任务调用：`loop { notified().await; drain().await }`）。
pub async fn drain(
    client: &Client,
    docs: &Mutex<DocStore>,
    ws: &WorkspaceState,
    sched: &Arc<DiagnosticScheduler>,
) {
    if !ws.is_ready() {
        return;
    }
    // ① 全部打开文档保鲜（工作区事实要看最新 overlay），收集 surface 变化
    let open: Vec<FileId> = docs.lock().unwrap().entries().map(|(f, _)| f).collect();
    let mut surface_changed = false;
    for file in open {
        surface_changed |= ws.ensure_file_fresh(file, docs);
    }
    // ② 全量队列：request_full（快照发布）或本轮检出结构变化（P4）
    if sched.take_full_flag() || surface_changed {
        let open_set: HashSet<FileId> = docs.lock().unwrap().entries().map(|(f, _)| f).collect();
        let mut files: Vec<FileId> = ws
            .with(|idx| {
                idx.files
                    .iter()
                    .filter(|(_, e)| e.kind == FileKind::Script)
                    .map(|(&f, _)| f)
                    .collect()
            })
            .unwrap_or_default();
        files.sort_by_key(|f| (!open_set.contains(f), *f)); // 打开在前，其余 FileId 升序
        sched.set_full_queue(files);
    }
    // ③ 工作区事实
    let facts = ws
        .with(|idx| DrainFacts {
            decl_missing: !idx.has_decl_files(),
            cycle_diags: idx.cycle_diags(),
        })
        .unwrap_or(DrainFacts { decl_missing: false, cycle_diags: HashMap::new() });
    // ④ pop 循环：热优先 → 队列；打开取 overlay（带 version），未打开取索引
    //    FileEntry（version=None）；都不在（并发删除）→ 跳过。
    //    锁序 docs → index 读（既有约定）。
    while let Some(file) = sched.pop() {
        let Some(path) = as_core::intern::file_path(file) else { continue };
        let Some(uri) = ls::Uri::from_file_path(path) else { continue };
        let is_script = kind_of_path(path) == FileKind::Script;
        let computed = {
            let store = docs.lock().unwrap();
            store.get(file).map(|doc| {
                (file_ls_diags(&doc.tree, &doc.text, &doc.lines, is_script, &facts, file), Some(doc.version))
            })
        };
        let computed = match computed {
            Some(x) => Some(x),
            None => ws.with(|idx| {
                idx.files.get(&file).map(|e| {
                    (file_ls_diags(&e.tree, &e.source, &e.lines, is_script, &facts, file), None)
                })
            }),
        };
        if let Some((diags, version)) = computed {
            client.publish_diagnostics(uri, diags, version).await;
        }
    }
}
```

diag.rs 测试模块：既有 2 条平移为 `file_ls_diags` 形态（`WorkspaceState::new` + `publish_and_replay` 构造 Ready / `DocStore` 取 doc 的 tree/text/lines，`DrainFacts` 手工构造 `decl_missing`），加上 Step 1 的 `file_ls_diags_merges_cycle_and_suppresses`（完整版：无注释分支再 build 一个 ws 断言 `len == 1 && code == "AS0907"`）。

**main.rs**：

1. Backend 结构体加字段 `pub sched: Arc<crate::diagnostic_scheduler::DiagnosticScheduler>,`
2. `LspService::build` 闭包内（diag_tx 创建处）：

```rust
let (diag_tx, mut diag_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
let docs = Arc::new(Mutex::new(DocStore::new()));
let ws = Arc::new(WorkspaceState::new());
let sched = diagnostic_scheduler::DiagnosticScheduler::new();
// Phase D（D40）：diag_tx 转发 = request_full（快照发布 → 全量重诊断，
// 吃掉旧 publish_all_open）
{
    let sched = Arc::clone(&sched);
    tokio::spawn(async move {
        while diag_rx.recv().await.is_some() {
            sched.request_full();
        }
    });
}
// consumer：防抖到期 / request_full → drain（热优先 → 全量队列）
{
    let cclient = client.clone();
    let cdocs = Arc::clone(&docs);
    let cws = Arc::clone(&ws);
    let csched = Arc::clone(&sched);
    tokio::spawn(async move {
        loop {
            csched.notified().await;
            diag::drain(&cclient, &cdocs, &cws, &csched).await;
        }
    });
}
ws.set_ready_tx(tx);
ws.set_diag_tx(diag_tx);
Backend { client, docs, config, ws, sched, folders, watch_supported, debouncer, debug_file_log }
```

3. `did_open`：`diag::publish_file(...)` 调用替换为：

```rust
// Phase D（D40）：标热 + 300ms 防抖（Loading 期不 schedule，seed 兜底）
if self.ws.is_ready() {
    self.sched.schedule(file, false);
}
```

4. `did_change`：同样替换为（删掉「不做防抖，体感卡顿再补」注释）：

```rust
if self.ws.is_ready() {
    self.sched.schedule(file, false);
}
```

5. `did_close`：推空数组之后追加：

```rust
if let Some(f) = file {
    self.sched.invalidate(f);
}
```

6. `did_change_watched_files`：ScriptCreate/ScriptChange 分支的 `self.ws.add_file(...)` 改为：

```rust
let surface = self.ws.add_file(file, workspace::kind_of_path(&path), module, text);
if self.ws.is_ready() {
    self.sched.schedule(file, surface);
}
```

ScriptDelete 分支的 `self.ws.remove_file(file);` 之后追加：

```rust
self.sched.invalidate(file);
```

- [ ] **Step 4: 全量测试**

```powershell
cargo test --workspace
```
预期：全绿（172 + 本任务 3 条 = 175 左右；既有 2 条平移不减语义断言）。

- [ ] **Step 5: Commit**

```powershell
git add lsp/crates/as-lsp/src/workspace.rs lsp/crates/as-lsp/src/diag.rs lsp/crates/as-lsp/src/main.rs
git commit -m "as-lsp：诊断发布切换调度队列（D36 翻案，300ms 防抖 + 结构变化全量）"
```

---

### Task 4: 语料验收 + 文档同步

**Files:**
- Modify: `docs/诊断码表.md`（v1.0：AS0907 登记）
- Modify: `docs/实现决策记录.md`（D40 + 索引表 + 变更记录 v1.10）
- Modify: `docs/index-architecture.md`（v1.5：§5.5 落地标注 + §11 映射 + 变更记录）
- Modify: `docs/LSP实现规划.md`（§8.1 时序按 D40 更新）

- [ ] **Step 1: 语料验收（三旗标不回归）**

```powershell
cargo build --release -p as-cli
.\target\release\as-cli.exe dump-index d:\DTmp\test-my-as-lsp --new-arch --resolve-stats --ref-stats
```

预期与基线逐位一致：resolve-stats 1028/1083=94.9%、ref-stats 25505 hits、decls 101322；`inheritance:` 行 cycles 0（语料无环）。数字抄进提交信息。**若不一致：停止**，回报（as-core 既有路径应零语义改动——本 Phase 只做增量）。

- [ ] **Step 2: 诊断码表 v1.0**

§3 表追加行：

```markdown
| `AS0907` | 类继承链成环（`cyclic_classes()` 检出） |
```

§3.1 表追加行：

```markdown
| `AS0907` | ✅ Phase D | Error | 环成员类的 base 名字区间 | 工作区级事实（每轮 drain 现算一次）；Decl 文件成员跳过（导出器不产环）；结构变化 / 快照发布触发全量重诊断时按文件分发 |
```

§4 占用总览 `AS09xx` 行改为：`AS0902`、`AS0903`、`AS0907`（…），下一个可用 `AS0908`。变更记录加 v1.0 行。

- [ ] **Step 3: 实现决策记录 D40**

索引表追加 `| D40 | **Phase D 诊断调度落地形态**（D36 翻案：防抖 / 全量队列 / AS0907 / surface 扩 bases） | ✅ 实现期定案，见本文件 D40 |`；逐条记录节追加（在 D39 之后，格式对齐）：

```markdown
### D40 Phase D 诊断调度落地形态（实现期定案）

- **背景**：索引重构 Phase D（spec `2026-09-28-index-refactor-phase-d-design.md`）：
  把 M6 的「发布即推」升级为 mylua 式调度队列。D36 的「didOpen/didChange 同步推、
  无防抖」经 brainstorming 四项裁决翻案。
- **裁决**：
  1. **D36 翻案（防抖）**：didOpen/didChange → 热文件标记 + 300ms 防抖（gen 合并），
     单发布通道、覆盖式全量发布；AS0903 反馈延迟 300ms 属业界常态；
  2. **级联 = 类型结构变化 → 全量重诊断**：`decl_surface` 扩含 bases（「`class A : B`
     改 `class A : C`」必须算结构变化，否则跨文件环诊断漏报）；didChange 的检测点是
     drain 头的惰性保鲜（`ensure_file_fresh` 返回值上供），watch 侧事件时即知；
     全量队列 = 索引内全部 Script 文件（`.d.as` 剔除——必然合法），**打开文件在前**，
     其余 FileId 升序；**固定 Full，无 scope 配置**（P5 变贵再加）；
  3. **插队 = 热文件表 pop 优先**（seq 降序，pop 前一直保留）：drain 中途的新事件
     （didOpen/didChange 都标热）即刻插队；
  4. **AS0907 继承环**（C4 素材的消费方）：`Workspace::cycle_diags()` 每个环成员类
     在 base 名字区间产一条 Error；Decl 文件成员跳过；经 `as-ignore` 同套抑制；
  5. **AS0902 发布面扩展**：从「已打开 Script 文档」扩到「全部 Script 文件」
     （Full 档自然语义）；
  6. **不做**：恒级联、last_published 只推变化、scope 配置（正确性优先 + YAGNI，
     全量重诊断的空集发布是清 stale 的必要手段）。
- **落盘**：as-lsp `diagnostic_scheduler.rs`（新增）/ `diag.rs`（drain）/ `workspace.rs` /
  `main.rs`；as-core `diag.rs`（As0907 + `filter_suppressions` 抽出）/ `workspace.rs`
  （`cycle_diags` + `decl_surface` 扩 bases）；诊断码表 v1.0；设计稿 v1.5。
```

变更记录加 v1.10 行。

- [ ] **Step 4: 设计稿 v1.5（index-architecture.md）**

- §5.5 表格下追加落地标注（对齐 §8 生死清单的标注风格）：
  「**Phase D 落地状态（v1.5，D40）**：语义诊断行已实施——as-lsp 调度队列
  （热 300ms 防抖 / 结构变化全量 / 打开优先 / 插队），AS0902 发布面扩到全部
  Script 文件，AS0907（继承环）作为首个工作区级语义诊断落地（`cyclic_classes()`
  的消费方，C4 素材兑现）。增量更新路径（贡献倒排消费）仍属 Phase E。」
- §11 步骤 6 标注：「诊断调度半边 = Phase D（D40）；增量更新路径 = Phase E」。
- 变更记录加 v1.5 行。

- [ ] **Step 5: LSP实现规划.md §8.1 时序更新**

在 `docs/LSP实现规划.md` 中 grep `D36` / `同步推`（§8.1 publishDiagnostics 行与 §2.2 / §9 M6 相关节），把发布时序描述替换为：

「didOpen/didChange 后标热 + 300ms 防抖（D40，D36 翻案）；结构变化（声明面 diff
含 bases）或快照发布触发全量重诊断（打开文件优先）；didClose 清空 + invalidate；
Loading 期不推，快照发布经 diag 通道 request_full 兜底。」

同时该文件变更记录（如有）加一行。

- [ ] **Step 6: 全量测试 + Commit**

```powershell
cargo test --workspace
git add docs/诊断码表.md docs/实现决策记录.md docs/index-architecture.md docs/LSP实现规划.md
git commit -m "Phase D 文档同步：码表 v1.0（AS0907）+ D40 + 设计稿 v1.5（三旗标验收数字见 as-cli）"
```

---

## Self-Review 记录

- **Spec 覆盖**：P1（Task 3 wiring）✓ P2（Task 1 cycle_diags + Task 4 码表）✓ P3（Task 3 drain ②）✓ P4（Task 3 drain ② + Task 1 decl_surface）✓ P5（Task 2 pop）✓ P6（Task 1）✓ P7（Task 3 Step 3 workspace.rs）✓ P8（Task 3 main.rs 转发任务）✓ P9（Task 3 file_ls_diags）✓ P10（Task 2 结构）✓——spec §6 验收全落在 Task 1/3/4。
- **占位符**：Task 3 Step 1 的 `ws2` 分支标注「见 Step 3 完整测试代码」——Step 3 文字已给出完整断言内容，执行时照写。
- **类型一致性**：`DrainFacts`/`file_ls_diags`/`drain`/`DiagnosticScheduler` 各 API 在 Task 2/3 间签名一致（已逐字核对）。
