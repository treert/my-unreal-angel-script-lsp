# delegate / event 专题

> 版本：v0.1
> 上游文档：[`原版AngelScript与UE-fork对比.md`](原版AngelScript与UE-fork对比.md)（§1.2 预处理器伪语法总览）
> 相关文档：[`设计取舍与使用限制.md`](设计取舍与使用限制.md)（§2 lambda 被禁的两原则推导、§3 替代品速查表）、[`struct类型专题.md`](struct类型专题.md)（生成的包装是脚本 struct）
> 证据来源（只读参考）：
> - `[ENGINE]` = `config.paths.unreal_engine`/Engine/Plugins/Angelscript/ThirdParty/source/
> - `[UE]` = `config.paths.unreal_engine`/Engine/Plugins/Angelscript/Source/AngelscriptCode/
> - `[DEMO]` = `config.paths.demo_as`/Script/Script-Examples/Examples/Example_Delegates.as（官方示例）

## 0. 为什么单独成篇

`delegate` / `event` 是 UE-AS 里**地位最特殊**的构造：它们不是语言关键字（不进 token 表），
却在预处理阶段凭空生成完整的 struct 类型；它们对应 UE 的动态委托，但脚本作者看到的
成员集（`Execute` / `Broadcast` / `BindUFunction` / `AddUFunction`）**没有一个是引擎注册的**——
全部来自预处理器的文本展开，而「解绑」这类看似缺失的能力又藏在 `_Inner` 字段的
绑定类型里。三个常见误解：

- 「event 有 Remove 吗」——包装层没有，`_Inner` 上有 `Unbind` / `UnbindObject`（§4）；
- 「delegate 是 funcdef 的别名」——不是，`funcdef` 是死 token，`delegate` 是预处理器伪语法（§1）；
- 「两者只是名字不同」——生成代码、内部字段、调用语义、可用场景全部不同（§3）。

## 1. 伪语法地位与识别条件

`delegate` / `event` 是**上下文关键字**：不进 `[ENGINE]as_tokendef.h` 的 token 表，
由预处理器在**分块（chunking）阶段**做字符串匹配识别（`[UE]Preprocessor/AngelscriptPreprocessor.cpp:3525-3574`）：

```
识别条件（全部满足才命中）：
- 顶层作用域（IsTopLevelScope）
- 当前是 Global 块（ChunkType == EChunkType::Global）⇒ 脚本侧只有「全局声明」一种形态
- 不在注释 / 字符串内，且位于标识符起点
- 匹配 "event" + 空白   → bIsMulticast = true
  或 "delegate" + 空白  → bIsMulticast = false
- 向后找到 '(' … ')' 配对，且 ')' 后紧跟 ';' —— 否则整段放弃（静默，不报错）
```

命中后记入 `FDelegateDesc`（记录 chunk 下标与文本区间），**原文被 `ReplaceWithBlank` 抹掉**
（`AngelscriptPreprocessor.cpp:693`）——所以 VM 的 parser 从未见过 `delegate` / `event` 这两个词。
类内声明不走这条路径；`Bind_Delegates.cpp:40-43` 里「类内委托加 `__ClassName` 后缀」的命名规则
服务于 **C++ 侧导出**的类内委托，不是脚本能写的形态。

顺带：静态 JIT 的预编译数据里 `DeclaredEvents` 恒标 `bIsMulticast = true`
（`[UE]StaticJIT/PrecompiledData.cpp:2879-2893`），与预处理器的判定互为印证。

## 2. 展开机制：`ProcessDelegates` 逐行还原

`[UE]Preprocessor/AngelscriptPreprocessor.cpp:534-695`，对每个 `FDelegateDesc`：

1. **取名字**：从 `(` 位置向前回扫标识符字符（`:544-557`）——签名类型里的名字，如
   `delegate void FExampleDelegateSignature(...)` 取出 `FExampleDelegateSignature`；
2. **记描述符**：`FAngelscriptDelegateDesc{ Name, bIsMulticast }` 进 module（`:559-563`），
   后续由 ClassGenerator 落成 `UDelegateFunction`；
3. **生成包装 struct**（见 §3 的成员集差异），签名形参/返回值由
   `ExtractArgumentList` / `ExtractReturnType` 从原文抠出（`:589-593`）；
