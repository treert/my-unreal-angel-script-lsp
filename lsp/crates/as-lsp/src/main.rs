//! as-lsp：LSP server 壳（LSP实现规划 §8）。
//!
//! M2 范围：tower-lsp-server 壳 + textDocumentSync Incremental +
//! documentSymbol / foldingRange / semanticTokens(full)——三个请求都是
//! CST 直映射（§8.1），冷启动 Loading 期间即可服务（§6.1）。
//! 工作区索引与语义类请求（hover/definition/…）随 M3 落地；
//! `floatIsFloat64` 配置先入 `ServerConfig` 存储（配置变更 ⇒ 全量重建的
//! 消费方是 M3 的索引，§5.3）。
//!
//! 语言 id：`angelscript-asl`（§5，避开与 Hazelight 扩展冲突）。
//! UTF-16 ↔ 字节换算只在本层发生（经 as-core `range.rs` 原语，§3.2.1）。

mod docs;
mod watch;
mod workspace;

// 文件日志设施在 as-core（宏随 `#[macro_export]` 落 as_core crate 根），
// init 由本 crate 在配置加载后调用（见 initialized）
use as_core::as_log;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tower_lsp_server::jsonrpc::Result as RpcResult;
use tower_lsp_server::ls_types::{self as ls, *};
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

use as_core::id::FileId;
use as_core::outline::{self, FoldKind, OutlineKind};
use as_core::resolve::Target;
use as_core::tokens;
use as_core::{LEGEND, LineIndex, TextRange};

use docs::{DocStore, TextChange};
use watch::DeclDebouncer;
use workspace::{WorkspaceConfig, WorkspaceState};

struct Backend {
    client: Client,
    docs: Arc<Mutex<DocStore>>,
    /// 索引级配置（任一变更 ⇒ 后台全量重建，§5.3）。Arc 共享给 `.d.as`
    /// 防抖线程——重建时读**当前**值（配置可能在防抖器存活期间变更）
    config: Arc<Mutex<WorkspaceConfig>>,
    ws: Arc<WorkspaceState>,
    folders: Arc<Mutex<Vec<String>>>,
    /// 客户端是否支持 didChangeWatchedFiles 动态注册（initialize 时探测）
    watch_supported: Mutex<bool>,
    /// `.d.as` 防抖器（initialized 注册成功后创建）
    debouncer: OnceLock<DeclDebouncer>,
    /// `myAngelScriptLsp.debug.fileLog`（重启生效——见 logger.rs 模块头）
    debug_file_log: Mutex<bool>,
}

/// 自定义通知 `myas/indexStatus`（server → client）：索引快照发布后的
/// 可观测性——状态栏 ready + `floatIsFloat64` 生效值（架构设计 §8 风险 11
/// 的既定缓解项）。Params 用 `serde_json::Value`（免直接依赖 serde derive）。
struct IndexStatus;
impl ls::notification::Notification for IndexStatus {
    type Params = serde_json::Value;
    const METHOD: &'static str = "myas/indexStatus";
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::build(|client| {
        // 索引就绪通知转发：后台 std 线程（冷启动 / 防抖重建）不能跨 await
        // 调 client——经 unbounded channel 转给本 tokio 任务（current_thread
        // runtime 由 serve() 驱动，任务随之运行）
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
        let notifier = client.clone();
        tokio::spawn(async move {
            while let Some(v) = rx.recv().await {
                let _ = notifier.send_notification::<IndexStatus>(v).await;
            }
        });
        let ws = Arc::new(WorkspaceState::new());
        ws.set_ready_tx(tx);
        Backend {
            client,
            docs: Arc::new(Mutex::new(DocStore::new())),
            config: Arc::new(Mutex::new(WorkspaceConfig::default())),
            ws,
            folders: Arc::new(Mutex::new(Vec::new())),
            watch_supported: Mutex::new(false),
            debouncer: OnceLock::new(),
            debug_file_log: Mutex::new(false),
        }
    })
    .finish();

    Server::new(stdin, stdout, socket).serve(service).await;
}

/// URI → 文件路径字符串（仅 file:// URI；分隔符规范化——`to_file_path`
/// 在 Windows 返回正斜杠形态，须与索引侧统一，否则 FileId 分裂）。
fn uri_path(uri: &ls::Uri) -> Option<String> {
    uri.to_file_path()
        .map(|p| workspace::normalize_path(&p.to_string_lossy()))
}

impl Backend {
    /// 语义请求的前置：URI+Position → (FileId, 字节偏移)。
    /// 锁内完成 UTF-16 → 字节换算（§3.2.1：换算只在本层发生）。
    fn doc_position(&self, p: &TextDocumentPositionParams) -> Option<(FileId, u32)> {
        let path = uri_path(&p.text_document.uri)?;
        let file = as_core::intern::file_id_of_path(&path)?;
        let store = self.docs.lock().unwrap();
        let doc = store.get(file)?;
        let byte = doc.lines.offset_of_utf16(&doc.text, p.position.line, p.position.character);
        Some((file, byte))
    }

    /// URI → (FileId, 文档表锁)。文件未打开（无 overlay）→ None。
    fn doc_of(&self, uri: &ls::Uri) -> Option<(FileId, std::sync::MutexGuard<'_, DocStore>)> {
        let path = uri_path(uri)?;
        let file = as_core::intern::file_id_of_path(&path)?;
        let store = self.docs.lock().unwrap();
        Some((file, store))
    }

