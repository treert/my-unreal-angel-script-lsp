//! 诊断内核（M6：框架 + 首批规则，D22——自主设计、按需实现）。
//!
//! - 诊断名唯一登记处是 `docs/诊断码表.md`（实现时命名并登记，G5 / D42）；
//!   [`DiagCode`] 只收录**已实现**的码，素材与 retired 见码表（不预登记）；
//! - severity / 措辞 / range 落点在实现时才定（D22）——本模块即 M6 定案处；
//! - 抑制注释（官方扩展无既有惯例，自定）：行级
//!   `// as-ignore: parse-error`（同行）/ `// as-ignore-next-line: parse-error`
//!   （上一行）/ 逗号分隔多名 / 无码形式 `// as-ignore` 抑制该行全部；
//! - 纯函数边界：输入 CST + 源码文本（+ 工作区事实），输出 [`Vec<Diag>`]，
//!   不依赖 LSP 协议类型（D17 同族）。LSP 映射在 as-lsp 侧。
//!
//! 抑制解析按**原始文本逐行扫描**：字符串字面量里恰好写 `// as-ignore…`
//! 会误报抑制（已知失真，文档化接受——比「ERROR 区吞掉注释节点导致漏抑制」
//! 的树扫描方案失真面更小）。
//!
//! P5 首条符号解析类诊断：[`undefined_call_diags`]（`undefined-function`）——
//! 裸 callee 调用点走完整查找链（局部 → 成员/访问器/mixin → 命名空间 →
//! 全局，`resolve_callee` 同链），无命中即报。与 hover / goto-definition
//! 共用同一解析真值：能跳转的不报，无 hover 的报。

use crate::as_syntax;
use crate::id::FileId;
use crate::range::{LineIndex, TextRange};
use crate::resolve::resolve_at_node;
use crate::syntax;
use crate::workspace::Workspace;
use as_syntax::tree_sitter::Tree;

/// 诊断码——**kebab-case 有意义名**（D42：弃数字码，名字自解释，可作
/// `as-ignore` / 未来配置项 / Problems 面板的稳定标识）。
/// **禁止实现里出现未登记的 code 字符串**（G5：统一走本枚举）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum DiagCode {
    /// `missing-type-decls`：typeDeclarationDirs 全为空，或目录下无任何
    /// `.d.as` ⇒ 引擎类型不可解析（工作区级事实，按打开文档逐个发布，
    /// range = (0,0)）。
    MissingTypeDecls,
    /// `parse-error`：解析错误（tree-sitter `ERROR`/`MISSING` 节点）——已知
    /// 文法缺口的最小闭环（如 f-string 嵌套格式说明符，grammar/README §9）。
    ParseError,
    /// `cyclic-inheritance`：继承环（`Workspace::cycle_diags` 检出的环成员类，
    /// range = base 名字区间——Phase D / D40）。
    CyclicInheritance,
    /// `undefined-function`：调用点解析失败（裸 callee 走完整查找链——局部 /
    /// 成员 / 访问器 / mixin / 命名空间 / 全局——无命中；P5 首条符号解析类
    /// 诊断，与 hover / goto-definition 同一解析管线）。
    UndefinedFunction,
    // 素材（引擎约束取证，码表 §3）不预登记——P5 实现时再命名变体（D22/D42）。
}

impl DiagCode {
    /// 抑制注释里的码字面量 → 枚举（未知码返回 None——静默忽略，与 D15
    /// 「未登记 tag 静默忽略」同族考量）。
    pub fn parse(s: &str) -> Option<DiagCode> {
        match s {
            "missing-type-decls" => Some(DiagCode::MissingTypeDecls),
            "parse-error" => Some(DiagCode::ParseError),
            "cyclic-inheritance" => Some(DiagCode::CyclicInheritance),
            "undefined-function" => Some(DiagCode::UndefinedFunction),
            _ => None,
        }
    }
}

impl std::fmt::Display for DiagCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            DiagCode::MissingTypeDecls => "missing-type-decls",
            DiagCode::ParseError => "parse-error",
            DiagCode::CyclicInheritance => "cyclic-inheritance",
            DiagCode::UndefinedFunction => "undefined-function",
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
/// // as-ignore: parse-error            ← 同行：抑制该行起始的 parse-error
/// // as-ignore: missing-type-decls, parse-error    ← 逗号分隔多码
/// // as-ignore-next-line: parse-error  ← 上一行：抑制下一行
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