4. **调用协议**：实参逐个 `__Evt_PushArgument{Type}` / `__Evt_PushArgumentRef{Type}` 压栈，
   最后 `__Evt_ExecuteDelegate(_Inner)` 经 `ProcessEvent` 派发（`:602-622, :631, :658`）；
   返回值经 `__Evt_PushArgumentRef{Ret}(__ReturnValue)` 回填（`:654`）。

两个分支**共享**的生成代码（`:569-582`）：

```angelscript
struct N {
    _FScriptDelegate _Inner;           // 单播；多播为 _FMulticastScriptDelegate
    N() no_discard {}
    N(const N& Other) no_discard { this = Other; }
    N& opAssign(const N& Other) { _Inner = Other._Inner; return this; }
    // …分支差异见 §3…
}
```

即：**包装本体是脚本 struct（值类型）**，语义见 [`struct类型专题.md`](struct类型专题.md)；
委托作为函数参数传递、赋值、拷贝全部依赖这三个隐式成员。

## 3. 单播 `delegate` vs 多播 `event`：全量对照

| | `delegate R N(A);` | `event R N(A);` |
|---|---|---|
| UE 等价物 | `DECLARE_DYNAMIC_DELEGATE` | `DECLARE_DYNAMIC_MULTICAST_DELEGATE` |
| `_Inner` 类型 | `_FScriptDelegate` | `_FMulticastScriptDelegate` |
| 调用成员 | `R Execute(A) const` —— 未绑定 **Throw** `"Executing unbound delegate."`（`:663-668`） | `R Broadcast(A) const` —— 未绑定直接 return，**永不报错**（`:628-632`） |
| 宽容调用 | `R ExecuteIfBound(A) const` —— 未绑定静默返回 | （无对应物） |
| 绑定成员 | `BindUFunction(Object, n"Fn")`（`:679`） | `AddUFunction(Object, n"Fn")`（`:634`）—— 可叠加多个绑定者 |
| 绑定构造 | `N(Object, n"Fn")`（`:683`）—— 声明即绑定 | （无） |
| 返回值 | 非 `void` 返回值真实回传（`__ReturnValue`） | 声明上仍写返回类型，多播语义下无意义 |
| 被 `ReplaceWithBlank` 的原文 | 同左 | 同左 |

生成代码中被**注释掉**（未暴露在包装层）的成员：多播的 `Unbind` / `UnbindObject`
（`:636-637`），单播的 `GetUObject` / `GetFunctionName`（`:680-681`），两者共同的
`IsBound` / `Clear`（`:686-688`）——它们不是不存在，而是只在 `_Inner` 上提供（§4）。

绑定目标约束：`BindUFunction` / `AddUFunction` 只能绑 `UFUNCTION()` 函数
（`[DEMO]:55-57` 注释明示；底层 `__Internal_*_AddUFunction` 经 `ProcessEvent` 派发，天然要求反射）。

## 4. `_Inner` 穿透：绑定/解绑 API 全景

包装 struct 的 `_Inner` 是**公开字段**，其类型是 C++ 注册的绑定类型，成员集完整：

**`_FScriptDelegate`（单播内芯，`[UE]Binds/Bind_Delegates.cpp:605-631`）**

| 脚本签名 | 底层 |
|---|---|
| `bool IsBound() const` | `FScriptDelegate::IsBound` |
| `UObject GetUObject() const` | 取绑定对象 |
| `FName GetFunctionName() const` | 取绑定函数名 |
| `void Clear()` | 解绑（单播只有一格，清空即解绑） |
| `void BindUFunction(UObject, const FName&)` | 绑定（穿透等价物） |

**`_FMulticastScriptDelegate`（多播内芯，`Bind_Delegates.cpp:1107-1134`、`Bind_Delegates.h:105-113`）**

| 脚本签名 | 底层 |
|---|---|
| `bool IsBound() const` | `FMulticastScriptDelegate::IsBound` |
| `void Clear()` | 清空全部绑定 |
| `void AddUFunction(const UObject, const FName&)` | 追加绑定 |
| **`void Unbind(UObject, const FName&)`** | **`Remove(Object, FnName)`——精确解绑某对象的某函数** |
| **`void UnbindObject(UObject)`** | **`RemoveAll(Object)`——解绑该对象的全部函数** |

