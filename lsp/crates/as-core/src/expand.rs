//! 声明展开（架构设计 §4.4 规则表）：delegate/event 成员集、类隐含成员。
//!
//! 引擎侧真值：
//! - delegate/event 的成员集来自预处理器 `ProcessDelegates` 的生成模板
//!   （[ENGINE] AngelscriptPreprocessor.cpp:534-695，逐行核对）；
//! - `C.StaticClass()` 是绑定层给每个 **UClass** 注册的命名空间全局函数
//!   （Bind_BlueprintType.cpp:661-680，`UClass StaticClass()`，经
//!   `PreviousBindPassScriptFunctionAsFirstParam` 把类作为隐藏首参——脚本侧
//!   签名就是零参）。struct 不绑定（无 UClass）。
//!   同处的 `__StaticType_<TypeName>` 全局变量是 `__` 前缀内部符号，
//!   「类名直接作值」由 resolve 层的语境判定处理，不在此合成。
//!
//! 合成 DefId 一律 `SYNTHETIC` + `origin` 回落源头声明（D10），
//! 不进任何声明统计（与 D25 内建同一纪律）。
//! `_Inner` 字段（`__` 前缀）不展开——架构设计 §4.4 成员表未列，
//! 展开它只会污染补全。
//!
//! 幂等：`expand_all` 在 finish / reindex 后都会调用，已展开的跳过
//! （reindex 的 remove 已摘除旧合成 DefId，只补被摘除的）。

use crate::id::{DefId, Sym};
use crate::index::WorkspaceIndex;
use crate::intern::intern_sym;
use crate::range::TextRange;
use crate::symbol::{DefData, DefExtra, DefFlags, DefKind, ParamDecl};
use crate::types::{RefKind, SynType};

/// 展开入口（finish / reindex_file 调用）。
pub fn expand_all(idx: &mut WorkspaceIndex) {
    expand_delegates(idx);
    expand_static_class(idx);
}

/// 推入一个合成 DefId。`in_main` = 同时进主索引（只有 StaticClass 的
/// 合成 namespace 需要——resolve 按 Sym 聚合同名 namespace 才找得到它）。
fn push_synth(
    idx: &mut WorkspaceIndex,
    name: Sym,
    kind: DefKind,
    parent: Option<DefId>,
    origin: DefId,
    flags: DefFlags,
    extra: DefExtra,
    in_main: bool,
) -> DefId {
    let src = idx.symbols.get(origin);
    let (file, name_span, full_span) = (src.file, src.name_span, src.full_span);
    let id = idx.symbols.push(DefData {
        name,
        kind,
        file,
        name_span,
        full_span,
        parent,
        origin: Some(origin),
        flags: flags | DefFlags::SYNTHETIC,
        extra,
        doc: None,
        tags: Vec::new(),
    });
    if let Some(p) = parent {
        idx.members.entry(p).or_default().push(id);
    }
    if in_main {
        idx.main.entry(name).or_default().push(id);
    }
    id
}

// ---------------------------------------------------------------------------
// delegate / event 成员集（ProcessDelegates 模板）
// ---------------------------------------------------------------------------

/// 公共集：默认构造、拷贝构造、`opAssign`（单播/多播都有）。
fn push_common_set(idx: &mut WorkspaceIndex, decl: DefId, name: Sym, span: TextRange) {
    // N()
    push_synth(
        idx,
        name,
        DefKind::Constructor,
        Some(decl),
        decl,
        DefFlags::NONE,
        DefExtra::Callable { return_type: None, params: Vec::new() },
        false,
    );
    // N(const N& Other)
    push_synth(
        idx,
        name,
        DefKind::Constructor,
        Some(decl),
        decl,
        DefFlags::NONE,
        DefExtra::Callable {
            return_type: None,
            params: vec![ParamDecl {
                name: intern_sym("Other"),
                ty: Some(t_const_ref_in(SynType::Named(name, span))),
                flags: DefFlags::NONE,
            }],
        },
        false,
    );
    // N& opAssign(const N& Other)
    push_synth(
        idx,
        intern_sym("opAssign"),
        DefKind::Operator,
        Some(decl),
        decl,
        DefFlags::NONE,
        DefExtra::Callable {
            return_type: Some(SynType::Ref(Box::new(SynType::Named(name, span)), RefKind::Plain)),
            params: vec![ParamDecl {
                name: intern_sym("Other"),
                ty: Some(t_const_ref_in(SynType::Named(name, span))),
                flags: DefFlags::NONE,
            }],
        },
        false,
    );
}

