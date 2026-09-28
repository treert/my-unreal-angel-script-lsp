//! workspaceSymbol 查询（agg.main 遍历，规划 §8.1 / M4；Phase B：符号
//! arena 删除，改为聚合层倒排遍历——增量维护的 agg 天然无残留条目，
//! 旧 live_def_ids 过滤与「文件已摘除」检查都不再需要）。
//!
//! 过滤规则：`SYNTHETIC`（builtin 伪文件的 primitive——D25 纪律：不进任何
//! 遍历统计；查询期合成成员不进 agg，天然缺席）、`LOCAL` 函数（模块私有，
//! 查询无文件语境无法按模块过滤 ⇒ 整体排除，M4 定案）。
//!
//! 匹配：大小写不敏感子串（客户端侧还会做自己的 fuzzy 过滤）。
//! 结果按 (root, FileId, local) 稳定序（§4.2），封顶 [`MAX_RESULTS`]
//!（空查询全量返回无消费场景且体积失控——100k+ 符号序列化数 MB）。

use crate::aggregation::DeclRef;
use crate::intern::sym_str;
use crate::symbol::DefFlags;
use crate::workspace::Workspace;

/// 结果上限（防空查询全量喷发；有 query 时远达不到）。
pub const MAX_RESULTS: usize = 2048;

pub fn query_symbols(ws: &Workspace, query: &str) -> Vec<DeclRef> {
    let q = query.to_ascii_lowercase();
    let mut out: Vec<DeclRef> = Vec::new();
    for defs in ws.agg.main.values() {
        for &r in defs {
            let d = ws.decl(&r);
            if d.flags.intersects(DefFlags::SYNTHETIC | DefFlags::LOCAL) {
                continue;
            }
            if !q.is_empty() && !sym_str(d.name).to_ascii_lowercase().contains(&q) {
                continue;
            }
            out.push(r);
        }
    }
    // 确定性序（agg.main 是 HashMap）：§4.2 的 (root, FileId, local) 同族
    // 简化——root 信息在 bucket 内已应用，这里按 (file, local) 全局排序
    out.sort_by_key(|r| (r.file, r.local));
    out.truncate(MAX_RESULTS);
    out
}
