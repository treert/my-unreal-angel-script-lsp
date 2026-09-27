//! 重载解析（LSP实现规划 §7.2，D6：一等模块骨架）。
//!
//! M3 骨架：排序键 **精确 > 通配**。`?` 通配形参匹配任意实参、排序最低
//! （引擎内部语法 §1.4——避免 `FString.opAdd(?)` 遮蔽 `opAdd(FString)`）；
//! 隐式转换表（`opImplConv`、`TSubclassOf→UClass` 等）M5 落地
//! （架构设计 §4.3）。
//!
//! 消费方：signatureHelp（排序 + 激活项）、completion（后缀过滤 + 排序）、
//! references（重载消歧）、hover（选中候选）。M3 仅 hover 接入。

use crate::id::{DefId, TypeId};
use crate::index::WorkspaceIndex;
use crate::symbol::DefExtra;
use crate::types::SynType;

/// 排序键。Ord 由劣到优：`ArityMiss < Wildcard < Exact`
/// （降序排序即 精确 > 通配 > 个数不匹配）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum OverloadScore {
    /// 参数个数不匹配。仍保留在结果尾部——references 消歧失败时
    /// 「报全部重载」（架构设计 §4.6）与后续诊断需要完整候选集
    ArityMiss,
    /// 依赖 `?` 通配形参吸收实参——排序最低
    Wildcard,
    /// 参数个数精确匹配且不依赖通配形参
    Exact,
}

/// 一个排序后的候选。
#[derive(Clone, Copy, Debug)]
pub struct Ranked {
    pub def: DefId,
    pub score: OverloadScore,
}

/// 重载解析：候选按可服务性降序（同分保持声明序——sort 稳定）。
/// 参数是实参的归一化类型（个数即可区分当前骨架的全部得分）。
pub fn resolve_overload(
    idx: &WorkspaceIndex,
    cands: &[DefId],
    args: &[TypeId],
) -> Vec<Ranked> {
    let mut out: Vec<Ranked> = cands
        .iter()
        .map(|&def| {
            let (n_params, wilds) = match &idx.def(def).extra {
                DefExtra::Callable { params, .. } => (
                    params.len(),
                    params
                        .iter()
                        .filter(|p| matches!(p.ty, Some(SynType::Wildcard)))
                        .count(),
                ),
                _ => (0, 0),
            };
            let score = if n_params != args.len() {
                OverloadScore::ArityMiss
            } else if wilds > 0 {
                OverloadScore::Wildcard
            } else {
                OverloadScore::Exact
            };
            Ranked { def, score }
        })
        .collect();
    out.sort_by(|a, b| b.score.cmp(&a.score));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{FileInput, FileKind, IndexConfig};
    use crate::intern::{intern_file, intern_sym};

    fn build(src: &str) -> WorkspaceIndex {
        let inputs = vec![FileInput {
            file: intern_file("unique://ovl/o.as", 0),
            kind: FileKind::Script,
            source: src.to_string(),
            module: None,
        }];
        WorkspaceIndex::build(IndexConfig::default(), inputs)
    }

    fn overloads_named(idx: &WorkspaceIndex, name: &str) -> Vec<DefId> {
        idx.main
            .get(&intern_sym(name))
            .map(|ds| ds.to_vec())
            .unwrap_or_default()
    }

    /// TypeId 占位（骨架只看实参个数；真实类型 M4/M5 接入）。
    fn args(n: usize) -> Vec<TypeId> {
        vec![TypeId::from_raw(0); n]
    }

    /// 用例源码全部内置（AGENTS.md 硬性规则 / D1）。

    #[test]
    fn wildcard_ranks_below_exact() {
        // ? 通配形参排序最低（引擎内部语法 §1.4：防 opAdd(?) 遮蔽 opAdd(FString)）
        let idx = build("void F(FString S) {}\nvoid F(? S) {}\n");
        let cands = overloads_named(&idx, "F");
        assert_eq!(cands.len(), 2);
        let ranked = resolve_overload(&idx, &cands, &args(1));
        // 一个实参：两个候选都是 1 参——精确(非通配)在前
        assert_eq!(ranked[0].score, OverloadScore::Exact);
        assert_eq!(ranked[1].score, OverloadScore::Wildcard);
    }

    #[test]
    fn arity_mismatch_ranks_last_but_kept() {
        let idx = build("void F(int A) {}\nvoid F(int A, int B) {}\n");
        let cands = overloads_named(&idx, "F");
        let ranked = resolve_overload(&idx, &cands, &args(0));
        // 零实参：全部 ArityMiss，保持声明序
        assert!(ranked.iter().all(|r| r.score == OverloadScore::ArityMiss));
        assert_eq!(ranked.len(), 2, "个数不匹配也保留在候选集（供诊断/全部重载回退）");

        let ranked = resolve_overload(&idx, &cands, &args(1));
        assert_eq!(ranked[0].score, OverloadScore::Exact);
    }
}