/// delegate（单播）：Execute / ExecuteIfBound / BindUFunction / 绑定构造。
fn push_unicast_set(
    idx: &mut WorkspaceIndex,
    decl: DefId,
    return_type: &Option<SynType>,
    params: &[ParamDecl],
) {
    let span = decl_span(idx, decl);
    // R Execute(Args) const —— unbound 时 Throw（引擎 :663）
    push_synth(
        idx,
        intern_sym("Execute"),
        DefKind::Method,
        Some(decl),
        decl,
        DefFlags::CONST,
        DefExtra::Callable { return_type: return_type.clone(), params: params.to_vec() },
        false,
    );
    // R ExecuteIfBound(Args) const —— unbound 时静默返回（引擎 :671）
    push_synth(
        idx,
        intern_sym("ExecuteIfBound"),
        DefKind::Method,
        Some(decl),
        decl,
        DefFlags::CONST,
        DefExtra::Callable { return_type: return_type.clone(), params: params.to_vec() },
        false,
    );
    // void BindUFunction(UObject Object, const FName& BindFunctionName)（引擎 :679）
    push_synth(
        idx,
        intern_sym("BindUFunction"),
        DefKind::Method,
        Some(decl),
        decl,
        DefFlags::NONE,
        DefExtra::Callable {
            return_type: None,
            params: vec![
                ParamDecl { name: intern_sym("Object"), ty: Some(SynType::Named(uobject(), span)), flags: DefFlags::NONE },
                ParamDecl { name: intern_sym("BindFunctionName"), ty: Some(t_const_ref_in(SynType::Named(fname(), span))), flags: DefFlags::NONE },
            ],
        },
        false,
    );
    // 绑定构造 N(UObject Object, const FName& BindFunctionName)（引擎 :683）
    push_synth(
        idx,
        idx.symbols.get(decl).name,
        DefKind::Constructor,
        Some(decl),
        decl,
        DefFlags::NONE,
        DefExtra::Callable {
            return_type: None,
            params: vec![
                ParamDecl { name: intern_sym("Object"), ty: Some(SynType::Named(uobject(), span)), flags: DefFlags::NONE },
                ParamDecl { name: intern_sym("BindFunctionName"), ty: Some(t_const_ref_in(SynType::Named(fname(), span))), flags: DefFlags::NONE },
            ],
        },
        false,
    );
}

/// event（多播）：Broadcast / AddUFunction（无 Execute——引擎 :626-638）。
fn push_multicast_set(
    idx: &mut WorkspaceIndex,
    decl: DefId,
    return_type: &Option<SynType>,
    params: &[ParamDecl],
) {
    let span = decl_span(idx, decl);
    // R Broadcast(Args) const —— 引擎保留返回类型（有返回值时未绑定返回默认值）
    push_synth(
        idx,
        intern_sym("Broadcast"),
        DefKind::Method,
        Some(decl),
        decl,
        DefFlags::CONST,
        DefExtra::Callable { return_type: return_type.clone(), params: params.to_vec() },
        false,
    );
    // void AddUFunction(const UObject Object, const FName& FunctionName)（引擎 :634）
    push_synth(
        idx,
        intern_sym("AddUFunction"),
        DefKind::Method,
        Some(decl),
        decl,
        DefFlags::NONE,
        DefExtra::Callable {
            return_type: None,
            params: vec![
                ParamDecl { name: intern_sym("Object"), ty: Some(SynType::Const(Box::new(SynType::Named(uobject(), span)))), flags: DefFlags::NONE },
                ParamDecl { name: intern_sym("FunctionName"), ty: Some(t_const_ref_in(SynType::Named(fname(), span))), flags: DefFlags::NONE },
            ],
        },
        false,
    );
}

pub fn expand_delegates(idx: &mut WorkspaceIndex) {
    let decls: Vec<DefId> = idx
        .symbols
        .iter()
        .filter(|(_, d)| matches!(d.kind, DefKind::Delegate | DefKind::Event))
        .map(|(id, _)| id)
        .collect();
    for decl in decls {
        // 幂等：已有合成子成员则跳过（reindex 只补被摘除的）
        let expanded = idx
            .members
            .get(&decl)
            .map_or(false, |ms| {
                ms.iter().any(|&m| idx.def(m).flags.contains(DefFlags::SYNTHETIC))
            });
        if expanded {
            continue;
        }
        let d = idx.symbols.get(decl);
        let (name, span) = (d.name, d.name_span);
        let is_multicast = d.kind == DefKind::Event;
        let (return_type, params) = match &d.extra {
            DefExtra::Callable { return_type, params } => (return_type.clone(), params.clone()),
            _ => (None, Vec::new()),
        };
        push_common_set(idx, decl, name, span);
        if is_multicast {
            push_multicast_set(idx, decl, &return_type, &params);
        } else {
            push_unicast_set(idx, decl, &return_type, &params);
        }
    }
}

