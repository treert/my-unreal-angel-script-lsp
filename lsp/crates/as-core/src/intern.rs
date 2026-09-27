//! intern 基础设施（实现优化.md 条目 1/2）：
//!
//! - **`FileId` 注册表**：append-only + 墓碑语义（D18）；
//! - **`Sym` intern**：`lasso::ThreadedRodeo`（读路径并发）。
//!
//! 两表都是 as-core 内的进程级全局 static——as-core 的「纯库」边界因此修正为
//! 「无 IO、无 async、允许只增 intern 表」（实现优化 §2.4）。interning 幂等、
//! append-only、线程安全，对单测并行无副作用。
//!
//! 墓碑的硬性约束（实现优化 §1.4）：所有遍历型查询必须过滤 `!alive`，
//! `iter_alive()` 是唯一合法的遍历入口，裸 `by_id` 迭代视为错误用法。

use std::collections::HashMap;
use std::fmt;
use std::sync::{OnceLock, RwLock};

use lasso::{Capacity, ThreadedRodeo};

use crate::id::{FileId, Sym};

// ---------------------------------------------------------------------------
// Sym intern（实现优化 条目 2）
// ---------------------------------------------------------------------------

/// 初始容量：20 万符号 / 8MB（实现优化 §2.3）。
const SYM_INITIAL_CAPACITY: usize = 200_000;
const SYM_INITIAL_BYTES: usize = 8 * 1024 * 1024;

static SYM_INTERN: OnceLock<ThreadedRodeo> = OnceLock::new();

fn sym_rodeo() -> &'static ThreadedRodeo {
    SYM_INTERN.get_or_init(|| {
        let bytes = std::num::NonZeroUsize::new(SYM_INITIAL_BYTES)
            .expect("SYM_INITIAL_BYTES is non-zero");
        ThreadedRodeo::with_capacity(Capacity::new(SYM_INITIAL_CAPACITY, bytes))
    })
}

/// intern 一个符号名（幂等）。
pub fn intern_sym(s: &str) -> Sym {
    Sym::from_spur(sym_rodeo().get_or_intern(s))
}

/// 取回符号的原始字符串。
///
/// # Panics
/// 若 `Sym` 不是经 `intern_sym` 产生的（本库内不可能——`Sym` 字段私有，
/// 只能经 intern 构造）。
pub fn sym_str(sym: Sym) -> &'static str {
    sym_rodeo().resolve(&sym.as_spur())
}

impl fmt::Display for Sym {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(sym_str(*self))
    }
}

// ---------------------------------------------------------------------------
// FileId 注册表（实现优化 条目 1）
// ---------------------------------------------------------------------------

/// 注册表初始容量：441 文件量级，Vec 自然增长即可（实现优化 §1.3）。
const FILE_INITIAL_CAPACITY: usize = 1024;

/// 一个已注册文件的元数据。
#[derive(Clone, Debug)]
pub struct FileMeta {
    /// `Box::leak` 的路径字符串（进程级生命周期，实现优化 §1.2）
    pub path: &'static str,
    /// 根序优先级——多根 workspace 同名冲突按「项目根优先于引擎根」消解（架构设计 §4.2）
    pub root_index: u32,
    /// 墓碑位（D18）：删除文件只打标记，不回收 id
    pub alive: bool,
}

struct FileRegistry {
    by_path: HashMap<&'static str, FileId>,
    by_id: Vec<FileMeta>,
}

static FILES: OnceLock<RwLock<FileRegistry>> = OnceLock::new();

fn files() -> &'static RwLock<FileRegistry> {
    FILES.get_or_init(|| {
        RwLock::new(FileRegistry {
            by_path: HashMap::new(),
            by_id: Vec::with_capacity(FILE_INITIAL_CAPACITY),
        })
    })
}

/// 注册（或复用）一个文件路径，返回其 `FileId`。
///
/// - 首次注册：分配新 id（append-only，永不回收）；
/// - 同路径重现：**复用原 id 并翻回 alive**（墓碑语义，D18），
///   `root_index` 以最新一次注册为准；
/// - 快路径只拿读锁，miss / 需复活才升级写锁 + 双检查（实现优化 §1.2）。
pub fn intern_file(path: &str, root_index: u32) -> FileId {
    let reg = files();

    // 快路径：已注册且存活
    {
        let r = reg.read().unwrap();
        if let Some(&id) = r.by_path.get(path) {
            if r.by_id[id.as_usize()].alive {
                return id;
            }
        }
    }

    let mut w = reg.write().unwrap();
    // 双检查（快路径读锁期间的并发注册）
    if let Some(&id) = w.by_path.get(path) {
        let meta = &mut w.by_id[id.as_usize()];
        meta.alive = true;
        meta.root_index = root_index;
        return id;
    }

    let leaked: &'static str = Box::leak(path.to_string().into_boxed_str());
    let id = FileId::from_raw(w.by_id.len() as u32);
    w.by_id.push(FileMeta {
        path: leaked,
        root_index,
        alive: true,
    });
    w.by_path.insert(leaked, id);
    id
}

