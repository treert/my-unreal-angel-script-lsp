# grammar

本目录是 **Unreal Angelscript 的 Tree-sitter 文法工程**（架构设计中的「模块二」）。
产出的 parser 由 `../lsp`（Rust）以同仓路径依赖的方式内嵌使用，不作为独立进程。

| 文件 | 说明 |
|------|------|
| [`angelscript.bnf`](angelscript.bnf) | **形式化语法规范**（Layer A = 用户实际书写的 `.as` 源码）。改文法前先改 BNF。 |
| `grammar.js` | Tree-sitter DSL 文法，按 BNF 实现（优先级表 §5.5、节点命名 §5.6、冲突清单 §5.4）。 |
| `src/scanner.c` | 外部扫描器：格式化字符串 `f"..."` 的文本块与格式说明符（**手写，入库**）。 |
| `test/corpus/` | 回归测试（声明 / 类型 / 语句 / 表达式 / 字面量 / 错误恢复），40 条，全部通过。 |
| `.gitignore` | 生成物不入库：`src/parser.c`、`src/grammar.json`、`src/node-types.json`、`src/tree_sitter/`。 |

> **生成物不进版本库**：克隆仓库后必须先 `npm install && npx tree-sitter generate`，
> 否则 `../lsp` 的 `build.rs` 会因找不到 `src/parser.c` 直接报错。
> 这样避免近百万行生成代码污染 diff，与 `ai-mylua-lsp/grammar` 的约定一致。

## 命令

```bash
cd grammar
npm install                 # 安装 tree-sitter-cli
npx tree-sitter generate    # grammar.js -> src/parser.c
npx tree-sitter test        # 跑 test/corpus 回归
npx tree-sitter parse <file.as>              # 查看单文件 CST
npx tree-sitter parse -q -s '<glob>'         # 批量语料验证（统计错误率）
```

## 验证状态

| 语料 | 文件数 | 结果 |
|---|---|---|
| `Demo_AS/Script/**/*.as`（含 unreal-angelscript 官方 demo） | 27 | 100% 无 ERROR |
| `Demo_AS/Saved/AS-Cache/*.d.as`（UE 导出的类型声明） | 414 | 100% 无 ERROR |
| `UnrealEngine/Script-Examples/**/*.as` | 26 | 100% 无 ERROR |
| `test/corpus` | 40 条 | 全部通过 |

corpus 覆盖率：**具名节点 94/94、终结符 129/129，均 100%**。
（2026-09 移除 lambda / import 两条用例与三个节点后复算。）
新增文法节点/终结符时应同步补用例以保持该数字，复算脚本（PowerShell）：

```powershell
# 具名节点覆盖（排除 supertype "_*"）
$named = (Get-Content src\node-types.json -Raw | ConvertFrom-Json) |
    Where-Object { $_.named -and $_.type -notlike '_*' } |
    ForEach-Object { $_.type } | Sort-Object -Unique
$used = [regex]::Matches((Get-ChildItem test\corpus\*.txt | Get-Content -Raw),
    '\(([a-z_][a-z0-9_]*)\b') | ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique
$named | Where-Object { $used -notcontains $_ }   # 输出为空即 100%
```

终结符覆盖同理：从 `src/grammar.json` 抽取所有 `"type":"STRING"` 的 `value`，
再在 corpus 的**源码段**（`===` 标题与 `---` 分隔符之间那一段）里查找。

## `.as` 与 `.d.as` 的关系：交叉，不是子集

两者使用**同一 parser**，但**语法集合是交叉关系**——各有对方非法的独占构造。
（早期文档写作「`.d.as` 是 `.as` 的子集」，**该表述错误**，已于 2026-09 修正。）

| 只在 `.d.as` 合法（脚本侧写了必报错） | 引擎依据 |
|---|---|
| `type_parameters`：`struct TArray<T>` / `struct TMap<K,V>` | 模板声明头走独立入口 `ParseTemplateDecl`（`as_parser.cpp:176`），只服务 C++ 注册串；脚本走的 `ParseClass`（`:3774-3810`）标识符后只接受 `;`/`:`/`{`，不认 `<` |
| `?` 通配类型 | 只能在 C++ 注册串里出现（`docs/架构设计-引擎内部语法.md` §1.1） |
| `unresolved_object` 类型后缀 | 由绑定层 `FObjectPtrType` 生成，脚本作者不书写（同上 §3） |
| `@templateSpecialization` + `class TArray<FVector>` | 模板实参是具体类型，脚本侧无此构造 |

