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
mod workspace;

use std::sync::{Arc, Mutex};

use tower_lsp_server::jsonrpc::Result as RpcResult;
use tower_lsp_server::ls_types::{self as ls, *};
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

use as_core::id::FileId;
use as_core::outline::{self, FoldKind, OutlineKind};
use as_core::resolve::Target;
use as_core::tokens;
use as_core::{LEGEND, LineIndex, TextRange};

use docs::{DocStore, TextChange};
use workspace::{WorkspaceConfig, WorkspaceState};

struct Backend {
    client: Client,
    docs: Arc<Mutex<DocStore>>,
    /// 索引级配置（任一变更 ⇒ 后台全量重建，§5.3）
    config: Mutex<WorkspaceConfig>,
    ws: Arc<WorkspaceState>,
    folders: Mutex<Vec<String>>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::build(|client| Backend {
        client,
        docs: Arc::new(Mutex::new(DocStore::new())),
        config: Mutex::new(WorkspaceConfig::default()),
        ws: Arc::new(WorkspaceState::new()),
        folders: Mutex::new(Vec::new()),
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
        }
        // 客户端不支持 workspace/configuration 时保持默认值（G9：默认对齐引擎）
    }

    /// 启动冷启动后台线程（§6：Phase 0-2 → 发布 → 重放 pending_dirty）。
    fn spawn_index_build(&self) {
        let cfg = self.config.lock().unwrap().clone();
        let folders = self.folders.lock().unwrap().clone();
        let docs = Arc::clone(&self.docs);
        let ws = Arc::clone(&self.ws);
        std::thread::spawn(move || {
            let overlays = {
                let store = docs.lock().unwrap();
                store.overlays()
            };
            let idx = workspace::build_index(&cfg, &folders, &overlays);
            ws.publish_and_replay(idx, &docs);
        });
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
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                document_symbol_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
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
                "my-as-lsp M3 ready (floatIsFloat64={}, scriptRoots={:?}, typeDeclarationDirs={:?})",
                config.float_is_float64, config.script_roots, config.decl_dirs
            )
        };
        self.client.log_message(MessageType::INFO, msg).await;
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