注意两侧的不对称：单播内芯**没有** `Unbind`（用 `Clear` 或重绑代替），多播内芯**没有**
`GetUObject` / `GetFunctionName`（可能绑了 N 个，问不出单一答案）。

所以脚本侧的实际解绑写法是穿透字段：

```angelscript
ExampleEvent._Inner.Unbind(this, n"ExampleFunction");
ExampleEvent._Inner.UnbindObject(SomeObject);
SingleDelegate._Inner.Clear();
```

引擎侧 sparse delegate（C++ 导出的稀疏多播）提供同一套脚本 API
（`IsBound/Clear/AddUFunction/Unbind/UnbindObject`，`Bind_Delegates.cpp:1274-1335`），
但走 `FSparseDelegateStorage` 的独立存储路径。

## 5. 与 UE 反射的映射

- **签名 → `UDelegateFunction`**：module 的 `Delegates` 描述符由 ClassGenerator 落成
  `UDelegateFunction`（名字后缀 `__DelegateSignature`）；AS 侧类型名经
  `CreateAngelscriptNameForDelegate` 反推：剥 `__DelegateSignature`、加 `F` 前缀、
  类内委托加 `__ClassName` 后缀（`Bind_Delegates.cpp:34-46`）；
- **签名传递协议**：包装 struct 的 asOBJ 类型把 `UDelegateFunction*` 存在
  `plainUserData`，`__DelegateSignature(this)` 由此取回签名做绑定校验
  （`Bind_Delegates.h:60-66`）；
- **event 作 `UPROPERTY`**：ClassGenerator 检查 `CPF_BlueprintAssignable`
  （`AngelscriptClassGenerator.cpp:2872`），这正是蓝图「事件分发器」可绑定节点的前提；
- **命名建议**：官方示例一律 `F` 前缀 + `Signature` 后缀，与反推规则天然吻合。

## 6. 使用限制（与两原则的关系）

- **`event` 不能作 `UFUNCTION()` 参数**（`[DEMO]:31-32` 注释明示）：UE 反射里
  `FMulticastScriptDelegate` 属性不可作函数参数，没有对应 `FProperty` 形态——
  违反原则二「每个构造必须能被反射描述」；但可作脚本类 `UPROPERTY()`（事件分发器的本命用法）；
- **`delegate` 可作 `UFUNCTION()` 参数**：单播 `FScriptDelegate` 有对应的
  `FDelegateProperty`，回调用（`[DEMO]:9-10`）；
- **没有 lambda / 闭包**：委托绑定只能 `(UObject, FName)` 二元组——生命周期归属明确
  （原则一）、反射可描述（原则二）。闭包需求的替代品见
  [`设计取舍与使用限制.md`](设计取舍与使用限制.md) §2：`FAngelscriptDelegateWithPayload`
  + `BindWithPayload`（委托 + 单个装箱捕获值）。

## 7. 对本 LSP 的影响（交付需求，不在本文展开）

- **合成成员集**：`expand.rs` 只需覆盖预处理器生成的成员（§3 表格），与
  `ProcessDelegates` 逐行对齐；`_Inner` 的成员集**不合成**，走普通字段访问链，
  真值在 `.d.as` 导出的 `_FScriptDelegate` / `_FMulticastScriptDelegate` 类型上
  （验收时需确认导出器未过滤 `Unbind` / `UnbindObject`）；
- **hover**：声明处应显示「delegate / event 声明」（`DefKind::Delegate` / `DefKind::Event`
  独立 kind），而不是展开出的 struct；
- **诊断**：类内写 `delegate` / `event` 不被预处理器识别（§1 的 Global 块条件），
  会以「意外的声明」形式落入后续解析——值得一条占位诊断码；
- **引用/定义**：`BindUFunction(this, n"Fn")` 的 `n"Fn"` 字符串实参指向 `UFUNCTION`
  名，是 references 的潜在关联点（按名索引天然可查，是否入引用计数另行决策）。

## 变更记录

| 版本 | 日期 | 内容 |
|---|---|---|
| v0.1 | 2026-09-29 | 初版：伪语法识别条件、ProcessDelegates 展开逐行还原、单播/多播全量对照、`_Inner` 解绑 API、反射映射、LSP 需求交付 |