| 只在 `.as` 合法（`.d.as` 不出现） |
|---|
| 函数体 / 语句 / 表达式 / `default` 块（`.d.as` 一律 `;` 收尾） |
| `delegate` / `event` / `asset` / `mixin` / `local` 声明 |
| 局部 `auto`、range-for、f-string、预处理行 |

**为什么措辞重要**：说成「子集」会诱发两个错误推论——① 反向推理「`.as` 能写的 `.d.as` 也能写」，
从而漏掉「`.as` 里的模板定义头 / `?` / `unresolved_object` 必须报语义诊断」；
② 以为 414 个 `.d.as` 零 ERROR 的语料验收能覆盖 `.as` 侧形态（或反之）。
两侧各有独占构造，**验收口径必须分开算**。

### corpus 格式陷阱（踩过）

每个用例**必须**是完整三段：`===` 标题 `===` / 源码 / `---` 分隔符 / 期望树。
若新增用例漏写 `---` 与期望树，tree-sitter 会把它当成**后一个用例源码的一部分**，
而 `tree-sitter test -u` 重写文件时会**静默丢弃**这些内容。新增用例后务必
复核用例数：

```powershell
Get-ChildItem test\corpus\*.txt | ForEach-Object {
    "{0,-22} {1}" -f $_.Name,
        (([regex]::Matches((Get-Content $_.FullName -Raw), '(?m)^={10,}\r?\n')).Count / 2)
}
```

另外：corpus 的源码段是**逐字解析**的，没有元注释语法 —— 想写说明只能用
`//`（会作为 `comment` 节点进入期望树），用 `#` 会被解析成 `preproc_line`。

## 文法设计要点

### 词法

- 纯 token 处理：标识符、数字（十进制 / `0x` / `0b` / `0o` / `0d` / 各种浮点及 `f` 后缀）、
  普通字符串、heredoc `"""..."""`、FName 字面量 `n"..."`、注释、预处理行。
- **预处理行**（`#if EDITOR` / `#endif` / …）作为 `extras` 中的 `preproc_line` 保持不透明，
  不做条件求值（BNF §1.7 / §5.7）。需要区域感知时再升级为结构化规则。
- **外部扫描器只负责格式化字符串**：
  `format_string_content` 逐字保留文本（含空格、`//`、`{{`/`}}` 转义），
  `format_spec` 捕获 `{Expr :#032b}` 中的说明符。
  插值表达式本身是正常的 `_expression` 子树 —— references / 跳转必须覆盖插值段，
  所以这里不能把整个 f-string 吞成一个 token。
- 嵌套模板 `TArray<TArray<int>>` 无需拆 token：模板实参位置只接受类型，
  该词法状态里 `>>` 不是合法 token，Tree-sitter 会自然切成两个 `>`。

### 冲突（对应 BNF §5.4）

`grammar.js` 只声明了 4 组 GLR 冲突，其余歧义由这几组 + `DYN` 动态优先级导出：

| 冲突 | 场景 |
|---|---|
| `type` / `_expression` | `Foo Bar;` 声明 vs `Foo(Bar);` 表达式语句 |
| `parameter_list` / `argument_list` | `Type Name(X)` 形参表 vs 构造实参 |
| `template_type` / `_expression` | `A<B, C>(x)` 模板构造 vs 关系表达式链 |
| `named_argument` / `_expression`(`/type`) | `f(Name = value)`、`f(Name: value)` |

命名实参**同时支持 `:` 与 `=`** 两种写法：引擎侧
`AngelscriptManager.cpp` 设置了 `asEP_ALTER_SYNTAX_NAMED_ARGS = 1`，
官方 demo 里 `Print("...", Duration=30)` 就是 `=` 形式。

### 错误恢复

不实现 BNF §3.7 的 `[lsp]` 容错产生式（不做具名规则），全部交给 Tree-sitter 内建恢复；
关键中途编辑形态（未写变量名、尾随 `::`、未闭合块、`Actor.` 后换行）在
`test/corpus/error_recovery.txt` 中固定快照。

