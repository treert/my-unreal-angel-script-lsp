//! 具名节点种类常量（LSP实现规划 §2.1 `pub mod node`）。
//!
//! 内容按 `grammar/src/node-types.json` 生成（排除 supertype `_` 前缀与匿名
//! 节点），与 `grammar/README.md#与-bnf-的偏差` 的节点命名表一致——类成员与
//! 全局声明共用 `function_declaration` / `variable_declaration`，由父节点判断语境。
//!
//! 重新生成（PowerShell，见 grammar/README 覆盖率脚本）：
//!
//! ```powershell
//! (Get-Content ..\..\..\grammar\src\node-types.json -Raw | ConvertFrom-Json) |
//!     Where-Object { $_.named -and $_.type -notlike '_*' } |
//!     ForEach-Object { $_.type } | Sort-Object -Unique
//! ```
//!
//! 文法节点增删后须同步本文件（单测 `node_kinds_sorted_and_complete` 只守长度与排序）。

/// tree-sitter 错误恢复节点的种类名（判定用 `Node::is_error()`）。
pub const ERROR: &str = "ERROR";
/// `MISSING` 不是 kind 而是 `Node::is_missing()` 标志；此常量仅用于渲染 `(MISSING <kind>)`。
pub const MISSING: &str = "MISSING";

