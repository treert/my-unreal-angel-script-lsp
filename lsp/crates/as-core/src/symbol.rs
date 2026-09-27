//! 符号 arena：DefData / DefKind / DefFlags（LSP实现规划 §3.2）。
//!
//! 重载组不落库（查询时按 name+scope 聚合）；属性访问器视图是对既有
//! Function DefId 的包装查询（§3.2），均不是新符号。

use crate::decl_tags::SemanticTag;
use crate::id::{DefId, FileId, Sym};
use crate::range::TextRange;
use crate::types::SynType;

/// 声明种类全集（对齐 grammar 声明节点 + `.d.as` 形态，规划 §3.2 全表）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DefKind {
    // 类型
    Class,
    Struct,
    Enum,
    EnumValue,
    Namespace,
    /// 一个 .as 模块（M1 未建模块 DefId，占位给后续 local 隔离）
    Module,
    // 类型别名式
    Delegate,
    Event,
    // 可调用
    Function,
    Method,
    Constructor,
    Destructor,
    /// opXxx 重载（索引期按名字判定：`op` + 大写字母开头，规划 §3.2）
    Operator,
    // 数据
    GlobalVar,
    Field,
    Param,
    LocalVar,
    TypeParam,
    AssetDecl,
    // 访问器
    VirtualProperty,
}

impl DefKind {
    pub fn label(self) -> &'static str {
        match self {
            DefKind::Class => "class",
            DefKind::Struct => "struct",
            DefKind::Enum => "enum",
            DefKind::EnumValue => "enum_value",
            DefKind::Namespace => "namespace",
            DefKind::Module => "module",
            DefKind::Delegate => "delegate",
            DefKind::Event => "event",
            DefKind::Function => "function",
            DefKind::Method => "method",
            DefKind::Constructor => "constructor",
            DefKind::Destructor => "destructor",
            DefKind::Operator => "operator",
            DefKind::GlobalVar => "global_var",
            DefKind::Field => "field",
            DefKind::Param => "param",
            DefKind::LocalVar => "local_var",
            DefKind::TypeParam => "type_param",
            DefKind::AssetDecl => "asset",
            DefKind::VirtualProperty => "virtual_property",
        }
    }

    /// 是否为「类型声明」（主索引类型解析的候选集）。
    pub fn is_type_decl(self) -> bool {
        matches!(self, DefKind::Class | DefKind::Struct | DefKind::Enum)
    }

    /// 类型**名**可指向的声明全集：is_type_decl + delegate/event
    /// （delegate/event 也是类型——可声明变量、作形参类型；展开成员集见 expand）。
    pub fn is_type_like(self) -> bool {
        matches!(
            self,
            DefKind::Class | DefKind::Struct | DefKind::Enum | DefKind::Delegate | DefKind::Event
        )
    }

    /// 是否为类型体的成员（成员表 / member_count 对账口径）。
    pub fn is_type_member(self) -> bool {
        matches!(
            self,
            DefKind::Field
                | DefKind::Method
                | DefKind::Constructor
                | DefKind::Destructor
                | DefKind::Operator
                | DefKind::VirtualProperty
        )
    }
}

/// 位标志（规划 §3.2）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DefFlags(u32);

impl DefFlags {
    /// 方法/访问器 const
    pub const CONST: DefFlags = DefFlags(1 << 0);
    pub const PROTECTED: DefFlags = DefFlags(1 << 1);
    /// local 函数：仅声明所在模块可见（BNF §2.6）
    pub const LOCAL: DefFlags = DefFlags(1 << 2);
    /// mixin 函数（两种声明形式等价，架构设计 §4.5.1）
    pub const MIXIN: DefFlags = DefFlags(1 << 3);
    /// 合成符号（模板克隆 / delegate 展开 / 内建基础类型）——永远不计入声明统计
    pub const SYNTHETIC: DefFlags = DefFlags(1 << 4);
    /// .d.as @editable：仅 default 块可写（架构设计 §2.4.3）
    pub const EDITABLE: DefFlags = DefFlags(1 << 5);
    /// .d.as @notProperty：非访问器（反向默认，§2.4.5）
    pub const NOT_PROPERTY: DefFlags = DefFlags(1 << 6);
    /// .d.as @notCallable
    pub const NOT_CALLABLE: DefFlags = DefFlags(1 << 7);
    /// 形参名是 InArgN 占位（§2.4.6），命名实参补全须跳过
    pub const UNNAMED_PARAM: DefFlags = DefFlags(1 << 8);

