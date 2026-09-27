//! 文件日志（仿 mylua-lsp `logger.rs`）：`myAngelScriptLsp.debug.fileLog`
//! 开启时写 `<第一个 workspace folder>/.vscode/my-as-lsp.log`，并双写
//! stderr——被 vscode-languageclient 捕获进「my-as-lsp」输出通道。
//!
//! 设计要点（与 mylua 同款）：
//! - **快速门**：`as_log!` 宏先读 `AtomicBool`（单次 relaxed load）再
//!   `format!`——关闭时热路径零分配、零锁、零 syscall；
//! - **复用句柄**：writer 只在 init 打开一次。逐行 open/append/close 会把
//!   每条日志串到内核 inode 锁上——冷启动 rayon 并行索引期是灾难；
//! - **截断式**：每次会话重开（日志只反映本次运行）；
//! - **多根工作区只写第一个根**（日志只需要一份）；
//! - **重启生效**：`debug.fileLog` 不在 `didChangeConfiguration` 的动态消费
//!   集里——改配置后须重启 server（与 mylua 同语义）。

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use chrono::Local;

/// `as_log!` 的快速门。
static ENABLED: AtomicBool = AtomicBool::new(false);

/// writer 持有者（init 一次性建立）。
static WRITER: Mutex<Option<BufWriter<File>>> = Mutex::new(None);

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// `[HH:MM:SS.mmm]` 本地时间前缀。
fn now_local_str() -> String {
    Local::now().format("[%H:%M:%S%.3f]").to_string()
}

/// 初始化（`initialized` 时在 `load_config` 之后调用，早于索引构建）。
/// 无 workspace folder（裸会话）时只保留 stderr 双写。
pub fn init(folders: &[String], enable_file_log: bool) {
    ENABLED.store(enable_file_log, Ordering::Relaxed);
    let new_writer = if enable_file_log && !folders.is_empty() {
        let vscode_dir = Path::new(&folders[0]).join(".vscode");
        let _ = std::fs::create_dir_all(&vscode_dir);
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(vscode_dir.join("my-as-lsp.log"))
            .ok()
            .map(BufWriter::new)
    } else {
        None
    };
    if let Ok(mut w) = WRITER.lock() {
        *w = new_writer;
    }
    if !enable_file_log {
        return;
    }
    // 会话头（fileLog 关闭时不产生任何输出）
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    log(&format!(
        "=== my-as-lsp started (profile={}, arch={}) ===",
        profile,
        std::env::consts::ARCH,
    ));
    // 可执行文件路径 + 修改时间：快速确认跑的是正确的二进制且是最新的
    match std::env::current_exe() {
        Ok(exe) => {
            let mtime = std::fs::metadata(&exe)
                .and_then(|m| m.modified())
                .map(|t| {
                    let dt: chrono::DateTime<Local> = t.into();
                    dt.format("%Y-%m-%d %H:%M:%S %Z").to_string()
                })
                .unwrap_or_else(|_| "unknown".to_string());
            log(&format!("[my-as-lsp] executable: {} (modified: {mtime})", exe.display()));
        }
        Err(e) => log(&format!("[my-as-lsp] executable: <unknown> ({e})")),
    }
    if folders.is_empty() {
        log("[my-as-lsp] no workspace folder: file log disabled, stderr only");
    } else {
        log(&format!("[my-as-lsp] workspace folders: {}", folders.join(", ")));
    }
}

pub fn log(msg: &str) {
    if !enabled() {
        return;
    }
    let stamped = format!("{} {}", now_local_str(), msg);
    // 双写 stderr（→ VSCode「my-as-lsp」输出通道）
    eprintln!("{stamped}");
    // 从毒化 mutex 恢复：logger 无不变量要守，坏状态继续写好过永久吞日志
    let mut guard = WRITER.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(ref mut w) = *guard {
        // 每行 flush：`tail -f` 式排障保持实时性
        let _ = writeln!(w, "{stamped}");
        let _ = w.flush();
    }
}

/// 条件日志宏：先查开关再 format（热路径零成本）。
#[macro_export]
macro_rules! as_log {
    ($($arg:tt)*) => {
        if $crate::logger::enabled() {
            $crate::logger::log(&format!($($arg)*))
        }
    };
}
