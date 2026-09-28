//! 局部作用域树（LSP实现规划 Phase A / index-architecture.md §3.3，D37）。
//!
//! **FileSummary 必含组件**：函数体内的局部/形参/迭代变量 + 预推导类型。
//! 遮蔽语义的正确性根基是块嵌套——提取期一次建好，查询期 O(链深) 上溯。
//!
//! AS 简化（相对 mylua scope.rs）：无 lambda / 闭包逃逸 ⇒ 树只反映语法块
//! 嵌套，无捕获记账、无 upvalue；`local` 函数的**模块级**可见性不在这棵树
//! （那是聚合层 `module_files` 的职责），树里只有函数体内局部。
//!
//! 可见性规则（与现行 `resolve.rs` 的 `span.start <= byte` 同口径）：
//! 候选 scope = 最内层包含查询点的块沿 parent 链上溯；scope 内仅
//! `name_span.start <= byte` 的声明可见；同链最近者优先（遮蔽）；
//! 同 scope 内同名后声明者优先。

use crate::id::Sym;
use crate::range::TextRange;
use crate::types::SynType;

/// 局部声明种类。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LocalKind {
    /// 局部变量（`int X = 1;`）
    Var,
    /// 形参
    Param,
    /// range-for 迭代变量（`for (auto E : Items)`）
    IterVar,
}

/// 一个局部声明（scope 内条目）。
#[derive(Clone, Debug)]
pub struct LocalDecl {
    pub name: Sym,
    pub kind: LocalKind,
    /// 声明锚点（rename / definition）
    pub name_span: TextRange,
    /// 预推导类型（summary 期四类：Cast/构造/复制/字面量）；推不出 = None
    /// （L3 查询期补——宁缺毋假，见 index-architecture §3.3.1）。
    pub ty: Option<SynType>,
}

/// 一个作用域节点（函数体 / 嵌套块 / for-range 体）。
#[derive(Clone, Debug)]
pub struct Scope {
    /// 父作用域（函数体为 None）
    pub parent: Option<u32>,
    /// 块 span（查询点包含判定）
    pub span: TextRange,
    pub decls: Vec<LocalDecl>,
}

/// 每文件作用域树：Scope arena + parent 链。
#[derive(Clone, Debug, Default)]
pub struct ScopeTree {
    scopes: Vec<Scope>,
}

impl ScopeTree {
    /// 空树（无函数体的文件）。
    pub fn empty() -> ScopeTree {
        ScopeTree { scopes: Vec::new() }
    }

    /// 提取器落树入口（arena 顺序即构造顺序，parent 索引指向 arena 下标）。
    pub fn from_scopes(scopes: Vec<Scope>) -> ScopeTree {
        ScopeTree { scopes }
    }

    fn scope(&self, i: u32) -> &Scope {
        &self.scopes[i as usize]
    }

    /// 最内层包含 `byte` 的 scope 下标（包含 = `span.start <= byte < span.end`；
    /// 多个包含时取嵌套最深者——子块 span 恒被父块包含，深度最大即最内）。
    fn innermost(&self, byte: u32) -> Option<u32> {
        let mut best: Option<(u32 /*depth*/, u32 /*idx*/)> = None;
        for (i, s) in self.scopes.iter().enumerate() {
            if !s.span.contains(byte) {
                continue;
            }
            let mut depth = 0u32;
            let mut p = s.parent;
            while let Some(px) = p {
                depth += 1;
                p = self.scope(px).parent;
            }
            if best.map_or(true, |(d, _)| depth > d) {
                best = Some((depth, i as u32));
            }
        }
        best.map(|(_, i)| i)
    }

    /// 在查询点解析一个局部名字：最内层 scope 沿 parent 链上溯，最近声明
    /// 优先（遮蔽）；scope 内同名后声明者优先。找不到 = None。
    pub fn resolve_local(&self, byte: u32, name: Sym) -> Option<&LocalDecl> {
        let mut cur = self.innermost(byte)?;
        loop {
            let s = self.scope(cur);
            // 同 scope 内取 name_span.start <= byte 的最后一个同名声明
            let mut hit: Option<&LocalDecl> = None;
            for d in &s.decls {
                if d.name == name && d.name_span.start <= byte {
                    hit = Some(d);
                }
            }
            if let Some(d) = hit {
                return Some(d);
            }
            cur = s.parent?;
        }
    }

