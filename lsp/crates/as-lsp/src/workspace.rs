//! 工作区索引接入（LSP实现规划 §5/§6）：
//!
//! - **Phase 0**：scriptRoots（空 = 全部 workspaceFolders）+ typeDeclarationDirs
//!   （`${workspaceFolder}` 逐 folder 展开）递归收集；**收集规则**：任意 root 下
//!   `.d.as` → Decl、其余 `.as` → Script，路径去重——「AS-Cache 目录本身是
//!   workspace folder」的双根布局无需额外配置即可命中；
//! - **冷启动**（§6）：后台线程构建（overlay 优先，§5.1）→ 完成后原子发布
//!   （RwLock 换根，旧快照全程可服务）→ 重放 pending_dirty（§6.1）；
//! - **Loading 期间**：非语义请求即时服务（只依赖 DocStore）；语义请求返回空；
//! - **Ready 后**：didChange 不做重活，语义请求进来时**惰性单文件重索引**
//!   （§5.2 粗粒度 remove+re-add，毫秒级，天然合并连击键）；didClose 落回磁盘。
//!
//! 并发约束：不跨 await 持锁（tower-lsp-server Send 约束）。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};

use as_core::id::{FileId, Sym};
use as_core::intern::{file_id_of_path, file_path, intern_file};
use as_core::references::{resolve_file_uses, UseResolution};
use as_core::{filename_to_module_name, FileInput, FileKind, IndexConfig, WorkspaceIndex};

use as_core::as_log;

use crate::docs::DocStore;

/// 索引级配置（§5）。任一变更 ⇒ 后台全量重建（§5.3）。
#[derive(Clone, Debug)]
pub struct WorkspaceConfig {
    pub float_is_float64: bool,
    pub script_roots: Vec<String>,
    pub decl_dirs: Vec<String>,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        WorkspaceConfig {
            float_is_float64: true,
            script_roots: Vec::new(),
            // 与 vscode-extension package.json 默认一致
            decl_dirs: vec!["${workspaceFolder}/Saved/AS-Cache".to_string()],
        }
    }
}

/// 工作区状态：Loading/Ready 状态机 + 发布后的索引（§6.1）+ UseSite 解析缓存。
pub struct WorkspaceState {
    /// None = Loading；Some = Ready。后台线程建好后整体替换（快照语义）
    index: RwLock<Option<WorkspaceIndex>>,
    /// 已入索引的 overlay 版本
    indexed_versions: Mutex<HashMap<FileId, i32>>,
    /// Loading 期间的 didOpen/didChange（发布后重放，§6.1）
    dirty: Mutex<HashSet<FileId>>,
    /// didClose 后待回落磁盘的文件（§5.1）
    stale: Mutex<HashSet<FileId>>,
    /// UseSite 解析缓存（D5：Phase 3 惰性 + 按文件缓存；编辑失效粒度 =
    /// 文件，联动失效见 [`WorkspaceState::reindex`] 的声明面指纹判定）
    use_cache: Mutex<HashMap<FileId, Arc<Vec<UseResolution>>>>,
    /// 重建互斥（`.d.as` 防抖触发 vs 配置变更触发的全量重建不并发——
    /// `watch::run_rebuild` 自旋占用）
    pub building: AtomicBool,
    /// 索引就绪通知通道（`myas/indexStatus`）：后台 std 线程（冷启动 /
    /// 防抖重建）不能跨 await 调 client——经 unbounded channel 转给 main
    /// 里 spawn 的 tokio 转发任务。None = 通道未接（单测）
    ready_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<serde_json::Value>>>,
    /// 诊断补推通道（M6，D36）：快照发布后通知 tokio 侧对全部已打开文档
    /// 推一轮诊断（Loading 期打开的文件 + AS0902 随重建刷新）。
    /// None = 通道未接（单测）
    diag_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<()>>>,
}

impl WorkspaceState {
    pub fn new() -> Self {
        WorkspaceState {
            index: RwLock::new(None),
            indexed_versions: Mutex::new(HashMap::new()),
            dirty: Mutex::new(HashSet::new()),
            stale: Mutex::new(HashSet::new()),
            use_cache: Mutex::new(HashMap::new()),
            building: AtomicBool::new(false),
            ready_tx: Mutex::new(None),
            diag_tx: Mutex::new(None),
        }
    }