## 与 BNF 的偏差

实现时相对 `angelscript.bnf` 的有意偏差，均为降低歧义/简化下游消费：

1. **类成员与全局声明共用节点名**：不区分 `method_declaration` / `property_declaration`
   （BNF §5.6 建议的命名），统一为 `function_declaration` / `variable_declaration`。
   是类成员还是全局声明由父节点（`class_body` / `namespace_body` / `source_file`）判断，
   查询侧只需记一套名字。
2. **`private` / `protected` / `access:Level` / `UPROPERTY()` / `UFUNCTION()` 是声明前缀**，
   不是独立成员节点，统一挂在 `function_declaration` / `variable_declaration` /
   `constructor_declaration` 等的开头（`specifiers` 字段 + 匿名关键字），顺序任意。
3. **`(void)` 形参不可达**（2026-09 M5 实测，修正原文档的失实描述——原文称
   「解析为单个类型为 `void` 的 `parameter`，消费侧把『仅一个 void 形参』视作
   空参表」）：
   `void_argument` 带 `prec(1)`，**裸** `void` 在括号语境恒解析为 `void_argument`
   （调用位跳过 out 参数的引擎构造），压过 `primitive_type` 的 `void` ⇒
   无名的 `(void)` 不可能走 `parameter_list` 路径。实测两种落点：
   `void F(void) {}`（有函数体）→ **ERROR 节点**（错误恢复吞掉，M6 起由
   `parse-error` 诊断兜底）；`void D(void);`（无体声明）→ 被变量声明的
   `variable_declarator name(arguments)` 构造吞掉（读作「类型 `void` 的
   变量 `D`，构造实参 `(void)`」，不报错但语义非预期）。
   带名的 `void X` 形参**可达**（parameter 是唯一完整解析，prec 不参与），
   但语料零出现。**消费侧无需任何「void 形参」特殊处理**——该节点形态
   不会出现；`void_argument` 本身是活的（调用位语义，见 BNF）。
4. **模板声明头 `struct TMap<K, V>`**（BNF 未收录，`.d.as` 实际存在）：
   新增 `type_parameters` / `type_parameter` 节点。该形态是 `.d.as` 独有的——
   引擎脚本侧根本不认（见上方「交叉，不是子集」表）。
   **已实测通过**（2026-09）：`struct TArray<T>`、`struct TMap<K, V>`、`class TSubclassOf<T>`，
   以及导出器的特化块 `// @templateSpecialization` + `class TArray<FVector>`
   （`TypeDeclarationExporter.cpp:617-618`，模板**实参是具体类型**）——
   四种形态**全部零 ERROR，文法无需改动**。
   注意：`type_parameter` 节点**同时承载**类型形参（`T`）与具体实参（`FVector`），
   语法层不区分；语义层按「声明前是否有 `@templateSpecialization` tag」消歧
   （`docs/架构设计-引擎内部语法.md` §4.3）。
   模板**实例**（使用位 `TArray<FVector> Arr;`）走的是 `template_type` 规则，不是本节点。
5. **`enum` 尾随 `;` 可选**（BNF 写作必需，`.d.as` 导出不带 `;`）；
   `class` / `struct` / `namespace` / `asset` 的尾随 `;` 同样可选，且被贪婪吸收
   （`prec.right`），不会被误判为 `empty_declaration`。
6. **`default` 语句接受任意表达式**（`default Tags.Add(n"Tag");`），不限于赋值。
7. **表达式层级**按 BNF Part 4 的显式优先级实现（引擎是扁平 EXPR + 编译期定序）；
   `**` 右结合，其余二元运算左结合。
