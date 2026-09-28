# 索引重构 Phase D 设计：语义诊断调度队列

> 版本：v1（2026-09-28，brainstorming 收敛终版）
>
> **定位**：`docs/index-architecture.md`（v1.4）§5.5「语义诊断」行的实施设计
> （§11 步骤 6 的**诊断调度**半边；增量更新路径仍属 Phase E）。前置
> Phase A/B/C（D37/D38/D39）已完成，TypeId 体系已退役，L3 全现算零缓存。
>
> 参考实现：`ai-mylua-lsp` `diagnostic_scheduler.rs`（结构同构；AS 侧简化点见 §2/§3）。

## 1. 裁决（brainstorming 定案）

| # | 裁决 | 理由 |
|---|---|---|
| P1 | **D36 全量翻案**：didOpen/didChange 同步推无防抖 → **300ms 防抖、单发布通道**（覆盖式全量发布，无合并语义）；D40 记翻案 | §5.5 原始依据（语义诊断归队列）；AS0903 反馈延迟 300ms 属业界常态（rust-analyzer / TS server 均防抖）；单通道无特例 |
| P2 | **cyclic_classes() 本期接消费方**：`AS0907` 继承环 | C4 预留的诊断素材接口；非平凡工作区级消费者，验证级联 / seed 路径；码表登记（下一个可用号） |
| P3 | **固定 Full**：队列全集 = 索引内全部 **Script** 文件（`.d.as` 剔除——必然合法，§1.5 语言前提），未打开文件也发布（Problems 可见）；**不新增 vscode 配置** | YAGNI——P5 语义诊断变贵时再加 mylua 式 scope |
| P4 | **级联 = 类型结构变化 → 全量重诊断**；不做恒级联、不做「只推变化」 | mylua 原味：平时 drain 只诊断热文件（不吵）；全量重诊断时的空集发布是**清 stale（跨文件环修复）的必要手段** |
| P5 | **插队 = 热文件表 pop 优先** | didOpen / didChange 都标热（seq 降序先出）；drain 中途新事件即刻插队——正确性无损（最坏多算一次），窗口小（drain 只在 300ms 静默后发生） |

**实施裁决**：

| # | 裁决 | 理由 |
|---|---|---|
| P6 | `decl_surface` 元组**扩含 bases** | 「`class A : B` → `class A : C`」必须算结构变化，否则跨文件造环/破环波及的未打开文件诊断漏报。仅扩展 diff 字段集，Phase E 的**定向失效**消费不因此提前 |
| P7 | `ensure_file_fresh` / `add_file` 的 **surface 布尔上供** | didChange 的重索引是 drain 时惰性做的（既有架构），结构变化在 drain 头检测；watch 侧事件时即知（`reindex_file_full` 返回值既有） |
| P8 | **diag_tx 通道保留**，语义改为「请求全量 + drain」（吃掉 `publish_all_open`） | 避免 WorkspaceState ↔ Scheduler 的 Arc 环；与 `myas/indexStatus` 同一转发模式不动 |
| P9 | **AS0902 发布面扩展**：从「已打开 Script 文档」扩到「全部 Script 文件」 | Full 档的自然语义；D40 对 D36 的第二处修订 |
| P10 | **调度器纯数据结构**（无 ws/docs 依赖），scope 收集全在 consumer | 单一收集点；可测性（无需 mylua 的 FileSource::Static 抽象） |

## 2. 终态架构

```
平时（编辑会话）：
  didOpen/didChange（Ready）─→ 标热（hot 表）─→ 300ms 防抖（gen 合并）─→ drain 只诊断热文件

全量重诊断（三来源）：
  ① drain 头 ensure_file_fresh 检出 surface_changed（含改基类，P6）
  ② watch Script 增/改：add_file 返回 surface 布尔 → schedule(file, full=true)
  ③ 快照发布（冷启动 / .d.as 防抖重建 / 配置变更）：
     publish_and_replay ─diag_tx()─→ 转发任务 request_full()（bypass 防抖）

Loading 期不 schedule（didOpen/didChange 的 is_ready 门控，既有），seed 兜底。
didClose：立即推空数组清空（D36 行为保留）+ invalidate。
watch ScriptDelete：remove_file（既有）+ invalidate。
```

