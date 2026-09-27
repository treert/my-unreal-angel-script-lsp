# 原版 AngelScript 与 Unreal fork 的差异

> 版本：v0.4
> 下游专题：
> - [`struct类型专题.md`](struct类型专题.md)（`class`/`struct` 全量对照、UE 映射、继承与展平）
> - [`诊断码表.md`](诊断码表.md)（`AS0xxx` 权威码表，唯一码号分配处）
>
> 前置文档：[`../grammar/angelscript.bnf`](../grammar/angelscript.bnf)（Layer A 完整语法规范）、[`架构设计.md`](架构设计.md)
> 证据来源（绝对路径，均为只读参考）：
> - `[ENGINE]` = `d:/WorkGit/UnrealEngine/Engine/Plugins/Angelscript/ThirdParty/source/`
> - `[UE]` = `d:/WorkGit/UnrealEngine/Engine/Plugins/Angelscript/Source/AngelscriptCode/`

## 0. 为什么需要这份文档

Unreal Angelscript（Hazelight 的 UE 插件）**不是**原版 AngelScript 加了几个绑定，而是**对解释器本体做过外科手术**的方言：
删掉了整套显式句柄与 GC 体系，新增了值类型声明语法，并让脚本对象与 `UObject` 共享同一块内存。

因此：

- **不要参考 angelcode.com 官方文档**推导语义，很多机制在这里已被移除或反转；
- LSP 的类型系统、诊断、`Cast<>` 可用性判定、赋值语义（拷贝 vs 重绑定）都直接依赖本文的结论；
- BNF 只描述「长什么样」，本文描述「为什么长这样」。

## 1. 词法层：token 表的增删

`[ENGINE]as_tokendef.h` 的 `tokenWords[]` 里，原版 token 被**注释掉**而非删除，改动一目了然。

### 1.1 被移除的原版 token（第 251-307 行，全部 `//` 注释）

| token | 原版用途 | 移除后果 |
|---|---|---|
| `@` (`ttHandle`) | 显式对象句柄 `Foo@ h` | 句柄语法彻底消失，class 改为**隐式句柄** |
| `null` | 空句柄字面量 | 替换为 `nullptr`（`asTokenDef("nullptr", ttNull)`，第 289 行复用了 `ttNull`） |
| `is` / `!is` | 句柄同一性比较 | 用 `==` / `!=` |
| `and` / `or` / `not` / `xor` | 关键字形式的逻辑运算 | 只保留 `&&` / `\|\|` / `!`；**`^^`（逻辑异或）无替代，直接没了** |
| `interface` | 接口声明 | 死 token，UE 用反射 + delegate/event 替代 |
| `funcdef` | 函数指针类型声明 | 死 token，UE 用 `delegate` / `event`（预处理器展开） |
| `typedef` | 基础类型别名 | 死 token |

> **对 LSP 的影响**：`interface` / `funcdef` / `typedef` 的解析器分支在 `as_parser.cpp` 里仍然存在但**不可达**
> （token 进不来）。BNF §2.9 把它们标为 `[dead]`，`grammar.js` 未实现——这是正确的。
> 但 `import` **不是**死 token（`as_tokendef.h:275` 仍然激活），只是 UE 脚本从不用。

### 1.2 fork 新增的 token

集中出现在枚举尾部（`as_tokendef.h:180-186`），这个排列本身就是「后来追加」的指纹：

```c
	ttStruct,              // struct
	ttLocal,               // local
	ttFallthrough,         // fallthrough

	ttAccess,
	ttUnresolvedObject,
```