8. **未实现 `[dead]` 顶层声明**（`typedef` / `interface` / `funcdef`，token 已从引擎移除）与
   引擎内部类型修饰（`+`、`if_handle_then_const`、`handle_only`、`__auto_constref_type`、
   `__any_implicit_integer`，后者只出现在 C++ 注册串里）。
   **`import` 与 lambda 也不实现**（BNF 标 `[paper]`）：两者的 token 都是活的、引擎 parser
   都接受，但下游**永远不可用**——lambda 恒报编译错误（唯一出口是转换到 funcdef，而
   funcdef 已死，`as_compiler.cpp:6291`）；import 的宿主绑定在 AngelscriptCode 全模块为 0
   （运行期才报 "Unbound function called"）。文法层直接拒绝比「放行再报语义错」更贴近
   实际运行的东西。同因不实现 mixin class：引擎 `ParseMixin` 只接受 mixin **函数**
   （`as_parser.cpp:3700`），mixin class 在引擎侧就是 parse error。
   `?`（通配类型，`.d.as` 的 `void opCast(? Address)`）和 `unresolved_object` 已实现。
9. **f-string 嵌套格式说明符**（`f"{a:{fmt}}"`）暂不支持，会落入错误恢复。

## 「语法接受 ≠ 语义合法」的已知点

文法刻意比引擎**宽松**，把这类错误留给 LSP 报语义诊断（而不是解析错误），
以免中途编辑时整棵树崩掉。目前已确认并有 corpus 快照的：

| 形态 | 引擎行为 |
|---|---|
| 无实现的虚属性访问器 `get;` / `set;` | `asCParser` 接受（`ParseVirtualPropertyDecl` 非 interface 分支允许 `;`），但 `asCBuilder::RegisterVirtualProperty` 报 **"Property accessor must be implemented"**。该形态本是给 `interface` 用的，而 `interface` 在 UE fork 是死 token，故**实际必错**。 |
| 非块内的变量声明，如 `if (x) int y = 5;` | `STATBLOCK ::= '{' {VAR \| STATEMENT} '}'` —— 声明只能直接出现在块内（或 for 初始化）。`ParseStatement` 显式报 `TXT_UNEXPECTED_VAR_DECL`。case 子句内同理。 |
| `fallthrough;` 出现在 case 子句末尾以外 | `ParseStatement` 没有 `ttFallthrough` 分支，只有 `ParseCase` 在末尾位置识别它（与 `break` 同位）。 |
| `struct` 内的 `default` 语句 | `ParseClass` 用 `!isStruct` 守卫，`default` 仅限 `class`。 |
| 全局变量 / 类成员上的 `&` | `ParseDeclaration` 用 `!isClassProp && !isGlobalVar` 守卫引用后缀，只有**局部**变量能是引用。 |
| 直接把赋值当实参 `f(a = b)` | 实参是 `ParseCondition()` 而非 ASSIGN；且 `a = b` 会被优先当成命名实参。 |
| 裸 enum 值 `Value`（不带 `MyEnum::`） | `asEP_REQUIRE_ENUM_SCOPE = 1`，符号查找跳过裸值兜底分支（`as_compiler.cpp:11815`），落入通用「未找到」错误（诊断归 `AS04xx`，P3/P4 占号）。 |
| **`.as` 里的模板声明头** `class TFoo<T> {...}` | 本文法接受（`type_parameters` 为 `.d.as` 而设，同一 parser 无法按后缀关闭）。引擎侧 `ParseClass` 在标识符后不认 `<` → **parse error `Expected '{'`**；UE 预处理器的类名正则也不含 `<>`（`AngelscriptPreprocessor.cpp:723`）。若要报错须由语义层做，**是否实现按需决定**（素材见 `docs/诊断码表.md` §2 末段）。 |
| **`.as` 里的 `?` / `unresolved_object`** | 同理：为 `.d.as` 而设的节点在 `.as` 中一律非法（引擎内部语法 §1.4 / §3.2）。 |

> lambda 与 import 原先也在此表（语法放行 + 对应语义诊断），
> 2026-09 决策改为**文法层直接不实现**（见偏差 §8），两个诊断码随之 retired。

虚属性访问器的属性集与普通方法相同（`ParseVirtualPropertyDecl` 直接调
`ParseMethodAttributes`），`function_attribute` 已覆盖其全部 12 个 token：
`final` `override` `property` `mixin` `accept_temporary_this`
`external_implicit_this` `no_discard` `allow_discard` `__generated`
`deprecated` `defaults` `unsafe_during_construction`。

## 与 `../lsp` 的边界

- **grammar**：只回答「长什么样、树节点是什么」。
- **lsp**：把树 + 工程信息变成定义、引用、类型、诊断；不在此目录写语义。
