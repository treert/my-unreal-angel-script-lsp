# `struct` 类型专题

> 版本：v0.2
> 上游文档：[`原版AngelScript与UE-fork对比.md`](原版AngelScript与UE-fork对比.md)（方言差异总览）
> 诊断码定义：[`诊断码表.md`](诊断码表.md)（本文只引用码号，不定义）
> 相关文档：[`架构设计.md`](架构设计.md)、[`../grammar/angelscript.bnf`](../grammar/angelscript.bnf)
> 证据来源（只读参考）：
> - `[ENGINE]` = `d:/WorkGit/UnrealEngine/Engine/Plugins/Angelscript/ThirdParty/source/`
> - `[UE]` = `d:/WorkGit/UnrealEngine/Engine/Plugins/Angelscript/Source/AngelscriptCode/`
> - `[DUMP]` = `d:/WorkGit/UEProjs/Demo_AS/Saved/AS-Cache/*.d.as`（414 个文件，实测样本）

## 0. 为什么单独成篇

`struct` 是 UE fork **新增**的语言构造，而且是整个方言里语义最容易被误解的一处：

- 它和 C++ 的 `struct`（仅默认访问权限不同）**毫无相似之处**；
- 它和 UE 的 `USTRUCT`（支持继承）**只有部分重叠**；
- 它在 `.as`（脚本声明）与 `.d.as`（引擎导出）两种语境下**含义不同**；
- 「struct 对应 `UStruct`」这种常见说法**是错的**（`UStruct` 在 AS 里是 `class`）。

本 LSP 的类型系统、成员查找、hover、`Cast<>` 判定、GC 诊断全都依赖这些结论，
散落在对比文档各章节里容易漏读，故独立成篇。

**诊断码不在本文定义**，统一由 [`诊断码表.md`](诊断码表.md) 管理；本文只引用码号。

## 1. 起源：原版 AngelScript 没有 `struct`

原版 AngelScript 的脚本**只能声明 `class`**（外加 `interface` / `funcdef` / `typedef`，这三个在 fork 里都是死 token）。

值类型机制（`asOBJ_VALUE`）在原版**是存在的**，但只能从 C++ 侧注册：

```cpp
engine->RegisterObjectType("Foo", sizeof(Foo), asOBJ_VALUE | asOBJ_POD);
```

脚本没有任何语法去声明一个值类型。

fork 的做法是新增 `ttStruct` token，**复用已有的 `asOBJ_VALUE` 基础设施**，让脚本自己能声明值类型。
token 追加在枚举尾部，和其他 fork 新增 token 挤在一起（`[ENGINE]as_tokendef.h:180-186`）：

```c
	ttStruct,              // struct
	ttLocal,               // local
	ttFallthrough,         // fallthrough

	ttAccess,
	ttUnresolvedObject,
```

## 2. 唯一的分叉点

`class` 与 `struct` **共用同一个 `ParseClass()`**，只靠一个 `bool isStruct` 区分
（`[ENGINE]as_parser.cpp:3761`）。类型标志的翻转发生在一处：

```cpp
// [ENGINE]as_builder.cpp:2246, 2265-2276
st->flags = asOBJ_REF | asOBJ_SCRIPT_OBJECT | asOBJ_NOCOUNT;   // 默认
if (isStruct)
{
    // All script structs are by value
    st->flags &= ~(asOBJ_REF | asOBJ_NOCOUNT);
    st->flags |= asOBJ_VALUE;
    st->flags |= asOBJ_NOINHERIT;        // 自动 final
}
else
{
    // All script classes are implicit handle
    st->flags |= asOBJ_IMPLICIT_HANDLE;
}
```

| | `class` | `struct` |
|---|---|---|
| 类型标志 | `asOBJ_REF \| SCRIPT_OBJECT \| NOCOUNT \| IMPLICIT_HANDLE` | `asOBJ_VALUE \| SCRIPT_OBJECT \| NOINHERIT`（可能 +`POD`） |

**所有后续差异都是这一行的下游结果**，包括虚表、factory、继承、句柄、`Cast`、CDO、热重载。
`asOBJ_POD` 的追加条件：struct 无自定义赋值运算符（`as_builder.cpp:885-978`，`:967`）。

## 3. 语法层：只差一条