**drain 流程**（main.rs consumer 单任务：`loop { notified().await; drain() }`）：

1. `ensure_file_fresh` **全部打开文档**（工作区事实要看最新 overlay；版本比对 O(打开数)），
   收集每个文件的 surface_changed（P7）；
2. `take_full_flag()`（schedule 带 full / request_full 置位）或 ① 检出为真
   → 收集全量队列：**打开文件在前，其余 FileId 升序**（P3 口径：索引内 Script 文件，
   剔除热表成员），`set_full_queue()` 喂入；
3. 工作区事实**每 drain 算一次**（ms 级）：
   `decl_missing = !has_decl_files()`；`cycle_diags = ws.cycle_diags()`（P2）；
4. pop 循环：**热（seq 降序）→ 队列**（queued 集校验跳 stale；pop 热时同步摘队列防双发）；
   打开文件取 DocStore overlay（tree + version），未打开取索引 `FileEntry`（version=None）；
   诊断 = AS0903 ∪ AS0902（`decl_missing && Script`，P9）∪ AS0907（经抑制过滤）→ publish；
5. pop 出的文件已不在索引（并发删除）→ 跳过。

## 3. 调度器（as-lsp 新增 `src/diagnostic_scheduler.rs`）

纯数据结构（mylua 同构薄化：无 scope 配置、无 FileSource 抽象、modified/explicit 合并为 hot）：

```rust
pub const DIAGNOSTIC_DEBOUNCE_MS: u64 = 300;

struct Inner {
    hot: HashMap<FileId, u64>,     // 未 pop 的热文件；值 = seq（新者优先，pop 前一直保留）
    seq: u64,
    queue: VecDeque<FileId>,      // 全量队列（排序由收集方排好：打开在前 → FileId 升序）
    queued: HashSet<FileId>,
    gen: u64,                      // 防抖合并：仅最新 gen 的 timer 有效
    pending_full: bool,            // 本轮防抖到期后需全量
}

pub struct DiagnosticScheduler { inner: Mutex<Inner>, notify: Notify }

impl DiagnosticScheduler {
    pub fn new() -> Arc<Self>;
    pub fn schedule(self: &Arc<Self>, file: FileId, full: bool); // 热标记 + 防抖 spawn
    pub fn request_full(&self);                                   // seed：bypass 防抖，立即 notify
    pub fn pop(&self) -> Option<FileId>;                          // 热优先（同步摘队列）→ 队列
    pub async fn notified(&self);
    pub fn take_full_flag(&self) -> bool;                         // drain 头消费（一次性）
    pub fn set_full_queue(&self, files: Vec<FileId>);             // consumer 排好序喂入
    pub fn invalidate(&self, file: FileId);                        // 删除/关闭清理（热 + 队列）
    fn spawn_debounce(self: &Arc<Self>, gen: u64);                // 300ms 后 gen 未变则 notify
    // *_now_for_test 变体（防抖旁路，单测用）
}
```

与 mylua 的对应 / 差异：`modified` + `explicit` 合并为 `hot`（didOpen 也插队，P5）；
scope 恒 Full（P3）；文件源由调用方传入（P10）；`seed_workspace` 拆为
`request_full`（标记）+ consumer 收集（drain 头统一做）。

## 4. as-core 变更

1. `DiagCode::As0907`（`parse` / `Display` 延伸；码表 v1.0 登记）；
2. `Workspace::cycle_diags() -> HashMap<FileId, Vec<Diag>>`：
   `cyclic_classes()` 每个环成员类在其 **`bases[0].span`**（base 名字区间，`BaseRef` 已带）
   产一条 Error（message 措辞实现期定，形如 `class 'X' participates in an
   inheritance cycle`）；**Decl 文件成员跳过**——导出器不产环，腐坏导出不在诊断面
   （代码注释 + 本 spec 文档化）；
3. 抑制过滤抽出 `pub fn filter_suppressions(diags, text)`：
   `script_diags` 内部复用；AS0907 消费同套 `// as-ignore` 语法
   （`as-ignore-next-line` 命中 base 行即生效）；