    /// 接上索引就绪通知通道（main 启动转发任务时调用）。
    pub fn set_ready_tx(&self, tx: tokio::sync::mpsc::UnboundedSender<serde_json::Value>) {
        *self.ready_tx.lock().unwrap() = Some(tx);
    }

    /// 接上诊断补推通道（main 启动转发任务时调用）。
    pub fn set_diag_tx(&self, tx: tokio::sync::mpsc::UnboundedSender<()>) {
        *self.diag_tx.lock().unwrap() = Some(tx);
    }

    /// 索引快照发布后发 `myas/indexStatus`（状态栏 ready + floatIsFloat64
    /// 生效值——架构设计 §8 风险 11 的既定缓解项）。send 是非阻塞的。
    pub fn notify_ready(&self, float_is_float64: bool, files: usize, elapsed_ms: u128) {
        if let Some(tx) = self.ready_tx.lock().unwrap().as_ref() {
            let _ = tx.send(serde_json::json!({
                "state": "ready",
                "files": files,
                "floatIsFloat64": float_is_float64,
                "elapsedMs": elapsed_ms as u64,
            }));
        }
    }

    pub fn is_ready(&self) -> bool {
        self.index.read().unwrap().is_some()
    }

    /// Ready 时的只读访问（锁内完成，不跨 await）。
    pub fn with<R>(&self, f: impl FnOnce(&WorkspaceIndex) -> R) -> Option<R> {
        let g = self.index.read().unwrap();
        g.as_ref().map(f)
    }

    pub fn mark_dirty(&self, file: FileId) {
        self.dirty.lock().unwrap().insert(file);
    }

    pub fn mark_stale(&self, file: FileId) {
        self.stale.lock().unwrap().insert(file);
    }

    /// 发布快照 + 重放 pending_dirty（§6.1：发布瞬间原子完成）。
    /// 全量换根 ⇒ UseSite 解析缓存整表失效。
    ///
    /// **indexed_versions 必须清空而非预填 overlay 版本**（M5b 修 M3 潜伏
    /// bug）：新索引来自磁盘文本，与 overlay 版本无对应关系——预填会让
    /// 重放的 `ensure_file_fresh` 判定「版本相等」而短路，didOpen 文本 ≠
    /// 磁盘内容时（probe / 真实编辑）重放成空操作，hover 恒 null。清空后
    /// dirty 集合的每个 overlay 文件都会真正 reindex。
    pub fn publish_and_replay(&self, idx: WorkspaceIndex, docs: &Mutex<DocStore>) {
        *self.index.write().unwrap() = Some(idx);
        self.indexed_versions.lock().unwrap().clear();
        self.use_cache.lock().unwrap().clear();
        let dirty: Vec<FileId> = self.dirty.lock().unwrap().drain().collect();
        as_log!("index published: {} pending dirty replay(s)", dirty.len());
        for file in dirty {
            self.ensure_file_fresh(file, docs);
        }
        // M6（D36）：快照发布 → 对全部已打开文档补推一轮诊断（Loading 期
        // 打开的文件 + AS0902 的 decl 计数随重建刷新）。后台线程经通道转发。
        if let Some(tx) = self.diag_tx.lock().unwrap().as_ref() {
            let _ = tx.send(());
        }
    }

    /// 单文件的 UseSite 解析缓存（命中返回克隆的 Arc；未命中解析并填入）。
    /// 调用约定：必须在**持有索引读锁的闭包内**同步调用（`with`），不跨 await。
    pub fn cached_uses(&self, idx: &WorkspaceIndex, file: FileId) -> Arc<Vec<UseResolution>> {
        if let Some(hit) = self.use_cache.lock().unwrap().get(&file) {
            return Arc::clone(hit);
        }
        let resolved = Arc::new(resolve_file_uses(idx, file));
        self.use_cache.lock().unwrap().insert(file, Arc::clone(&resolved));
        resolved
    }