pub const ACCESS_DECLARATION: &str = "access_declaration";
pub const ACCESS_GRANT: &str = "access_grant";
pub const ACCESS_MODIFIER: &str = "access_modifier";
pub const ACCESS_SPECIFIER: &str = "access_specifier";
pub const ARGUMENT: &str = "argument";
pub const ARGUMENT_LIST: &str = "argument_list";
pub const ARRAY_SUFFIX: &str = "array_suffix";
pub const ASSET_DECLARATION: &str = "asset_declaration";
pub const ASSIGNMENT_EXPRESSION: &str = "assignment_expression";
pub const AUTO_TYPE: &str = "auto_type";
pub const BINARY_EXPRESSION: &str = "binary_expression";
pub const BLOCK: &str = "block";
pub const BOOLEAN_LITERAL: &str = "boolean_literal";
pub const BREAK_STATEMENT: &str = "break_statement";
pub const CALL_EXPRESSION: &str = "call_expression";
pub const CASE_CLAUSE: &str = "case_clause";
pub const CAST_EXPRESSION: &str = "cast_expression";
pub const CLASS_BODY: &str = "class_body";
pub const CLASS_DECLARATION: &str = "class_declaration";
pub const COMMENT: &str = "comment";
pub const CONDITIONAL_EXPRESSION: &str = "conditional_expression";
pub const CONSTRUCTOR_DECLARATION: &str = "constructor_declaration";
pub const CONTINUE_STATEMENT: &str = "continue_statement";
pub const DEFAULT_BLOCK: &str = "default_block";
pub const DEFAULT_CLAUSE: &str = "default_clause";
pub const DEFAULT_STATEMENT: &str = "default_statement";
pub const DELEGATE_DECLARATION: &str = "delegate_declaration";
pub const DESTRUCTOR_DECLARATION: &str = "destructor_declaration";
pub const DO_WHILE_STATEMENT: &str = "do_while_statement";
pub const EMPTY_DECLARATION: &str = "empty_declaration";
pub const EMPTY_STATEMENT: &str = "empty_statement";
pub const ENUM_BODY: &str = "enum_body";
pub const ENUM_DECLARATION: &str = "enum_declaration";
pub const ENUMERATOR: &str = "enumerator";
pub const EVENT_DECLARATION: &str = "event_declaration";
pub const EXPRESSION_STATEMENT: &str = "expression_statement";
pub const FALLTHROUGH_STATEMENT: &str = "fallthrough_statement";
pub const FOR_EACH_STATEMENT: &str = "for_each_statement";
pub const FOR_STATEMENT: &str = "for_statement";
pub const FORMAT_INTERPOLATION: &str = "format_interpolation";
pub const FORMAT_SPEC: &str = "format_spec";
pub const FORMAT_STRING: &str = "format_string";
pub const FORMAT_STRING_CONTENT: &str = "format_string_content";
pub const FUNCTION_ATTRIBUTE: &str = "function_attribute";
pub const FUNCTION_DECLARATION: &str = "function_declaration";
pub const HEREDOC_STRING: &str = "heredoc_string";
pub const IDENTIFIER: &str = "identifier";
pub const IF_STATEMENT: &str = "if_statement";
pub const INITIALIZER_LIST: &str = "initializer_list";
pub const MACRO_ARGUMENT: &str = "macro_argument";
pub const MACRO_ARGUMENT_LIST: &str = "macro_argument_list";
pub const MACRO_VALUE: &str = "macro_value";
pub const MEMBER_EXPRESSION: &str = "member_expression";
pub const NAME_LITERAL: &str = "name_literal";
pub const NAMED_ARGUMENT: &str = "named_argument";
pub const NAMESPACE_BODY: &str = "namespace_body";
pub const NAMESPACE_DECLARATION: &str = "namespace_declaration";
pub const NULL_LITERAL: &str = "null_literal";
pub const NUMBER: &str = "number";
pub const PARAMETER: &str = "parameter";
pub const PARAMETER_LIST: &str = "parameter_list";
pub const PARENTHESIZED_EXPRESSION: &str = "parenthesized_expression";
pub const PREPROC_LINE: &str = "preproc_line";
pub const PRIMITIVE_TYPE: &str = "primitive_type";
pub const QUALIFIED_IDENTIFIER: &str = "qualified_identifier";
pub const REFERENCE_MODIFIER: &str = "reference_modifier";
pub const RETURN_STATEMENT: &str = "return_statement";
pub const SCOPED_NAME: &str = "scoped_name";
pub const SOURCE_FILE: &str = "source_file";
pub const STRING_LITERAL: &str = "string_literal";
pub const STRUCT_DECLARATION: &str = "struct_declaration";
pub const SUBSCRIPT_EXPRESSION: &str = "subscript_expression";
pub const SWITCH_BODY: &str = "switch_body";
pub const SWITCH_STATEMENT: &str = "switch_statement";
pub const TEMPLATE_ARGUMENTS: &str = "template_arguments";
pub const TEMPLATE_TYPE: &str = "template_type";
pub const TYPE: &str = "type";
pub const TYPE_PARAMETER: &str = "type_parameter";
pub const TYPE_PARAMETERS: &str = "type_parameters";
pub const UCLASS_SPECIFIERS: &str = "uclass_specifiers";
pub const UENUM_SPECIFIERS: &str = "uenum_specifiers";
pub const UFUNCTION_SPECIFIERS: &str = "ufunction_specifiers";
pub const UMETA_SPECIFIERS: &str = "umeta_specifiers";
pub const UNARY_EXPRESSION: &str = "unary_expression";
pub const UPDATE_EXPRESSION: &str = "update_expression";
pub const UPROPERTY_SPECIFIERS: &str = "uproperty_specifiers";
pub const USTRUCT_SPECIFIERS: &str = "ustruct_specifiers";
pub const VARIABLE_DECLARATION: &str = "variable_declaration";
pub const VARIABLE_DECLARATOR: &str = "variable_declarator";
pub const VIRTUAL_PROPERTY_ACCESSOR: &str = "virtual_property_accessor";
pub const VIRTUAL_PROPERTY_DECLARATION: &str = "virtual_property_declaration";
pub const VOID_ARGUMENT: &str = "void_argument";
pub const WHILE_STATEMENT: &str = "while_statement";
pub const WILDCARD_TYPE: &str = "wildcard_type";

