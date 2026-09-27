# 原版 AngelScript 与 Unreal fork 的差异

> 版本：v0.6
> 下游专题：
> - [`struct类型专题.md`](struct类型专题.md)（`class`/`struct` 全量对照、UE 映射、继承与展平）
> - [`诊断码表.md`](../docs/诊断码表.md)（码号登记处 + 取证素材库；诊断按需设计，不构成实现承诺）
>
> 前置文档：[`../grammar/angelscript.bnf`](../../grammar/angelscript.bnf)（Layer A 完整语法规范）、[`架构设计.md`](../docs/架构设计.md)
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
> 但 `import` **不是**死 token（`as_tokendef.h:275` 仍然激活），只是 UE 脚本从不用——
> 它的实际拦截点在宿主层（宿主从不绑定），见 [§5.2](#52-清单已逐项取证)。

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
> 继承违规由 LSP 报语义诊断（见 [`诊断码表.md` §4](../docs/诊断码表.md#4-as02xx--ue-反射约束)）。`grammar.js` 现状符合此约定。

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
生成不了就直接编译报错（`AS0301`，见 [`诊断码表.md` §5](../docs/诊断码表.md#5-as03xx--gc--属性生成)）。
这与 UE C++ 中「`USTRUCT` 内必须 `UPROPERTY()` 才被 GC 追踪」是同一模型。

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

## 5. 解析层放行、实际不可用的构造（paper features）

> 本节回答一个问题：**哪些语法 token 活着、parser 也认，但写出来必然失败？**
> （§1.1 的死 token 是词法层拦截，不在本节重复。）
> 这直接决定 LSP 的诊断发在哪一层：parse 层的 ERROR 节点（树崩掉），
> 还是 semantic 层带 code 的诊断（树保持完整）。本节是这类构造的权威清单。

### 5.1 四层拦截模型

一个构造从「写得出来」到「跑得起来」要过四道闸门：

| 层 | 闸门 | 死在这层的形态 |
|---|---|---|
| L1 词法 | `as_tokendef.h` token 表 | parse error（§1.1：`@` / `null` / `is` / `interface` / `funcdef` / `typedef` …） |
| L2 编译 | `as_compiler.cpp` / `as_builder.cpp` | 编译期报错 |
| L3 UE 认领 | 预处理器 / ClassGenerator | VM 编译通过，但 UClass 世界里无人认领 |
| L4 宿主绑定 | AngelscriptCode 运行时 | 运行期才报错 |

### 5.2 清单（已逐项取证）

| 构造 | 层 | 机制 | 证据 | LSP 处置 |
|---|---|---|---|---|
| **lambda** `function(...) {...}` | L2 | 编译器对 lambda 的唯一出口是隐式转换到 funcdef（`ImplicitConvLambdaToFunc` 开头即 `asASSERT(to.IsFuncdef() && ctx->IsLambda())`）；而 funcdef 是死 token（§1.1），且 AngelscriptCode 从不注册脚本可见的 funcdef → lambda 恒报 `Invalid expression: stand-alone anonymous function` | `[ENGINE]as_compiler.cpp:8339`（断言）、`6291`（报错点）、`as_texts.h:150`；AngelscriptCode 全模块无 `RegisterFuncDef` | **文法不实现**（parse error，2026-09 决策）：语法层拒绝与「实际跑不起来」一致；原 `AS0107` 随之 retired |
| **裸 enum 值** `Value`（非 `MyEnum::Value`） | L2 | `asEP_REQUIRE_ENUM_SCOPE = 1`：符号查找跳过「不写枚举类型名」的兜底分支，落入通用「未找到」错误 | `[UE]Private/AngelscriptManager.cpp:337`；`[ENGINE]as_compiler.cpp:11815`（`!engine->ep.requireEnumScope` 守卫） | 归入 `AS04xx`（符号解析，P3/P4 落地时占号） |
| **mixin class** `mixin class Foo {...}` | L2 | `mixin` token 活着，但 `ParseMixin` 已被重定义为「mixin + **函数**声明」；`as_builder` 里的 mixin class 机器（`RegisterMixinClass` / `IncludeMethodsFromMixins` 等）是上游遗留、不可达 | `[ENGINE]as_parser.cpp:3700-3714`（"A mixin token must be followed by a function declaration"）；`as_builder.cpp:2022 / 2872 / 3827` | parse error 是**正确**行为：grammar 不实现 mixin class，与引擎一致。**但 mixin 函数是活功能且语义完整**，见 [§5.4](#54-mixin-函数活功能与-mixin-class-无关) |
| **import** `import void f() from "mod";` | L4 | token 活、`ParseScript` 正常分发（`ttImport` → `ParseImport`）、VM 编译通过；但宿主必须调 `BindAllImportedFunctions` 才可用——AngelscriptCode 全模块 **0** 调用 → 调用时运行期报 `Unbound function called` | `[ENGINE]as_tokendef.h:275`、`as_parser.cpp:2471`、`as_texts.h:363` | **文法不实现**（parse error，2026-09 决策）：语法层拒绝比「放行再等运行期炸」更诚实；原 `AS0904` 随之 retired |
| **虚属性（带 body）** `int X { get { ... } }` | L3 | VM 把访问器编译成 opGet/opSet 方法，但预处理器 / ClassGenerator 对 VirtualProperty **零处理**，UClass 侧无人认领；官方 pegjs 无此语法，引擎/插件全部脚本零使用 | AngelscriptCode 全模块 `VirtualProperty` **0** 命中；`grammar.js` / BNF §2.3.7 注释已记录 | 语法放行；语义层**暂不报错**（使用率为零，低优先级）；bodyless 形态已有 `AS0005` |

### 5.3 对照：Hazelight 官方 LSP 的「有效语法集合」

官方扩展的 pegjs 语法（`language-server/pegjs/angelscript.js`、`grammar/node_types.js`）
里没有：lambda、interface、funcdef、import、mixin class、虚属性、`@`。
这个集合基本是「真正可用」的**下界**。本 LSP 的取向（2026-09 决策）：**文法与实际可运行
的集合对齐**——lambda / import / mixin class / `@` 一律不实现（parse error，与 pegjs 一致）；
例外是虚属性（AS VM 自身完整支持，拦截发生在 UE 认领层，且 bodyless 形态有 `AS0005`
诊断依赖），语法放行、语义层暂不报错。

### 5.4 mixin 函数：活功能，与 mixin class 无关

`mixin` 这个词在 UE fork 里对应**三套互不相关的机制**，极易混淆：

| # | 形态 | 状态 |
|---|---|---|
| 1 | `mixin class Foo {...}`（原版 AS 语法） | ❌ **不存在**，parse error（§5.2） |
| 2 | `mixin void Foo(AActor T)`（脚本侧扩展方法） | ✅ **活功能**，语义完整 |
| 3 | `UCLASS(Meta=(ScriptMixin="FVector"))`（C++ 侧） | ✅ 活功能，但**绑定期就变成真成员方法** |

**形态 2（脚本 mixin 函数）**——两种写法等价，都会被打上 `asTRAIT_MIXIN`
（`[ENGINE]as_builder.cpp:4604`）：

```angelscript
mixin void Heal(AActor Target, float Amount) { ... }    // 前置：ParseMixin（as_parser.cpp:3700）
void Heal(AActor Target, float Amount) mixin { ... }    // 后置属性（as_parser.cpp:3249 / 5083）
```

函数注册为**普通全局函数**，扩展方法效果由编译器在符号查找时补上——
真值 `[ENGINE]as_compiler.cpp:13387-13450`（`// Look for mixin functions`），
守卫条件五条**全部满足**才查：

1. `funcs.GetLength() == 0` —— **纯 fallback**，常规成员查找命中就绝不查 mixin；
2. `objectType != 0 || ThisObjectType != nullptr` —— 需对象上下文（显式 `obj.` 或类方法体内隐式 `this`，引擎自动压栈 `:13436-13449`）；
3. `scope.GetLength() == 0` —— `NS::Heal(obj)` **不走** mixin；
4. 沿 `GetParentNameSpace` 逐级回退父命名空间；
5. 首参存在 && `IsObject()` && `availableObjectType->DerivesOrShadows(首参类型)`
   —— 注意是 `DerivesOrShadows`（含 AS 类 shadow C++ 类），比单纯继承宽；
   且 value type 亦满足 `IsObject()` ⇒ **可为 struct 写 mixin**。

`asTRAIT_MIXIN` 在编译器里仅有一处额外用途：no-discard 判定时按 const 方法对待
（`as_compiler.cpp:18627`），返回值未使用会警告。符号查找本身不因该 trait 特殊化。

UE 反射层效果仅限编辑器：给生成的 UFunction 打 `MixinArgument` + `DefaultToSelf` meta
（`[UE]Private/ClassGenerator/AngelscriptClassGenerator.cpp:3324-3331`，`#if WITH_EDITOR`），
用于蓝图节点首参默认连 self。

**形态 3（C++ `ScriptMixin`）** 是 UE fork 的主力用法，引擎自带一大批
（`AngelscriptMathLibrary.h` 的 `FVector`/`FRotator`/`FQuat`/`FTransform` 库、
`GameplayTag*MixinLibrary`、`InputComponentScriptMixinLibrary`、`UWidget`/`UWorld` 库等）。
绑定期 `[UE]Private/Binds/Helper_FunctionSignature.h:283-345` 把静态 UFUNCTION 转成目标类型的
**真成员方法**（`EBindTargetType::MixinMethod`，摘掉首参）⇒ 脚本侧与普通成员方法无差别。

> **使用现状**：引擎自带 `.as` 脚本中形态 2 的使用数为 **0**（实测），但官方 LSP 有大量
> `isMixin` 处理逻辑（`as_parser.ts` 数十处）——它是给项目脚本准备的活功能，不是遗迹。

LSP 侧的落地约定见 [`架构设计.md` §4.5.1 / §4.5.2](../docs/架构设计.md)。

### 5.5 相关引擎属性开关

`[UE]Private/AngelscriptManager.cpp:325-355` 集中设置了一批引擎属性，与「写了能不能编过」直接相关的：

| 属性 | 值 | 后果 |
|---|---|---|
| `asEP_REQUIRE_ENUM_SCOPE` | 1 | 裸 enum 值编译报错（见 §5.2） |
| `asEP_TYPECHECK_SWITCH_ENUMS` | 1 | `switch` 条件与 `case` 值的枚举类型严格校验 |
| `asEP_PROPERTY_ACCESSOR_MODE` | `AS_PROPERTY_ACCESSOR_MODE` = 3（`AngelscriptManager.h:24`） | 绑定层用 `property` 方法属性暴露属性访问（引擎默认模式 3）——脚本虚属性（§5.2）与之无关 |

（浮点宽度 `asEP_FLOAT_IS_FLOAT64` 见 §1.3；命名实参 `asEP_ALTER_SYNTAX_NAMED_ARGS` 见 §4；
隐式句柄 `asEP_ALLOW_IMPLICIT_HANDLE_TYPES` 见 §1.1。）

## 6. 诊断：引擎错误文本如何落成诊断码

> **完整码表已独立成篇：[`诊断码表.md`](../docs/诊断码表.md)**
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

## 7. 对本项目各模块的影响

### 7.1 grammar（模块二）

- 已正确不实现 `[dead]` 顶层声明与 `@` / `null` / `is` / `and`/`or`/`xor`；
- `struct` 与 `class` 作为独立节点，`default_statement` 只出现在 `class_declaration` 内；
- struct 的继承列表、struct 内 `UFUNCTION()` **照常解析**，交给 LSP 报诊断；
- paper features（§5.2）中 lambda / import / mixin class **不实现**（parse error，
  2026-09 决策，见 §5.3）；虚属性照常解析（§5.2，AS VM 支持）；
- 浮点字面量的 `f` 后缀需要保留在 CST 中（`float32` vs `float64` 影响类型推导）。

### 7.2 lsp（模块三）类型系统

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
| 虚属性（语法放行、UE 不认领） | §5.2 | `AS0005`（bodyless 形态） |

### 7.3 文档职责划分与同步约定

文档分两个目录（2026-09 拆分）：`as-docs/` = UE Angelscript **语言分析**（与 LSP 实现无关的真值），
`docs/` = **LSP 设计**；`docs/` 单向引用 `as-docs/`（本文偶尔指回 docs 的实现契约属例外）。

| 文档 | 目录 | 唯一负责 |
|---|---|---|
| **本文** | `as-docs/` | 原版 ↔ fork 的方言差异（token 增删、GC 归属、机制演化）及其**因果** |
| [`设计取舍与使用限制.md`](设计取舍与使用限制.md) | `as-docs/` | **设计取舍总纲**：两条第一性原则、取舍因果链、「想要 X 用 Y」速查 |
| [`struct类型专题.md`](struct类型专题.md) | `as-docs/` | `class`/`struct` 二分的全部细节、UE 映射、继承与展平、成员查找规则 |
| [`诊断码表.md`](../docs/诊断码表.md) | `docs/` | **码号分配**、引擎错误原文、range/quick-fix 约定 |
| [`架构设计.md`](../docs/架构设计.md) | `docs/` | 模块划分、数据流、路线图、风险 |

以下改动必须同步更新对应文档：

| 改动 | 需更新位置 |
|---|---|
| 新增/调整任何诊断 | **码表**对应子表 + §10 占用总览（**先占号再写代码**） |
| 引擎错误措辞变化 | **码表** `Message` 列（其它文档不得抄录原文） |
| 发现新的 fork 与原版差异 | 本文 §1 / §4，并在 §8 追加版本记录 |
| 发现新的 paper feature（解析放行但不可用） | 本文 §5.2 清单，需诊断的同步在**码表** `AS01xx`/`AS09xx` 占号 |
| 发现新的设计取舍 / 官方替代品 | [`设计取舍与使用限制.md`](设计取舍与使用限制.md) §2 / §3 |
| 发现新的 `class`/`struct` 语义差异 | **专题** §3 / §4，并在专题 §11 追加记录 |
| `grammar.js` 放宽/收紧某个形态 | 本文 §2.3 或专题 §3、`grammar/README.md`「语法接受 ≠ 语义合法」表 |
| 类型系统实现值/引用二分 | 专题 §9.1 能力表（标注实现状态） |
| `.d.as` 导出格式新增元信息 | 专题 §9.3 待办表 + `架构设计.md` §2 |

## 8. 变更记录

| 版本 | 内容 |
|---|---|
| v0.6 | 新增 **§5.4「mixin 函数：活功能，与 mixin class 无关」**：厘清 `mixin` 一词对应的**三套互不相关机制**（原版 mixin class 不存在 / 脚本侧 mixin 函数活且语义完整 / C++ `ScriptMixin` meta 绑定期即转真成员方法）；含两种声明形式、`as_compiler.cpp:13387-13450` 的五条准入条件取证、struct 可作首参、no-discard 特殊待遇、UE 反射层 meta 效果。原 §5.4（引擎属性开关）顺移为 §5.5；§5.2 的 mixin class 行补指引 |
| v0.5.2 | §7.3 职责表与同步约定加入新文档 [`设计取舍与使用限制.md`](设计取舍与使用限制.md)（设计取舍总纲，「为什么」层面的因果在此展开，本文保持「是什么」） |
| v0.5.3 | 文档目录拆分：语言分析类（本文 / struct 专题 / 设计取舍）移入 `as-docs/`，LSP 设计类（架构设计 / 诊断码表 / 引擎内部语法）留在 `docs/`；跨目录引用全部更新 |
| v0.5 | 新增 §5「解析层放行、实际不可用的构造（paper features）」：四层拦截模型（L1 词法 / L2 编译 / L3 UE 认领 / L4 宿主绑定）、五项逐项取证清单（lambda / 裸 enum 值 / mixin class / import / 带体虚属性）、Hazelight 官方 LSP 有效语法对照、相关引擎属性开关表；原 §5/§6/§7 顺移为 §6/§7/§8；修正 §3.3 指向旧 §5.5 的失效锚点 |
| v0.5.1 | §5.2/§5.3/§7.1：lambda 与 import 的 LSP 处置从「语法放行 + `AS0107`/`AS0904`」改为**文法层不实现**（2026-09 决策：文法与实际可运行集合对齐）；虚属性维持语法放行（AS VM 自身支持）。两个诊断码在码表 retired |
| v0.3 | §2 瘦身为摘要，`struct` 完整内容独立为 [`struct类型专题.md`](struct类型专题.md)；§6.2 改为只列非 struct 能力，避免与专题重复；§6.3 同步约定覆盖两份文档 |
| v0.2 | §5 升级为**诊断码表**（`AS0001`+，含编码规则、6 个区段、实施约束）；正文差异表加诊断码交叉引用；补 §6.3 文档同步约定；`架构设计.md` 前置文档与 P5 行加入本文链接 |
| v0.1 | 首版：token 增删、`struct` 起源、GC 摘除与 UObject 内存共享、错误文本清单 |