    async fn load_config(&self) {
        // workspace/configuration（配置项 myAngelScriptLsp.*，§5）
        let items = vec![ConfigurationItem {
            scope_uri: None,
            section: Some("myAngelScriptLsp".to_string()),
        }];
        if let Ok(values) = self.client.configuration(items).await {
            if let Some(v) = values.first() {
                let mut cfg = self.config.lock().unwrap();
                if let Some(b) = v.get("floatIsFloat64").and_then(|b| b.as_bool()) {
                    cfg.float_is_float64 = b;
                }
                if let Some(roots) = v.get("scriptRoots").and_then(|r| r.as_array()) {
                    let parsed: Vec<String> = roots
                        .iter()
                        .filter_map(|s| s.as_str().map(str::to_string))
                        .collect();
                    if !parsed.is_empty() {
                        cfg.script_roots = parsed;
                    }
                }
                if let Some(dirs) = v.get("typeDeclarationDirs").and_then(|r| r.as_array()) {
                    let parsed: Vec<String> = dirs
                        .iter()
                        .filter_map(|s| s.as_str().map(str::to_string))
                        .collect();
                    if !parsed.is_empty() {
                        cfg.decl_dirs = parsed;
                    }
                }
            }
            if let Some(v) = values.first() {
                if let Some(b) = v.get("debug").and_then(|d| d.get("fileLog")).and_then(|b| b.as_bool())
                {
                    *self.debug_file_log.lock().unwrap() = b;
                }
            }
        }
        // 客户端不支持 workspace/configuration 时保持默认值（G9：默认对齐引擎）
    }

    /// 启动冷启动后台线程（§6：Phase 0-2 → 发布 → 重放 pending_dirty）。
    /// 与 `.d.as` 防抖触发共用 `watch::run_rebuild`（互斥 + 换根语义一致）。
    fn spawn_index_build(&self) {
        let cfg = Arc::clone(&self.config);
        let folders = Arc::clone(&self.folders);
        let docs = Arc::clone(&self.docs);
        let ws = Arc::clone(&self.ws);
        std::thread::spawn(move || {
            watch::run_rebuild(&cfg, &folders, &docs, &ws);
        });
    }

    /// 光标处的引用查询目标（M4）：重载组**原样保留**——消歧失败由消费方按
    /// 「报全部重载」处理（架构设计 §4.6）；class 合成 namespace 归一到类
    /// （`AActor::` 限定段计入类引用）。None = 光标处不可解析。
    fn resolve_query_targets(&self, file: FileId, byte: u32) -> Option<Vec<as_core::RefTarget>> {
        self.ws
            .with(|idx| {
                let r = as_core::resolve::resolve_at(idx, file, byte)?;
                let mut targets: Vec<as_core::RefTarget> = r
                    .targets
                    .iter()
                    .filter_map(|t| match t {
                        Target::Def(id) => Some(as_core::RefTarget::Def(
                            as_core::references::origin_fallback(idx, *id),
                        )),
                        Target::Local(l) => {
                            Some(as_core::RefTarget::Local { file, span: l.name_span })
                        }
                    })
                    .collect();
                targets.dedup();
                (!targets.is_empty()).then_some(targets)
            })
            .flatten()
    }