| 项 | `class` | `struct` | 证据 |
|---|---|---|---|
| `default X = ...;` | 支持 | **不支持**（唯一的 parser 级拒绝）→ `AS0001` | `as_parser.cpp:3828` `else if( !isStruct && IsClassDefaultStatement() )`；违规落到 `:3835` |
| `: Base` 继承列表 | 支持 | **parser 接受**，被上层拒绝 → `AS0201` | `as_parser.cpp:3786-3803` |
| 方法 / 构造 / 析构 / 运算符重载 / 虚属性 / `access` 声明 | 支持 | 支持（完全一致） | `as_parser.cpp:3818-3827` |

> **文法决策**：`struct F : G {}` 在 tree-sitter 层应当**正常解析成功**，
> 继承违规由 LSP 报语义诊断。`grammar.js` 现状符合此约定。

注意第三行——**struct 有方法、有构造/析构、能重载运算符**。
它不是「只能放数据的 POD 容器」，这点和 C++ 的 `USTRUCT` 一致。

## 4. 语义层完整对照

| 维度 | `class` | `struct` | 证据 |
|---|---|---|---|
| 赋值语义 | 句柄重绑定 | `PerformCopy` **深拷贝** | `as_compiler.cpp:10230-10235`；`[UE]Private/ClassGenerator/ASStruct.cpp:170-179` |
| 可否取句柄 | 隐式句柄（写不写都是） | **否** → `AS0103` | `as_compiler.cpp:13883-13888`（`TXT_OBJECT_HANDLE_NOT_SUPPORTED`） |
| 可否 `Cast<>` | 是 | **否**（绑定层直接 return）→ `AS0106` | `[UE]Private/Binds/Bind_UObject.cpp:135-137`（注释原文 `// Structs cannot be cast to uobjects`） |
| factory | 有 | **无**（有 constructor，无 factory） | `as_builder.cpp:2315-2323, 4192, 5219-5255` |
| 实例化方式 | `NewObject`/`SpawnActor`/`UXxx::Create`；构造语法被禁 → `AS0104` | **声明即构造** | `as_compiler.cpp:12859-12861` |
| 虚函数表 | 有，多态派发 | **无**，静态派发 | `as_builder.cpp:3720-3728`；`[UE]Private/StaticJIT/PrecompiledData.cpp:742-775` |
| 继承 / 被继承 | 均可 | **均禁**（见 §6） | `AngelscriptPreprocessor.cpp:1122-1127`；`as_builder.cpp:2270, 3200-3207` |
| 无显式父类时 | 隐式补 `: UObject` | `SetSuperStruct(nullptr)`，永远无父 | `AngelscriptPreprocessor.cpp:747-753`；`AngelscriptClassGenerator.cpp:2633` |
| 内存布局基址 | `basePropertyOffset` = C++ 父类 `GetPropertiesSize()`，`shadowType` = C++ 类型 | 从 0 开始，无 shadow base | `as_builder.cpp:2283-2289`；`[UE]Private/AngelscriptManager.cpp:2974-2978` |
| 作为成员时 | 指针（`GetSizeOnStackDWords()*4`） | **内嵌值**（`GetSizeInMemoryBytes()`） | `as_builder.cpp:3557-3563` |
| 布局自递归 | 不可能（是指针） | **报错** → `AS0105` | `as_builder.cpp:3415-3427` |
| 命名前缀剥离 | `U` / `A` | `F` | `AngelscriptClassGenerator.cpp:133-141` |
| 生成的静态符号 | `TSubclassOf<UObject> __StaticType_X` + `X::StaticClass()` | `TStructType<FScriptStructWildcard> __StaticType_X`（**无 `StaticClass()`**） | `AngelscriptPreprocessor.cpp:796-845` |
| `UFUNCTION()` | 支持 | **禁止** → `AS0202` | `AngelscriptPreprocessor.cpp:1257-1262` |
| `UPROPERTY()` | 支持 | 支持（默认 Edit 级别用 `DefaultPropertyEditSpecifierForStructs`） | `AngelscriptPreprocessor.cpp:2253-2254, 3360-3361` |
| `NotReplicated` specifier | **禁止** → `AS0203` | **仅 struct 允许** | `AngelscriptPreprocessor.cpp:2471-2477` |
| CDO / 默认对象 | 有（`InitDefaultObject`） | **无** | `AngelscriptClassGenerator.cpp:5628-5647` |
| Tick 设置 | 有 | **无** | `AngelscriptClassGenerator.cpp:5612-5618` |
| 网络复制 | 支持（`GetLifetimeScriptReplicationList`） | 本身不是复制单元 | `[UE]Private/ClassGenerator/ASClass.cpp:857-883` |
| 热重载 | soft reload | **仅 full reload**（新增属性、offset 变化都强制） | `AngelscriptClassGenerator.cpp:1165-1198, 2226-2299` |
| 蓝图类型 | 由 `UCLASS(Blueprintable/...)` 决定 | **自动 `BlueprintType=true`** | `AngelscriptClassGenerator.cpp:3094` |
| 特殊方法被接管 | — | `opEquals` / `ToString` / `Hash` → `FASStructOps`（见 §7） | `ASStruct.cpp:16-80, 95-123` |