/// `node-types.json` 的全部具名节点种类（字典序，供 `is_named_kind` 二分）。
pub const ALL: &[&str] = &[
    ACCESS_DECLARATION,
    ACCESS_GRANT,
    ACCESS_MODIFIER,
    ACCESS_SPECIFIER,
    ARGUMENT,
    ARGUMENT_LIST,
    ARRAY_SUFFIX,
    ASSET_DECLARATION,
    ASSIGNMENT_EXPRESSION,
    AUTO_TYPE,
    BINARY_EXPRESSION,
    BLOCK,
    BOOLEAN_LITERAL,
    BREAK_STATEMENT,
    CALL_EXPRESSION,
    CASE_CLAUSE,
    CAST_EXPRESSION,
    CLASS_BODY,
    CLASS_DECLARATION,
    COMMENT,
    CONDITIONAL_EXPRESSION,
    CONSTRUCTOR_DECLARATION,
    CONTINUE_STATEMENT,
    DEFAULT_BLOCK,
    DEFAULT_CLAUSE,
    DEFAULT_STATEMENT,
    DELEGATE_DECLARATION,
    DESTRUCTOR_DECLARATION,
    DO_WHILE_STATEMENT,
    EMPTY_DECLARATION,
    EMPTY_STATEMENT,
    ENUM_BODY,
    ENUM_DECLARATION,
    ENUMERATOR,
    EVENT_DECLARATION,
    EXPRESSION_STATEMENT,
    FALLTHROUGH_STATEMENT,
    FOR_EACH_STATEMENT,
    FOR_STATEMENT,
    FORMAT_INTERPOLATION,
    FORMAT_SPEC,
    FORMAT_STRING,
    FORMAT_STRING_CONTENT,
    FUNCTION_ATTRIBUTE,
    FUNCTION_DECLARATION,
    HEREDOC_STRING,
    IDENTIFIER,
    IF_STATEMENT,
    INITIALIZER_LIST,
    MACRO_ARGUMENT,
    MACRO_ARGUMENT_LIST,
    MACRO_VALUE,
    MEMBER_EXPRESSION,
    NAME_LITERAL,
    NAMED_ARGUMENT,
    NAMESPACE_BODY,
    NAMESPACE_DECLARATION,
    NULL_LITERAL,
    NUMBER,
    PARAMETER,
    PARAMETER_LIST,
    PARENTHESIZED_EXPRESSION,
    PREPROC_LINE,
    PRIMITIVE_TYPE,
    QUALIFIED_IDENTIFIER,
    REFERENCE_MODIFIER,
    RETURN_STATEMENT,
    SCOPED_NAME,
    SOURCE_FILE,
    STRING_LITERAL,
    STRUCT_DECLARATION,
    SUBSCRIPT_EXPRESSION,
    SWITCH_BODY,
    SWITCH_STATEMENT,
    TEMPLATE_ARGUMENTS,
    TEMPLATE_TYPE,
    TYPE,
    TYPE_PARAMETER,
    TYPE_PARAMETERS,
    UCLASS_SPECIFIERS,
    UENUM_SPECIFIERS,
    UFUNCTION_SPECIFIERS,
    UMETA_SPECIFIERS,
    UNARY_EXPRESSION,
    UPDATE_EXPRESSION,
    UPROPERTY_SPECIFIERS,
    USTRUCT_SPECIFIERS,
    VARIABLE_DECLARATION,
    VARIABLE_DECLARATOR,
    VIRTUAL_PROPERTY_ACCESSOR,
    VIRTUAL_PROPERTY_DECLARATION,
    VOID_ARGUMENT,
    WHILE_STATEMENT,
    WILDCARD_TYPE,
];

/// 该 kind 是否为本文法的具名节点种类。
/// `ERROR` 是恢复节点、`MISSING` 是标志位，均不算具名语法种类，返回 `false`。
pub fn is_named_kind(kind: &str) -> bool {
    ALL.binary_search(&kind).is_ok()
}