    /// references / rename 共用：引用倒排给候选文件集 → 分批解析 UseSite
    /// （按文件缓存）→ 匹配。批间释放索引读锁、上报 $/progress（规划 §9 M4
    /// 长任务；客户端不支持时静默跳过——进度是咨询性增强）。
    /// `strict`：rename 只取「唯一指向目标」的站点（歧义站点可能属于其它
    /// 重载，改写会误伤——宁缺毋假）。
    async fn collect_use_matches(
        &self,
        targets: &[as_core::RefTarget],
        strict: bool,
        title: &str,
    ) -> Vec<(FileId, TextRange)> {
        let files: Vec<FileId> = self
            .ws
            .with(|idx| as_core::references::candidate_files(idx, targets))
            .unwrap_or_default();
        if files.is_empty() {
            return Vec::new();
        }
        let token = ls::NumberOrString::Number(PROGRESS_SEQ.fetch_add(1, Ordering::Relaxed) as i32);
        let ongoing = match self.client.create_work_done_progress(token.clone()).await {
            Ok(()) => Some(
                self.client
                    .progress(token, title)
                    .with_percentage(0)
                    .begin()
                    .await,
            ),
            Err(_) => None, // 客户端不支持 workDoneProgress：跳过进度
        };
        const BATCH: usize = 32;
        let total = files.len();
        let mut out = Vec::new();
        for (i, chunk) in files.chunks(BATCH).enumerate() {
            let hits: Vec<(FileId, TextRange)> = self
                .ws
                .with(|idx| {
                    let mut out = Vec::new();
                    for &f in chunk {
                        let resolved = self.ws.cached_uses(idx, f);
                        let matched = if strict {
                            as_core::references::match_uses_strict(targets, &resolved)
                        } else {
                            as_core::references::match_uses(targets, &resolved)
                        };
                        for s in matched {
                            out.push((f, s));
                        }
                    }
                    out
                })
                .unwrap_or_default();
            out.extend(hits);
            if let Some(p) = &ongoing {
                let done = ((i + 1) * BATCH).min(total);
                p.report((done as u64 * 100 / total as u64) as u32).await;
            }
        }
        if let Some(p) = ongoing {
            p.finish().await;
        }
        out
    }
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        // workspaceFolders：scriptRoots 空 = 全部根（§5）；${workspaceFolder} 展开
        if let Some(folders) = params.workspace_folders {
            let paths: Vec<String> = folders
                .iter()
                .filter_map(|f| uri_path(&f.uri))
                .collect();
            *self.folders.lock().unwrap() = paths;
        }
        // didChangeWatchedFiles 动态注册支持探测（M4：文件监视）
        let watch_ok = params
            .capabilities
            .workspace
            .and_then(|w| w.did_change_watched_files)
            .and_then(|c| c.dynamic_registration)
            .unwrap_or(false);
        *self.watch_supported.lock().unwrap() = watch_ok;
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                document_symbol_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        ".".to_string(),  // `X.` 成员
                        ":".to_string(),  // `A::` 命名空间 / enum
                        "(".to_string(),  // 实参位（命名实参）
                        ",".to_string(),  // 实参位（下一实参）
                    ]),
                    ..CompletionOptions::default()
                }),
                references_provider: Some(OneOf::Left(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    ..SignatureHelpOptions::default()
                }),
                inlay_hint_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                        legend: SemanticTokensLegend {
                            token_types: LEGEND
                                .iter()
                                .map(|s| SemanticTokenType::new(*s))
                                .collect(),
                            token_modifiers: Vec::new(),
                        },
                        full: Some(SemanticTokensFullOptions::Bool(true)),
                        range: None,
                        work_done_progress_options: WorkDoneProgressOptions::default(),
                    }),
                ),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: "my-as-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            offset_encoding: None, // 默认 UTF-16（LSP 标准）
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.load_config().await;
        let msg = {
            let config = self.config.lock().unwrap();
            format!(
                "my-as-lsp M4 ready (floatIsFloat64={}, scriptRoots={:?}, typeDeclarationDirs={:?})",
                config.float_is_float64, config.script_roots, config.decl_dirs
            )
        };
        self.client.log_message(MessageType::INFO, msg).await;
        // 文件日志（logger::init 必须早于索引构建，会话头/构建过程才能进文件）
        let folders = self.folders.lock().unwrap().clone();
        as_core::logger::init(&folders, *self.debug_file_log.lock().unwrap());
        {
            let config = self.config.lock().unwrap();
            as_log!(
                "[my-as-lsp] config: floatIsFloat64={} scriptRoots={:?} typeDeclarationDirs={:?} debug.fileLog={}",
                config.float_is_float64, config.script_roots, config.decl_dirs,
                *self.debug_file_log.lock().unwrap()
            );
        }
        // 文件监视动态注册（M4）：`**/*.as` 同时覆盖 `.d.as`；kind 7 = 增|改|删
        if *self.watch_supported.lock().unwrap() {
            watch::register_watcher(&self.client).await;
            let debouncer = DeclDebouncer::spawn(
                Arc::clone(&self.config),
                Arc::clone(&self.folders),
                Arc::clone(&self.docs),
                Arc::clone(&self.ws),
            );
            let _ = self.debouncer.set(debouncer);
        }
        self.spawn_index_build();
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        if let Some(path) = uri_path(&doc.uri) {
            // 可观测性：确认文件真正到达本 server（语言归属排查用——
            // 架构设计 §8 风险 4：与 Hazelight 扩展共存时 .as 归属可能旁落）
            self.client
                .log_message(
                    MessageType::INFO,
                    format!("didOpen {path} (languageId={})", doc.language_id),
                )
                .await;
            let file = {
                let mut store = self.docs.lock().unwrap();
                store.open(&path, doc.version, doc.text)
            };
            // Loading 期间的编辑记入 pending_dirty（§6.1；Ready 后惰性重索引）
            if !self.ws.is_ready() {
                self.ws.mark_dirty(file);
            }
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let changes: Vec<TextChange> = params
            .content_changes
            .into_iter()
            .map(|c| TextChange {
                range: c
                    .range
                    .map(|r| ((r.start.line, r.start.character), (r.end.line, r.end.character))),
                text: c.text,
            })
            .collect();
        if let Some(path) = uri_path(&params.text_document.uri) {
            let mut store = self.docs.lock().unwrap();
            if let Some(file) = as_core::intern::file_id_of_path(&path) {
                store.apply_changes(file, params.text_document.version, changes);
            }
        }
    }

    async fn did_save(&self, _params: DidSaveTextDocumentParams) {
        // 不触发重读：磁盘内容此刻必然等于 overlay（§5.1）
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        if let Some(path) = uri_path(&params.text_document.uri) {
            let file = {
                let mut store = self.docs.lock().unwrap();
                let file = as_core::intern::file_id_of_path(&path);
                if let Some(f) = file {
                    store.close(f);
                }
                file
            };
            // overlay 丢弃 → 下次语义请求前回落磁盘重读（§5.1）
            if let Some(f) = file {
                self.ws.mark_stale(f);
            }
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        // 事件分类与处置见 watch.rs 模块头；collect_roots 与 build_index
        // 同口径（D26 裁决二：任意收集根下 .d.as → Decl、其余 .as → Script）
        let (cfg, folders) = {
            let cfg = self.config.lock().unwrap().clone();
            let folders = self.folders.lock().unwrap().clone();
            (cfg, folders)
        };
        let roots = workspace::collect_roots(&cfg, &folders);
        for event in params.changes {
            let Some(path) = uri_path(&event.uri) else { continue };
            let file = as_core::intern::file_id_of_path(&path);
            // overlay 优先（§5.1）：打开中的文件忽略监视事件（外部工具改盘
            // 不引发抖动；未保存缓冲由 overlay 保护）
            let has_overlay = file.is_some_and(|f| self.docs.lock().unwrap().get(f).is_some());
            let relevant = file.is_some_and(|f| {
                self.ws.with(|idx| idx.files.contains_key(&f)).unwrap_or(false)
            }) || roots.iter().any(|r| under_root(&path, r));
            match watch::classify(event.typ, &path, has_overlay, relevant) {
                watch::WatchAction::DeclChange => {
                    // .d.as 任一变化 → 防抖（500ms/5s，D24）→ 全量重建
                    as_log!("watch: DeclChange -> debounce: {path}");
                    if let Some(d) = self.debouncer.get() {
                        d.ping();
                    }
                }
                watch::WatchAction::ScriptCreate | watch::WatchAction::ScriptChange => {
                    // 读盘入索引（改名 = 删 + 增，模块名随新路径重算——
                    // local 可见域随之改变，规划 §5.3）
                    let Ok(text) = std::fs::read_to_string(&path) else { continue };
                    let file = as_core::intern::intern_file(&path, 0);
                    let module = workspace::module_for_path(&roots, &path);
                    as_log!("watch: script create/change {path} -> index (module={module:?})");
                    self.ws.add_file(file, workspace::kind_of_path(&path), module, text);
                }
                watch::WatchAction::ScriptDelete => {
                    as_log!("watch: ScriptDelete {path}");
                    if let Some(file) = file {
                        self.ws.remove_file(file);
                    }
                }
                watch::WatchAction::Ignore => {}
            }
        }
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        // settings 结构：{ "myAngelScriptLsp": { "floatIsFloat64": bool, ... } }
        let new_value = params
            .settings
            .get("myAngelScriptLsp")
            .and_then(|v| v.get("floatIsFloat64"))
            .and_then(|v| v.as_bool());
        let new_roots = params
            .settings
            .get("myAngelScriptLsp")
            .and_then(|v| v.get("scriptRoots"))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect::<Vec<String>>()
            });
        let new_dirs = params
            .settings
            .get("myAngelScriptLsp")
            .and_then(|v| v.get("typeDeclarationDirs"))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect::<Vec<String>>()
            });
        let mut changed = false;
        {
            let mut config = self.config.lock().unwrap();
            if let Some(b) = new_value {
                if config.float_is_float64 != b {
                    config.float_is_float64 = b;
                    changed = true;
                }
            }
            if let Some(r) = new_roots {
                if config.script_roots != r {
                    config.script_roots = r;
                    changed = true;
                }
            }
            if let Some(d) = new_dirs {
                if config.decl_dirs != d {
                    config.decl_dirs = d;
                    changed = true;
                }
            }
        }
        if changed {
            // 索引级配置变更 ⇒ 后台全量重建（§5.3——floatIsFloat64 是唯一
            // 不可替代的全量触发源；roots/dirs 换根同理）
            self.client
                .log_message(MessageType::INFO, "index config changed -> rebuilding")
                .await;
            self.spawn_index_build();
        }
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> RpcResult<Option<DocumentSymbolResponse>> {
        let Some((file, store)) = self.doc_of(&params.text_document.uri) else {
            return Ok(None);
        };
        let Some(doc) = store.get(file) else {
            return Ok(None);
        };
        let symbols = outline::document_symbols(doc.tree.root_node(), &doc.text);
        let result: Vec<DocumentSymbol> = symbols
            .into_iter()
            .map(|s| to_lsp_symbol(s, &doc.text, &doc.lines))
            .collect();
        Ok(Some(DocumentSymbolResponse::Nested(result)))
    }

    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> RpcResult<Option<Vec<FoldingRange>>> {
        let Some((file, store)) = self.doc_of(&params.text_document.uri) else {
            return Ok(None);
        };
        let Some(doc) = store.get(file) else {
            return Ok(None);
        };
        let folds = outline::folding_ranges(doc.tree.root_node(), &doc.text, &doc.lines);
        let result: Vec<FoldingRange> = folds
            .into_iter()
            .map(|f| {
                let start_line = doc.lines.line_of(f.start) as u32;
                let end_line = doc.lines.line_of(f.end.saturating_sub(1)) as u32;
                FoldingRange {
                    start_line,
                    end_line,
                    start_character: None,
                    end_character: None,
                    kind: match f.kind {
                        FoldKind::Comment => Some(FoldingRangeKind::Comment),
                        FoldKind::Block => None,
                    },
                    collapsed_text: None,
                }
            })
            .filter(|r| r.end_line > r.start_line)
            .collect();
        Ok(Some(result))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> RpcResult<Option<SemanticTokensResult>> {
        let Some((file, store)) = self.doc_of(&params.text_document.uri) else {
            return Ok(None);
        };
        let Some(doc) = store.get(file) else {
            return Ok(None);
        };
        let raw = tokens::semantic_tokens(doc.tree.root_node(), &doc.text);
        let data = delta_encode(&raw, &doc.text, &doc.lines);
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    async fn hover(&self, params: HoverParams) -> RpcResult<Option<Hover>> {
        let Some((file, byte)) = self.doc_position(&params.text_document_position_params) else {
            return Ok(None);
        };
        if !self.ws.is_ready() {
            return Ok(None); // Loading：语义请求返回空（§6.1 默认）
        }
        self.ws.ensure_file_fresh(file, &self.docs);
        let contents = self
            .ws
            .with(|idx| {
                as_core::resolve::resolve_at(idx, file, byte)
                    .and_then(|r| as_core::hover::hover_markdown(idx, &r.targets))
            })
            .flatten();
        Ok(contents.map(|value| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: None,
        }))
    }

    async fn completion(&self, params: CompletionParams) -> RpcResult<Option<CompletionResponse>> {
        // M5b：语境判定与候选收集在 as-core completion.rs；本层只做映射。
        // CompletionParams 的位置字段（fork 无 _params 后缀）
        let Some((file, byte)) = self.doc_position(&params.text_document_position) else {
            return Ok(None);
        };
        if !self.ws.is_ready() {
            return Ok(None); // Loading：语义请求返回空（§6.1 默认）
        }
        self.ws.ensure_file_fresh(file, &self.docs);
        let cands = self
            .ws
            .with(|idx| as_core::completion::complete_at(idx, file, byte))
            .unwrap_or_default();
        let items = cands
            .into_iter()
            .map(|c| {
                // 命名实参 `Name=` 后面直接继续输值；成员/方法不自动截断
                let insert_fmt = c.insert.as_ref().map(|_| InsertTextFormat::SNIPPET);
                CompletionItem {
                    label: c.label,
                    kind: Some(match c.kind {
                        as_core::completion::CandidateKind::Field => CompletionItemKind::FIELD,
                        as_core::completion::CandidateKind::Method => CompletionItemKind::METHOD,
                        as_core::completion::CandidateKind::Function => CompletionItemKind::FUNCTION,
                        as_core::completion::CandidateKind::Property => CompletionItemKind::FIELD,
                        as_core::completion::CandidateKind::Class => CompletionItemKind::CLASS,
                        as_core::completion::CandidateKind::Struct => CompletionItemKind::STRUCT,
                        as_core::completion::CandidateKind::Enum => CompletionItemKind::ENUM,
                        as_core::completion::CandidateKind::EnumValue => {
                            CompletionItemKind::ENUM_MEMBER
                        }
                        as_core::completion::CandidateKind::Namespace => CompletionItemKind::MODULE,
                        as_core::completion::CandidateKind::GlobalVar => {
                            CompletionItemKind::VARIABLE
                        }
                        as_core::completion::CandidateKind::Param
                        | as_core::completion::CandidateKind::LocalVar => {
                            CompletionItemKind::VARIABLE
                        }
                        as_core::completion::CandidateKind::Keyword => CompletionItemKind::KEYWORD,
                        as_core::completion::CandidateKind::Delegate
                        | as_core::completion::CandidateKind::Event => CompletionItemKind::EVENT,
                        as_core::completion::CandidateKind::NamedArg => CompletionItemKind::FIELD,
                    }),
                    detail: c.detail,
                    insert_text: c.insert,
                    insert_text_format: insert_fmt,
                    ..CompletionItem::default()
                }
            })
            .collect();
        Ok(Some(CompletionResponse::List(CompletionList {
            is_incomplete: false,
            items,
        })))
    }

    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> RpcResult<Option<SignatureHelp>> {
        let Some((file, byte)) = self.doc_position(&params.text_document_position_params) else {
            return Ok(None);
        };
        if !self.ws.is_ready() {
            return Ok(None); // Loading：语义请求返回空（§6.1 默认）
        }
        self.ws.ensure_file_fresh(file, &self.docs);
        // label/doc 提取在索引读锁内完成（with 闭包），壳只映射容器
        let out = self
            .ws
            .with(|idx| {
                as_core::signature::signature_help(idx, file, byte).map(|sh| {
                    let signatures = sh
                        .overloads
                        .iter()
                        .map(|&d| SignatureInformation {
                            label: as_core::signature::overload_label(idx, d),
                            documentation: as_core::signature::overload_doc(idx, d)
                                .map(Documentation::String),
                            parameters: None, // label 内联形参（客户端按子串高亮）
                            active_parameter: None,
                        })
                        .collect();
                    SignatureHelp {
                        signatures,
                        active_signature: Some(sh.active),
                        active_parameter: Some(sh.active_parameter),
                    }
                })
            })
            .flatten();
        Ok(out)
    }

    async fn inlay_hint(
        &self,
        params: InlayHintParams,
    ) -> RpcResult<Option<Vec<InlayHint>>> {
        // file 只需路径 intern（overlay 一致性由 ensure_file_fresh 保证）
        let Some(file) = uri_path(&params.text_document.uri)
            .and_then(|p| as_core::intern::file_id_of_path(&p))
        else {
            return Ok(None);
        };
        if !self.ws.is_ready() {
            return Ok(None); // Loading：语义请求返回空（§6.1 默认）
        }
        self.ws.ensure_file_fresh(file, &self.docs);
        let hints = self
            .ws
            .with(|idx| {
                let doc = idx.files.get(&file)?;
                let src = &doc.source;
                let lines = &doc.lines;
                Some(
                    as_core::inlay::inlay_hints(idx, file)
                        .into_iter()
                        .map(|h| {
                            let (line, character) = lines.line_col_utf16(src, h.position);
                            InlayHint {
                                position: Position { line, character },
                                label: InlayHintLabel::String(h.ty),
                                kind: Some(InlayHintKind::TYPE),
                                text_edits: None,
                                tooltip: None,
                                padding_left: Some(true),
                                padding_right: None,
                                data: None,
                            }
                        })
                        .collect(),
                )
            })
            .flatten();
        Ok(hints)
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> RpcResult<Option<GotoDefinitionResponse>> {
        let Some((file, byte)) = self.doc_position(&params.text_document_position_params) else {
            return Ok(None);
        };
        if !self.ws.is_ready() {
            return Ok(None);
        }
        // 请求文档快照（局部/形参的落点换算用；克隆避免跨锁持有）
        let doc_snapshot = {
            let store = self.docs.lock().unwrap();
            store.get(file).map(|d| {
                (
                    params.text_document_position_params.text_document.uri.clone(),
                    d.lines.clone(),
                    d.text.clone(),
                )
            })
        };
        let Some((uri, lines, text)) = doc_snapshot else {
            return Ok(None);
        };
        self.ws.ensure_file_fresh(file, &self.docs);
        let locations = self
            .ws
            .with(|idx| {
                as_core::resolve::resolve_at(idx, file, byte).map(|r| {
                    let mut out: Vec<ls::Location> = Vec::new();
                    for t in &r.targets {
                        match t {
                            Target::Def(id) => {
                                if let Some(loc) = target_location(idx, *id) {
                                    out.push(loc);
                                }
                            }
                            Target::Local(l) => {
                                let (sl, sc) = lines.line_col_utf16(&text, l.name_span.start);
                                let (el, ec) = lines.line_col_utf16(&text, l.name_span.end);
                                out.push(ls::Location {
                                    uri: uri.clone(),
                                    range: Range::new(Position::new(sl, sc), Position::new(el, ec)),
                                });
                            }
                        }
                    }
                    out
                })
            })
            .flatten()
            .unwrap_or_default();
        Ok((!locations.is_empty()).then(|| GotoDefinitionResponse::Array(locations)))
    }

    async fn references(&self, params: ReferenceParams) -> RpcResult<Option<Vec<ls::Location>>> {
        let Some((file, byte)) = self.doc_position(&params.text_document_position) else {
            return Ok(None);
        };
        if !self.ws.is_ready() {
            return Ok(None); // Loading：语义请求返回空（§6.1 默认）
        }
        self.ws.ensure_file_fresh(file, &self.docs);
        let Some(targets) = self.resolve_query_targets(file, byte) else {
            return Ok(None);
        };

        let t0 = std::time::Instant::now();
        let mut spans = self.collect_use_matches(&targets, false, "Finding references").await;
        as_log!(
            "references: {} query target(s) -> {} site(s) in {:?}",
            targets.len(),
            spans.len(),
            t0.elapsed()
        );

        // includeDeclaration：声明位置（Def 走 name_span——合成成员即源头声明
        // 的锚点，D10；Local 即声明 span）
        if params.context.include_declaration {
            let decls: Vec<(FileId, TextRange)> = self
                .ws
                .with(|idx| {
                    targets
                        .iter()
                        .filter_map(|t| match t {
                            as_core::RefTarget::Def(id) => {
                                let d = idx.def(*id);
                                Some((d.file, d.name_span))
                            }
                            as_core::RefTarget::Local { file, span } => Some((*file, *span)),
                        })
                        .collect()
                })
                .unwrap_or_default();
            spans.extend(decls);
        }

        let locations: Vec<ls::Location> = self
            .ws
            .with(|idx| {
                spans
                    .iter()
                    .filter_map(|(f, s)| file_range_location(idx, *f, *s))
                    .collect()
            })
            .unwrap_or_default();
        Ok((!locations.is_empty()).then_some(locations))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> RpcResult<Option<PrepareRenameResponse>> {
        let Some((file, byte)) = self.doc_position(&params) else {
            return Ok(None);
        };
        if !self.ws.is_ready() {
            return Ok(None);
        }
        self.ws.ensure_file_fresh(file, &self.docs);
        // 单一目标才可 rename（重载组 / 歧义组拒绝——一次只能改一个名字）
        let Some(targets) = self.resolve_query_targets(file, byte) else {
            return Ok(None);
        };
        if targets.len() != 1 {
            return Ok(None);
        }
        let resp = self
            .ws
            .with(|idx| {
                let (tf, span, placeholder) = match &targets[0] {
                    as_core::RefTarget::Def(id) => {
                        let d = idx.def(*id);
                        // 合成成员（Execute / StaticClass 等，origin 名 ≠ 自名）：
                        // 无独立源码声明，不可 rename（改名语义落到源头声明上，
                        // 用户应在那儿发起）。合成 namespace 已归一到类（同名）。
                        if let Some(o) = d.origin {
                            if idx.def(o).name != d.name {
                                return None;
                            }
                        }
                        if idx.files.get(&d.file).is_none() {
                            return None; // 内建 primitive：无源码声明
                        }
                        (d.file, d.name_span, as_core::intern::sym_str(d.name).to_string())
                    }
                    as_core::RefTarget::Local { file: lf, span } => {
                        let snap = idx.files.get(lf)?;
                        let name = snap
                            .source
                            .get(span.start as usize..span.end as usize)?
                            .to_string();
                        (*lf, *span, name)
                    }
                };
                let loc = file_range_location(idx, tf, span)?;
                Some(PrepareRenameResponse::RangeWithPlaceholder {
                    range: loc.range,
                    placeholder,
                })
            })
            .flatten();
        Ok(resp)
    }

    async fn rename(&self, params: RenameParams) -> RpcResult<Option<WorkspaceEdit>> {
        let new_name = params.new_name;
        if !is_valid_identifier(&new_name) {
            return Err(tower_lsp_server::jsonrpc::Error::invalid_params(format!(
                "'{new_name}' 不是合法的 Angelscript 标识符（或与保留字冲突）"
            )));
        }
        let Some((file, byte)) = self.doc_position(&params.text_document_position) else {
            return Ok(None);
        };
        if !self.ws.is_ready() {
            return Ok(None);
        }
        self.ws.ensure_file_fresh(file, &self.docs);

        // 与 prepare_rename 同规则：单一目标 + 非合成成员 + 非内建
        let Some(targets) = self.resolve_query_targets(file, byte) else {
            return Ok(None);
        };
        if targets.len() != 1 {
            return Ok(None);
        }
        let Some(target) = self
            .ws
            .with(|idx| {
                match &targets[0] {
                    as_core::RefTarget::Def(id) => {
                        let d = idx.def(*id);
                        // 合成成员（origin 名 ≠ 自名）无独立源码声明，不可 rename；
                        // 内建 primitive 同理
                        if let Some(o) = d.origin {
                            if idx.def(o).name != d.name {
                                return None;
                            }
                        }
                        if idx.files.get(&d.file).is_none() {
                            return None;
                        }
                    }
                    as_core::RefTarget::Local { .. } => {}
                }
                Some(targets[0].clone())
            })
            .flatten()
        else {
            return Ok(None);
        };

        // 严格匹配（歧义站点不改）+ 声明名 span
        let mut spans = self.collect_use_matches(&[target.clone()], true, "Renaming").await;
        let decl = self
            .ws
            .with(|idx| match &target {
                as_core::RefTarget::Def(id) => {
                    let d = idx.def(*id);
                    Some((d.file, d.name_span))
                }
                as_core::RefTarget::Local { file: lf, span } => Some((*lf, *span)),
            })
            .flatten();
        if let Some(d) = decl {
            spans.push(d);
        }
        if spans.is_empty() {
            return Ok(None);
        }

        let changes: HashMap<ls::Uri, Vec<TextEdit>> = self
            .ws
            .with(|idx| {
                let mut map: HashMap<ls::Uri, Vec<TextEdit>> = HashMap::new();
                for (f, s) in &spans {
                    if let Some(loc) = file_range_location(idx, *f, *s) {
                        map.entry(loc.uri).or_default().push(TextEdit {
                            range: loc.range,
                            new_text: new_name.clone(),
                        });
                    }
                }
                map
            })
            .unwrap_or_default();
        Ok((!changes.is_empty()).then(|| WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> RpcResult<Option<WorkspaceSymbolResponse>> {
        if !self.ws.is_ready() {
            return Ok(None);
        }
        let out: Vec<ls::SymbolInformation> = self
            .ws
            .with(|idx| {
                as_core::search::query_symbols(idx, &params.query)
                    .into_iter()
                    .filter_map(|id| {
                        let d = idx.def(id);
                        let location = target_location(idx, id)?;
                        Some(ls::SymbolInformation {
                            name: as_core::intern::sym_str(d.name).to_string(),
                            kind: def_kind_to_symbol_kind(d.kind),
                            tags: None,
                            #[allow(deprecated)]
                            deprecated: None,
                            location,
                            container_name: d
                                .parent
                                .map(|p| as_core::intern::sym_str(idx.def(p).name).to_string()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok((!out.is_empty()).then_some(WorkspaceSymbolResponse::Flat(out)))
    }
}

/// Def 落点 → LSP Location（origin 回落 D10：合成符号跳转源头声明；
/// `.d.as` 声明是合法落点）。局部/形参的落点在请求文件内，
/// 由 goto_definition 用请求文档的行首表换算。
fn target_location(idx: &as_core::WorkspaceIndex, id: as_core::DefId) -> Option<ls::Location> {
    let mut d = idx.def(id);
    if let Some(origin) = d.origin {
        let o = idx.def(origin);
        if o.origin.is_none() {
            d = o;
        }
    }
    let path = as_core::intern::file_path(d.file)?;
    let uri = ls::Uri::from_file_path(path)?;
    let snap = idx.files.get(&d.file)?;
    let (sl, sc) = snap.lines.line_col_utf16(&snap.source, d.name_span.start);
    let (el, ec) = snap.lines.line_col_utf16(&snap.source, d.name_span.end);
    Some(ls::Location {
        uri,
        range: Range::new(Position::new(sl, sc), Position::new(el, ec)),
    })
}

/// (FileId, TextRange) → LSP Location（引用/重命名站点；UTF-16 换算只在本层，§3.2.1）。
fn file_range_location(
    idx: &as_core::WorkspaceIndex,
    file: FileId,
    range: TextRange,
) -> Option<ls::Location> {
    let snap = idx.files.get(&file)?;
    let path = as_core::intern::file_path(file)?;
    let uri = ls::Uri::from_file_path(path)?;
    let (sl, sc) = snap.lines.line_col_utf16(&snap.source, range.start);
    let (el, ec) = snap.lines.line_col_utf16(&snap.source, range.end);
    Some(ls::Location {
        uri,
        range: Range::new(Position::new(sl, sc), Position::new(el, ec)),
    })
}

/// DefKind → LSP SymbolKind（workspaceSymbol；与 documentSymbol 的
/// `to_symbol_kind` 同口径）。
fn def_kind_to_symbol_kind(kind: as_core::DefKind) -> SymbolKind {
    use as_core::DefKind as D;
    match kind {
        D::Class => SymbolKind::CLASS,
        D::Struct => SymbolKind::STRUCT,
        D::Enum => SymbolKind::ENUM,
        D::EnumValue => SymbolKind::ENUM_MEMBER,
        D::Namespace => SymbolKind::NAMESPACE,
        D::Module => SymbolKind::MODULE,
        D::Delegate => SymbolKind::INTERFACE,
        D::Event => SymbolKind::EVENT,
        D::Function => SymbolKind::FUNCTION,
        D::Method => SymbolKind::METHOD,
        D::Constructor => SymbolKind::CONSTRUCTOR,
        D::Destructor => SymbolKind::METHOD,
        D::Operator => SymbolKind::OPERATOR,
        D::GlobalVar => SymbolKind::VARIABLE,
        D::Field => SymbolKind::FIELD,
        D::Param | D::LocalVar | D::AssetDecl => SymbolKind::VARIABLE,
        D::TypeParam => SymbolKind::TYPE_PARAMETER,
        D::VirtualProperty => SymbolKind::PROPERTY,
    }
}

/// 路径是否位于收集根之下（大小写不敏感——Windows 盘符大小写不可控；
/// 两侧均已 normalize，只比前缀）。
fn under_root(path: &str, root: &std::path::Path) -> bool {
    let root = root.to_string_lossy().to_ascii_lowercase();
    let mut p = path.to_ascii_lowercase();
    if root.ends_with('\\') || root.ends_with('/') {
        p.starts_with(&root)
    } else {
        p.push('\\');
        let hit = p.starts_with(&root) && p.as_bytes().get(root.len()) == Some(&b'\\');
        p.pop();
        hit
    }
}

/// $/progress token 发号器（server 侧自增即可，客户端只按 token 关联流）。
static PROGRESS_SEQ: AtomicU32 = AtomicU32::new(1);

/// 保留字（grammar token 集的实用子集：类型/语句/声明/访问/宏关键字）。
const KEYWORDS: &[&str] = &[
    // primitive_type 全集 + auto
    "void", "bool", "int8", "int16", "int", "int32", "int64", "uint8", "uint16", "uint",
    "uint32", "uint64", "float", "float32", "float64", "double", "auto",
    // 字面量 / 空值
    "true", "false", "null", "nullptr",
    // 语句
    "if", "else", "while", "do", "for", "switch", "case", "default", "break", "continue",
    "fallthrough", "return",
    // 声明
    "class", "struct", "enum", "namespace", "delegate", "event", "asset", "mixin", "local",
    "const", "private", "protected", "final", "override", "property", "access",
    // 表达式 / 语境关键字
    "this", "Super", "super", "Cast", "get", "set", "of",
    // UE 反射宏
    "UCLASS", "USTRUCT", "UENUM", "UFUNCTION", "UPROPERTY", "UMETA",
];

/// 新名合法性：identifier 模式（grammar `identifier: /[A-Za-z_][A-Za-z0-9_]*/`）
/// 且非保留字。
fn is_valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else { return false };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    !KEYWORDS.contains(&s)
}

// ---------------------------------------------------------------------------
// LSP 类型映射（唯一发生 UTF-16 换算的地方，§3.2.1）
// ---------------------------------------------------------------------------

fn to_lsp_symbol(s: outline::OutlineSymbol, text: &str, lines: &LineIndex) -> DocumentSymbol {
    DocumentSymbol {
        name: s.name,
        detail: None,
        kind: to_symbol_kind(s.kind),
        tags: None,
        #[allow(deprecated)]
        deprecated: None,
        range: to_lsp_range(s.range, text, lines),
        selection_range: to_lsp_range(s.selection_range, text, lines),
        children: if s.children.is_empty() {
            None
        } else {
            Some(
                s.children
                    .into_iter()
                    .map(|c| to_lsp_symbol(c, text, lines))
                    .collect(),
            )
        },
    }
}

fn to_symbol_kind(kind: OutlineKind) -> SymbolKind {
    match kind {
        OutlineKind::Class => SymbolKind::CLASS,
        OutlineKind::Struct => SymbolKind::STRUCT,
        OutlineKind::Enum => SymbolKind::ENUM,
        OutlineKind::EnumValue => SymbolKind::ENUM_MEMBER,
        OutlineKind::Namespace => SymbolKind::NAMESPACE,
        OutlineKind::Delegate => SymbolKind::INTERFACE,
        OutlineKind::Event => SymbolKind::EVENT,
        OutlineKind::Function => SymbolKind::FUNCTION,
        OutlineKind::Method => SymbolKind::METHOD,
        OutlineKind::Constructor => SymbolKind::CONSTRUCTOR,
        OutlineKind::Destructor => SymbolKind::METHOD,
        OutlineKind::Operator => SymbolKind::OPERATOR,
        OutlineKind::GlobalVar => SymbolKind::VARIABLE,
        OutlineKind::Field => SymbolKind::FIELD,
        OutlineKind::VirtualProperty => SymbolKind::PROPERTY,
        OutlineKind::Asset => SymbolKind::VARIABLE,
    }
}

fn to_lsp_range(range: TextRange, text: &str, lines: &LineIndex) -> Range {
    let (sl, sc) = lines.line_col_utf16(text, range.start);
    let (el, ec) = lines.line_col_utf16(text, range.end);
    Range::new(Position::new(sl, sc), Position::new(el, ec))
}

/// LSP semanticTokens 数据格式：行/列相对上一个 token 的增量，
/// 长度按 UTF-16 单元计。
fn delta_encode(
    raw: &[tokens::SemanticToken],
    text: &str,
    lines: &LineIndex,
) -> Vec<ls::SemanticToken> {
    let mut data = Vec::with_capacity(raw.len());
    let mut prev = (0u32, 0u32);
    for t in raw {
        let (line, col) = lines.line_col_utf16(text, t.start);
        let len16: u32 = text[t.start as usize..(t.start + t.len) as usize]
            .chars()
            .map(|c| c.len_utf16() as u32)
            .sum();
        let (dl, dc) = if line == prev.0 {
            (0, col - prev.1)
        } else {
            (line - prev.0, col)
        };
        data.push(ls::SemanticToken {
            delta_line: dl,
            delta_start: dc,
            length: len16,
            token_type: t.ty as u32,
            token_modifiers_bitset: 0,
        });
        prev = (line, col);
    }
    data
}
