//! 诊断内核（M6：框架 + 首批规则，D22——自主设计、按需实现）。
//!
//! - 码号唯一登记处是 `docs/诊断码表.md`（先占号再写代码，G5）；[`DiagCode`]
//!   只收录**已实现**的码，素材段与 retired 位以注释列出（码号不复用）；
//! - severity / 措辞 / range 落点在实现时才定（D22）——本模块即 M6 定案处；
//! - 抑制注释（官方扩展无既有惯例，自定）：行级
//!   `// as-ignore: AS0903`（同行）/ `// as-ignore-next-line: AS0903`（上一行）/
//!   逗号分隔多码 / 无码形式 `// as-ignore` 抑制该行全部；
//! - 纯函数边界：输入 CST + 源码文本（+ 工作区事实），输出 [`Vec<Diag>`]，
//!   不依赖 LSP 协议类型（D17 同族）。LSP 映射在 as-lsp 侧。
//!
//! 抑制解析按**原始文本逐行扫描**：字符串字面量里恰好写 `// as-ignore…`
//! 会误报抑制（已知失真，文档化接受——比「ERROR 区吞掉注释节点导致漏抑制」
//! 的树扫描方案失真面更小）。

use crate::as_syntax;
use crate::range::{LineIndex, TextRange};
use as_syntax::tree_sitter::Tree;

/// 诊断码。**禁止实现里出现未登记的 code 字符串**（G5：统一走本枚举）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum DiagCode {
    /// `AS0902`：typeDeclarationDirs 全为空，或目录下无任何 `.d.as`
    /// ⇒ 引擎类型不可解析（工作区级事实，按打开文档逐个发布，range = (0,0)）。
    As0902,
    /// `AS0903`：解析错误（tree-sitter `ERROR`/`MISSING` 节点）——已知文法
    /// 缺口的最小闭环（如 f-string 嵌套格式说明符，grammar/README 偏差 §9）。
    As0903,
    /// `AS0907`：继承环（`Workspace::cycle_diags` 检出的环成员类，
    /// range = base 名字区间——Phase D / D40）。
    As0907,
    // —— 登记位（未实现 / retired，不设变体；码号不复用，码表 §1 纪律）——
    // AS0001-0005 / AS0101-0106 / AS0201-0206 / AS0301-0302：引擎约束素材
    //   （码表 §2，P5 按需设计——不构成实现承诺，D22）
    // AS0107 / AS0901 / AS0904 / AS0905 / AS0906：retired
    // 下一个可用：AS0908
}

impl DiagCode {
    /// 抑制注释里的码字面量 → 枚举（未知码返回 None——静默忽略，与 D15
    /// 「未登记 tag 静默忽略」同族考量）。
    pub fn parse(s: &str) -> Option<DiagCode> {
        match s {
            "AS0902" => Some(DiagCode::As0902),
            "AS0903" => Some(DiagCode::As0903),
            "AS0907" => Some(DiagCode::As0907),
            _ => None,
        }
    }
}

impl std::fmt::Display for DiagCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            DiagCode::As0902 => "AS0902",
            DiagCode::As0903 => "AS0903",
            DiagCode::As0907 => "AS0907",
        };
        f.write_str(s)
    }
}

/// 诊断严重度（as-core 自有枚举，不依赖 LSP 协议类型；映射在 as-lsp）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiagSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

/// 一条诊断（range 为字节偏移，§3.2.1）。
#[derive(Clone, Debug)]
pub struct Diag {
    pub code: DiagCode,
    pub range: TextRange,
    pub severity: DiagSeverity,
    pub message: String,
}

/// 一条行级抑制（`// as-ignore…`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suppression {
    /// 生效行（0 起）
    pub line: u32,
    /// `None` = 无码形式（抑制该行全部诊断）；`Some` = 只抑制列出的码
    /// （全为未知码 ⇒ 空表，等于无操作）
    pub codes: Option<Vec<DiagCode>>,
}

