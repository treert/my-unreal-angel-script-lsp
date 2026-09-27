//! 语义配置（as-core 消费的、影响索引 / 类型系统**输出**的开关集合）。
//!
//! 与 as-lsp 的 `WorkspaceConfig` 分层（刻意不合并）：
//! - 本模块 = 纯语义开关——as-core 自身可消费、可测试，无 IO 无路径；
//! - as-lsp `WorkspaceConfig` = 宿主配置——收集根（`script_roots` /
//!   `decl_dirs`）、`${workspaceFolder}` 展开、LSP section 映射，全是
//!   IO / 路径职责，as-core 纯库原则（no IO）不接纳。
//!
//! 任何新增项先问一句：改动它要不要「视同全量重建」？要 → 落这里
//! （改动 IndexConfig ⇒ 重建，规划 §5.3）；不要（如纯服务端 UI 行为）→
//! as-lsp。

/// 索引构建输入参数（规划 §3.3）。改动该配置 ⇒ 视同全量重建（§5.3）。
#[derive(Clone, Copy, Debug)]
pub struct IndexConfig {
    /// 对应引擎 `bScriptFloatIsFloat64`（默认 true）：裸 `float` 归一化到
    /// `float64`（true）还是 `float32`（false）。架构设计 §2.5 / §5。
    pub float_is_float64: bool,
}

impl Default for IndexConfig {
    fn default() -> Self {
        IndexConfig { float_is_float64: true }
    }
}