// ---------------------------------------------------------------------------
// 类隐含成员：StaticClass()
// ---------------------------------------------------------------------------

/// 每个 class C：合成同名 namespace + `UClass StaticClass()`。
/// 合成 namespace 进主索引（resolve 按 Sym 聚合同名 namespace 时命中）。
/// struct 不合成（引擎只给 UClass 绑定，Bind_BlueprintType.cpp:661-680）。
pub fn expand_static_class(idx: &mut WorkspaceIndex) {
    let classes: Vec<DefId> = idx
        .symbols
        .iter()
        .filter(|(_, d)| d.kind == DefKind::Class && !d.flags.contains(DefFlags::SYNTHETIC))
        .map(|(id, _)| id)
        .collect();
    for class in classes {
        let name = idx.symbols.get(class).name;
        // 幂等：该 class 已有对应合成 namespace
        let already = idx.main.get(&name).map_or(false, |ds| {
            ds.iter().any(|&d| {
                let dd = idx.def(d);
                dd.kind == DefKind::Namespace
                    && dd.flags.contains(DefFlags::SYNTHETIC)
                    && dd.origin == Some(class)
            })
        });
        if already {
            continue;
        }
        let ns = push_synth(
            idx,
            name,
            DefKind::Namespace,
            None,
            class,
            DefFlags::NONE,
            DefExtra::None,
            true,
        );
        let span = idx.symbols.get(class).name_span;
        push_synth(
            idx,
            intern_sym("StaticClass"),
            DefKind::Function,
            Some(ns),
            class,
            DefFlags::NONE,
            DefExtra::Callable {
                return_type: Some(SynType::Named(intern_sym("UClass"), span)),
                params: Vec::new(),
            },
            false,
        );
    }
}

/// `const T&in` 的语法层包装（拷贝构造 / opAssign / FName 形参形态）。
fn t_const_ref_in(inner: SynType) -> SynType {
    SynType::Ref(Box::new(SynType::Const(Box::new(inner))), RefKind::In)
}

fn uobject() -> Sym {
    intern_sym("UObject")
}

fn fname() -> Sym {
    intern_sym("FName")
}