一句话：**`class` ≈ C++ 的 `UObject*`，`struct` ≈ C++ 的 `USTRUCT` 值类型。**

## 5. UE 映射：必须区分「描述符」与「实例」

常见误解是「class 对应 `UClass`，struct 对应 `UStruct`」。这句话有两处错。

### 5.1 `UStruct` 在 AS 里是 `class`

UE 的反射元类型层次：

```
UObject → UField → UStruct → ┬ UClass         (类的描述符)
                             ├ UScriptStruct  (结构体的描述符)
                             └ UFunction      (函数的描述符)
```

`UStruct` 自身是 UObject 派生的，所以在 AS 里它是 `class`：

```52:57:d:\WorkGit\UEProjs\Demo_AS\Saved\AS-Cache\CoreUObject.d.as
class UStruct : UField
class UScriptStruct : UStruct
```

### 5.2 描述符 ≠ 实例

| 脚本写法 | 生成的**描述符对象** | 描述符的类型 | **实例**是什么 |
|---|---|---|---|
| `class Foo` | 一个 `UASClass` 实例 | `UASClass : UClass` | UObject 派生对象 |
| `struct FFoo` | 一个 `UASStruct` 实例 | `UASStruct : UScriptStruct` | 一块内嵌的值内存（**非 UObject**） |

证据：`[UE]Public/ClassGenerator/ASClass.h:10-11`、`ASStruct.h:9-10`；
分派点 `AngelscriptClassGenerator.cpp:2136-2139`
（`if (ClassData.NewClass->bIsStruct) CreateFullReloadStruct(...) else CreateFullReloadClass(...)`）。

`Foo::StaticClass()` 返回的就是那个 `UASClass` 实例——**类型的类型**，和继承链无关。

### 5.3 「无父类 → 隐式 `UObject`」（不是 `UClass`）

```745:754:d:\WorkGit\UnrealEngine\Engine\Plugins\Angelscript\Source\AngelscriptCode\Private\Preprocessor\AngelscriptPreprocessor.cpp
	// Determine the direct superclass of this type
	ClassDesc->SuperClass = MatchClass.GetCaptureGroup(5);
	if (ClassDesc->SuperClass.Len() == 0)
	{
		if (!ClassDesc->bIsStruct)
		{
			// No superclass specified on a non-struct means this is a type of UObject
			ClassDesc->SuperClass = TEXT("UObject");
		}
	}
```

三个要点：

1. 补的是 `UObject`（**继承链上的基类**），不是 `UClass`（描述符类型）；
2. `if (!bIsStruct)` —— struct 不补父；
3. 这是**预处理器规则，只作用于 `.as`**。`.d.as` 里无父类是字面意义的根。

### 5.4 映射的严格性边界

| 命题 | 严格性 | 依据 |
|---|---|---|
| 脚本 `class` → `UASClass`，实例是 UObject | ✅ 严格 | `AngelscriptPreprocessor.cpp:732, 757` 无条件建 desc（**与有无 `UCLASS()` 宏无关**） |
| 脚本 `struct` → `UASStruct : UScriptStruct` | ✅ 严格 | 同上 + `:742-743` |
| AS 里的 `class` → 一定是 UObject 派生 | ✅ 严格 | `[DUMP]` 实测：唯一无父类的 class 是 `CoreUObject.d.as:9 class UObject`，其余全部链到它 |
| AS 里的 `struct` → 一定对应 `UScriptStruct` | ❌ **不成立** | 见下 |
| AS 里的 `struct` → 一定是 `asOBJ_VALUE` 值类型 | ✅ 严格（**这才是真正的不变式**） | `as_builder.cpp:2265-2271` |

反例来自 `[DUMP]`——大量**纯 C++ 值类型**也用 `struct` 暴露，它们在 UE 里没有 `UScriptStruct`：