    pub const NONE: DefFlags = DefFlags(0);

    #[inline]
    pub fn contains(self, other: DefFlags) -> bool {
        self.0 & other.0 == other.0
    }

    #[inline]
    pub fn insert(&mut self, other: DefFlags) {
        self.0 |= other.0;
    }

    #[inline]
    pub fn union(self, other: DefFlags) -> DefFlags {
        DefFlags(self.0 | other.0)
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for DefFlags {
    type Output = DefFlags;
    fn bitor(self, rhs: DefFlags) -> DefFlags {
        self.union(rhs)
    }
}

impl std::ops::BitOrAssign for DefFlags {
    fn bitor_assign(&mut self, rhs: DefFlags) {
        self.insert(rhs);
    }
}

/// 一个形参（语法层记录，进 Callable 特化数据）。
#[derive(Clone, Debug)]
pub struct ParamDecl {
    pub name: Sym,
    /// 名字 token（局部形参 definition/hover 的锚点）
    pub span: TextRange,
    /// 声明类型（语法层，未归一化）
    pub ty: Option<SynType>,
    /// `InArgN` 占位名 → UNNAMED_PARAM（架构设计 §2.4.6，命名实参补全须跳过）
    pub flags: DefFlags,
}

/// 继承基名（Phase 1 语法层记录，未解析；class 闭包在 Phase 2 构建）。
#[derive(Clone, Debug)]
pub struct BaseRef {
    pub name: Sym,
    pub span: TextRange,
    /// 纯标识符形态（可参与闭包解析）；template/qualified 形态 M1 不解析
    pub simple: bool,
}

/// kind 特化数据（规划 §3.2「放 variant」）。
#[derive(Clone, Debug)]
pub enum DefExtra {
    None,
    /// class / struct：基类名 + 模板形参（`.d.as` 模板声明头）
    TypeDecl {
        bases: Vec<BaseRef>,
        template_params: Vec<Sym>,
    },
    /// 函数/方法/构造/析构/delegate/event：返回类型（语法层）+ 形参列表
    Callable {
        return_type: Option<SynType>,
        params: Vec<ParamDecl>,
    },
    /// 字段/全局变量/asset/虚属性：声明类型（语法层）
    Variable {
        ty: Option<SynType>,
    },
    /// enum 成员：`= Expr` 的原文（表达式求值是 Phase 3 的事）
    EnumValue {
        value: Option<Box<str>>,
    },
}

/// 一个符号声明（规划 §3.2 草案字段）。
#[derive(Clone, Debug)]
pub struct DefData {
    pub name: Sym,
    pub kind: DefKind,
    pub file: FileId,
    /// 名字 token（definition/rename/hover 锚点）
    pub name_span: TextRange,
    /// 整个声明
    pub full_span: TextRange,
    /// 所属 class / namespace；顶层为 None（模块归属表是 M3 的 local 隔离）
    pub parent: Option<DefId>,
    /// 合成符号 → 源头声明（D10）
    pub origin: Option<DefId>,
    pub flags: DefFlags,
    pub extra: DefExtra,
    /// 声明前 doc 注释（tag 已分流走）
    pub doc: Option<Box<str>>,
    /// 语义 tag（§2.4.3 白名单；flag 类同时镜像进 flags）
    pub tags: Vec<SemanticTag>,
}

/// 符号 arena：`Vec<DefData>`，DefId 即下标（「相等即同一」）。
#[derive(Debug, Default)]
pub struct SymbolTable {
    defs: Vec<DefData>,
}

impl SymbolTable {
    pub fn new() -> Self {
        SymbolTable { defs: Vec::new() }
    }

    pub fn push(&mut self, def: DefData) -> DefId {
        let id = DefId::from_raw(self.defs.len() as u32);
        self.defs.push(def);
        id
    }

    pub fn get(&self, id: DefId) -> &DefData {
        &self.defs[id.as_usize()]
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (DefId, &DefData)> {
        self.defs
            .iter()
            .enumerate()
            .map(|(i, def)| (DefId::from_raw(i as u32), def))
    }
}
