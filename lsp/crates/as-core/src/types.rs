//! 语法层类型（LSP实现规划 §3.3 的活跃子集）：`SynType` 是 CST 解析产物，
//! 名字还是 Sym、带 span，未归一化。
//!
//! **TypeId 归一化体系（TypeTable / TypeKind，D17 / G7）随 Phase C（D39 /
//! C7）退役**：实际实现走 DeclRef + SynType 双件套（expr 的 `syn_type_base`
//! / `template_map` / `subst_syn`），TypeId 从未接入活跃查询路径（消费面
//! 取证见 expr.rs `def_expr_ty` 注释）。M3 若需类型身份系统，按当时需求
//! 重设计（git 历史可考）。
//!
//! 裸 `float` 的归一化（`IndexConfig.float_is_float64` → `float64` /
//! `float32`）在消费点现场判定（expr `syn_type_base`、summary 字面量预推导），
//! 不再有索引期/表级的归一化步骤。

use crate::id::Sym;
use crate::range::TextRange;

/// 引用方向（`&in` / `&out` / `&inout` / 裸 `&`）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RefKind {
    In,
    Out,
    InOut,
    Plain,
}

impl RefKind {
    pub fn label(self) -> &'static str {
        match self {
            RefKind::In => "&in",
            RefKind::Out => "&out",
            RefKind::InOut => "&inout",
            RefKind::Plain => "&",
        }
    }
}

/// 语法层类型（CST 解析产物，名字还是 Sym，未归一化）。
#[derive(Clone, Debug)]
pub enum SynType {
    /// primitive_type token：`float` 归一化目标由 IndexConfig 决定
    Primitive(Sym, TextRange),
    Auto,
    Wildcard,
    /// 单个标识符
    Named(Sym, TextRange),
    /// `Name<Args...>`（使用位模板实例）
    Template {
        name: Sym,
        name_span: TextRange,
        args: Vec<SynType>,
    },
    /// `A::B`（M1 不解析，仅保形）
    Qualified(Vec<(Sym, TextRange)>),
    Array(Box<SynType>),
    Const(Box<SynType>),
    Ref(Box<SynType>, RefKind),
    /// `unresolved_object` 后缀（D8：语法 flag，类型按基类型——解析时剥掉）
    UnresolvedObject(Box<SynType>),
}
