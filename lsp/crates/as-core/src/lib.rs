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

pub mod decl_tags;
pub mod id;
pub mod index;
pub mod intern;
pub mod outline;
pub mod range;
pub mod symbol;
pub mod syntax;
pub mod tokens;
pub mod types;

pub use as_syntax;

pub use decl_tags::{DocBlock, SemanticTag, TagKind, TagValue};
pub use id::{DefId, FileId, Sym, TypeId};
pub use index::{FileInput, FileKind, IndexConfig, WorkspaceIndex};
pub use outline::{document_symbols, folding_ranges, Fold, FoldKind, OutlineKind, OutlineSymbol};
pub use range::{LineIndex, TextRange};
pub use symbol::{BaseRef, DefData, DefExtra, DefFlags, DefKind, SymbolTable};
pub use tokens::{semantic_tokens, SemanticToken, LEGEND};
pub use types::{RefKind, SynType, TypeKind, TypeTable};
