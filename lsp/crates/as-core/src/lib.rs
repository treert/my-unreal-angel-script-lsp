//! as-core：索引、类型系统、查找链（LSP实现规划 §2.2）。
//!
//! M0 骨架：ID 体系（`id`）+ intern 基础设施（`intern`，实现优化.md 条目 1/2，
//! 决策 D12/D13——接口不变量类，第一天就位）。
//! M1：三阶段流水线 Phase 1+2（`index`）、符号 arena（`symbol`）、
//! 类型表与规范形式 intern（`types`，D17/G7）、tag/doc 分流（`decl_tags`，D15）、
//! CST 访问辅助（`syntax`）、TextRange/行首表（`range`）。
//! `resolve` / `overload` / `expand` 随 M3 落地。
//!
//! 纯库边界：**无 IO、无 async、允许只增 intern 表**（实现优化 §2.4）。
//! 依赖方向：as-core → as-syntax；服务壳（as-lsp）与工具（as-cli）一律经
//! `pub use as_syntax` 取语法能力，不直连 as-syntax。

pub mod aggregation;
pub mod config;
pub mod completion;
pub mod decl_tags;
pub mod diag;
pub mod expand;
pub mod expr;
pub mod hover;
pub mod id;
pub mod inlay;
pub mod index;
pub mod intern;
pub mod logger;
pub mod outline;
pub mod overload;
pub mod range;
pub mod references;
pub mod resolve;
pub mod scope;
pub mod search;
pub mod signature;
pub mod specifiers;
pub mod summary;
pub mod symbol;
pub mod syntax;
pub mod tokens;
pub mod types;
pub mod uses;

pub use as_syntax;

pub use decl_tags::{DocBlock, SemanticTag, TagKind, TagValue};
pub use diag::{script_diags, Diag, DiagCode, DiagSeverity, Suppression};
pub use hover::{hover_markdown, render_doc, render_syn, signature};
pub use id::{DefId, FileId, Sym, TypeId};
pub use aggregation::{Aggregation, DeclRef};
pub use config::IndexConfig;
pub use index::{filename_to_module_name, FileInput, FileKind, WorkspaceIndex};
pub use outline::{document_symbols, folding_ranges, Fold, FoldKind, OutlineKind, OutlineSymbol};
pub use overload::{disambiguate, OverloadScore, Ranked};
pub use range::{LineIndex, TextRange};
pub use references::{
    candidate_files, find_references, match_uses, match_uses_strict, resolve_file_uses, RefTarget,
    UseResolution,
};
pub use search::{query_symbols, MAX_RESULTS};
pub use resolve::{
    resolve_at, LocalDecl, Resolution, Target, LEVEL_ACCESSOR, LEVEL_DECL_SELF, LEVEL_GLOBAL,
    LEVEL_LOCAL, LEVEL_MEMBER, LEVEL_MIXIN, LEVEL_NAMESPACE, LEVEL_THIS_SUPER,
};
pub use symbol::{BaseRef, DefData, DefExtra, DefFlags, DefKind, ParamDecl, SymbolTable};
pub use tokens::{semantic_tokens, SemanticToken, LEGEND};
pub use types::{RefKind, SynType, TypeKind, TypeTable};
pub use uses::{UseRole, UseSite};
