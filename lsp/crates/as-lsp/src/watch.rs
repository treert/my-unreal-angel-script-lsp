//! DidChangeWatchedFiles（LSP实现规划 §5.3 / D18 / D24 / D26 裁决二）。
//!
//! - **`.as` 增删改名**：新增/变更 = 读盘 → 单文件入索引（`reindex_file_full`
//!   ——FileId 复用/新建，模块名随新路径重算）；删除 = `remove_file` + 墓碑。
//!   改名 = 删 + 增两个事件，天然覆盖（模块名随路径变——local 可见域随之改变）；
//! - **`.d.as` 任一变化** = 整目录全量重建 + 防抖（D24：导出器恒清空重写，
//!   变更集恒等全集，增量路径写了也用不上；防抖只防重复做功，正确性靠
//!   `publish_and_replay` 的换根——旧快照全程可服务）；
//! - **overlay 优先**（§5.1）：文档打开中的文件忽略监视事件（外部工具改盘
//!   不引发抖动）；
//! - `_manifest.dctx` 不匹配 `*.as`，天然不在监视范围（D20）。
//!
//! 防抖时序（D24 裁决四）：500ms 静默窗（窗口内事件只重置计时器）+
//! **5s 硬上限**（累计等待超时即强制重建，防慢盘写盘期间计时器被无限
//! 推迟；其后事件再走一轮）。

use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tower_lsp_server::ls_types::{self as ls, FileChangeType};

use crate::docs::DocStore;
use crate::workspace::{self, WorkspaceConfig, WorkspaceState};

/// `.d.as` 防抖静默窗（D24）。
pub const DECL_SILENCE: Duration = Duration::from_millis(500);
/// `.d.as` 防抖硬上限（D24 裁决四）。
pub const DECL_HARD_CAP: Duration = Duration::from_secs(5);

/// 事件处置分类（纯函数，单测覆盖）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WatchAction {
    /// `.d.as` 任一变化 → 送防抖器（整目录全量重建，D24）
    DeclChange,
    /// `.as` 新增（改名 = 删 + 增，两个事件天然覆盖）
    ScriptCreate,
    /// `.as` 删除
    ScriptDelete,
    /// `.as` 磁盘内容变化（读盘重索引）
    ScriptChange,
    /// 忽略：非 `.as`（`_manifest.dctx` 天然不匹配）/ 有 overlay（§5.1）/
    /// 不在收集范围
    Ignore,
}

/// 事件分类。`has_overlay`：文档打开中（overlay 优先）；`relevant`：路径在
/// 收集根下或已在索引中。
pub fn classify(typ: FileChangeType, path: &str, has_overlay: bool, relevant: bool) -> WatchAction {
    let lower = path.to_ascii_lowercase();
    if !lower.ends_with(".as") {
        return WatchAction::Ignore;
    }
    if has_overlay {
        return WatchAction::Ignore;
    }
    if !relevant {
        return WatchAction::Ignore;
    }
    if lower.ends_with(".d.as") {
        return WatchAction::DeclChange;
    }
    if typ == FileChangeType::CREATED {
        WatchAction::ScriptCreate
    } else if typ == FileChangeType::CHANGED {
        WatchAction::ScriptChange
    } else if typ == FileChangeType::DELETED {
        WatchAction::ScriptDelete
    } else {
        WatchAction::Ignore
    }
}

/// `.d.as` 防抖器。
pub struct DeclDebouncer {
    tx: Sender<()>,
}

impl DeclDebouncer {
    /// 生产入口：触发动作 = 读**当前**配置与根的全量重建（配置可能在
    /// 防抖线程存活期间变更，不能在 spawn 时快照）。
    pub fn spawn(
        cfg: Arc<Mutex<WorkspaceConfig>>,
        folders: Arc<Mutex<Vec<String>>>,
        docs: Arc<Mutex<DocStore>>,
        ws: Arc<WorkspaceState>,
    ) -> DeclDebouncer {
        Self::spawn_with_timers(DECL_SILENCE, DECL_HARD_CAP, move || {
            run_rebuild(&cfg, &folders, &docs, &ws);
        })
    }

    /// 测试入口（时序参数可调；`fire` 可注入）。
    pub fn spawn_with_timers<F>(silence: Duration, hard_cap: Duration, fire: F) -> DeclDebouncer
    where
        F: FnMut() + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || debounce_loop(&rx, silence, hard_cap, fire));
        DeclDebouncer { tx }
    }

    /// 一个 `.d.as` 事件（重置静默窗计时器）。
    pub fn ping(&self) {
        let _ = self.tx.send(());
    }
}

/// 防抖主循环。两段结构：
/// - 外层 `recv()` **阻塞**等本轮首事件——静默期（fire 之后）不做任何事，
///   这是「不重复 fire」的关键；
/// - 内层窗口期：等待时长 = min(静默窗, 硬上限剩余)——窗口内事件重置静默
///   计时；静默到点或硬上限到点（无论事件是否持续）⇒ fire 一次，回到外层。
fn debounce_loop<F>(rx: &Receiver<()>, silence: Duration, hard_cap: Duration, mut fire: F)
where
    F: FnMut(),
{
    loop {
        // 等本轮首事件（阻塞；通道关闭则收尾退出）
        match rx.recv() {
            Ok(()) => {}
            Err(_) => break,
        }
        let started = Instant::now();
        loop {
            let wait = silence.min(hard_cap.saturating_sub(started.elapsed()));
            match rx.recv_timeout(wait) {
                Ok(()) => {
                    if started.elapsed() >= hard_cap {
                        break; // 硬上限：强制重建，其后事件再走一轮
                    }
                }
                Err(RecvTimeoutError::Timeout) => break, // 静默窗（或上限）到点
                Err(RecvTimeoutError::Disconnected) => {
                    fire(); // 冲刷待办（测试断言依赖）
                    return;
                }
            }
        }
        fire();
    }
}

