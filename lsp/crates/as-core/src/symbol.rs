//! 符号基础类型：DefKind / DefFlags / ParamDecl / BaseRef（LSP实现规划 §3.2）。
//!
//! Phase B（D37）：符号 arena（`DefData` / `SymbolTable` / `DefExtra`）随
//! `WorkspaceIndex` 删除——声明的存储形态统一为 `summary::RawDecl`
//! （per-file，parent = 文件内局部 id），跨文件锚点 = `aggregation::DeclRef`。
//! 重载组不落库（查询时按 name+scope 聚合）；属性访问器视图是对既有
//! Method 声明的包装查询（§3.2），均不是新符号。

use crate::id::Sym;
use crate::range::TextRange;
use crate::types::SynType;

/// 声明种类全集（对齐 grammar 声明节点 + `.d.as` 形态，规划 §3.2 全表）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
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
    /// 脚本方法带 `UFUNCTION(...)` 宏前缀（M5c：`AddUFunction(this, n"|")`
    /// 的 UFUNCTION 名单候选；`.d.as` 侧对应 `@ufunction`/`@event` tag）
    pub const SCRIPT_UFUNCTION: DefFlags = DefFlags(1 << 9);

    pub const NONE: DefFlags = DefFlags(0);

    #[inline]
    pub fn contains(self, other: DefFlags) -> bool {
        self.0 & other.0 == other.0
    }

    #[inline]
    pub fn intersects(self, other: DefFlags) -> bool {
        self.0 & other.0 != 0
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