/// 解析源码文本里的全部行级抑制注释。
///
/// 形态（注释内容须以 `as-ignore` 开头，词边界严格——`as-ignored` 不匹配）：
///
/// ```text
/// // as-ignore: AS0903            ← 同行：抑制该行起始的 AS0903
/// // as-ignore: AS0902, AS0903    ← 逗号分隔多码
/// // as-ignore-next-line: AS0903  ← 上一行：抑制下一行
/// // as-ignore                    ← 无码：抑制该行全部诊断
/// ```
///
/// 块注释 `/* … */` 不识别（最小实现）；一行只取第一条有效标记。
pub fn parse_suppressions(text: &str) -> Vec<Suppression> {
    let mut out = Vec::new();
    for (line_no, raw_line) in text.split('\n').enumerate() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        // 一行可能含多个 "//"（含字符串字面量误报）——逐个尝试，命中即止
        let mut search = 0usize;
        while let Some(rel) = line[search..].find("//") {
            let comment = &line[search + rel + 2..];
            if let Some((next_line, codes)) = parse_marker(comment) {
                out.push(Suppression {
                    line: line_no as u32 + u32::from(next_line),
                    codes,
                });
                break;
            }
            search += rel + 2;
        }
    }
    out
}

/// 注释文本 → `(是否 next-line 形态, 码表)`；非抑制注释返回 None。
fn parse_marker(comment: &str) -> Option<(bool, Option<Vec<DiagCode>>)> {
    let rest = comment.trim_start().strip_prefix("as-ignore")?;
    let (next_line, rest) = match rest.strip_prefix("-next-line") {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let rest = rest.trim_start();
    if rest.is_empty() {
        return Some((next_line, None)); // 无码形式
    }
    let codes_str = rest.strip_prefix(':')?;
    let codes = codes_str
        .split(',')
        .filter_map(|s| DiagCode::parse(s.trim()))
        .collect::<Vec<_>>();
    Some((next_line, Some(codes)))
}

/// AS0902 的措辞（M6 定案：工作区级配置/数据缺失，Warning 而非 Error）。
pub const AS0902_MESSAGE: &str = "未找到任何 .d.as 类型声明，引擎类型将无法解析。\
请检查 myAngelScriptLsp.typeDeclarationDirs 配置，或在 UE 编辑器中执行 \
Tools > Angelscript > Export Type Declarations (.d.as)。";

/// AS0903 的措辞（M6 定案：把「可能是文法缺口而非用户错误」讲清楚）。
pub const AS0903_MESSAGE: &str = "语法无法解析。若代码本身正确，\
可能是已知文法缺口（如 f-string 嵌套格式说明符，grammar/README 偏差 §9）。";

/// AS0907 的措辞（Phase D / D40）。
pub const AS0907_MESSAGE: &str = "该类的继承链成环（cyclic inheritance），引擎侧无法编译。";

/// 一个脚本文件的诊断全集（M6：AS0903 + 可选 AS0902，经抑制过滤）。
///
/// - `engine_decls_missing`：索引中 `.d.as` 数为 0（工作区级事实）——只对
///   **Script** 文件传 `true`（`.d.as` 自身不适用；由 as-lsp 侧判定）；
/// - AS0903 逐 `ERROR`/`MISSING` 节点一条（数据源 `as_syntax::verify_tree`，
///   不深入 ERROR 子树重复计数——该函数既有契约）；
/// - 输出按 range.start 排序，再按行级抑制过滤（诊断 range **起始行**命中
///   即丢弃）。AS0902 落 (0,0) ⇒ 文件首行 `// as-ignore: AS0902` 可抑制。
pub fn script_diags(tree: &Tree, text: &str, engine_decls_missing: bool) -> Vec<Diag> {
    let mut out = Vec::new();
    if engine_decls_missing {
        out.push(Diag {
            code: DiagCode::As0902,
            range: TextRange::new(0, 0),
            severity: DiagSeverity::Warning,
            message: AS0902_MESSAGE.to_string(),
        });
    }
    for e in as_syntax::verify_tree(tree) {
        out.push(Diag {
            code: DiagCode::As0903,
            range: TextRange::new(e.start_byte as u32, e.end_byte as u32),
            severity: DiagSeverity::Error,
            message: AS0903_MESSAGE.to_string(),
        });
    }
    out.sort_by_key(|d| d.range.start);
    filter_suppressions(out, text)
}

/// 行级抑制过滤（`parse_suppressions` + 诊断 range 起始行命中即丢弃）。
/// `script_diags` 与 AS0907（`cycle_diags` 的产物）共用同一套抑制语义
/// （Phase D / D40 抽出；对已过滤集合幂等）。
pub fn filter_suppressions(mut diags: Vec<Diag>, text: &str) -> Vec<Diag> {
    let sups = parse_suppressions(text);
    if sups.is_empty() {
        return diags;
    }
    let lines = LineIndex::new(text);
    diags.retain(|d| {
        let line = lines.line_of(d.range.start) as u32;
        !sups.iter().any(|s| {
            s.line == line && s.codes.as_ref().map_or(true, |c| c.contains(&d.code))
        })
    });
    diags
}

#[cfg(test)]
mod tests {
    use super::*;

    // 单测用例内置于源码（D1）。

    #[test]
    fn diag_code_display_and_parse() {
        assert_eq!(DiagCode::As0902.to_string(), "AS0902");
        assert_eq!(DiagCode::As0903.to_string(), "AS0903");
        assert_eq!(DiagCode::parse("AS0902"), Some(DiagCode::As0902));
        assert_eq!(DiagCode::parse("AS0903"), Some(DiagCode::As0903));
        assert_eq!(DiagCode::parse("AS9999"), None); // 未登记码静默忽略
        assert_eq!(DiagCode::parse("as0903"), None); // 大小写敏感（码号形态唯一）
    }

    #[test]
    fn as0903_reports_error_nodes() {
        let src = "void F(\n";
        let tree = as_syntax::parse(src, None);
        let diags = script_diags(&tree, src, false);
        assert!(!diags.is_empty(), "未闭合声明应产出 AS0903");
        assert!(diags.iter().all(|d| d.code == DiagCode::As0903));
        assert!(diags.iter().all(|d| d.severity == DiagSeverity::Error));
        // range 落在文件内
        assert!(diags.iter().all(|d| d.range.start < src.len() as u32));
    }

    #[test]
    fn as0903_on_broken_macro_line() {
        // smoke 用例同款形态：宏参数未闭合 + 后续声明（错误恢复吞掉片段）
        let src = "void GlobalFn() {}\nUFUNCTION(Blueprint\nvoid Other() {}\n";
        let tree = as_syntax::parse(src, None);
        let diags = script_diags(&tree, src, false);
        assert!(!diags.is_empty(), "UFUNCTION(Blueprint 未闭合应有 AS0903");
        assert!(diags.iter().any(|d| d.range.start >= 20), "错误不在首行");
    }

    #[test]
    fn clean_source_has_no_diags() {
        let src = "class Foo : UObject\n{\n    int Count;\n    void Tick(float Delta)\n    {\n        Count = Count + 1;\n    }\n}\n";
        let tree = as_syntax::parse(src, None);
        assert!(script_diags(&tree, src, false).is_empty());
    }

    #[test]
    fn as0902_fires_only_when_decls_missing() {
        let src = "int X = 1;\n";
        let tree = as_syntax::parse(src, None);
        let diags = script_diags(&tree, src, true);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, DiagCode::As0902);
        assert_eq!(diags[0].severity, DiagSeverity::Warning);
        assert_eq!((diags[0].range.start, diags[0].range.end), (0, 0));
        assert!(script_diags(&tree, src, false).is_empty());
    }

    #[test]
    fn suppression_same_line() {
        // ERROR 在行 0，同行尾注释抑制
        let src = "))) // as-ignore: AS0903\n";
        let tree = as_syntax::parse(src, None);
        assert!(script_diags(&tree, src, false).is_empty());
        // 无关码不抑制
        let src = "))) // as-ignore: AS0902\n";
        let tree = as_syntax::parse(src, None);
        assert!(!script_diags(&tree, src, false).is_empty());
    }

    #[test]
    fn suppression_next_line() {
        let src = "// as-ignore-next-line: AS0903\n)))\n";
        let tree = as_syntax::parse(src, None);
        assert!(script_diags(&tree, src, false).is_empty());
        // 只抑制下一行：再下一行的错误保留
        let src = "// as-ignore-next-line: AS0903\nint X = 1;\n)))\n";
        let tree = as_syntax::parse(src, None);
        assert!(!script_diags(&tree, src, false).is_empty());
    }

    #[test]
    fn suppression_bare_and_multi_codes() {
        // 无码形式：抑制该行全部
        let src = "))) // as-ignore\n";
        let tree = as_syntax::parse(src, None);
        assert!(script_diags(&tree, src, false).is_empty());
        // 逗号分隔多码：命中的码被抑制
        let src = "))) // as-ignore: AS0902, AS0903\n";
        let tree = as_syntax::parse(src, None);
        assert!(script_diags(&tree, src, false).is_empty());
    }

    #[test]
    fn suppression_unknown_code_is_noop() {
        // 全为未知码 ⇒ 空表 ⇒ 不抑制任何诊断
        let src = "))) // as-ignore: AS9999\n";
        let tree = as_syntax::parse(src, None);
        assert!(!script_diags(&tree, src, false).is_empty());
    }

    #[test]
    fn as0902_suppressible_on_line0() {
        let src = "// as-ignore: AS0902\nint X = 1;\n";
        let tree = as_syntax::parse(src, None);
        assert!(script_diags(&tree, src, true).is_empty());
        // 无码形式同样可抑制
        let src = "// as-ignore\nint X = 1;\n";
        let tree = as_syntax::parse(src, None);
        assert!(script_diags(&tree, src, true).is_empty());
    }

    #[test]
    fn plain_and_doxygen_comments_are_not_suppressions() {
        let src = "// @param X some doc\n// as-ignored: AS0903\n// just a comment\n)))\n";
        let tree = as_syntax::parse(src, None);
        let diags = script_diags(&tree, src, false);
        assert!(!diags.is_empty(), "doxygen/普通注释/as-ignored 均不应抑制");
    }

    #[test]
    fn parse_suppressions_shapes() {
        let text = "int A;\n// as-ignore\n// as-ignore: AS0902, AS0903\n// as-ignore-next-line: AS0903\nint B;\n";
        let sups = parse_suppressions(text);
        assert_eq!(sups.len(), 3);
        assert_eq!(
            sups[0],
            Suppression { line: 1, codes: None }
        );
        assert_eq!(
            sups[1],
            Suppression { line: 2, codes: Some(vec![DiagCode::As0902, DiagCode::As0903]) }
        );
        assert_eq!(
            sups[2],
            Suppression { line: 4, codes: Some(vec![DiagCode::As0903]) }
        );
    }

    #[test]
    fn parse_suppressions_code_before_comment() {
        // 代码后缀注释：命中 marker
        let text = "foo(1); // as-ignore: AS0903\n";
        let sups = parse_suppressions(text);
        assert_eq!(sups, vec![Suppression { line: 0, codes: Some(vec![DiagCode::As0903]) }]);
        // 字符串里的普通 "//" 不误停：跳过它继续找，仍命中真注释
        let text = "string s = \"//\"; // as-ignore\n";
        let sups = parse_suppressions(text);
        assert_eq!(sups, vec![Suppression { line: 0, codes: None }]);
        // 已知失真（模块头文档化接受）：字符串里恰好写标记会被误认——
        // 本例尾缀 `" ;` 使码解析失败 ⇒ 空码表，等价无操作，实际无害
        let text = "string s = \"// as-ignore: AS0903\";\n";
        let sups = parse_suppressions(text);
        assert_eq!(sups, vec![Suppression { line: 0, codes: Some(vec![]) }]);
    }

    #[test]
    fn diag_code_as0907_display_and_parse() {
        assert_eq!(DiagCode::As0907.to_string(), "AS0907");
        assert_eq!(DiagCode::parse("AS0907"), Some(DiagCode::As0907));
        assert_eq!(DiagCode::parse("AS0908"), None); // 未登记
    }
}
