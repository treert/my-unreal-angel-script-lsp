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

use std::sync::Mutex;

use tower_lsp_server::jsonrpc::Result as RpcResult;
use tower_lsp_server::ls_types::{self as ls, *};
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

use as_core::id::FileId;
use as_core::outline::{self, FoldKind, OutlineKind};
use as_core::tokens;
use as_core::{LEGEND, LineIndex, TextRange};

use docs::{DocStore, TextChange};

/// 索引级配置（M2 先存储；全量重建的消费方随 M3 落地，§5.3 / §8.1）。
#[derive(Debug)]
struct ServerConfig {
    /// 对应引擎 `bScriptFloatIsFloat64`（默认 true，架构设计 §5 / G9）
    float_is_float64: bool,
}

struct Backend {
    client: Client,
    docs: Mutex<DocStore>,
    config: Mutex<ServerConfig>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::build(|client| Backend {
        client,
        docs: Mutex::new(DocStore::new()),
        config: Mutex::new(ServerConfig { float_is_float64: true }),
    })
    .finish();

    Server::new(stdin, stdout, socket).serve(service).await;
}

/// URI → 文件路径字符串（仅 file:// URI；Cow 免拷贝）。
fn uri_path(uri: &ls::Uri) -> Option<String> {
    uri.to_file_path()
        .map(|p| p.to_string_lossy().into_owned())
}

impl Backend {
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
            if let Some(b) = values
                .first()
                .and_then(|v| v.get("floatIsFloat64"))
                .and_then(|v| v.as_bool())
            {
                self.config.lock().unwrap().float_is_float64 = b;
            }
        }
        // 客户端不支持 workspace/configuration 时保持默认值（G9：默认对齐引擎）
    }
}

impl LanguageServer for Backend {
    async fn initialize(&self, _params: InitializeParams) -> RpcResult<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                document_symbol_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
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
            format!("my-as-lsp M2 ready (floatIsFloat64={})", config.float_is_float64)
        };
        self.client.log_message(MessageType::INFO, msg).await;
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        if let Some(path) = uri_path(&doc.uri) {
            let mut store = self.docs.lock().unwrap();
            store.open(&path, doc.version, doc.text);
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
            let mut store = self.docs.lock().unwrap();
            if let Some(file) = as_core::intern::file_id_of_path(&path) {
                store.close(file);
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
        if let Some(b) = new_value {
            let changed = {
                let mut config = self.config.lock().unwrap();
                if config.float_is_float64 != b {
                    config.float_is_float64 = b;
                    true
                } else {
                    false
                }
            };
            if changed {
                // 配置变更 ⇒ 全量重建声明索引（§5.3）——消费方随 M3 落地
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!("floatIsFloat64 -> {b} (index rebuild applies from M3)"),
                    )
                    .await;
            }
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
