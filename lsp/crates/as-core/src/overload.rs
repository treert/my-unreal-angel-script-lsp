//! 重载解析（LSP实现规划 §7.2，D6：一等模块骨架）。
//!
//! M3 骨架：排序键 **精确 > 通配**。`?` 通配形参匹配任意实参、排序最低
//! （引擎内部语法 §1.4——避免 `FString.opAdd(?)` 遮蔽 `opAdd(FString)`）；
//! 隐式转换表（`opImplConv`、`TSubclassOf→UClass` 等）M5 落地
//! （架构设计 §4.3）。
//!
//! 消费方：signatureHelp（排序 + 激活项）、completion（后缀过滤 + 排序）、
//! references（重载消歧）、hover（选中候选）。M3 仅 hover 接入。

use crate::id::{DefId, Sym, TypeId};
use crate::index::WorkspaceIndex;
use crate::intern::{intern_sym, sym_str};
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

/// 形参的基名（消歧比对用）：剥 Ref/Const/UnresolvedObject/Array 包装；
/// Primitive 按 IndexConfig 归一化（裸 float → float64/float32，D25）；
/// `T[]` → TArray（与 resolve 侧「数组类型落模板本体」同族约定）。
fn param_base_name(idx: &WorkspaceIndex, ty: &SynType) -> Option<Sym> {
    match ty {
        SynType::Primitive(name, _) => Some(if sym_str(*name) == "float" {
            intern_sym(if idx.config.float_is_float64 { "float64" } else { "float32" })
        } else {
            *name
        }),
        SynType::Named(name, _) | SynType::Template { name, .. } => Some(*name),
        SynType::Const(inner) | SynType::Ref(inner, _) | SynType::UnresolvedObject(inner) => {
            param_base_name(idx, inner)
        }
        SynType::Array(_) => Some(intern_sym("TArray")),
        SynType::Qualified(_) | SynType::Auto | SynType::Wildcard => None,
    }
}

/// 调用点消歧（M4 / 架构设计 §4.6）：候选集中 arity 一致、不含 `?` 通配
/// 形参、且全部**可定型实参**与形参基名一致的候选。恰好一个 ⇒ 唯一命中；
/// 否则 None（消歧失败——保留全部重载，消费方「报全部重载」）。
///
/// 实参侧的定型子集见 `resolve::arg_type_base`（字面量 / 标识符 / 链式成员；
/// 运算符 / f-string / range-for 留 M5 与 signatureHelp 同批）。
/// 不可定型实参（None）不排除也不确认候选——两个候选都过 ⇒ 仍 None。
pub fn disambiguate(
    idx: &WorkspaceIndex,
    cands: &[DefId],
    arg_bases: &[Option<DefId>],
) -> Option<DefId> {
    let mut winner: Option<DefId> = None;
    let mut winners = 0;
    for &def in cands {
        let DefExtra::Callable { params, .. } = &idx.def(def).extra else { continue };
        if params.len() != arg_bases.len() {
            continue;
        }
        if params.iter().any(|p| matches!(p.ty, Some(SynType::Wildcard))) {
            continue; // ? 通配形参不参与精确判定（引擎内部语法 §1.4）
        }
        let mut matched = true;
        for (p, a) in params.iter().zip(arg_bases.iter()) {
            let Some(arg_base) = *a else { continue }; // 不可定型：不排除也不确认
            let Some(pname) = p.ty.as_ref().and_then(|t| param_base_name(idx, t)) else {
                matched = false;
                break;
            };
            if pname != idx.def(arg_base).name {
                matched = false;
                break;
            }
        }
        if matched {
            winners += 1;
            if winners > 1 {
                return None;
            }
            winner = Some(def);
        }
    }
    (winners == 1).then_some(winner?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IndexConfig;
    use crate::index::{FileInput, FileKind};
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
