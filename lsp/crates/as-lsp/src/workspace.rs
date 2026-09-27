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
use std::sync::{Arc, Mutex, RwLock};

use as_core::id::FileId;
use as_core::intern::{file_id_of_path, file_path, intern_file};
use as_core::references::{resolve_file_uses, UseResolution};
use as_core::{filename_to_module_name, FileInput, FileKind, IndexConfig, WorkspaceIndex};

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
}

impl WorkspaceState {
    pub fn new() -> Self {
        WorkspaceState {
            index: RwLock::new(None),
            indexed_versions: Mutex::new(HashMap::new()),
            dirty: Mutex::new(HashSet::new()),
            stale: Mutex::new(HashSet::new()),
            use_cache: Mutex::new(HashMap::new()),
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
    pub fn publish_and_replay(&self, idx: WorkspaceIndex, docs: &Mutex<DocStore>) {
        let versions: HashMap<FileId, i32> = {
            let store = docs.lock().unwrap();
            store.overlays().into_iter().map(|(f, v, _)| (f, v)).collect()
        };
        *self.index.write().unwrap() = Some(idx);
        *self.indexed_versions.lock().unwrap() = versions;
        self.use_cache.lock().unwrap().clear();
        let dirty: Vec<FileId> = self.dirty.lock().unwrap().drain().collect();
        for file in dirty {
            self.ensure_file_fresh(file, docs);
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
            cache.clear();
        } else {
            cache.remove(&file);
        }
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

/// 构建全工作区索引（后台线程调用）。
pub fn build_index(
    cfg: &WorkspaceConfig,
    folders: &[String],
    overlays: &[(FileId, i32, String)],
) -> WorkspaceIndex {
    let folders: Vec<String> = folders.iter().map(|f| normalize_path(f)).collect();
    let script_roots: Vec<PathBuf> = if cfg.script_roots.is_empty() {
        folders.iter().map(PathBuf::from).collect()
    } else {
        expand_roots(&cfg.script_roots, &folders)
    };
    let decl_roots = expand_roots(&cfg.decl_dirs, &folders);

    let mut seen: HashSet<String> = HashSet::new();
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    for root in script_roots.iter().chain(decl_roots.iter()) {
        collect(root, root, &mut seen, &mut files, 0);
    }
    files.sort_by(|a, b| a.1.cmp(&b.1));

    let mut inputs = Vec::with_capacity(files.len());
    for (root, path) in files {
        let Some(path_str) = path.to_str() else { continue };
        let kind = kind_of_path(path_str);
        let source = match file_id_of_path(path_str).and_then(|f| {
            overlays.iter().find(|(of, _, _)| *of == f).map(|(_, _, t)| t.clone())
        }) {
            Some(t) => t,
            None => match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(_) => continue,
            },
        };
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

    WorkspaceIndex::build(IndexConfig { float_is_float64: cfg.float_is_float64 }, inputs)
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

fn kind_of_path(path: &str) -> FileKind {
    if path.to_ascii_lowercase().ends_with(".d.as") {
        FileKind::Decl
    } else {
        FileKind::Script
    }
}
