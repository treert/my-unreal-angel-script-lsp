/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

// Unreal Angelscript grammar.
//
// Specification: ./angelscript.bnf  (Layer A = what users write in .as files).
// The same grammar parses `.d.as` declaration files.  The two syntax sets
// CROSS, they are not subsets of each other (see ./README.md): `.d.as` alone
// has template declaration headers / `?` / `unresolved_object` /
// `@templateSpecialization` blocks, while `.as` alone has function bodies and
// statements.  Deviations from the BNF node-naming table are listed in
// ./README.md.
//
// Expression precedence — angelscript.bnf Part 4 / §5.5.
const PREC = {
  ASSIGN: 1,
  TERNARY: 2,
  LOGIC_OR: 3,
  LOGIC_AND: 4,
  BIT_OR: 6,
  BIT_XOR: 7,
  BIT_AND: 8,
  EQUALITY: 9,
  RELATIONAL: 10,
  SHIFT: 11,
  ADDITIVE: 12,
  MULTIPLICATIVE: 13,
  POWER: 14,
  UNARY: 15,
  POSTFIX: 16,
};

// Dynamic precedence used to resolve the C++-style declaration/expression
// ambiguities (angelscript.bnf §5.4) that survive GLR exploration.
const DYN = {
  FUNCTION_DECL: 3,
  VARIABLE_DECL: 2,
  NAMED_ARGUMENT: 2,
  TYPE: 1,
};

/** @param {RuleOrLiteral} rule */
function commaSep1(rule) {
  return seq(rule, repeat(seq(',', rule)));
}

/** @param {RuleOrLiteral} rule */
function commaSep(rule) {
  return optional(commaSep1(rule));
}