/// missing-type-decls 的措辞（M6 定案：工作区级配置/数据缺失，Warning 而非 Error）。
pub const MISSING_TYPE_DECLS_MESSAGE: &str = "未找到任何 .d.as 类型声明，引擎类型将无法解析。\
请检查 myAngelScriptLsp.typeDeclarationDirs 配置，或在 UE 编辑器中执行 \
Tools > Angelscript > Export Type Declarations (.d.as)。";

/// parse-error 的措辞（M6 定案：把「可能是文法缺口而非用户错误」讲清楚）。
pub const PARSE_ERROR_MESSAGE: &str = "语法无法解析。若代码本身正确，\
可能是已知文法缺口（如 f-string 嵌套格式说明符，grammar/README 偏差 §9）。";

/// cyclic-inheritance 的措辞（Phase D / D40）。
pub const CYCLIC_INHERITANCE_MESSAGE: &str = "该类的继承链成环（cyclic inheritance），引擎侧无法编译。";

/// 一个脚本文件的诊断全集（M6：parse-error + 可选 missing-type-decls，经抑制过滤）。
///
/// - `engine_decls_missing`：索引中 `.d.as` 数为 0（工作区级事实）——只对
///   **Script** 文件传 `true`（`.d.as` 自身不适用；由 as-lsp 侧判定）；
/// - parse-error 逐 `ERROR`/`MISSING` 节点一条（数据源 `as_syntax::verify_tree`，
///   不深入 ERROR 子树重复计数——该函数既有契约）；
/// - 输出按 range.start 排序，再按行级抑制过滤（诊断 range **起始行**命中
///   即丢弃）。missing-type-decls 落 (0,0) ⇒ 文件首行 `// as-ignore: missing-type-decls` 可抑制。
pub fn script_diags(tree: &Tree, text: &str, engine_decls_missing: bool) -> Vec<Diag> {
    let mut out = Vec::new();
    if engine_decls_missing {
        out.push(Diag {
            code: DiagCode::MissingTypeDecls,
            range: TextRange::new(0, 0),
            severity: DiagSeverity::Warning,
            message: MISSING_TYPE_DECLS_MESSAGE.to_string(),
        });
    }
    for e in as_syntax::verify_tree(tree) {
        out.push(Diag {
            code: DiagCode::ParseError,
            range: TextRange::new(e.start_byte as u32, e.end_byte as u32),
            severity: DiagSeverity::Error,
            message: PARSE_ERROR_MESSAGE.to_string(),
        });
    }
    out.sort_by_key(|d| d.range.start);
    filter_suppressions(out, text)
}

/// 行级抑制过滤（`parse_suppressions` + 诊断 range 起始行命中即丢弃）。
/// `script_diags` 与 cyclic-inheritance（`cycle_diags` 的产物）共用同一套抑制语义
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

// ---------------------------------------------------------------------------
// undefined-function（P5 首条符号解析类诊断）
// ---------------------------------------------------------------------------

/// 一个文件的**裸 callee 调用点**全集（`call_expression` 的 `function` 字段
/// 为 `identifier`）。共用一个 TreeCursor（`node.walk` 的 FFI 开销，
/// `verify_tree` 同款）；**不深入 ERROR 子树**——错误恢复区域的调用点
/// 语义语境不可靠（parse-error 已在报，双重噪音无意义）。
fn collect_bare_callees<'t>(tree: &'t Tree) -> Vec<as_syntax::tree_sitter::Node<'t>> {
    let mut out = Vec::new();
    let mut c = tree.root_node().walk();
    walk_bare_callees(&mut c, &mut out);
    out
}

fn walk_bare_callees<'t>(
    c: &mut as_syntax::tree_sitter::TreeCursor<'t>,
    out: &mut Vec<as_syntax::tree_sitter::Node<'t>>,
) {
    let node = c.node();
    if node.is_error() || node.is_missing() {
        return;
    }
    if node.kind() == "call_expression" {
        if let Some(f) = node.child_by_field_name("function") {
            if f.kind() == "identifier" {
                out.push(f);
            }
        }
    }
    // 嵌套调用（`F(G())`）仍要深入
    if c.goto_first_child() {
        loop {
            walk_bare_callees(c, out);
            if !c.goto_next_sibling() {
                break;
            }
        }
        c.goto_parent();
    }
}