/// 取文件元数据（含墓碑行；`alive` 由调用方过滤）。
pub fn file_meta(file: FileId) -> Option<FileMeta> {
    files().read().unwrap().by_id.get(file.as_usize()).cloned()
}

/// 取文件路径。墓碑行的 path 仍在（id 与 path 保留，实现优化 §1.4）。
pub fn file_path(file: FileId) -> Option<&'static str> {
    files()
        .read()
        .unwrap()
        .by_id
        .get(file.as_usize())
        .map(|m| m.path)
}

/// 打墓碑：文件删除。id 与 path 保留；该 FileId 的 DefId / UseSite 摘除由
/// 索引层负责（M1，LSP实现规划 §5.3），本层只管注册表状态。
pub fn tombstone_file(file: FileId) -> bool {
    match files().write().unwrap().by_id.get_mut(file.as_usize()) {
        Some(meta) => {
            meta.alive = false;
            true
        }
        None => false,
    }
}

/// **唯一合法的遍历入口**：只产出 alive 的文件（墓碑被过滤）。
/// 441 文件量级直接收集为 Vec，不做借用式迭代器。
pub fn iter_alive() -> Vec<(FileId, FileMeta)> {
    files()
        .read()
        .unwrap()
        .by_id
        .iter()
        .enumerate()
        .filter(|(_, meta)| meta.alive)
        .map(|(i, meta)| (FileId::from_raw(i as u32), meta.clone()))
        .collect()
}

/// 注册过的文件总数（含墓碑；append-only 注册表的物理长度）。
pub fn file_count() -> usize {
    files().read().unwrap().by_id.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    // 注意：注册表是进程级全局，测试并行运行——路径一律用本测试独占前缀。

    #[test]
    fn sym_intern_is_idempotent_and_resolves() {
        let a = intern_sym("FVector");
        let b = intern_sym("FVector");
        assert_eq!(a, b, "Sym 相等即同一：同字符串必须拿到同一 id");
        assert_eq!(sym_str(a), "FVector");
        assert_eq!(a.to_string(), "FVector");
    }

    #[test]
    fn file_id_intern_is_idempotent() {
        let a = intern_file("unique://idempotent/a.as", 0);
        let b = intern_file("unique://idempotent/a.as", 0);
        assert_eq!(a, b);
        assert_eq!(file_path(a), Some("unique://idempotent/a.as"));
        assert!(file_meta(a).unwrap().alive);
    }

    #[test]
    fn tombstone_then_revive_reuses_id() {
        let id = intern_file("unique://tombstone/x.as", 1);
        assert!(file_meta(id).unwrap().alive);
        assert!(tombstone_file(id));

        // 墓碑行：alive=false，但 id 与 path 都还在
        let meta = file_meta(id).unwrap();
        assert!(!meta.alive);
        assert_eq!(meta.path, "unique://tombstone/x.as");
        assert!(
            !iter_alive().into_iter().any(|(fid, _)| fid == id),
            "iter_alive 必须过滤墓碑"
        );

        // 同路径重现：复用原 id、翻回 alive、root_index 取最新
        let id2 = intern_file("unique://tombstone/x.as", 2);
        assert_eq!(id, id2, "同路径重现必须复用原 FileId（D18）");
        let meta = file_meta(id2).unwrap();
        assert!(meta.alive);
        assert_eq!(meta.root_index, 2);
        assert!(iter_alive().into_iter().any(|(fid, _)| fid == id2));
    }

    #[test]
    fn append_only_never_reuses_ids() {
        // 并行测试共享全局注册表，不能断言精确增量——只断言单调不变量：
        // 后注册的 id 严格大于先注册的，且物理长度覆盖两者（id 永不回收）。
        let a = intern_file("unique://append/a.as", 0);
        let b = intern_file("unique://append/b.as", 0);
        assert_ne!(a, b, "不同路径必须拿到不同 FileId");
        assert!(b.as_raw() > a.as_raw(), "注册必须单调递增（append-only）");
        assert!(
            file_count() as u32 > b.as_raw(),
            "已分配的 id 必须始终在注册表内"
        );
    }
}