| token | 作用 | 定义位置 |
|---|---|---|
| `struct` | **声明脚本值类型**（见 §2） | `as_tokendef.h:180, 295` |
| `local` | 模块私有函数（不进全局符号表） | `as_tokendef.h:181, 285` |
| `fallthrough` | `switch` 中显式贯穿（仅 case 子句末尾） | `as_tokendef.h:182, 308` |
| `access` | 细粒度访问控制声明 / 默认级别切换 | `as_tokendef.h:184, 254` |
| `unresolved_object` | 引擎内部的未解析类型标记 | `as_tokendef.h:185, 304` |
| `Cast`（**大写**） | 类型转换。原版是小写 `cast` | `as_tokendef.h:259` |
| `float32` / `float64` | 显式宽度浮点 | `as_tokendef.h:270-271` |
| `int32` / `uint32` | `int` / `uint` 的**词法级别名**（映射到同一个 `ttInt`/`ttUInt`） | `as_tokendef.h:281, 302` |

另有预处理器层（Layer A）独有的伪语法：`UCLASS/USTRUCT/UENUM/UFUNCTION/UPROPERTY/UMETA`、
`delegate` / `event`、`asset X of Y`、`f"..."` / `n"..."` 字面量、`#if` 条件块——
这些是**上下文关键字**，不进 token 表，由 `[UE]Private/Preprocessor/AngelscriptPreprocessor.cpp` 改写。

### 1.3 默认浮点宽度反转

原版 `float` = 32 位、`double` = 64 位。fork 里 `ep.floatIsFloat64` 生效：
**无后缀浮点字面量是 `float64`，`1.5f` 才是 `float32`**，`float` 本身是 `float64` 的别名、`double` 被标记 deprecated。
（BNF §1.3 / §1.4 已记录。）

## 2. `struct`：fork 新增的脚本值类型（摘要）

> **完整内容已独立成篇：[`struct类型专题.md`](struct类型专题.md)**
> 该文覆盖：起源、`class`/`struct` 全量对照、描述符 vs 实例的三层模型、映射严格性边界、
> 继承四道锁与 C++ USTRUCT 展平机制、`FASStructOps` 方法映射、LSP 实现要求与导出插件待办。
> 本节只保留「作为方言差异」的最小结论。

### 2.1 原版没有这个东西

原版 AngelScript 的脚本**只能声明 `class`**（外加 `interface`/`funcdef`/`typedef`，后三者在 fork 里是死 token）。
值类型（`asOBJ_VALUE`）机制在原版是存在的，但**只能从 C++ 侧注册**：

```cpp
engine->RegisterObjectType("Foo", sizeof(Foo), asOBJ_VALUE | asOBJ_POD);
```

脚本没有任何语法去声明一个值类型。fork 新增 `ttStruct`，复用已有的 `asOBJ_VALUE` 基础设施，
让脚本能直接声明值类型。

### 2.2 单一分叉点

`class` 和 `struct` **共用同一个 `ParseClass()`**，只靠一个 `bool isStruct` 区分。
类型标志的翻转发生在 `[ENGINE]as_builder.cpp:2265`——**这一行是所有后续差异的源头**：

```cpp
st->flags = asOBJ_REF | asOBJ_SCRIPT_OBJECT | asOBJ_NOCOUNT;   // 默认
if (isStruct) {
    st->flags &= ~(asOBJ_REF | asOBJ_NOCOUNT);
    st->flags |= asOBJ_VALUE;
    st->flags |= asOBJ_NOINHERIT;        // 自动 final
} else {
    st->flags |= asOBJ_IMPLICIT_HANDLE;  // 隐式句柄
}
```

