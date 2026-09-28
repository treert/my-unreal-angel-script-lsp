//! DiagnosticScheduler——语义诊断调度（Phase D / D40，mylua 同构薄化）。
//!
//! - **热文件表**（didOpen/didChange 标记，seq 新者优先，pop 前一直保留）：
//!   drain 中途的新事件即刻插队——`pop` 每次先看热表（spec P5）；
//! - **全量队列**只在结构变化（surface_changed）或快照发布（`request_full`）
//!   时由收集方（consumer 的 `diag::drain`）喂入：打开文件在前，其余
//!   FileId 升序（spec P3/P4）；
//! - **300ms 防抖**（gen 合并，仅最新 gen 的 timer 有效）；
//! - 本结构是**纯数据**（spec P10）：不持有 ws/docs，scope 收集全在
//!   as-lsp `diag::drain`。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use as_core::id::FileId;

/// 防抖静默窗（mylua 同款）。
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
    /// 立即唤醒 consumer 做全量（吃掉旧 publish_all_open 语义，spec P8）。
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
        let seq = inner.seq;
        inner.hot.insert(file, seq);
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

    #[test]
    fn pop_returns_hot_immediately_for_queue_jump() {
        // P5（设计语义，与 mylua 差异）：防抖只门控 consumer 唤醒（notify），
        // 不门控 pop——drain 中途的新事件即刻插队。consumer 平时只在
        // 防抖到期后被唤醒，故无「每键一算」问题。
        let s = DiagnosticScheduler::new();
        s.schedule_now_for_test(id(1), false);
        assert_eq!(s.pop(), Some(id(1)), "热文件即时可取（插队语义）");
        assert_eq!(s.pop(), None);
    }

    #[tokio::test]
    async fn schedule_debounces_notify_with_gen_collapse() {
        let s = DiagnosticScheduler::new();
        s.schedule(id(1), false);
        tokio::time::sleep(Duration::from_millis(50)).await;
        s.schedule(id(2), false);
        tokio::time::sleep(Duration::from_millis(50)).await;
        s.schedule(id(1), false); // 重置窗口（gen 合并）
        // 防抖窗内（距最后事件 ~100ms < 300ms）不应唤醒
        let r = tokio::time::timeout(Duration::from_millis(150), s.notified()).await;
        assert!(r.is_err(), "防抖窗内不应唤醒");
        // 静默满 300ms 后唤醒（旧 gen 的 timer 到期不误唤醒）
        let r = tokio::time::timeout(Duration::from_millis(400), s.notified()).await;
        assert!(r.is_ok(), "静默后应唤醒");
    }

    #[tokio::test]
    async fn notify_wakes_consumer() {
        let s = DiagnosticScheduler::new();
        s.schedule_now_for_test(id(1), false);
        let s2 = s.clone();
        let handle = tokio::spawn(async move {
            s2.notified().await;
            assert_eq!(s2.pop(), Some(id(1)), "唤醒后热文件可取");
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        s.request_full(); // bypass 防抖立即唤醒
        tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("consumer 应在 500ms 内被唤醒")
            .expect("任务应正常结束");
    }
}