```
struct FString            ← 无反射类型
struct FName              ← 无反射类型
struct TArray<T>          ← C++ 模板
struct TMap<K, V>         ← C++ 模板
struct TSubclassOf<T>     ← C++ 模板
struct TSoftObjectPtr<T>  ← C++ 模板
struct TArrayIterator<T>  ← 迭代器，连数据类型都不是
struct FVector            ← 真 USTRUCT
struct FTimerHandle       ← 真 USTRUCT
```

**结论**：`struct` 在 AS 里的语义是「值类型」，`UScriptStruct` 只是「脚本声明的 struct」这一子集的落地形式。
`.d.as` 现状**无法区分**两者（见 §9 待办）。

### 5.5 `UCLASS()` / `USTRUCT()` 宏不是必需的

项目里现成的例子就没写宏：

```1:1:d:\WorkGit\UEProjs\Demo_AS\Script\IntroductionActor.as
class AIntroductionActor : AActor
```

预处理器在 `:732` 无条件构造 `FAngelscriptClassDesc`、`:757` 无条件 `Classes.Add(ClassDesc)`，
宏（`ProcessClassMacro`）只负责**追加 specifier**。`UCLASS(` / `USTRUCT(` 甚至走**完全同一套** specifier 解析
（`AngelscriptPreprocessor.cpp:3395-3421` 只差偏移 7 / 8；`:2106-2195` 共用 `ProcessClassMacro`）。

## 6. 继承：AS 禁止，但 UE 支持——展平机制

这是 struct 最容易踩坑的一处。

### 6.1 UE C++ 侧：`USTRUCT` 支持继承

`UScriptStruct : UStruct` 有完整的 `SuperStruct` 链，UE 里大量使用：

```cpp
USTRUCT() struct FTickFunction { ... };
USTRUCT() struct FActorTickFunction : public FTickFunction { ... };
USTRUCT() struct FTableRowBase { ... };   // 所有 DataTable 行的基类
```

### 6.2 AS 脚本侧：四道锁禁止

```1122:1127:d:\WorkGit\UnrealEngine\Engine\Plugins\Angelscript\Source\AngelscriptCode\Private\Preprocessor\AngelscriptPreprocessor.cpp
	// Structs cannot inherit from anything
	if (ClassDesc->SuperClass.Len() != 0)
	{
		ChunkError(File, Chunk, FString::Printf(TEXT("Error parsing script struct %s. Structs may not inherit from anything."), *ClassDesc->ClassName));
		bHasError = true;
	}
```

| 层 | 机制 | 位置 |
|---|---|---|
| 预处理器 | 显式报错 → `AS0201` | `AngelscriptPreprocessor.cpp:1123-1127` |
| builder | `asOBJ_NOINHERIT` → struct **自动 final**，别人也不能继承它 → `AS0102` | `as_builder.cpp:2270`；`:3200-3207` |
| builder | 无 `basePropertyOffset` / 无 `shadowType` | `as_builder.cpp:2283-2285` |
| builder | 内部断言（有基类时不可能是 struct） | `as_builder.cpp:3596` `check(!bIsStruct)` |
| ClassGenerator | `SetSuperStruct(nullptr)` 硬写死 | `AngelscriptClassGenerator.cpp:2633` |

这不是「暂未实现」，而是**刻意设计**：struct 无虚表、静态派发、可 POD、按值拷贝；
支持继承就必须引入虚表或面对切片（slicing）问题。

### 6.3 已有的 C++ USTRUCT 继承体系 → **展平**

绑定层用 `TFieldIterator`，它**默认包含 SuperStruct 的字段**：

```430:430:d:\WorkGit\UnrealEngine\Engine\Plugins\Angelscript\Source\AngelscriptCode\Private\Binds\Bind_UStruct.cpp
		for (TFieldIterator<FProperty> It(UsedStruct); It; ++It)
```

于是派生 struct 把基类成员**原样复制**一份，但不声明父类。`[DUMP]` 实测：

| 类型 | `.d.as` 位置 | 成员 |
|---|---|---|
| `struct FTickFunction` | `Core.d.as:12614` | `TickGroup` `EndTickGroup` `TickInterval` `bTickEvenWhenPaused` `bStartWithTickEnabled` `bAllowTickOnDedicatedServer` |
| `struct FActorTickFunction` | `Core.d.as:28747` | `TickGroup` `EndTickGroup` ...（**基类成员原样出现，连注释都一致，但没有 `: FTickFunction`**） |