由此导出：值拷贝 vs 句柄重绑定、无虚表 vs 有虚表、无 factory vs 有 factory、禁止继承 vs 支持继承、
禁止 `Cast`/句柄 vs 支持、无 CDO/Tick/复制/soft-reload vs 全有。
**逐项证据见 [专题 §4](struct类型专题.md#4-语义层完整对照)。**

### 2.3 语法层只差一条

`struct` 内不允许 `default` 语句（`as_parser.cpp:3828` 用 `!isStruct` 守卫）→ `AS0001`。
其余成员形态（方法、构造/析构、运算符重载、虚属性、`access` 声明）**完全一致**；
`: Base` 继承列表 parser 也接受，由上层拒绝 → `AS0201`。

> **文法决策**：`struct F : G {}` 在 tree-sitter 层应当**正常解析成功**，
> 继承违规由 LSP 报语义诊断（见 [§5.4](#54-as02xx--ue-反射约束)）。`grammar.js` 现状符合此约定。

### 2.4 一句话结论

**`class` ≈ C++ 的 `UObject*`，`struct` ≈ C++ 的 `USTRUCT` 值类型。**
和 C++ 里 `class`/`struct` 只差默认访问权限完全不是一回事。

两处**常见误解**（详见 [专题 §5](struct类型专题.md#5-ue-映射必须区分描述符与实例)、[§6](struct类型专题.md#6-继承as-禁止但-ue-支持展平机制)）：

- 「struct 对应 `UStruct`」是错的——`UStruct` 是反射元类型基类，在 AS 里本身是 `class`；
  正确映射是 `UASStruct : UScriptStruct`，且要区分**描述符**与**实例**；
- 「AS 的 struct 就是 UE 的 USTRUCT」也是错的——`.d.as` 里 `FString`/`TArray<T>`/`TSubclassOf<T>`
  等纯 C++ 值类型同样用 `struct` 暴露。真正的不变式是「`struct` = `asOBJ_VALUE` 值类型」。

## 3. GC：AS 自带垃圾回收被摘除

### 3.1 最直接的证据——注释还是原版的，代码已被替换

```c
// [ENGINE]as_builder.cpp:2239-2246
// By default all script classes are marked as garbage collected.
// Only after the complete structure and relationship between classes
// is known, can the flag be cleared for those objects that truly cannot
// form circular references. ...
st->flags = asOBJ_REF | asOBJ_SCRIPT_OBJECT | asOBJ_NOCOUNT;
```

原版这一行是 `... | asOBJ_GC`，后面还有一整套「分析完类关系后判断能否去掉 GC 标记」的逻辑。
fork 直接换成 `asOBJ_NOCOUNT`（AS 不做引用计数、不参与 GC），**注释忘了删**。

旁证：

- `[ENGINE]as_scriptobject.cpp:300` — `scriptTypeBehaviours.flags = asOBJ_SCRIPT_OBJECT | asOBJ_REF | asOBJ_NOCOUNT`
- `as_scriptobject.cpp` 中 `asOBJ_GC` 出现 **0 次**
- `as_gc.cpp` / `as_gc.h` 文件仍在（原版遗留），但整个 `[UE]AngelscriptCode` 模块**没有任何地方调用 AS 的 `GarbageCollect()`**；
  所有 GC 引用都是 UE 的 `UObject/GarbageCollection.h` / `GarbageCollectionSchema.h`

### 3.2 根本原因：脚本 class 实例**就是**那个 UObject

```cpp
// [ENGINE]as_builder.cpp:2283-2289
if (!isStruct)
    st->basePropertyOffset = (int)classData->PropertyOffset;
st->shadowType = classData->ShadowType;
```

`PropertyOffset` 来自 `[UE]Private/AngelscriptManager.cpp:2974-2978` 的
`ClassDesc->CodeSuperClass->GetPropertiesSize()`。也就是说：

**脚本类的成员变量被排布在 C++ 父类属性之后的同一块内存里。AS 的 `this` 和 `UObject*` 是同一个地址。**

于是：

- AS 手里没有独立的堆对象 → 无从 GC，只能 `asOBJ_NOCOUNT`；
- 生命周期 100% 归 UE：`NewObject` / `SpawnActor` 分配，UE GC 通过生成的 `FProperty` 追踪；
- 因此脚本里**禁止用构造语法创建 class**（`asOBJ_DISALLOW_INSTANTIATION`，由
  `[UE]Private/Binds/Bind_BlueprintType.cpp:655-656` 设置），报错见 §5。

这个设计在解释器里留下两处不得不改的地方：

```c
// [ENGINE]as_scriptobject.cpp:357  —— 原版会把整个对象清零，这里必须注释掉
//memset((void*)((size_t)this+(size_t)objType->basePropertyOffset), 0, objType->size - objType->basePropertyOffset);
```
否则会把 C++ 父类的 `UObject` 头与属性一起抹掉。

```c
// [ENGINE]as_scriptobject.cpp:780  —— 遍历属性时跳过 C++ 父类区间
if (prop->byteOffset < objType->basePropertyOffset)
```

### 3.3 struct 的 GC 怎么处理

struct 是值类型、内嵌在宿主内存中，**自身不是 UObject**，UE GC 无法直接扫到它内部持有的 UObject 引用。
对策是强制生成隐藏 `FProperty`：

```cpp
// [UE]Private/ClassGenerator/AngelscriptClassGenerator.cpp:283-287
if (ClassData.NewClass->bIsStruct)
    bShouldMakeProperty = !PropertyType.NeverRequiresGC();
if (PropertyType.RequiresProperty())
    bShouldMakeProperty = true;
```

struct 里只要成员**可能**含 UObject 引用，即使没写 `UPROPERTY()` 也会被强制生成属性；
生成不了就直接编译报错（`AS0301`，见 [§5.5](#55-as03xx--gc--属性生成)）。这与 UE C++ 中
「`USTRUCT` 内必须 `UPROPERTY()` 才被 GC 追踪」是同一模型。

## 4. 其它已确认的机制级差异

| 机制 | 原版 | UE fork | 证据 |
|---|---|---|---|
| 命名实参 | 仅 `:` | **`:` 与 `=` 都支持**（`asEP_ALTER_SYNTAX_NAMED_ARGS = 1`） | `[UE]Private/AngelscriptManager.cpp`；BNF §4.4 |
| 表达式优先级 | 解析器扁平 `EXPR ::= EXPRTERM {EXPROP EXPRTERM}`，编译期定序 | 同（未改） | BNF Part 4 开头；`grammar.js` 改为显式层级 |
| 逻辑异或 `^^` | 有 | **移除** | token 表无 `ttXor` |
| `switch` 贯穿 | 隐式（C 风格） | 新增显式 `fallthrough`，仅限 case 子句末尾 | `as_parser.cpp` `ParseCase` |
| 模块私有函数 | 无 | 新增 `local` | `as_tokendef.h:285` |
| 访问控制 | `private` / `protected` | 另加 `access Name = private(readonly), X, *;` 与 `access : Level` | `as_parser.cpp` `ParseAccessDecl`；BNF §2.3.2 |
| POD 推导 | — | struct 无自定义赋值运算符时自动加 `asOBJ_POD` | `as_builder.cpp:885-978` |
| 字节码缓存 / 静态 JIT | 无 | `PrecompiledData` + C++ 离线转译 | `[UE]Private/StaticJIT/` |

## 5. 诊断：引擎错误文本如何落成诊断码

> **完整码表已独立成篇：[`诊断码表.md`](诊断码表.md)**
> 该文是**唯一的码号分配处**，含编码规则、6 个区段的全部码号、range/quick-fix 约定、
> 实施约束与占用总览。新增诊断必须先在那里占号。

本文各章节用 `→ AS0xxx` 标记引用具体约束，不重复列出触发条件。段位速查：

| 段 | 含义 | 本文对应章节 |
|---|---|---|
| `AS00xx` | 语法 / 解析（parser 拒绝的形态） | §2.3（struct `default`） |
| `AS01xx` | 类型系统（值/引用二分、句柄、转换、布局） | §2.2、[专题 §4](struct类型专题.md#4-语义层完整对照) |
| `AS02xx` | UE 反射 specifier 约束 | [专题 §6](struct类型专题.md#6-继承as-禁止但-ue-支持展平机制) |
| `AS03xx` | GC / 属性生成 | §3.3 |
| `AS04xx` | 符号解析（预留，P3/P4 填充） | — |
| `AS09xx` | 本 LSP 独有（`.d.as` 过期、文法缺口等） | — |

**与本文的分工**：本文解释「引擎为什么这样限制」（方言演化的因果），
码表解释「LSP 报什么、报在哪、怎么修」（实现契约）。
引擎错误原文统一由码表的 `Message` 列承载——**不要在本文再抄一份**，
否则改措辞时必然两处不一致。

## 6. 对本项目各模块的影响

### 6.1 grammar（模块二）

- 已正确不实现 `[dead]` 顶层声明与 `@` / `null` / `is` / `and`/`or`/`xor`；
- `struct` 与 `class` 作为独立节点，`default_statement` 只出现在 `class_declaration` 内；
- struct 的继承列表、struct 内 `UFUNCTION()` **照常解析**，交给 LSP 报诊断；
- 浮点字面量的 `f` 后缀需要保留在 CST 中（`float32` vs `float64` 影响类型推导）。

### 6.2 lsp（模块三）类型系统

必须在类型模型里区分**值类型 / 引用类型**。值/引用二分衍生出的完整能力清单见
[专题 §9.1](struct类型专题.md#91-类型系统模块三)，本文只列非 struct 相关的部分：

| LSP 能力 | 依赖的本文结论 | 相关诊断码 |
|---|---|---|
| 浮点字面量宽度推导（`1.5` vs `1.5f`） | §1.3 | — |
| `Cast<>` 只认大写形式 | §1.2 | — |
| 命名实参同时支持 `:` 与 `=` | §4 | — |
| 不提供 `@` / `null` / `is` / `and`/`or`/`xor` 的补全与高亮 | §1.1 | — |
| `interface` / `funcdef` / `typedef` 不作为关键字 | §1.1 | — |
| `fallthrough` 位置校验 | §4 | `AS0003` |
| `local` 函数不进跨模块符号表 | §4 | — |

### 6.3 文档职责划分与同步约定

`docs/` 下四份文档的职责边界：

| 文档 | 唯一负责 |
|---|---|
| **本文** | 原版 ↔ fork 的方言差异（token 增删、GC 归属、机制演化）及其**因果** |
| [`struct类型专题.md`](struct类型专题.md) | `class`/`struct` 二分的全部细节、UE 映射、继承与展平、成员查找规则 |
| [`诊断码表.md`](诊断码表.md) | **码号分配**、引擎错误原文、range/quick-fix 约定 |
| [`架构设计.md`](架构设计.md) | 模块划分、数据流、路线图、风险 |

以下改动必须同步更新对应文档：

| 改动 | 需更新位置 |
|---|---|
| 新增/调整任何诊断 | **码表**对应子表 + §10 占用总览（**先占号再写代码**） |
| 引擎错误措辞变化 | **码表** `Message` 列（其它文档不得抄录原文） |
| 发现新的 fork 与原版差异 | 本文 §1 / §4，并在 §7 追加版本记录 |
| 发现新的 `class`/`struct` 语义差异 | **专题** §3 / §4，并在专题 §11 追加记录 |
| `grammar.js` 放宽/收紧某个形态 | 本文 §2.3 或专题 §3、`grammar/README.md`「语法接受 ≠ 语义合法」表 |
| 类型系统实现值/引用二分 | 专题 §9.1 能力表（标注实现状态） |
| `.d.as` 导出格式新增元信息 | 专题 §9.3 待办表 + `架构设计.md` §2 |

## 7. 变更记录

| 版本 | 内容 |
|---|---|
| v0.4 | §5 瘦身为「诊断分工说明 + 段位速查」，完整码表独立为 [`诊断码表.md`](诊断码表.md)；§6.3 升级为四文档职责划分表 |
| v0.3 | §2 瘦身为摘要，`struct` 完整内容独立为 [`struct类型专题.md`](struct类型专题.md)；§6.2 改为只列非 struct 能力，避免与专题重复；§6.3 同步约定覆盖两份文档 |
| v0.2 | §5 升级为**诊断码表**（`AS0001`+，含编码规则、6 个区段、实施约束）；正文差异表加诊断码交叉引用；补 §6.3 文档同步约定；`架构设计.md` 前置文档与 P5 行加入本文链接 |
| v0.1 | 首版：token 增删、`struct` 起源、GC 摘除与 UObject 内存共享、错误文本清单 |
