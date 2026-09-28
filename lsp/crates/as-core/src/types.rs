//! 类型表（LSP实现规划 §3.3）：`TypeKind` + 规范形式 intern（D17 / G7）。
//!
//! M1 子集：归一化 intern（修饰符规范序 + 折叠）与「相等即同一」不变量。
//! 模板实例化 `instantiate()` 与成员克隆（§3.3 后半）随 M3 落地。
//!
//! **基础类型是合成 DefId（D25）**：语料证实 `.d.as` 没有任何基础类型声明
//! （`float64` 全语料仅 2 次且均在注释里）——规划 §3.3 原文「在 .d.as 中有
//! 真实 DefId」与实际导出物不符。落地方式：内建 primitive 按名建 SYNTHETIC
//! DefId（index.rs 注入），类型表仍统一走 `Named`，不设特例。
//! 裸 `float` 在解析层按 `IndexConfig.float_is_float64` 归一化到
//! `float64` / `float32` 的内建 DefId（架构设计 §2.5）。

use std::collections::HashMap;

use crate::aggregation::DeclRef;
use crate::id::{Sym, TypeId};
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

/// 归一化类型（intern 之后「相等即同一」）。
///
/// 规范形式（D17）：修饰符嵌套顺序由外到内 **Ref → Const → Array → 基名**。
/// 所有 TypeId 只能经 [`TypeTable::intern`] 构造（G7：构造入口私有化的
/// 等价物——`TypeKind` 字面量可以拼，但只有 intern 会入表并保证规范序）。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum TypeKind {
    /// FVector / TArray<t_?>（模板实参已 intern）。`def` 是跨文件声明锚点
    /// （Phase B / D37：DefId → DeclRef；builtin 伪文件的 SYNTHETIC decl
    /// 与普通声明同一形态）
    Named { def: DeclRef, args: Vec<TypeId> },
    /// T[]
    Array(TypeId),
    Const(TypeId),
    Ref(TypeId, RefKind),
    /// 模板形参，按声明体内下标（仅 .d.as 模板体内）
    Param(u32),
    /// `?` 通配类型（引擎内部语法 §1.4：与 Auto 严格区分）
    Wildcard,
    /// `auto` 声明侧推导占位（局部变量 / range-for）
    Auto,
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

/// 类型表：结构相等 → 同一 TypeId。
#[derive(Debug, Default)]
pub struct TypeTable {
    kinds: Vec<TypeKind>,
    index: HashMap<TypeKind, TypeId>,
}

impl TypeTable {
    pub fn new() -> Self {
        TypeTable { kinds: Vec::new(), index: HashMap::new() }
    }

    pub fn len(&self) -> usize {
        self.kinds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    pub fn get(&self, id: TypeId) -> &TypeKind {
        &self.kinds[id.as_usize()]
    }

    /// intern 入口：先排到规范形式（重排修饰符 + 折叠 `Const(Const(T))`），
    /// 再查重入表。**所有 TypeId 必须经此构造**（G7）。
    pub fn intern(&mut self, kind: TypeKind) -> TypeId {
        let canonical = self.canonicalize(kind);
        if let Some(&id) = self.index.get(&canonical) {
            return id;
        }
        let id = TypeId::from_raw(self.kinds.len() as u32);
        self.kinds.push(canonical.clone());
        self.index.insert(canonical, id);
        id
    }

    /// 把一条（可能乱序的）修饰符链排到规范序：Ref → Const → Array → 基名。
    ///
    /// 归纳保证：表内已有 TypeId 全部规范 ⇒ 剥壳时内层 TypeId 无需重排，
    /// 只需拆顶层包装、按规范序重裹。折叠：多个 Const 折成一个（D17）。
    fn canonicalize(&mut self, kind: TypeKind) -> TypeKind {
        let mut ref_kind: Option<RefKind> = None;
        let mut has_const = false;
        let mut arrays = 0u32;
        let mut cur = kind;
        let base: TypeId;
        loop {
            match cur {
                TypeKind::Ref(t, k) => {
                    if ref_kind.is_none() {
                        ref_kind = Some(k);
                    }
                    cur = self.kinds[t.as_usize()].clone();
                }
                TypeKind::Const(t) => {
                    has_const = true;
                    cur = self.kinds[t.as_usize()].clone();
                }
                TypeKind::Array(t) => {
                    arrays += 1;
                    cur = self.kinds[t.as_usize()].clone();
                }
                leaf => {
                    // 基名（Named / Param / Wildcard / Auto）——本身也要 intern 去重
                    base = self.insert_canonical(leaf);
                    break;
                }
            }
        }
        let mut out = base;
        for _ in 0..arrays {
            out = self.insert_canonical(TypeKind::Array(out));
        }
        if has_const {
            out = self.insert_canonical(TypeKind::Const(out));
        }
        if let Some(k) = ref_kind {
            out = self.insert_canonical(TypeKind::Ref(out, k));
        }
        self.kinds[out.as_usize()].clone()
    }

    /// 入表（输入须已规范；带去重）。canonicalize 的内部构件，不对外。
    fn insert_canonical(&mut self, kind: TypeKind) -> TypeId {
        if let Some(&id) = self.index.get(&kind) {
            return id;
        }
        let id = TypeId::from_raw(self.kinds.len() as u32);
        self.kinds.push(kind.clone());
        self.index.insert(kind, id);
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_ref() -> DeclRef {
        DeclRef { file: crate::id::FileId::from_raw(0), local: 0 }
    }

    fn named(tt: &mut TypeTable) -> TypeId {
        tt.intern(TypeKind::Named { def: dummy_ref(), args: vec![] })
    }

    /// G7：语义相同的两种修饰符写法必须拿到同一 TypeId（D17 规范形式）。
    #[test]
    fn canonical_form_dedups_modifier_order() {
        let mut tt = TypeTable::new();
        let t = named(&mut tt);
        // Const(Ref(T)) —— 规范序
        let r = tt.intern(TypeKind::Ref(t, RefKind::In));
        let a = tt.intern(TypeKind::Const(r));
        // Ref(Const(T)) —— 语法上等价（`const T&` 的另一种结构）
        let c = tt.intern(TypeKind::Const(t));
        let b = tt.intern(TypeKind::Ref(c, RefKind::In));
        assert_eq!(a, b, "const T&in 的两种结构必须 intern 到同一 TypeId");
        // 表里只有：T、Ref(T)、Const(T)、Ref(Const(T)) 四条，无冗余
        assert_eq!(tt.len(), 4);
    }

    #[test]
    fn const_const_folds() {
        let mut tt = TypeTable::new();
        let t = named(&mut tt);
        let c1 = tt.intern(TypeKind::Const(t));
        let c2 = tt.intern(TypeKind::Const(c1));
        assert_eq!(c1, c2, "Const(Const(T)) 必须折叠为 Const(T)");
    }

    #[test]
    fn identical_kinds_dedup() {
        let mut tt = TypeTable::new();
        let a = tt.intern(TypeKind::Wildcard);
        let b = tt.intern(TypeKind::Wildcard);
        assert_eq!(a, b);
        let n1 = tt.intern(TypeKind::Named { def: DeclRef { file: crate::id::FileId::from_raw(7), local: 0 }, args: vec![a] });
        let n2 = tt.intern(TypeKind::Named { def: DeclRef { file: crate::id::FileId::from_raw(7), local: 0 }, args: vec![b] });
        assert_eq!(n1, n2);
        let n3 = tt.intern(TypeKind::Named { def: DeclRef { file: crate::id::FileId::from_raw(8), local: 0 }, args: vec![a] });
        assert_ne!(n1, n3);
    }
}