    /// 单文件保鲜（语义请求前）：
    /// ① didClose 后回落磁盘重读（§5.1）；② overlay 领先 → 惰性重索引。
    pub fn ensure_file_fresh(&self, file: FileId, docs: &Mutex<DocStore>) {
        if !self.is_ready() {
            return;
        }
        if self.stale.lock().unwrap().remove(&file) {
            if let Some(path) = file_path(file) {
                if let Ok(text) = std::fs::read_to_string(path) {
                    self.reindex(file, kind_of_path(path), text);
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
                    self.reindex(file, kind, text);
                    self.indexed_versions.lock().unwrap().insert(file, v);
                }
            }
        }
    }

    fn reindex(&self, file: FileId, kind: FileKind, text: String) {
        // 声明面指纹（D29）：函数体/局部改动 ⇒ 只失效该文件的解析缓存；
        // 对外可见声明增删改（含重载增删）⇒ 跨文件可见性变化，整表失效
        // （先正确后优化——精确「依赖该声明的文件集」失效 M5+ 按需）
        let surface_changed = {
            let mut idx = self.index.write().unwrap();
            match idx.as_mut() {
                Some(i) => i.reindex_file(file, kind, text),
                None => false,
            }
        };
        let mut cache = self.use_cache.lock().unwrap();
        if surface_changed {
            as_log!("reindex: decl surface changed -> use-site cache invalidated entirely");
            cache.clear();
        } else {
            cache.remove(&file);
        }
    }

    /// watched-files 新增 / 改名（规划 §5.3）：单文件入索引（FileId 由调用方
    /// intern——同路径复活自动复用，D18）。结构性变更 ⇒ 整表失效解析缓存。
    pub fn add_file(&self, file: FileId, kind: FileKind, module: Option<Sym>, text: String) {
        let bytes = text.len();
        {
            let mut idx = self.index.write().unwrap();
            if let Some(i) = idx.as_mut() {
                i.reindex_file_full(file, kind, module, text);
            }
        }
        as_log!("add_file: indexed {bytes} bytes (kind={:?})", kind);
        self.use_cache.lock().unwrap().clear();
    }

    /// watched-files 删除（规划 §5.3）：摘除全部查询表条目 + 墓碑（D18）。
    pub fn remove_file(&self, file: FileId) {
        {
            let mut idx = self.index.write().unwrap();
            if let Some(i) = idx.as_mut() {
                i.remove_file(file);
            }
        }
        as_log!("remove_file: tombstoned");
        self.use_cache.lock().unwrap().clear();
    }
}

/// 路径分隔符规范化（Windows：`/` → `\`）。workspaceFolders / 配置根 /
/// read_dir 拼接可能产生混合分隔符，且 `Uri::to_file_path` 返回正斜杠形态——
/// 不统一会让 `intern_file` 给同一文件发两个 FileId（overlay 与索引对不上）。
pub fn normalize_path(s: &str) -> String {
    if cfg!(windows) {
        s.replace('/', "\\")
    } else {
        s.to_string()
    }
}

/// `${workspaceFolder}` 逐 folder 展开（笛卡尔积），路径规范化。
fn expand_roots(entries: &[String], folders: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in entries {
        if entry.contains("${workspaceFolder}") {
            for folder in folders {
                out.push(PathBuf::from(normalize_path(&entry.replace("${workspaceFolder}", folder))));
            }
        } else {
            out.push(PathBuf::from(normalize_path(entry)));
        }
    }
    out
}

/// 收集根全集（scriptRoots 空 = 全部 workspaceFolders + decl dirs 展开并集）。
/// watched-files 事件的相关性判定与模块名计算与 `build_index` 共用同一口径。
pub fn collect_roots(cfg: &WorkspaceConfig, folders: &[String]) -> Vec<PathBuf> {
    let folders: Vec<String> = folders.iter().map(|f| normalize_path(f)).collect();
    let mut roots: Vec<PathBuf> = if cfg.script_roots.is_empty() {
        folders.iter().map(PathBuf::from).collect()
    } else {
        expand_roots(&cfg.script_roots, &folders)
    };
    roots.extend(expand_roots(&cfg.decl_dirs, &folders));
    roots
}