4. `decl_surface` 扩含 sorted base 名列表：
   `(Sym, DefKind, Option<Sym>)` → `(Sym, DefKind, Option<Sym>, Vec<Sym>)`（P6）；
5. as-lsp `WorkspaceState::ensure_file_fresh` / `add_file` 返回 surface_changed 布尔
   （`reindex_file` / `reindex_file_full` 本就返回，上供即可，P7）。

## 5. as-lsp 集成

- **main.rs**：
  - 调度器实例化 + consumer 任务（`loop { notified().await; drain().await }`）；
  - diag_tx 转发任务改为 `request_full()`（`publish_all_open` 调用点移除，P8）；
  - didOpen：Ready → `schedule(file, false)`；Loading → `mark_dirty`（既有）；
  - didChange：Ready → `schedule(file, false)`；
  - didClose：推空（保留 D36）+ `invalidate`；
  - watch ScriptCreate/ScriptChange：`add_file` 返回 surface → `schedule(file, surface)`；
    ScriptDelete → `invalidate`；
- **diag.rs**：
  - `publish_file`（didOpen/didChange 直推路径）**删除**——统一走调度器（P1）；
  - `publish_all_open` **删除**（diag_tx 语义转移，P8）；
  - 新增 drain 支撑：`DrainFacts { decl_missing, cycle_diags }` 计算与 per-file 统一入口
    `file_ls_diags(source, tree, lines, is_script, facts)`（打开/未打开同构；
    UTF-16 映射与协议映射平移既有 `doc_ls_diags` 逻辑）。

## 6. 测试与验收

- **调度器单测**（mylua 9 条平移改写）：热优先 / 最新先出；防抖 gen 合并；
  Notify 唤醒 consumer；full flag 消费；`set_full_queue` 按序出；
  热 + 队列去重（pop 热时摘队列）；invalidate 清热 + 队列；
- **as-core**：cycle_diags（双文件互继承 → 两侧各一条落 base span；Decl 成员跳过；
  无环空集）；decl_surface 扩 bases（改基类 → true、增删类 → true、局部变量改 → false）；
  filter_suppressions 对 AS0907 生效（next-line 命中 base 行）；
- **as-lsp**：ensure_file_fresh surface 布尔上供；未打开文件经 FileEntry 计算
  （AS0902 / AS0903）；既有 `doc_ls_diags` 2 条测试适配平移；
- **基线**：`cargo test --workspace` 160 + 新增全绿（Rust 单测 .as 用例内置字符串字面量，D1）；
  as-cli 三旗标语料验收不回归（resolve-stats 94.9% / ref-stats 25505 / decls 101322
  ——as-core 既有路径零语义改动，AS0907 是纯增量）。

## 7. 风险

| # | 风险 | 缓解 |
|---|---|---|
| 1 | 300ms 延迟体感（D36 翻案代价） | 业界常态；若实测不可接受，后续单独评估 didOpen fast-path（本期不做） |
| 2 | drain 中途的热文件即达即诊断（未再防抖） | drain 只在 300ms 静默后发生，窗口小；正确性无损，最坏多算一次（P5） |
| 3 | 全量 drain 的 441 空集发布 | 仅结构变化 / 重建触发，量级可接受；若实测吵再补「只推变化」（本期明确不做，P4） |
| 4 | decl_surface 扩 bases 与 Phase E 定向失效口径耦合 | Phase E 消费 diff 时按新字段集设计；本 spec P6 已记 |
| 5 | cyclic 全扫每 drain 一次 | 语料 ms 级；超大工作区时再评估按代缓存（零缓存哲学 C6 优先） |

## 8. 后续（不在本批）

- Phase E：scope_tree 消费、reindex 声明面 diff 定向失效消费方（届时 cascade 可收敛
  为 surface 精确子集）、文档同步；
- P5：语义诊断扩充（AS04xx）时评估 scope 配置与只推变化；
- 文档：D40（决策记录 + 索引表）、设计稿 v1.5（§5.5 / §11 映射）、
  诊断码表 v1.0（AS0907）、LSP实现规划 §8.1 时序更新——随 Phase D 收尾 Task 落。
