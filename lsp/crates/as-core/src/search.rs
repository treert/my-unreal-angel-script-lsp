//! workspaceSymbol 查询（主索引遍历，规划 §8.1 / M4）。
//!
//! 过滤规则：`SYNTHETIC`（内建 / delegate 展开 / 合成 namespace——D25 纪律：
//! 不进任何遍历统计）、不可达 DefId（append-only arena 的墓碑前残留——
//! 按 main 可达性过滤）、`LOCAL` 函数（模块私有，查询无文件语境无法按
//! 模块过滤 ⇒ 整体排除，M4 定案）。文件已摘除（无快照）同样不可见。
//!
//! 匹配：大小写不敏感子串（客户端侧还会做自己的 fuzzy 过滤）。
//! 结果按声明序，封顶 [`MAX_RESULTS`]（空查询全量返回无消费场景且体积
//! 失控——100k+ 符号序列化数 MB）。

use crate::id::DefId;
use crate::index::WorkspaceIndex;
use crate::intern::sym_str;
use crate::symbol::DefFlags;

/// 结果上限（防空查询全量喷发；有 query 时远达不到）。
pub const MAX_RESULTS: usize = 2048;

pub fn query_symbols(idx: &WorkspaceIndex, query: &str) -> Vec<DefId> {
    let live = idx.live_def_ids();
    let q = query.to_ascii_lowercase();
    let mut out = Vec::new();
    for (id, d) in idx.symbols.iter() {
        if out.len() >= MAX_RESULTS {
            break;
        }
        if !live.contains(&id) {
            continue; // arena 残留（remove+re-add 前的旧 DefId）
        }
        if d.flags.intersects(DefFlags::SYNTHETIC | DefFlags::LOCAL) {
            continue;
        }
        if idx.files.get(&d.file).is_none() {
            continue; // 声明文件已摘除
        }
        if !q.is_empty() && !sym_str(d.name).to_ascii_lowercase().contains(&q) {
            continue;
        }
        out.push(id);
    }
    out
}