**全量实测：414 个 `.d.as` 中带 `:` 父类的 struct 数量 = 0。**
整个 UE 的 USTRUCT 继承体系在 AS 里被压平成互不相关的扁平类型。

`Bind_UStruct.cpp` 里唯一的 `opImplConv` 是 `TStructType<T>` → `UScriptStruct` / `UObject`
（`:1620-1621`，取描述符对象），**没有任何 base↔derived 的 struct 转换**。

### 6.4 展平的后果

| 场景 | C++ | AS |
|---|---|---|
| 访问继承来的成员 | ✅ | ✅（展平后成了自己的成员） |
| 派生传给收基类的函数 | ✅ 隐式向上转换 | ❌ **两个无关类型** |
| 基类引用遍历异构容器 | ✅ | ❌ |
| `TArray<FTickFunction>` 存派生 | ⚠️ 会切片 | ❌ 类型不匹配 |
| 反射层的 `IsChildOf` | ✅ | 底层 `UScriptStruct` 仍有 SuperStruct，但 **AS 类型系统看不到** |

**推论**：`FTableRowBase` 派生的 DataTable 行结构，在 AS 里无法用「传 `FTableRowBase&`」的泛化写法处理，
只能逐个具体类型写。

> **双向缺失**：AS 的 struct 继承在两个方向都不存在——脚本不能写，引擎已有的也传不进来。

## 7. `FASStructOps`：struct 的行为由脚本方法驱动

`UASStruct` 注册了一套伪 vtable（`[UE]Private/ClassGenerator/ASStruct.cpp:26-80`），
把 UE 的 `ICppStructOps` 接口转发到脚本方法：

| `ICppStructOps` 槽位 | 实现 | 脚本侧对应 | 位置 |
|---|---|---|---|
| `Construct` | `new(Dest) asCScriptObject(ScriptType)` + 调脚本构造函数 | `FFoo()` | `ASStruct.cpp:134-156` |
| `Destruct` | `ScriptObject->CallDestructor(ScriptType)` | `~FFoo()` | `:158-168` |
| `Copy` | `DestObject->PerformCopy(...)` | （自动，或 `opAssign`） | `:170-179` |
| `Identical` | 调脚本 `opEquals` | `bool opEquals(const FFoo&) const` | `:181-196` |
| `GetStructTypeHash` | 调脚本 `Hash` | `uint32 Hash() const` | `:198-208` |
| `ExportTextItem` 类 | 调脚本 `ToString` | `FString ToString() const` | `:106-113` |
| `IsPlainOldData` | 读 `asOBJ_POD` | （无自定义 `opAssign` 时自动） | `:75-79` |
| `FGuid` | `SetGuid()`（蓝图结构体兼容） | — | `:250-261` |

**对 LSP 的意义**：这几个方法名在 struct 上是**语义特殊**的。
hover 时应标注「此方法会被 UE 用于结构体比较 / 哈希 / 序列化」，
补全时可作为 snippet 优先推荐（尤其 struct 要进 `TSet` / `TMap` 键时必须有 `Hash`）。

## 8. GC：struct 的特殊处理

脚本 class 实例**就是** UObject（`basePropertyOffset` + `shadowType`，详见对比文档 §3.2），
所以 AS 自带 GC 被整体摘除（`asOBJ_NOCOUNT`）。

但 struct 是值类型、内嵌在宿主内存中，**自身不是 UObject**，UE GC 无法直接扫到它内部的 UObject 引用。
对策是强制生成隐藏 `FProperty`：

```283:287:d:\WorkGit\UnrealEngine\Engine\Plugins\Angelscript\Source\AngelscriptCode\Private\ClassGenerator\AngelscriptClassGenerator.cpp
			if (ClassData.NewClass->bIsStruct)
				bShouldMakeProperty = !PropertyType.NeverRequiresGC();
			if (PropertyType.RequiresProperty())
				bShouldMakeProperty = true;
```

即：struct 里只要成员**可能**含 UObject 引用，即使没写 `UPROPERTY()` 也会被强制生成属性；
生成不了就直接编译报错（`AS0301`）。

相关的 offset 处理差异：

```314:324:d:\WorkGit\UnrealEngine\Engine\Plugins\Angelscript\Source\AngelscriptCode\Private\ClassGenerator\AngelscriptClassGenerator.cpp
			if (bMarkNonUpropertyPropertiesAsTransient || !bIsStruct)
				PropDesc->bTransient = true;
			if (bMarkNonUpropertyPropertiesAsNotReplicated && bIsStruct)
				PropDesc->bSkipReplication = true;
```