/// 委托/事件声明的名字 span（合成成员的占位锚点，definition 走 origin 回落）。
fn decl_span(idx: &WorkspaceIndex, decl: DefId) -> TextRange {
    idx.symbols.get(decl).name_span
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{FileInput, FileKind, IndexConfig};
    use crate::intern::{intern_file, intern_sym, sym_str};

    fn build(srcs: &[(&str, &str)]) -> WorkspaceIndex {
        let inputs = srcs
            .iter()
            .map(|(path, src)| FileInput {
                file: intern_file(path, 0),
                kind: if path.ends_with(".d.as") { FileKind::Decl } else { FileKind::Script },
                source: (*src).to_string(),
                module: None,
            })
            .collect();
        WorkspaceIndex::build(IndexConfig::default(), inputs)
    }

    fn member_names(idx: &WorkspaceIndex, of: DefId) -> Vec<&'static str> {
        idx.members
            .get(&of)
            .map(|ms| ms.iter().map(|&m| sym_str(idx.def(m).name)).collect())
            .unwrap_or_default()
    }

    /// 用例源码全部内置（AGENTS.md 硬性规则 / D1）。

    #[test]
    fn delegate_expansion_member_set() {
        const SRC: &str = "delegate void FMyDelegate(int X, float Y);\n";
        let idx = build(&[("unique://exp/del.as", SRC)]);
        let d = idx
            .main
            .get(&intern_sym("FMyDelegate"))
            .unwrap()
            .iter()
            .copied()
            .find(|&id| idx.def(id).kind == DefKind::Delegate)
            .unwrap();

        let members = idx.members.get(&d).unwrap();
        let names: Vec<&str> = member_names(&idx, d);
        // 单播：ctor×3 + opAssign + Execute + ExecuteIfBound + BindUFunction
        assert_eq!(members.len(), 7, "成员集：{names:?}");
        for expect in ["Execute", "ExecuteIfBound", "BindUFunction", "opAssign"] {
            assert!(names.contains(&expect), "缺 {expect}：{names:?}");
        }
        assert_eq!(members.iter().filter(|&&m| idx.def(m).kind == DefKind::Constructor).count(), 3);

        // origin 回落（D10）+ SYNTHETIC + 不进声明统计
        for &m in members {
            let md = idx.def(m);
            assert_eq!(md.origin, Some(d));
            assert!(md.flags.contains(DefFlags::SYNTHETIC));
        }

        // Execute 的形参从委托声明克隆 + const 方法
        let exec = members
            .iter()
            .copied()
            .find(|&m| sym_str(idx.def(m).name) == "Execute")
            .unwrap();
        let md = idx.def(exec);
        assert!(md.flags.contains(DefFlags::CONST));
        match &md.extra {
            DefExtra::Callable { params, .. } => {
                assert_eq!(params.len(), 2);
                assert_eq!(sym_str(params[0].name), "X");
            }
            _ => panic!("Execute 应有 Callable extra"),
        }

        // 拷贝构造形参形态：const FMyDelegate& Other；绑定构造带 UObject/FName
        let paramed_ctors: Vec<&DefId> = members
            .iter()
            .filter(|&&m| {
                idx.def(m).kind == DefKind::Constructor
                    && matches!(&idx.def(m).extra,
                        DefExtra::Callable { params, .. } if !params.is_empty())
            })
            .collect();
        assert_eq!(paramed_ctors.len(), 2, "拷贝构造 + 绑定构造");
    }

    #[test]
    fn event_expansion_member_set() {
        const SRC: &str = "event void FOnSomething(bool B);\n";
        let idx = build(&[("unique://exp/evt.as", SRC)]);
        let e = idx
            .main
            .get(&intern_sym("FOnSomething"))
            .unwrap()
            .iter()
            .copied()
            .find(|&id| idx.def(id).kind == DefKind::Event)
            .unwrap();

        let names = member_names(&idx, e);
        // 多播：ctor×2 + opAssign + Broadcast + AddUFunction（无 Execute）
        assert_eq!(names.len(), 5, "成员集：{names:?}");
        assert!(names.contains(&"Broadcast"));
        assert!(names.contains(&"AddUFunction"));
        assert!(!names.contains(&"Execute"), "多播没有 Execute（引擎 :626-638）");
        assert!(!names.contains(&"BindUFunction"));
    }

    #[test]
    fn static_class_synthesized_per_class_only() {
        const SRC: &str = "class AActor {}\nstruct FVector {}\n";
        let idx = build(&[("unique://exp/sc.as", SRC)]);
        let actor = idx.lookup_type_def(intern_sym("AActor")).unwrap();

        // 合成 namespace（SYNTHETIC + origin → class）进主索引
        let ns = idx
            .main
            .get(&intern_sym("AActor"))
            .unwrap()
            .iter()
            .copied()
            .find(|&id| idx.def(id).kind == DefKind::Namespace)
            .expect("class 应有合成同名 namespace");
        assert_eq!(idx.def(ns).origin, Some(actor));
        assert!(idx.def(ns).flags.contains(DefFlags::SYNTHETIC));

        // StaticClass()：UClass 返回、零参
        let f = idx.members.get(&ns).unwrap()[0];
        let fd = idx.def(f);
        assert_eq!(sym_str(fd.name), "StaticClass");
        assert_eq!(fd.kind, DefKind::Function);
        match &fd.extra {
            DefExtra::Callable { return_type, params } => {
                assert!(matches!(return_type, Some(SynType::Named(n, _)) if sym_str(*n) == "UClass"));
                assert!(params.is_empty());
            }
            _ => panic!(),
        }
        assert_eq!(fd.origin, Some(actor), "definition 落回类声明（D10）");

        // struct 不合成
        assert!(
            !idx.main
                .get(&intern_sym("FVector"))
                .unwrap()
                .iter()
                .any(|&id| idx.def(id).kind == DefKind::Namespace),
            "struct 不绑定 StaticClass"
        );
    }

    #[test]
    fn expansion_is_idempotent_and_reindex_reexpands() {
        const V1: &str = "delegate void FD(int X);\n";
        const V2: &str = "delegate void FD(int X);\nevent void FE(int Y);\n";
        let path = "unique://exp/re.as";
        let mut idx = build(&[(path, V1)]);
        let d1 = idx
            .main
            .get(&intern_sym("FD"))
            .unwrap()
            .iter()
            .copied()
            .find(|&id| idx.def(id).kind == DefKind::Delegate)
            .unwrap();
        assert_eq!(idx.members.get(&d1).unwrap().len(), 7);

        idx.reindex_file(intern_file(path, 0), FileKind::Script, V2.to_string());
        let d2 = idx
            .main
            .get(&intern_sym("FD"))
            .unwrap()
            .iter()
            .copied()
            .find(|&id| idx.def(id).kind == DefKind::Delegate)
            .unwrap();
        assert_eq!(idx.members.get(&d2).unwrap().len(), 7, "重索引后重新展开，不重复");
        let e = idx
            .main
            .get(&intern_sym("FE"))
            .unwrap()
            .iter()
            .copied()
            .find(|&id| idx.def(id).kind == DefKind::Event)
            .unwrap();
        assert_eq!(member_names(&idx, e).len(), 5, "新增 event 同样展开");
    }
}