/// 全量重建（`.d.as` 防抖触发 / 索引级配置变更共用）：读当前配置与根，
/// 后台构造新快照 → `publish_and_replay` 换根发布（旧快照全程可服务，
/// D24：「引擎类型瞬间全空」的中间态从根上不存在）。
///
/// 互斥：`WorkspaceState::building` 自旋占用——防抖触发与 `didChange
/// Configuration` 触发的重建不并发（两者建的都是同一磁盘状态的快照，
/// 排队重做一次的代价可接受，换来不做增量合并）。
pub fn run_rebuild(
    cfg: &Arc<Mutex<WorkspaceConfig>>,
    folders: &Arc<Mutex<Vec<String>>>,
    docs: &Arc<Mutex<DocStore>>,
    ws: &Arc<WorkspaceState>,
) {
    while ws.building.swap(true, Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let cfg = cfg.lock().unwrap().clone();
    let folders = folders.lock().unwrap().clone();
    let overlays = {
        let store = docs.lock().unwrap();
        store.overlays()
    };
    let idx = workspace::build_index(&cfg, &folders, &overlays);
    ws.publish_and_replay(idx, docs);
    ws.building.store(false, Ordering::SeqCst);
}

/// 动态注册 `workspace/didChangeWatchedFiles`（watcher `**/*.as` 同时覆盖
/// `.d.as`；kind 7 = Create|Change|Delete）。失败只记日志（降级：无文件
/// 监视，.as 增删改名不感知——语义请求仍正确，只是索引不更新）。
pub async fn register_watcher(client: &tower_lsp_server::Client) {
    let options = ls::DidChangeWatchedFilesRegistrationOptions {
        watchers: vec![ls::FileSystemWatcher {
            glob_pattern: ls::GlobPattern::String("**/*.as".to_string()),
            kind: Some(ls::WatchKind::Create | ls::WatchKind::Change | ls::WatchKind::Delete),
        }],
    };
    let registration = ls::Registration {
        id: "watch-as-files".to_string(),
        method: "workspace/didChangeWatchedFiles".to_string(),
        register_options: serde_json::to_value(options).ok(),
    };
    match client
        .register_capability(vec![registration])
        .await
    {
        Ok(()) => {}
        Err(e) => {
            let _ = client
                .log_message(ls::MessageType::WARNING, format!("watcher registration failed: {e}"))
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::FileChangeType;

    fn created() -> FileChangeType {
        FileChangeType::CREATED
    }
    fn changed() -> FileChangeType {
        FileChangeType::CHANGED
    }
    fn deleted() -> FileChangeType {
        FileChangeType::DELETED
    }

    #[test]
    fn classify_rules() {
        use WatchAction::*;
        // .as 三类
        assert_eq!(classify(created(), "D:\\proj\\New.as", false, true), ScriptCreate);
        assert_eq!(classify(changed(), "D:\\proj\\Mod.as", false, true), ScriptChange);
        assert_eq!(classify(deleted(), "D:\\proj\\Old.as", false, true), ScriptDelete);
        // .d.as：create/change/delete 一律全量重建
        for t in [created(), changed(), deleted()] {
            assert_eq!(classify(t, "D:\\proj\\Core.d.as", false, true), DeclChange);
        }
        // overlay 优先（§5.1）：打开中的文件忽略
        assert_eq!(classify(created(), "D:\\proj\\Open.as", true, true), Ignore);
        // 不在收集范围
        assert_eq!(classify(changed(), "D:\\elsewhere\\X.as", false, false), Ignore);
        // 非 .as（_manifest.dctx 天然不匹配——D20）
        assert_eq!(classify(changed(), "D:\\proj\\_manifest.dctx", false, true), Ignore);
        // 大小写后缀
        assert_eq!(classify(created(), "D:\\proj\\X.D.AS", false, true), DeclChange);
    }

    #[test]
    fn debounce_collapses_burst_into_one_rebuild() {
        // 连发事件（间隔远小于静默窗）⇒ 静默后只重建一次（D24：防抖只防
        // 重复做功）。并行测试下 sleep 有抖动，余量按 10 倍放
        let fires = Arc::new(Mutex::new(0usize));
        let counter = Arc::clone(&fires);
        let d = DeclDebouncer::spawn_with_timers(
            Duration::from_millis(500),
            Duration::from_secs(10),
            move || {
                *counter.lock().unwrap() += 1;
            },
        );
        for _ in 0..5 {
            d.ping();
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(1200));
        let n = *fires.lock().unwrap();
        assert_eq!(n, 1, "连发 5 事件只应重建一次，实际 {n}");
    }

    #[test]
    fn debounce_hard_cap_forces_rebuild_under_continuous_events() {
        // 持续事件（间隔小于静默窗）+ 硬上限 ⇒ 到点强制重建，不被无限推迟。
        // 静默窗设得极长——只允许硬上限路径触发，判定不受抖动影响
        let fires = Arc::new(Mutex::new(0usize));
        let counter = Arc::clone(&fires);
        let d = DeclDebouncer::spawn_with_timers(
            Duration::from_secs(5),
            Duration::from_millis(300),
            move || {
                *counter.lock().unwrap() += 1;
            },
        );
        // 事件持续 ~600ms > 硬上限 300ms
        for _ in 0..10 {
            d.ping();
            std::thread::sleep(Duration::from_millis(60));
        }
        std::thread::sleep(Duration::from_millis(700));
        let n = *fires.lock().unwrap();
        assert!((1..=3).contains(&n), "硬上限应在持续事件下触发重建（1-3 次皆可），实际 {n}");
    }
}