绑定层对 C++ USTRUCT 的 GC 判定另有一套（`[UE]Private/Binds/Bind_UStruct.cpp:124-172`
`HasReferences` / `EmitReferenceInfo`，走 `STRUCT_AddStructReferencedObjects` + `PropertyLink` 遍历）。

## 9. 对本 LSP 的实现要求

### 9.1 类型系统（模块三）

| 要求 | 依据 | 相关诊断码 |
|---|---|---|
| 类型模型必须有**值 / 引用**二分标志 | §2 | — |
| struct 成员查找是**单层**的，不爬继承链 | §6.3 展平 | — |
| class 成员查找必须爬链，`.as` 无父类时补 `UObject` | §5.3 | — |
| **`.d.as` 中 `class UObject` 无父类 → 必须做自环保护** | §5.3 要点 3 | — |
| struct 不参与 `Cast<>` 候选 | §4 | `AS0106` |
| struct 不接受句柄语法 | §4 | `AS0103` |
| class 不接受构造语法 | §4 | `AS0104` |
| `StaticClass()` 只在 class 上补全 | §4 生成的静态符号行 | — |
| `default` 语句只在 class body 内补全 | §3 | `AS0001` |
| 「所有实现 / override」查询跳过 struct | §4 虚函数表行 | — |
| struct 成员 GC 可达性检查 | §8 | `AS0301` |
| struct 布局自递归检查 | §4 | `AS0105` |
| `opEquals` / `Hash` / `ToString` 在 struct 上标注特殊语义 | §7 | — |

### 9.2 struct 相关诊断

本文涉及的码：`AS0001` `AS0102` `AS0103` `AS0105` `AS0106` `AS0201` `AS0202` `AS0203` `AS0301` `AS0302`。

消息文本、range 与 quick fix 约定统一见 [`诊断码表.md`](诊断码表.md)
（§2-§5 各段表 + [§8 range 与 quick fix 约定](诊断码表.md#8-range-与-quick-fix-约定)）。
**本文不重复这些内容**，只提供触发它们的语义背景。

### 9.3 `.d.as` 导出插件（模块一）待补的元信息

现状 `.d.as` 丢失了两类信息，导致 LSP 无法提供本该有的体验：

| 缺失 | 影响的能力 | 建议方案 |
|---|---|---|
| **无法区分 USTRUCT 支撑 vs 纯 C++ 值类型**（§5.4） | `AS0301` GC 诊断的适用范围、`UPROPERTY()` 可用性判定、蓝图可用性提示、`Identical`/`GetTypeHash` 相关 hover | 导出时标注，如 `struct FVector /* @ustruct */`，或走 manifest 侧表 |
| **展平后丢失字段来源**（§6.3） | hover 无法告知「`FActorTickFunction.TickGroup` 来自 `FTickFunction`」 | 导出时加 `// @inherited_from FTickFunction` 注释行 |

这两项都是**导出插件多写一点元信息、LSP 就能多给一档体验**的典型，
且都优于官方 Hazelight LSP（它完全没有这些信息）。

## 10. 速查：给使用者的结论

写 `.as` 脚本时关于 struct 需要记住的：

1. **能有方法、构造、析构、运算符重载**——不是纯数据容器；
2. **不能继承，也不能被继承**——需要复用就用组合，或改用 `class`；
3. **不能 `Cast<>`，不能取句柄**——它不是 UObject；
4. **赋值是深拷贝**——大 struct 传参注意用 `const FFoo&`；
5. **不能有 `UFUNCTION()`**，也不能写 `default` 语句；
6. **要进 `TSet` / `TMap` 键就得写 `Hash()`**，要比较就写 `opEquals()`；
7. **自动是 `BlueprintType`**，无需手写 specifier；
8. **改任何字段都触发 full reload**（不像 class 支持 soft reload），热重载会更慢。

## 11. 变更记录

| 版本 | 内容 |
|---|---|
| v0.2 | §9.2 的 range/quick-fix 表移交 [`诊断码表.md`](诊断码表.md) §8，本节改为「涉及码号清单 + 指引」，避免两处维护 |
| v0.1 | 从对比文档 §2 独立成篇，并补入：描述符/实例三层模型、映射严格性边界（`.d.as` 实测）、继承四道锁与展平机制、`FASStructOps` 方法映射表、LSP 实现要求与导出插件待办、使用者速查 |