module.exports = grammar({
  name: 'angelscript',

  externals: $ => [
    $.format_string_content,
    $.format_spec,
  ],

  extras: $ => [
    /[\s\uFEFF\u2060\u200B]/,
    $.comment,
    $.preproc_line,
  ],

  word: $ => $.identifier,

  supertypes: $ => [
    $._declaration,
    $._statement,
    $._expression,
  ],

  inline: $ => [
    $._top_level_item,
    $._class_member,
    $._member_modifier,
    $._type_identifier,
    $._callee,
  ],

  // GLR conflicts — angelscript.bnf §5.4.  Only the four listed below are
  // actually needed; the remaining ambiguities of §5.4 (function vs variable
  // declaration, classic vs range for, virtual property vs variable) fall out
  // of these plus the dynamic precedences in DYN.
  conflicts: $ => [
    // `Foo Bar;` vs `Foo(Bar);` — declaration vs expression statement.
    [$.type, $._expression],
    // `Type Name(X)` — parameter declaration vs constructor-call arguments.
    [$.parameter_list, $.argument_list],
    // `A<B, C>(x)` — template instantiation vs relational expression chain.
    [$.template_type, $._expression],
    // `f(Name = value)` / `f(Name: value)` — named argument vs expression
    // vs parameter declaration (`Type Name = default`).
    [$.named_argument, $._expression],
    [$.named_argument, $._expression, $.type],
  ],

  rules: {
    // ========================================================================
    // Part 2.1 — Script
    // ========================================================================

    source_file: $ => repeat($._top_level_item),

    _top_level_item: $ => choice(
      $.empty_declaration,
      $._declaration,
    ),

    empty_declaration: _ => ';',

    _declaration: $ => choice(
      $.namespace_declaration,
      $.class_declaration,
      $.struct_declaration,
      $.enum_declaration,
      $.delegate_declaration,
      $.event_declaration,
      $.asset_declaration,
      $.function_declaration,
      $.variable_declaration,
      // ParseScript dispatches virtual properties at top level too
      $.virtual_property_declaration,
    ),

    // REMOVED (see angelscript.bnf §2.9 [paper]): `import` is a live token
    // and the engine parser accepts it, but the UE host never binds imported
    // functions (runtime failure "Unbound function called").  Not implemented
    // to keep the grammar aligned with what actually runs.
    // import_declaration: $ => seq(
    //   'import',
    //   field('type', $.type),
    //   field('name', $.identifier),
    //   field('parameters', $.parameter_list),
    //   'from',
    //   field('source', $.string_literal),
    //   ';',
    // ),

    // ========================================================================
    // Part 2.2 — Namespace
    // ========================================================================

    // NOTE: the optional trailing ';' of a declaration is grabbed greedily
    // (prec.right) so it is never mistaken for an `empty_declaration`.
    namespace_declaration: $ => prec.right(seq(
      'namespace',
      field('name', $.scoped_name),
      field('body', $.namespace_body),
      optional(';'),
    )),

    namespace_body: $ => seq('{', repeat($._top_level_item), '}'),

    scoped_name: $ => seq($.identifier, repeat(seq('::', $.identifier))),

    // ========================================================================
    // Part 2.3 — Class / Struct
    // ========================================================================

    class_declaration: $ => prec.right(seq(
      optional(field('specifiers', $.uclass_specifiers)),
      'class',
      field('name', $.identifier),
      optional(field('type_parameters', $.type_parameters)),
      choice(
        ';',                                  // forward declaration
        seq(
          optional(seq(':', commaSep1(field('base', $._type_identifier)))),
          field('body', $.class_body),
          optional(';'),
        ),
      ),
    )),

    struct_declaration: $ => prec.right(seq(
      optional(field('specifiers', $.ustruct_specifiers)),
      'struct',
      field('name', $.identifier),
      optional(field('type_parameters', $.type_parameters)),
      choice(
        ';',                                  // forward declaration
        seq(
          optional(seq(':', commaSep1(field('base', $._type_identifier)))),
          field('body', $.class_body),
          optional(';'),
        ),
      ),
    )),

    // `.d.as` template declarations: `struct TMap<K, V>`
    type_parameters: $ => seq(
      '<',
      commaSep1(alias($.identifier, $.type_parameter)),
      '>',
    ),

    class_body: $ => seq('{', repeat($._class_member), '}'),

    _class_member: $ => choice(
      $.empty_declaration,
      $.access_declaration,
      $.default_statement,
      $.constructor_declaration,
      $.destructor_declaration,
      $.function_declaration,
      $.variable_declaration,
      $.virtual_property_declaration,
    ),

    // 2.3.1 — visibility / access level / UE macro applied to the following
    // declaration.  The engine accepts these in any order and any combination
    // of `private`/`protected`/`access:Level`/`UPROPERTY()`/`UFUNCTION()`.
    _member_modifier: $ => choice(
      'private',
      'protected',
      $.access_specifier,
      field('specifiers', $.uproperty_specifiers),
      field('specifiers', $.ufunction_specifiers),
    ),

    access_specifier: $ => seq('access', ':', field('name', $.identifier)),

    // 2.3.2 — `access Name = private, UOther (readonly), *;`
    access_declaration: $ => seq(
      'access',
      field('name', $.identifier),
      '=',
      field('level', choice('private', 'protected')),
      repeat(seq(',', $.access_grant)),
      ';',
    ),

    access_grant: $ => seq(
      field('target', choice($.scoped_name, '*')),
      optional(seq('(', commaSep1($.access_modifier), ')')),
    ),

    access_modifier: _ => choice('readonly', 'editdefaults', 'inherited'),

    // 2.3.3 — `default X = Y;` / `default { ... }`
    default_statement: $ => seq(
      'default',
      choice(
        seq(field('value', $._expression), ';'),
        field('body', $.default_block),
      ),
    ),

    default_block: $ => seq('{', repeat($.expression_statement), '}'),

    // 2.3.4 — constructor / destructor
    constructor_declaration: $ => seq(
      repeat($._member_modifier),
      field('name', $.identifier),
      field('parameters', $.parameter_list),
      repeat(field('attribute', $.function_attribute)),
      choice(field('body', $.block), ';'),
    ),

    destructor_declaration: $ => seq(
      repeat($._member_modifier),
      '~',
      field('name', $.identifier),
      field('parameters', $.parameter_list),
      repeat(field('attribute', $.function_attribute)),
      choice(field('body', $.block), ';'),
    ),

    // 2.3.7 — virtual property.  The engine parser supports it
    // (ParseVirtualPropertyDecl) but it is a paper feature in the UE fork:
    // zero usage across engine/plugin scripts.  Parsed anyway so it never
    // produces a hard error.
    virtual_property_declaration: $ => seq(
      repeat($._member_modifier),
      field('type', $.type),
      field('name', $.identifier),
      '{',
      repeat($.virtual_property_accessor),
      '}',
    ),

    // Accessors take the full method attribute set (as_parser.cpp
    // ParseVirtualPropertyDecl -> ParseMethodAttributes).
    //
    // NOTE: the bodyless form (`get;` / `set;`) parses, but the builder
    // rejects it with "Property accessor must be implemented" unless the
    // owner is an interface (as_builder.cpp RegisterVirtualProperty) — and
    // `interface` is a dead token in the UE fork.  Kept accepted here so the
    // LSP can report a *semantic* error instead of a parse error.
    virtual_property_accessor: $ => seq(
      choice('get', 'set'),
      optional('const'),
      repeat(field('attribute', $.function_attribute)),
      choice(field('body', $.block), ';'),
    ),

    // ========================================================================
    // Part 2.4 — Enum
    // ========================================================================

    enum_declaration: $ => prec.right(seq(
      optional(field('specifiers', $.uenum_specifiers)),
      'enum',
      field('name', $.identifier),
      choice(
        ';',                                  // forward declaration
        seq(field('body', $.enum_body), optional(';')),
      ),
    )),

    enum_body: $ => seq(
      '{',
      optional(seq(commaSep1($.enumerator), optional(','))),
      '}',
    ),

    enumerator: $ => seq(
      field('name', $.identifier),
      optional(seq('=', field('value', $._expression))),
      optional(field('specifiers', $.umeta_specifiers)),
    ),

    // ========================================================================
    // Part 2.5 — Delegate / Event
    // ========================================================================

    delegate_declaration: $ => seq(
      'delegate',
      field('type', $.type),
      field('name', $.identifier),
      field('parameters', $.parameter_list),
      optional('const'),
      repeat(field('attribute', $.function_attribute)),
      ';',
    ),

    event_declaration: $ => seq(
      'event',
      field('type', $.type),
      field('name', $.identifier),
      field('parameters', $.parameter_list),
      optional('const'),
      repeat(field('attribute', $.function_attribute)),
      ';',
    ),

    // ========================================================================
    // Part 2.8 — `asset Name of Type`
    // ========================================================================

    asset_declaration: $ => prec.right(seq(
      'asset',
      field('name', $.identifier),
      'of',
      field('type', $._type_identifier),
      optional(';'),
    )),

    // ========================================================================
    // Part 2.6 / 2.7 — Functions & variables (global and member)
    // ========================================================================

    function_declaration: $ => prec.dynamic(DYN.FUNCTION_DECL, seq(
      repeat($._member_modifier),
      repeat(field('qualifier', choice('mixin', 'local'))),
      field('type', $.type),
      field('name', $.identifier),
      field('parameters', $.parameter_list),
      optional('const'),
      repeat(field('attribute', $.function_attribute)),
      choice(field('body', $.block), ';'),
    )),

    // as_parser.cpp ParseMethodAttributes — the complete set (12 tokens).
    function_attribute: _ => choice(
      'final',
      'override',
      'property',
      'mixin',
      'accept_temporary_this',
      'external_implicit_this',
      'no_discard',
      'allow_discard',
      '__generated',
      'deprecated',
      'defaults',
      'unsafe_during_construction',
    ),

    // NOTE: the explicit empty-list form `(void)` is not a separate
    // production — it parses as a single `parameter` whose type is `void`
    // (see README.md "与 BNF 的偏差").
    parameter_list: $ => seq(
      '(',
      optional(seq(commaSep1($.parameter), optional(','))),
      ')',
    ),

    parameter: $ => seq(
      field('type', $.type),
      optional(field('name', $.identifier)),
      optional(seq('=', field('default', choice($._expression, $.initializer_list)))),
    ),

    variable_declaration: $ => prec.dynamic(DYN.VARIABLE_DECL, seq(
      repeat($._member_modifier),
      field('type', $.type),
      commaSep1($.variable_declarator),
      ';',
    )),

    variable_declarator: $ => seq(
      field('name', $.identifier),
      optional(choice(
        seq('=', field('value', choice($._expression, $.initializer_list))),
        field('arguments', $.argument_list),
      )),
    ),

    // ========================================================================
    // Part 3.3 — UE reflection macros
    // ========================================================================

    uclass_specifiers: $ => seq('UCLASS', $.macro_argument_list),
    ustruct_specifiers: $ => seq('USTRUCT', $.macro_argument_list),
    uenum_specifiers: $ => seq('UENUM', $.macro_argument_list),
    ufunction_specifiers: $ => seq('UFUNCTION', $.macro_argument_list),
    uproperty_specifiers: $ => seq('UPROPERTY', $.macro_argument_list),
    umeta_specifiers: $ => seq('UMETA', $.macro_argument_list),

    macro_argument_list: $ => seq(
      '(',
      optional(seq(commaSep1($.macro_argument), optional(','))),
      ')',
    ),

    macro_argument: $ => seq(
      field('name', choice($.identifier, $.string_literal)),
      optional(seq(
        '=',
        field('value', choice($.macro_value, $.macro_argument_list)),
      )),
    ),

    macro_value: $ => choice(
      $.string_literal,
      $.name_literal,
      seq(optional(choice('-', '+')), $.number),
      'true',
      'false',
      seq(
        optional('!'),
        $.identifier,
        repeat(seq(choice('::', '|', ':', '.'), $.identifier)),
      ),
    ),

    // ========================================================================
    // Part 3.1 — Types
    // ========================================================================

    type: $ => prec.dynamic(DYN.TYPE, seq(
      optional('const'),
      field('name', choice(
        $.primitive_type,
        $.auto_type,
        $.wildcard_type,
        $._type_identifier,
      )),
      repeat($.array_suffix),
      optional('unresolved_object'),
      optional(field('reference', $.reference_modifier)),
    )),

    primitive_type: _ => choice(
      'void', 'bool',
      'int8', 'int16', 'int', 'int32', 'int64',
      'uint8', 'uint16', 'uint', 'uint32', 'uint64',
      'float', 'float32', 'float64', 'double',
    ),

    auto_type: _ => 'auto',

    // Engine-internal wildcard type (`? Address` in generated declarations).
    wildcard_type: _ => '?',

    array_suffix: _ => seq('[', ']'),

    reference_modifier: _ => seq('&', optional(choice('in', 'out', 'inout'))),

    _type_identifier: $ => choice(
      $.identifier,
      $.template_type,
      $.qualified_identifier,
    ),

    template_type: $ => seq(
      field('name', $.identifier),
      field('arguments', $.template_arguments),
    ),

    template_arguments: $ => seq('<', commaSep1($.type), '>'),

    // Right-nested: `A::B::C` == A::(B::C).  prec.right picks that reading.
    qualified_identifier: $ => prec.right(seq(
      optional('::'),
      field('scope', choice($.identifier, $.template_type)),
      '::',
      field('name', choice($.identifier, $.template_type, $.qualified_identifier)),
    )),

    // ========================================================================
    // Part 3.4 — Statements
    // ========================================================================

    block: $ => seq('{', repeat($._statement), '}'),

    _statement: $ => choice(
      $.empty_statement,
      $.block,
      $.variable_declaration,
      $.expression_statement,
      $.if_statement,
      $.while_statement,
      $.do_while_statement,
      $.for_statement,
      $.for_each_statement,
      $.switch_statement,
      $.return_statement,
      $.break_statement,
      $.continue_statement,
      $.fallthrough_statement,
    ),

    empty_statement: _ => ';',

    expression_statement: $ => seq($._expression, ';'),

    if_statement: $ => prec.right(seq(
      'if',
      '(', field('condition', $._expression), ')',
      field('consequence', $._statement),
      optional(seq('else', field('alternative', $._statement))),
    )),

    while_statement: $ => seq(
      'while',
      '(', field('condition', $._expression), ')',
      field('body', $._statement),
    ),

    do_while_statement: $ => seq(
      'do',
      field('body', $._statement),
      'while',
      '(', field('condition', $._expression), ')',
      ';',
    ),

    for_statement: $ => seq(
      'for',
      '(',
      field('initializer', choice($.variable_declaration, $.expression_statement, ';')),
      optional(field('condition', $._expression)),
      ';',
      optional(field('update', commaSep1($._expression))),
      ')',
      field('body', $._statement),
    ),

    for_each_statement: $ => seq(
      'for',
      '(',
      field('type', $.type),
      field('name', $.identifier),
      ':',
      field('range', $._expression),
      ')',
      field('body', $._statement),
    ),

    switch_statement: $ => seq(
      'switch',
      '(', field('condition', $._expression), ')',
      field('body', $.switch_body),
    ),

    switch_body: $ => seq(
      '{',
      repeat(choice($.case_clause, $.default_clause)),
      '}',
    ),

    case_clause: $ => seq(
      'case',
      field('value', $._expression),
      ':',
      repeat($._statement),
    ),

    default_clause: $ => seq(
      'default',
      ':',
      repeat($._statement),
    ),

    return_statement: $ => seq(
      'return',
      optional(field('value', choice($._expression, $.initializer_list))),
      ';',
    ),

    break_statement: _ => seq('break', ';'),
    continue_statement: _ => seq('continue', ';'),
    fallthrough_statement: _ => seq('fallthrough', ';'),

    // ========================================================================
    // Part 4 — Expressions
    // ========================================================================

    _expression: $ => choice(
      $.identifier,
      $.qualified_identifier,
      $.number,
      $.string_literal,
      $.heredoc_string,
      $.name_literal,
      $.format_string,
      $.boolean_literal,
      $.null_literal,
      $.assignment_expression,
      $.conditional_expression,
      $.binary_expression,
      $.unary_expression,
      $.update_expression,
      $.call_expression,
      $.member_expression,
      $.subscript_expression,
      $.cast_expression,
      $.parenthesized_expression,
    ),

    parenthesized_expression: $ => seq('(', $._expression, ')'),

    assignment_expression: $ => prec.right(PREC.ASSIGN, seq(
      field('left', $._expression),
      field('operator', choice(
        '=', '+=', '-=', '*=', '/=', '%=', '**=',
        '|=', '&=', '^=', '<<=', '>>=', '>>>=',
      )),
      field('right', choice($._expression, $.initializer_list)),
    )),

    conditional_expression: $ => prec.right(PREC.TERNARY, seq(
      field('condition', $._expression),
      '?',
      field('consequence', $._expression),
      ':',
      field('alternative', $._expression),
    )),

    binary_expression: $ => {
      const table = [
        ['||', PREC.LOGIC_OR],
        ['&&', PREC.LOGIC_AND],
        ['|', PREC.BIT_OR],
        ['^', PREC.BIT_XOR],
        ['&', PREC.BIT_AND],
        ['==', PREC.EQUALITY],
        ['!=', PREC.EQUALITY],
        ['<', PREC.RELATIONAL],
        ['<=', PREC.RELATIONAL],
        ['>', PREC.RELATIONAL],
        ['>=', PREC.RELATIONAL],
        ['<<', PREC.SHIFT],
        ['>>', PREC.SHIFT],
        ['>>>', PREC.SHIFT],
        ['+', PREC.ADDITIVE],
        ['-', PREC.ADDITIVE],
        ['*', PREC.MULTIPLICATIVE],
        ['/', PREC.MULTIPLICATIVE],
        ['%', PREC.MULTIPLICATIVE],
      ];

      return choice(
        ...table.map(([operator, precedence]) => prec.left(
          /** @type {number} */(precedence),
          seq(
            field('left', $._expression),
            field('operator', /** @type {string} */(operator)),
            field('right', $._expression),
          ),
        )),
        // '**' is right-associative
        prec.right(PREC.POWER, seq(
          field('left', $._expression),
          field('operator', '**'),
          field('right', $._expression),
        )),
      );
    },

    unary_expression: $ => prec.right(PREC.UNARY, seq(
      field('operator', choice('-', '+', '!', '~')),
      field('argument', $._expression),
    )),

    update_expression: $ => choice(
      prec.right(PREC.UNARY, seq(
        field('operator', choice('++', '--')),
        field('argument', $._expression),
      )),
      prec.left(PREC.POSTFIX, seq(
        field('argument', $._expression),
        field('operator', choice('++', '--')),
      )),
    ),

    member_expression: $ => prec.left(PREC.POSTFIX, seq(
      field('object', $._expression),
      '.',
      field('property', $.identifier),
    )),

    // Index brackets reuse ARGLIST (ParseExprPostOp -> ParseArgList(false)),
    // so named arguments are legal here: `Arr[Index: 3]`, `Grid[X, Y]`.
    subscript_expression: $ => prec.left(PREC.POSTFIX, seq(
      field('object', $._expression),
      '[',
      commaSep1(field('index', $.argument)),
      ']',
    )),

    // Covers plain calls, method calls, and construct calls (`FVector(1,2,3)`,
    // `TArray<int>(...)`).  Whether the callee is a type is a semantic
    // decision left to the LSP (angelscript.bnf §4.4).
    call_expression: $ => prec.left(PREC.POSTFIX, seq(
      field('function', $._callee),
      field('arguments', $.argument_list),
    )),

    _callee: $ => choice($._expression, $.template_type, $.primitive_type),

    argument_list: $ => seq(
      '(',
      optional(seq(commaSep1($.argument), optional(','))),
      ')',
    ),

    argument: $ => choice(
      $.named_argument,
      $.void_argument,
      $._expression,
      $.initializer_list,
    ),

    // `Print("x", Duration=30)` / `Math::Clamp(X: 2.0)`
    // (both forms are enabled: asEP_ALTER_SYNTAX_NAMED_ARGS = 1)
    named_argument: $ => prec.dynamic(DYN.NAMED_ARGUMENT, seq(
      field('name', $.identifier),
      choice(':', '='),
      field('value', choice($._expression, $.initializer_list, $.void_argument)),
    )),

    // `Function(void)` skips an out-parameter.  Higher precedence than
    // `primitive_type` so that a bare `void` in argument position is never
    // read as a type.
    void_argument: _ => prec(1, 'void'),

    cast_expression: $ => seq(
      'Cast',
      '<', field('type', $.type), '>',
      '(', field('value', $._expression), ')',
    ),

    // Part 3.6 — init lists; empty slots are allowed: `{1, , 3}`
    initializer_list: $ => seq(
      '{',
      optional(seq(
        optional($._initializer_item),
        repeat(seq(',', optional($._initializer_item))),
      )),
      '}',
    ),

    _initializer_item: $ => choice($._expression, $.initializer_list),

    // REMOVED (see angelscript.bnf §4.6 [paper]): the engine parser accepts
    // `function(...) {...}`, but the compiler's only exit for a lambda is an
    // implicit conversion to a funcdef — and funcdef is a dead token in the
    // UE fork, so a lambda ALWAYS fails to compile.  Not implemented to keep
    // the grammar aligned with what actually runs.
    // lambda_expression: $ => seq(
    //   'function',
    //   '(',
    //   commaSep($.lambda_parameter),
    //   ')',
    //   field('body', $.block),
    // ),
    //
    // lambda_parameter: $ => seq(
    //   optional(field('type', $.type)),
    //   field('name', $.identifier),
    // ),

    // ========================================================================
    // Part 1 — Lexical
    // ========================================================================

    identifier: _ => /[A-Za-z_][A-Za-z0-9_]*/,

    boolean_literal: _ => choice('true', 'false'),
    null_literal: _ => 'nullptr',

    number: _ => {
      const decimal = /[0-9]+/;
      const hex = /0[xX][0-9a-fA-F]+/;
      const binary = /0[bB][01]+/;
      const octal = /0[oO][0-7]+/;
      const explicitDecimal = /0[dD][0-9]+/;
      const float1 = /[0-9]+\.[0-9]*([eE][+-]?[0-9]+)?[fF]?/;
      const float2 = /\.[0-9]+([eE][+-]?[0-9]+)?[fF]?/;
      const float3 = /[0-9]+[eE][+-]?[0-9]+[fF]?/;
      const float4 = /[0-9]+[fF]/;

      return token(choice(
        hex, binary, octal, explicitDecimal,
        float1, float2, float3, float4,
        decimal,
      ));
    },

    string_literal: _ => token(seq(
      '"',
      repeat(choice(/[^"\\\r\n]/, /\\(.|\r?\n)/)),
      '"',
    )),

    // `"""..."""` — multi-line, no escape processing
    heredoc_string: _ => token(seq(
      '"""',
      repeat(choice(/[^"]/, /"[^"]/, /""[^"]/)),
      '"""',
    )),

    // FName literal: n"Name"
    name_literal: _ => token(seq(
      'n"',
      repeat(choice(/[^"\\\r\n]/, /\\./)),
      '"',
    )),

    // Format string: f"text {expr =:spec} text"
    format_string: $ => seq(
      'f"',
      repeat(choice(
        $.format_string_content,
        $.format_interpolation,
      )),
      '"',
    ),

    format_interpolation: $ => seq(
      '{',
      field('expression', $._expression),
      optional('='),
      optional(seq(':', field('format', $.format_spec))),
      '}',
    ),

    comment: _ => token(choice(
      seq('//', /[^\r\n]*/),
      seq('/*', /[^*]*\*+([^/*][^*]*\*+)*/, '/'),
    )),

    // Preprocessor lines are kept opaque (angelscript.bnf §1.7 / §5.7).
    preproc_line: _ => token(seq('#', /[^\r\n]*/)),
  },
});
