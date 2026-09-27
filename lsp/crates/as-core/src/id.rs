//! ID 体系（LSP实现规划 §3）：四个 u32 索引贯穿全库，**相等即同一**，杜绝深比较。
//!
//! | ID | arena | 含义 |
//! |----|-------|------|
//! | `FileId` | 文件注册表（`intern.rs`） | 一个已加载的 `.as` / `.d.as` 文本 |
//! | `Sym` | 字符串 intern（`intern.rs`） | 标识符 / 名字 |
//! | `DefId` | `Vec<DefData>`（M1 `symbol.rs`） | 一个符号声明 |
//! | `TypeId` | 类型表（M1 `types.rs`） | 一个归一化类型 |
//!
//! 接口不变量类骨架（实现优化 §1.1 判据）：这些类型第一天就位，后续任何模块
//! 都不得用裸字符串 / 裸下标替代。`DefId` / `TypeId` 在 M0 仅有类型定义，
//! 其 arena 与构造入口随 M1 落地。

use lasso::Spur;

macro_rules! define_u32_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
        pub struct $name(u32);

        impl $name {
            #[inline]
            pub fn from_raw(raw: u32) -> Self {
                $name(raw)
            }

            #[inline]
            pub fn as_raw(self) -> u32 {
                self.0
            }

            #[inline]
            pub fn as_usize(self) -> usize {
                self.0 as usize
            }
        }
    };
}

define_u32_id!(
    /// 已加载文件的 id。注册表见 `intern.rs`：append-only + 墓碑（D18）。
    FileId
);

define_u32_id!(
    /// 符号声明的 id（arena `Vec<DefData>` 的下标，M1 落地）。
    DefId
);

define_u32_id!(
    /// 归一化类型的 id（类型表下标，M1 落地）。构造只能经 `intern_type()`，
    /// 修饰符须先排到规范形式（LSP实现规划 §3.3，D17）。
    TypeId
);

/// intern 过的符号名（lasso `Spur` 的 newtype：u32，相等即同一字符串）。
///
/// 只能经 `intern::intern_sym()` 构造；渲染回原字符串走 `intern::sym_str()`
/// 或 `Display`（实现优化 §2.2）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Sym(Spur);

impl Sym {
    #[inline]
    pub fn from_spur(spur: Spur) -> Self {
        Sym(spur)
    }

    #[inline]
    pub fn as_spur(self) -> Spur {
        self.0
    }
}
