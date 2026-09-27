//! as-core：索引、类型系统、查找链（LSP实现规划 §2.2）。
//!
//! M0 骨架只落 **ID 体系**（`id`）与 **intern 基础设施**（`intern`，实现优化.md
//! 条目 1/2，决策 D12/D13——接口不变量类，第一天就位）。`symbol` / `types` /
//! `index` / `resolve` 等模块随 M1+ 逐个加入，不提前占位。
//!
//! 纯库边界：**无 IO、无 async、允许只增 intern 表**（实现优化 §2.4）。
//! 依赖方向：as-core → as-syntax；服务壳（as-lsp）与工具（as-cli）一律经
//! `pub use as_syntax` 取语法能力，不直连 as-syntax。

pub mod id;
pub mod intern;

pub use as_syntax;