/// 新增 / 改名文件的模块名（规划 §5.3：模块名随新路径重算，local 可见域
/// 随之改变）：相对首个包含它的收集根按引擎 `FilenameToModuleName` 计算。
/// 不在任何根下 → None（调用方已按相关性过滤，仅防御）。
pub fn module_for_path(roots: &[PathBuf], path: &str) -> Option<Sym> {
    let p = std::path::Path::new(path);
    for root in roots {
        if let Ok(rel) = p.strip_prefix(root) {
            let rel = rel.to_string_lossy();
            if !rel.is_empty() {
                return Some(intern_sym_for_module(&filename_to_module_name(&rel)));
            }
        }
    }
    None
}

fn intern_sym_for_module(name: &str) -> Sym {
    as_core::intern::intern_sym(name)
}

/// 构建全工作区索引（后台线程调用）。
pub fn build_index(
    cfg: &WorkspaceConfig,
    folders: &[String],
    overlays: &[(FileId, i32, String)],
) -> WorkspaceIndex {
    // 分阶段耗时（fileLog）：目录 walk / 逐文件读盘 / 索引构建
    let t0 = std::time::Instant::now();
    let roots = collect_roots(cfg, folders);

    let mut seen: HashSet<String> = HashSet::new();
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    for root in roots.iter() {
        collect(root, root, &mut seen, &mut files, 0);
    }
    files.sort_by(|a, b| a.1.cmp(&b.1));
    let t_walk = t0.elapsed();

    let mut inputs = Vec::with_capacity(files.len());
    let walked = files.len();
    let mut total_bytes = 0usize;
    let mut overlay_hits = 0usize;
    for (root, path) in files {
        let Some(path_str) = path.to_str() else { continue };
        let kind = kind_of_path(path_str);
        let source = match file_id_of_path(path_str).and_then(|f| {
            overlays.iter().find(|(of, _, _)| *of == f).map(|(_, _, t)| t.clone())
        }) {
            Some(t) => {
                overlay_hits += 1;
                t
            }
            None => match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(_) => continue,
            },
        };
        total_bytes += source.len();
        let file = intern_file(path_str, 0);
        let module = path
            .strip_prefix(&root)
            .ok()
            .filter(|rel| !rel.as_os_str().is_empty())
            .map(|rel| {
                as_core::intern::intern_sym(&filename_to_module_name(&rel.to_string_lossy()))
            });
        inputs.push(FileInput { file, kind, module, source });
    }
    let t_read = t0.elapsed();
    as_log!(
        "rebuild: phase0 walk {walked} files in {:?}, read {} KB (overlay {}) in {:?}",
        t_walk,
        total_bytes / 1024,
        overlay_hits,
        t_read - t_walk,
    );

    let t_build = std::time::Instant::now();
    let idx = WorkspaceIndex::build(IndexConfig { float_is_float64: cfg.float_is_float64 }, inputs);
    as_log!("rebuild: phase1+2 index built in {:?}", t_build.elapsed());
    idx
}

/// 深度受限的递归收集（.d.as → Decl、其余 .as → Script；去重）。
fn collect(
    root: &PathBuf,
    dir: &PathBuf,
    seen: &mut HashSet<String>,
    out: &mut Vec<(PathBuf, PathBuf)>,
    depth: usize,
) {
    if depth > 16 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.metadata().map(|m| m.is_dir()).unwrap_or(false) {
            subdirs.push(path);
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let lower = name.to_ascii_lowercase();
        if !lower.ends_with(".as") {
            continue;
        }
        let key = path.to_string_lossy().to_lowercase();
        if seen.insert(key) {
            out.push((root.clone(), path));
        }
    }
    for sub in subdirs {
        collect(root, &sub, seen, out, depth + 1);
    }
}

/// 路径 → 文件类别（D26 裁决二：任意收集根下 `.d.as` → Decl，其余 `.as` → Script）。
pub fn kind_of_path(path: &str) -> FileKind {
    if path.to_ascii_lowercase().ends_with(".d.as") {
        FileKind::Decl
    } else {
        FileKind::Script
    }
}