    /// 查询点可见的全部局部声明（completion 候选）：沿链收集，同名只留
    /// 最近者；返回序 = 内层在前。
    pub fn locals_visible(&self, byte: u32) -> Vec<&LocalDecl> {
        let mut out: Vec<&LocalDecl> = Vec::new();
        let mut seen: Vec<Sym> = Vec::new();
        let mut cur = match self.innermost(byte) {
            Some(c) => c,
            None => return out,
        };
        loop {
            let s = self.scope(cur);
            // 后声明者优先 ⇒ 倒序扫描，先见者即该 scope 的胜者
            for d in s.decls.iter().rev() {
                if d.name_span.start <= byte && !seen.contains(&d.name) {
                    seen.push(d.name);
                    out.push(d);
                }
            }
            match s.parent {
                Some(p) => cur = p,
                None => return out,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intern::{intern_sym, sym_str};

    // 用例源码全部内置（AGENTS.md 硬性规则 / D1）。
    // 手工构造树（提取逻辑在 summary.rs，本模块只测数据结构 + 原语）。

    fn tree() -> ScopeTree {
        ScopeTree::from_scopes(vec![
            Scope {
                parent: None,
                span: TextRange::new(0, 100),
                decls: vec![LocalDecl {
                    name: intern_sym("A"),
                    kind: LocalKind::Var,
                    name_span: TextRange::new(5, 6),
                    ty: None,
                }],
            },
            Scope {
                parent: Some(0),
                span: TextRange::new(40, 80),
                decls: vec![LocalDecl {
                    name: intern_sym("A"),
                    kind: LocalKind::Var,
                    name_span: TextRange::new(50, 51),
                    ty: None,
                }],
            },
        ])
    }

    #[test]
    fn shadowing_inner_wins() {
        let t = tree();
        let d = t.resolve_local(60, intern_sym("A")).unwrap();
        assert_eq!(d.name_span, TextRange::new(50, 51), "内层遮蔽外层");
    }

    #[test]
    fn visibility_starts_at_decl() {
        let t = tree();
        assert!(
            t.resolve_local(10, intern_sym("A")).is_some(),
            "外层声明 5 已可见"
        );
        // byte=45 在内层块（40..80），但内层声明在 50 未到 → 命中外层
        let d = t.resolve_local(45, intern_sym("A")).unwrap();
        assert_eq!(d.name_span, TextRange::new(5, 6), "声明点之前回落外层");
    }

    #[test]
    fn locals_visible_unions_chain() {
        let t = tree();
        let mut names: Vec<&str> =
            t.locals_visible(60).into_iter().map(|d| sym_str(d.name)).collect();
        names.sort();
        assert_eq!(names, vec!["A"], "同名遮蔽只留最近者");
    }

    #[test]
    fn resolve_outside_any_scope_is_none() {
        let t = tree();
        assert!(t.resolve_local(150, intern_sym("A")).is_none(), "树外查询");
        assert!(t.locals_visible(150).is_empty(), "树外无可见局部");
    }

    #[test]
    fn same_scope_last_decl_wins() {
        // 同 scope 内重声明（`int A = 1; float A = 2;`）：后声明者优先
        let t = ScopeTree::from_scopes(vec![Scope {
            parent: None,
            span: TextRange::new(0, 100),
            decls: vec![
                LocalDecl {
                    name: intern_sym("A"),
                    kind: LocalKind::Var,
                    name_span: TextRange::new(10, 11),
                    ty: None,
                },
                LocalDecl {
                    name: intern_sym("A"),
                    kind: LocalKind::Var,
                    name_span: TextRange::new(50, 51),
                    ty: None,
                },
            ],
        }]);
        assert_eq!(t.resolve_local(60, intern_sym("A")).unwrap().name_span.start, 50);
        assert_eq!(t.resolve_local(20, intern_sym("A")).unwrap().name_span.start, 10);
    }
}