/// `undefined-function`（未经抑制过滤——由 as-lsp 侧统一滤，与
/// cyclic-inheritance 同构）：逐裸 callee 走 [`resolve_at_node`]
/// （`resolve_callee` 同链：局部 → 成员/访问器/mixin → 命名空间 → 全局），
/// 解析 None 即报。
///
/// 范围与已知边界：
/// - 只查**裸标识符 callee**（`Printxxx("x")`）：成员调用（`A.B()`）依赖
///   接收者定型（D14 宁缺毋假——接收者不可定型时无从判定真伪，不查）、
///   模板构造（`TArray<int>(x)`）文法独立成 `template_type`，均不在内；
/// - `FVector(1,2,3)` 构造调用 / 局部委托变量 `D(5)` / 隐式 this 成员
///   `Tick()` / `Super(...)` 均在链内，解析成功不报；
/// - 引擎函数存在但未导出（数据源缺口，future-work 4.x）时会误报——
///   已知失真，靠 `.d.as` 重新导出自愈，可 `as-ignore` 抑制；
/// - 性能：与 references 同一成本形态（逐站点全链解析），只跑 Script 文件。
pub fn undefined_call_diags(ws: &Workspace, file: FileId, tree: &Tree, text: &str) -> Vec<Diag> {
    let mut out = Vec::new();
    for callee in collect_bare_callees(tree) {
        if resolve_at_node(ws, file, text, callee).is_none() {
            out.push(Diag {
                code: DiagCode::UndefinedFunction,
                range: syntax::span(callee),
                severity: DiagSeverity::Error,
                message: format!(
                    "未定义的函数 '{}'：局部 / 成员 / 命名空间 / 全局作用域中均未找到\
                     该名字（函数、委托或可构造类型）。若这是引擎侧函数，请检查 \
                     .d.as 类型声明是否已导出并配置。",
                    syntax::text(callee, text)
                ),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // 单测用例内置于源码（D1）。

    #[test]
    fn diag_code_display_and_parse() {
        assert_eq!(DiagCode::MissingTypeDecls.to_string(), "missing-type-decls");
        assert_eq!(DiagCode::ParseError.to_string(), "parse-error");
        assert_eq!(DiagCode::parse("missing-type-decls"), Some(DiagCode::MissingTypeDecls));
        assert_eq!(DiagCode::parse("parse-error"), Some(DiagCode::ParseError));
        assert_eq!(DiagCode::parse("no-such-code"), None); // 未登记名静默忽略
        assert_eq!(DiagCode::parse("parse_error"), None); // 名字形态唯一（kebab-case）
    }

    #[test]
    fn parse_error_reports_error_nodes() {
        let src = "void F(\n";
        let tree = as_syntax::parse(src, None);
        let diags = script_diags(&tree, src, false);
        assert!(!diags.is_empty(), "未闭合声明应产出 parse-error");
        assert!(diags.iter().all(|d| d.code == DiagCode::ParseError));
        assert!(diags.iter().all(|d| d.severity == DiagSeverity::Error));
        // range 落在文件内
        assert!(diags.iter().all(|d| d.range.start < src.len() as u32));
    }

    #[test]
    fn parse_error_on_broken_macro_line() {
        // smoke 用例同款形态：宏参数未闭合 + 后续声明（错误恢复吞掉片段）
        let src = "void GlobalFn() {}\nUFUNCTION(Blueprint\nvoid Other() {}\n";
        let tree = as_syntax::parse(src, None);
        let diags = script_diags(&tree, src, false);
        assert!(!diags.is_empty(), "UFUNCTION(Blueprint 未闭合应有 parse-error");
        assert!(diags.iter().any(|d| d.range.start >= 20), "错误不在首行");
    }

    #[test]
    fn clean_source_has_no_diags() {
        let src = "class Foo : UObject\n{\n    int Count;\n    void Tick(float Delta)\n    {\n        Count = Count + 1;\n    }\n}\n";
        let tree = as_syntax::parse(src, None);
        assert!(script_diags(&tree, src, false).is_empty());
    }

    #[test]
    fn missing_type_decls_fires_only_when_decls_missing() {
        let src = "int X = 1;\n";
        let tree = as_syntax::parse(src, None);
        let diags = script_diags(&tree, src, true);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, DiagCode::MissingTypeDecls);
        assert_eq!(diags[0].severity, DiagSeverity::Warning);
        assert_eq!((diags[0].range.start, diags[0].range.end), (0, 0));
        assert!(script_diags(&tree, src, false).is_empty());
    }

    #[test]
    fn suppression_same_line() {
        // ERROR 在行 0，同行尾注释抑制
        let src = "))) // as-ignore: parse-error\n";
        let tree = as_syntax::parse(src, None);
        assert!(script_diags(&tree, src, false).is_empty());
        // 无关码不抑制
        let src = "))) // as-ignore: missing-type-decls\n";
        let tree = as_syntax::parse(src, None);
        assert!(!script_diags(&tree, src, false).is_empty());
    }

    #[test]
    fn suppression_next_line() {
        let src = "// as-ignore-next-line: parse-error\n)))\n";
        let tree = as_syntax::parse(src, None);
        assert!(script_diags(&tree, src, false).is_empty());
        // 只抑制下一行：再下一行的错误保留
        let src = "// as-ignore-next-line: parse-error\nint X = 1;\n)))\n";
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
        let src = "))) // as-ignore: missing-type-decls, parse-error\n";
        let tree = as_syntax::parse(src, None);
        assert!(script_diags(&tree, src, false).is_empty());
    }

    #[test]
    fn suppression_unknown_code_is_noop() {
        // 全为未知码 ⇒ 空表 ⇒ 不抑制任何诊断
        let src = "))) // as-ignore: no-such-code\n";
        let tree = as_syntax::parse(src, None);
        assert!(!script_diags(&tree, src, false).is_empty());
    }

    #[test]
    fn missing_type_decls_suppressible_on_line0() {
        let src = "// as-ignore: missing-type-decls\nint X = 1;\n";
        let tree = as_syntax::parse(src, None);
        assert!(script_diags(&tree, src, true).is_empty());
        // 无码形式同样可抑制
        let src = "// as-ignore\nint X = 1;\n";
        let tree = as_syntax::parse(src, None);
        assert!(script_diags(&tree, src, true).is_empty());
    }

    #[test]
    fn plain_and_doxygen_comments_are_not_suppressions() {
        let src = "// @param X some doc\n// as-ignored: parse-error\n// just a comment\n)))\n";
        let tree = as_syntax::parse(src, None);
        let diags = script_diags(&tree, src, false);
        assert!(!diags.is_empty(), "doxygen/普通注释/as-ignored 均不应抑制");
    }

    #[test]
    fn parse_suppressions_shapes() {
        let text = "int A;\n// as-ignore\n// as-ignore: missing-type-decls, parse-error\n// as-ignore-next-line: parse-error\nint B;\n";
        let sups = parse_suppressions(text);
        assert_eq!(sups.len(), 3);
        assert_eq!(
            sups[0],
            Suppression { line: 1, codes: None }
        );
        assert_eq!(
            sups[1],
            Suppression { line: 2, codes: Some(vec![DiagCode::MissingTypeDecls, DiagCode::ParseError]) }
        );
        assert_eq!(
            sups[2],
            Suppression { line: 4, codes: Some(vec![DiagCode::ParseError]) }
        );
    }

    #[test]
    fn parse_suppressions_code_before_comment() {
        // 代码后缀注释：命中 marker
        let text = "foo(1); // as-ignore: parse-error\n";
        let sups = parse_suppressions(text);
        assert_eq!(sups, vec![Suppression { line: 0, codes: Some(vec![DiagCode::ParseError]) }]);
        // 字符串里的普通 "//" 不误停：跳过它继续找，仍命中真注释
        let text = "string s = \"//\"; // as-ignore\n";
        let sups = parse_suppressions(text);
        assert_eq!(sups, vec![Suppression { line: 0, codes: None }]);
        // 已知失真（模块头文档化接受）：字符串里恰好写标记会被误认——
        // 本例尾缀 `" ;` 使码解析失败 ⇒ 空码表，等价无操作，实际无害
        let text = "string s = \"// as-ignore: parse-error\";\n";
        let sups = parse_suppressions(text);
        assert_eq!(sups, vec![Suppression { line: 0, codes: Some(vec![]) }]);
    }

    #[test]
    fn diag_code_cyclic_inheritance_display_and_parse() {
        assert_eq!(DiagCode::CyclicInheritance.to_string(), "cyclic-inheritance");
        assert_eq!(DiagCode::parse("cyclic-inheritance"), Some(DiagCode::CyclicInheritance));
        assert_eq!(DiagCode::parse("missing-type-decl"), None); // 未登记（近形名不模糊匹配）
    }

    // ------------------------------------------------------------------
    // undefined-function（P5 首条符号解析类诊断）
    // ------------------------------------------------------------------

    fn build_ws(srcs: &[(&str, &str)]) -> Workspace {
        let inputs = srcs
            .iter()
            .map(|(path, src)| crate::workspace::FileInput {
                file: crate::intern::intern_file(path, 0),
                kind: if path.ends_with(".d.as") {
                    crate::workspace::FileKind::Decl
                } else {
                    crate::workspace::FileKind::Script
                },
                module: None,
                source: (*src).to_string(),
            })
            .collect();
        Workspace::build(crate::config::IndexConfig::default(), inputs)
    }

    /// 用户 case 的最小化：Global.d.as 声明 Print；脚本调 Print（可解析）
    /// 与 Printxxx（全链落空）——只报后者，range 落名字区间。
    #[test]
    fn undefined_function_flags_missing_global_only() {
        const DECL: &str = "void Print(float32 Duration);\n";
        const SRC: &str = "\
void test_001()
{
    Print(1.0f);
    Printxxx(\"123\");
}
";
        let ws = build_ws(&[
            ("unique://diag/global.d.as", DECL),
            ("unique://diag/test.as", SRC),
        ]);
        let file = crate::intern::intern_file("unique://diag/test.as", 0);
        let e = ws.files.get(&file).unwrap();
        let diags = undefined_call_diags(&ws, file, &e.tree, &e.source);
        assert_eq!(diags.len(), 1, "只有 Printxxx 报：{diags:?}");
        let d = &diags[0];
        assert_eq!(d.code, DiagCode::UndefinedFunction);
        assert_eq!(d.severity, DiagSeverity::Error);
        let start = SRC.find("Printxxx").unwrap() as u32;
        assert_eq!(d.range, TextRange::new(start, start + "Printxxx".len() as u32));
        assert!(d.message.contains("Printxxx"));
    }

    /// 各可解析形态不报：同文件引用 / 构造调用 / 委托局部 / 隐式 this 成员 /
    /// 命名空间内函数 / 嵌套调用内层命中。
    #[test]
    fn undefined_function_resolves_known_shapes() {
        const SRC: &str = "\
struct FVector { float X; }
delegate void FDel(int X);
class C
{
    int Field;
    void Tick()
    {
        Tick();
        Field = 1;
        FDel D;
        D(5);
        FVector V = FVector();
        Helper(Later());
    }
}
namespace NS
{
    void NsFn() { NsFn(); }
}
void Later() {}
void Helper(int V) {}
";
        let ws = build_ws(&[("unique://diag/shapes.as", SRC)]);
        let file = crate::intern::intern_file("unique://diag/shapes.as", 0);
        let e = ws.files.get(&file).unwrap();
        let diags = undefined_call_diags(&ws, file, &e.tree, &e.source);
        assert!(diags.is_empty(), "全部可解析形态不应报：{diags:?}");
    }

    /// ERROR 子树内的调用点不查（错误恢复区域的语义语境不可靠）。
    #[test]
    fn undefined_function_skips_error_subtree() {
        const SRC: &str = "void F()\n{\n    ??? Printxxx(1);\n}\n";
        let ws = build_ws(&[("unique://diag/err.as", SRC)]);
        let file = crate::intern::intern_file("unique://diag/err.as", 0);
        let e = ws.files.get(&file).unwrap();
        assert!(!as_syntax::verify_tree(&e.tree).is_empty(), "前置：该源确有 parse-error");
        assert!(
            undefined_call_diags(&ws, file, &e.tree, &e.source).is_empty(),
            "ERROR 子树内的 Printxxx 不报"
        );
    }

    #[test]
    fn diag_code_undefined_function_display_and_parse() {
        assert_eq!(DiagCode::UndefinedFunction.to_string(), "undefined-function");
        assert_eq!(DiagCode::parse("undefined-function"), Some(DiagCode::UndefinedFunction));
        assert_eq!(DiagCode::parse("undefined-function2"), None); // 近形名不模糊匹配
    }
}
