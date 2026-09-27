# LSP 实现规划（模块三/四 落地设计）

> 版本：v1.1（Q1-Q4 已裁决，D1-D25 定案；`FileId`/`Sym` 的 intern 实现见 [`实现优化.md`](实现优化.md)）
> 定位：把 [`架构设计.md`](架构设计.md) §4/§5/§6 的骨架细化到**可开工**粒度——crate 内部结构、
> 数据模型、流水线时序、里程碑与验收。实现前的最后一份设计文档，开工后转为进度跟踪。
>
> 前置（全部必读）：
> - [`架构设计.md`](架构设计.md)——总体架构、三阶段索引、查找链、路线图 P0-P6
> - [`架构设计-引擎内部语法.md`](架构设计-引擎内部语法.md)——`?` / `auto` 的语义层约定（TypeRef 必须区分）
> - [`诊断码表.md`](诊断码表.md)——码号唯一登记处（本文不发明码号）。诊断规则**按需设计**，
>   不预先规定 severity / 措辞 / range / quick-fix（D22）
> - [`实现优化.md`](实现优化.md)——基础设施优化技术（`FileId` 注册表、`Sym` intern），被本文 §3 引用
> - [`../grammar/README.md`](../grammar/README.md)——节点命名偏差、错误恢复约定、「语法接受 ≠ 语义合法」清单
>
> 本文不做的事：诊断引擎详细设计（P5，另立文档）、引擎在线通道（P6）。

## 1. 范围

| 在范围 | 不在范围 |
|---|---|
| `lsp/` Cargo workspace 四个 crate 的内部设计 | UE 导出插件（P1 已完成初版） |
| 冷启动 / 增量索引的精确时序 | 诊断规则逐条取证（P5） |
| 数据模型（ID 体系、符号、类型表） | DAP、资产补全、跳 C++ 源码（P6） |
| 查找链与重载解析的 API 形态 | 文法改动（grammar 已冻结验收） |
| 里程碑 M0-M6 拆分与验收标准 | — |

## 2. crate 结构与依赖图

```
lsp/                          # Cargo workspace（根 Cargo.toml 仅 workspace + 共享 profile）
└── crates/
    ├── as-syntax/            # tree-sitter 包装：build.rs 编译 ../../grammar
    ├── as-core/             # 索引/类型/查找链（纯库，无 IO、无 async，可全量单测）
    ├── as-lsp/              # server 壳：tower-lsp-server、增量同步、请求路由
    └── as-cli/              # dump-tree / dump-index（调试与验收工具）
```

依赖方向（严格单向，禁止环）：

```
as-lsp ──► as-core ──► as-syntax
as-cli ──► as-core
```

- **as-lsp 不直接触 tree-sitter 节点**：所有 CST 访问经 as-core 提供的辅助 API（`syntax.rs`），
  保证 server 壳薄、语义逻辑全部落在可单测的纯库里。
- **as-core 无 IO**：文件读取、配置解析在 as-lsp 完成，as-core 只接受「已读好的字符串 +
  虚拟 FileId」。单测因此不需要真实文件系统。

**命名裁决**：采用 `as-syntax`（架构设计 §4.1 原文），不采用 AGENTS.md 仓库结构快照里的
`tree-sitter-as`。理由：架构设计是权威文档；`as-syntax` 表意的是「本项目对语法层的包装」
而非「文法 crate 本身」（文法在 `grammar/`，不随 Cargo workspace 走）。

### 2.1 as-syntax

- `build.rs`：检查 `../../grammar/src/parser.c` 存在（不存在则报错并提示
  `cd grammar && npm install && npx tree-sitter generate`，沿用 grammar/README 约定）；
  `cc` 编译 `parser.c` + `scanner.c`，暴露 `tree_sitter_as()` language 指针。
- 公开 API（全部免 unsafe 细节）：

```rust
pub fn language() -> Language;
pub fn parse(src: &str, old_tree: Option<&Tree>) -> Tree;   // 增量 parse 入口
pub mod node;   // 节点种类常量（从 node-types.json 生成或手写对齐 grammar/README §偏差）
```

- 附带 `verify(src) -> Vec<ErrorNode>`——收集 `ERROR`/`MISSING` 节点，供 M0 验收与 P5 诊断
  的「解析错误」分类复用。

### 2.2 as-core 模块划分

```
as-core/src/
├── id.rs          # FileId / DefId / TypeId / Sym（newtype，全部 u32）
├── intern.rs      # Sym/TypeId intern + FileId 注册表（实现见实现优化.md 条目 1/2）
├── syntax.rs      # CST 访问辅助：声明种类判定、名字 span 提取、说明符解析
├── symbol.rs      # DefData / DefKind / 符号 arena
├── types.rs       # TypeKind / 类型表 / 模板实例化
├── expand.rs      # 声明展开：delegate/event 成员集、类隐含成员
├── index.rs       # WorkspaceIndex：三阶段流水线的数据产物与构建入口
├── resolve.rs     # 查找链（架构设计 §4.5 的 0-6 级，见 §7.1）
├── overload.rs    # 重载解析与排序（一等模块）
├── decl_tags.rs   # .d.as 注解标签解析与 tag/doc 分流（架构设计 §2.4.3/§2.4.4）
├── diag.rs        # enum DiagCode + 诊断结构（M6 建；码号只在诊断码表登记，§12 G5）
└── range.rs       # TextRange（字节）+ 行首偏移表；UTF-16 换算原语（移植 mylua 方案）
```

- **无 `manifest.rs`**：LSP 不读 `_manifest.dctx`（决策 D20，架构设计 §2.5）。
  `float_is_float64` 由配置项经 `IndexConfig` 传入。
- **无版本协商代码**：`@cache_format` 只被 `decl_tags.rs` 识别（防止混入 doc 文本），
  **其值不消费**（决策 D21）——`.d.as` 是源码非协议，且 tag 白名单规则本身版本无关。
  ⇒ as-core 只认一种输入：`.as`/`.d.as` 源码文本，且对其「版本」无感。

## 3. 数据模型（ID 体系）

四个 u32 索引贯穿全库，**相等即同一**，杜绝深比较：

| ID | arena | 含义 |
|---|---|---|
| `FileId` | 文件注册表 | 一个已加载的 `.as` / `.d.as` 文本 |
| `Sym` | 字符串 intern | 标识符 / 名字 |
| `DefId` | `Vec<DefData>` | 一个符号声明（见下） |
| `TypeId` | 类型表 | 一个归一化类型（intern，结构相等 → 同 id） |

> `FileId` / `Sym` 的 intern 实现模板（移植自 mylua）与适配改动见
> [`实现优化.md`](实现优化.md) 条目 1 / 2；`TypeId` 属类型系统本体（§3.3）。

### 3.1 DefId 的两类来源

1. **真实声明**：源码 / `.d.as` 里写出来的；
2. **合成声明**：模板实例化的克隆成员（`types.rs` 分配）、delegate/event 展开成员（`expand.rs`
   分配）、类隐含成员（`StaticClass()` 等）。

合成 DefId 的 `DefData::origin` 指回源头（委托声明 / 模板原成员），hover 与 definition
**落回源头声明**，合成符号只做查找链的中间产物。这是「跳转永远落在用户看得见的地方」的保证。

### 3.2 DefData 草案

```rust
struct DefData {
    name: Sym,
    kind: DefKind,
    file: FileId,
    name_span: TextRange,    // 名字 token（definition/rename/hover 的锚点）
    full_span: TextRange,    // 整个声明
    parent: Option<DefId>,   // 所属 class / namespace；顶层则为 Module
    origin: Option<DefId>,   // 合成符号 → 源头声明（D10）
    flags: DefFlags,         // 见下
    // kind 特化数据放 variant：Function 签名与形参、Property 类型、
    // Class 的 specifier 集合与 doc 注释等
}
```

**`DefKind` 全集**（对齐 grammar 的声明节点 + `.d.as` 形态，不留「以后再加」）：

| 分组 | 变体 |
|---|---|
| 类型 | `Class` / `Struct` / `Enum` / `EnumValue` / `Namespace` / `Module` |
| 类型别名式 | `Delegate` / `Event`（声明本体；展开出的成员另计，见 §3.1） |
| 可调用 | `Function` / `Method` / `Constructor` / `Destructor` / `Operator` |
| 数据 | `GlobalVar` / `Field` / `Param` / `LocalVar` / `TypeParam` / `AssetDecl` |
| 访问器 | `VirtualProperty`（`int X { get {...} }`，顶层与类内均可出现，`AS0005` 依赖它） |

- `Operator` 与 `Method` 分开：引擎内部语法 §1.5 要求对 `opCast`/`opImplCast` 等降权，
  补全排序需要按 kind 区分，事后用名字前缀判断不可靠。
- `Delegate`/`Event` 必须有独立 kind：hover 在声明处应显示「delegate 声明」，
  而不是展开出的 struct。

**`DefFlags`**（位标志，替代散落的 bool）：

```
CONST  PROTECTED  LOCAL  MIXIN  SYNTHETIC
EDITABLE        // .d.as @editable：仅 default 块可写（架构设计 §2.4.3）
NOT_PROPERTY    // .d.as @notProperty：非访问器（反向默认，§2.4.5）
NOT_CALLABLE    // .d.as @notCallable
UNNAMED_PARAM   // 形参名是 InArgN 占位（架构设计 §2.4.6），命名实参补全须跳过
```

- **重载组**：同名同作用域的多个可调用 DefId 天然共存，重载组是「查询时按 name+scope 聚合」
  的视图，不落库——避免维护组的成员增删同步。
- **属性访问器**（架构设计 §4.5 第 3 级）：`Get<X>`/`Set<X>` 命中时返回**属性访问器视图**
  （读写侧各自的签名），它是对既有 Function DefId 的包装查询，不是新符号。
  判定按**反向默认**：不带 `NOT_PROPERTY` 即为访问器候选。
- **doc 注释**：声明前连续 `//` 行挂在 DefData。`.d.as` 的 `@tag` 按架构设计 §2.4.4 分流——
  白名单 tag 进 `flags` / 特化数据，其余留在 doc 文本。

### 3.2.1 span 表示与位置换算（as-core 不依赖 LSP 类型）

- `TextRange` = `(start: u32, end: u32)` **字节偏移**，as-core 内部一律用它；
- `Position`（行 + UTF-16 列）换算只在 as-lsp 边界发生，实现落 `range.rs`；
- 换算依赖**每文件行首偏移表**（`Vec<u32>`）+ 行内 UTF-8→UTF-16 扫描。
  该表是 Phase 2 的持久产物（§4），否则每次换算要重扫整个文件（O(文件长度)）；
- 行首表随文件文本一起失效/重建（增量编辑时重算该文件）。

### 3.3 类型表

```rust
enum TypeKind {
    Named  { def: DefId, args: Vec<TypeId> },   // FVector / TArray<t_?>（模板实参已 intern）
    Array  (TypeId),                            // T[]
    Const  (TypeId),
    Ref    (TypeId, RefKind),                   // &in / &out / &
    Param  (u32),                               // 模板形参，按声明体内下标（仅 .d.as 模板体内）
    Wildcard,                                    // `?`（引擎内部语法 §1.4：与 Auto 严格区分）
    Auto,                                       // `auto` 声明侧推导占位（局部变量 / range-for）
}
```

- 基础类型（`void`/`int`/`float`…）**没有任何 `.d.as` 声明**（M1 语料实证，D25）——
  索引构建期注入为**合成 builtin DefId**（`SYNTHETIC`，不进声明统计），类型表仍统一走
  `Named`，不设特例——减少类型表分支。
- `floatIsFloat64`（**配置项**，架构设计 §5 / D20）：决定裸 `float` 归一化到
  `float64` 还是 `float32` 的 DefId，并影响**字面量表达式的推导结果**。
  它是索引构建的输入参数（`IndexConfig` 字段），**不是**类型表结构的一部分；
  改动该配置 ⇒ 视同全量重建声明索引（§5.3）。

**规范形式（canonical form，intern 的前置条件）**：`const T&` 既可表示为
`Const(Ref(T))` 也可表示为 `Ref(Const(T))`，两种结构不相等 ⇒ 同一类型拿到两个 TypeId，
「相等即同一」的不变量被破坏。因此强制：

```
修饰符嵌套顺序（由外到内）：Ref → Const → Array → Named / Param / Wildcard / Auto
```

- 所有 TypeId 只能经 `intern_type()` 构造，该函数内部重排修饰符到规范顺序并折叠重复
  （`Const(Const(T))` → `Const(T)`）；
- 直接 `TypeKind` 字面量构造类型表条目在 as-core 内部标记为私有，杜绝绕过；
- 单测：随机生成修饰符组合，断言「语义相同的两种写法 → 同一 TypeId」。

**`unresolved_object`**（D8）：不进 `TypeKind`，作为 `.d.as` 成员声明上的语法 flag 存在
`DefFlags` 之外的渲染信息里，类型本身按基类型 intern。

**模板实例化** `instantiate(DefId, Vec<TypeId>) -> TypeId`：
  - 缓存键即 `(def, args)`，TypeId 相等天然去重；
  - 成员克隆惰性：首次被成员查询触达才展开，展开结果缓存为合成 DefId 列表；
  - 递归保护：深度上限（32）+ 正在展开的 `(def, args)` 占位符，防 `TArray<TArray<…>>` 自指死循环；
  - `@outputTypeIndex` 方法**不**在实例化时处理：重载解析选定候选后从实参类型回填返回类型
    （架构设计 §4.3 原约定）。

## 4. 索引流水线（冷启动时序）

三阶段对齐架构设计 §4.2（其阶段 1-3 ≙ 本文 Phase 1-3），本文精确化各阶段的输入输出，
并额外显式列出 Phase 0 文件发现：

```
├────────── 冷启动构建（后台线程，一次性） ──────────┤ ├── 查询期（常驻） ──┤

Phase 0 收集       Phase 1 声明快扫            Phase 2 全量构建              Phase 3 惰性语义
─────────         ──────────────              ──────────────                ──────────────
scriptRoots ──►  rayon 并行 parse 全部文件 ──► 符号 arena / 成员表 /        请求驱动：
typeDeclDirs      （不做无函数体轻扫）          继承闭包 / 命名空间树 /        表达式类型推导
（纯文件发现）      提取全部声明级符号 ────────►  UseSite 记录                  模板实例化
                 主索引（全局名表）              （per-file 引用使用点）        重载排序缓存
```

**Phase 0 是纯文件发现，不含任何计算**——等价于 mylua 遍历 `.lua` 文件。单列编号只为
交代「441 文件」这个数量级的来源；实现上它就是一次 `walkdir`，结果直接喂给 Phase 1 的
rayon，**不需要独立调度、也不必单独成阶段**。它只有两件实质产出，且都必须在这一步定：

| 产出 | 为什么不能延后 |
|---|---|
| 文件**类别**标签（`.as` / `.d.as`） | 两类的失效粒度与允许构造不同：`.d.as` 整目录重建 + 防抖（§5.3），且独有 `?` / `unresolved_object` / 模板声明头（D19「交叉非子集」） |
| 文件**所属根** | 模块名 = 相对该根的路径按引擎 `FilenameToModuleName` 计算，决定 `local` 符号的可见域；多根 workspace 的同名冲突按「项目根优先于引擎根」消解（§5.2） |

**与 mylua 的对照（编号看着多，实质同构）**：

| mylua 的两步 | 本文编号 |
|---|---|
| 遍历 `.lua` 文件 | Phase 0 |
| ① 并行解析单文件 | Phase 1 |
| ② 合并进索引层 | Phase 2 |
| （查询期按需推导，未编号） | Phase 3 |

⇒ **真正的「构建」只有 Phase 1+2 两步**。Phase 3 不是冷启动的一个阶段，而是**查询层**
（请求驱动 + 按文件缓存），画在同一张图里只为展示数据流终点——故 §6 的冷启动后台线程
职责写的是 `Phase 0-2`，不含 Phase 3。

> **编号不重排**：D4 / D5 的标题即「Phase 1『声明快扫』= 全量 parse」
> 「UseSite 解析惰性化（Phase 3）」，架构设计 §4.2 亦有「阶段 1-3 ≙ 规划的 Phase 1-3」
> 的对齐声明。按文档治理规则 1（决策条目不改写），编号保持现状，此处仅澄清定性。

**Phase 1 即全量 parse，不设独立轻扫描**——441 文件（27 `.as` + 414 `.d.as`）tree-sitter
并行 parse 是秒级开销，为省这点时间维护「两套扫描口径」不值。快扫的产出是**主索引**
（全局名表：`name -> Vec<DefId>`），它使 Phase 2 构建成员表时不存在「依赖类型还没见到」的
时序问题（这正是旧 LSP 四阶段队列要解决的问题，架构设计 §4.2 已论证）。

**Phase 2 构建内容**（仍 rayon 并行，按文件切分、合并冲突极小）：

| 产物 | 说明 |
|---|---|
| 符号 arena + 成员表 | `class C : P` 成员查找沿 supertype 链 = parent 的成员表串联 |
| 继承闭包 | 预计算每个 **class** 的祖先链（含环检测——错误源码可能出现环，报诊断不 panic）。**struct 不建**：`.d.as` 的 struct 无父类且 C++ 继承已展平，成员查找单层（架构设计 §4.5 第 2 级） |
| 命名空间树 | 逐级嵌套关系，供查找链第 5 级回退 |
| **mixin 倒排** | `首参类型 DefId -> Vec<DefId>`（mixin 函数）。供查找链第 4 级 fallback 使用（准入条件见架构设计 §4.5.1）。**键不预展开到子类**（D23）：引擎判定是 `DerivesOrShadows(首参类型)`，查 `C` 时须覆盖「首参为 `C` 或 `C` 任一祖先」的全部 mixin ⇒ **查询时沿继承闭包逐级查倒排**（闭包已有，省内存且与引擎语义一致）。首参类型未解析的 mixin 暂挂「待定」桶，不阻塞构建。**C++ `ScriptMixin` 库函数不进此表**（导出时已是成员方法，架构设计 §4.5.2） |
| **module 归属表** | `FileId -> Sym`（模块名，按引擎 `FilenameToModuleName`）+ `local` 符号集合，供模块隔离过滤 |
| **行首偏移表** | 每文件 `Vec<u32>`，UTF-16 位置换算的前置（§3.2.1） |
| UseSite 记录 | 每文件所有「标识符使用点」：`(name, span, 语法角色)`，**不做解析** |
| delegate/event 展开 | `expand.rs` 按架构设计 §4.4 规则表生成合成成员 |
| 引用倒排 | `name -> 出现该名字的文件集合`（references 的候选集剪枝） |

**Phase 3 惰性语义**：UseSite → DefId 的解析、表达式类型、模板成员展开、重载排序，全部
请求驱动 + 缓存。缓存失效粒度 = 文件（见 §5）。

### 4.1 为什么 UseSite 解析放 Phase 3（关键取舍）

引用解析需要作用域上下文（局部变量遮蔽、类成员、命名空间），在 Phase 2 做等于全工作区
表达式级分析——冷启动变慢且编辑失效面大。放 Phase 3 后：

- `references(DefId)` 只需解析「引用倒排给出的候选文件集」的 UseSite，逐文件缓存；
- 中途编辑只失效一个文件的解析缓存；
- 代价：首次 references 稍慢（可 `$/progress`），后续命中缓存。

### 4.2 表达式定型管线（Phase 3 内核）

`auto`、hover、成员补全、重载排序全部依赖同一个原语：**表达式定型**。

```rust
pub fn expr_type(idx: &Index, file: FileId, expr: Node) -> Option<TypeId>;   // None = 推导失败
```

- **自底向上**：子表达式类型先定（per-file 缓存，随编辑失效），父表达式查成员表 +
  重载解析得返回类型；链式访问逐段进行（`this.GetActorTransform().GetLocation()`
  = `this` 定型 → 成员访问 → 再成员访问）；
- **`this` 定型**：类/struct 方法体内 = 所在类/struct；静态与命名空间函数中不出现（引擎规则）；
- **`auto` 局部变量**：声明定型时取 `expr_type(初始化表达式)`，与 `?`（形参位匹配任意）
  是完全不同的角色（引擎内部语法 §1.4/§2）；
- **range-for 协议**（引擎真值 `[ENGINE]as_compiler.cpp:5745-5873`，新迭代器模式 = 默认）：
  迭代元素类型 = `容器类型.Iterator()` 返回类型的 `.Iterate()` 返回类型（保留引用性）；
  `Iterator()` 取 0 参、返回对象、constness 匹配者（无匹配回退 const 迭代器）；
  两跳都可能触发模板成员实例化。旧迭代器模式（`UseNewIterators=0`）不建模；
- **失败降级（宁缺毋假）**：表达式含 ERROR 节点、符号未解析、类型未知 → 返回 `None`；
  `auto` 变量 hover 原样显示声明、成员补全不给出——强类型语言用户预期精确，
  错误补全比没有补全伤害大。

**`auto` 的解析时机——扫描阶段完全不碰表达式定型**。因为 auto 只允许出现在局部变量与
range-for 迭代器（class/struct 字段、形参、返回值、全局变量一律显式），持久索引中
auto 只是**占位记录**（`TypeKind::Auto`），不存在阻塞索引构建的待解项。需要解析的
场景全部请求驱动：

| 场景 | 文件范围 |
|---|---|
| inlay hints（展示推导类型，如 `auto location = ...` 旁显示 `: FVector`） | 打开的文件 |
| hover / 成员补全（auto 变量及其使用点） | 打开的文件 |
| references / rename 的重载消歧（引用链穿过 auto 局部变量时） | **可能是未打开文件**——按需惰性解析 + 缓存，仍属 Phase 3 |
| 诊断（P5，类型不匹配等） | 打开的文件 |

第二个红利：auto 推导是**函数体局部的自包含分析**——初始化表达式就在声明处，
无需流敏感 / 跨函数传播，只读已建好的成员表。

## 5. 文本来源、增量更新与文件生命周期

### 5.1 overlay：文本的唯一真值来源

as-core 只接受「已读好的字符串 + FileId」（§2），**谁提供字符串必须有唯一规则**，
否则未保存的编辑会被磁盘内容覆盖：

| 状态 | 文本来源 |
|---|---|
| `didOpen` 之后 | **overlay**（客户端发来的内存文本），版本号随 `didChange` 递增 |
| `didClose` 之后 | 丢弃 overlay，回落磁盘内容并重读一次（客户端可能放弃了未保存修改） |
| 从未打开 | 磁盘 |

- overlay 表在 as-lsp 侧（`HashMap<FileId, (version, String)>`），as-core 不感知其存在；
- `didSave` **不触发**重读（磁盘内容此刻必然等于 overlay）；
- 文件监视事件若命中有 overlay 的文件 → **忽略**（overlay 优先），避免外部工具改盘引发抖动。

### 5.2 编辑时序

```
didChange（增量，含 old_tree）
  └─ 单文件增量 parse（毫秒级，请求线程内同步完成）
       ├─ 声明级 diff：增/删/改的 DefId → 更新主索引、成员表、继承闭包（局部重算）
       ├─ 该文件 UseSite / 行首偏移表 / mixin 倒排条目重建
       ├─ Phase3 该文件全部缓存失效
       └─ 粗粒度联动失效：若类成员集合变化 → 全局「成员访问解析缓存」失效
            （先正确后优化；441 文件量级重算大概率无感，性能不达标再收敛为
             「依赖该类的文件集」精确失效）
```

多根 workspace：每根独立收集，主索引合并时同名冲突按「项目根优先于引擎根」消解
（即配置顺序，架构设计 §4.2）。

### 5.3 文件增删改名（`DidChangeWatchedFiles`）

| 事件 | 处理 |
|---|---|
| `.as` 新增 | 读盘 → 单文件 Phase 1+2 → 并入主索引 |
| `.as` 删除 | 该 FileId 的全部 DefId 标记失效、从主索引/引用倒排/mixin 倒排/module 表摘除 |
| `.as` 改名 | = 删除 + 新增。**注意模块名随路径变**（`FilenameToModuleName`），`local` 符号的可见域随之改变，不能简单改 FileId 的路径字段 |
| `.d.as` 任一变化 | 视为整个 `typeDeclarationDirs` 失效 → **全量重建声明索引**（不做增量，理由见下，D24） |
| `_manifest.dctx` 变化 | **忽略**（不在监视范围内） |
| `floatIsFloat64` 配置变更（`didChangeConfiguration`） | 同样**全量重建声明索引**——它改变裸 `float` 的归一化目标（§3.3） |

**为什么 `.d.as` 不做增量**（D24，修正原「不读 manifest ⇒ 无从判断增量」的错误论证）：
判断增量本不需要 manifest——`DidChangeWatchedFiles` 自带变更文件列表。真实理由有两条：

1. **增量路径几乎永不触发**：导出器执行时先 `DeleteDirectory` 清空目录再重写全部 414 个文件
   （`TypeDeclarationExporter.cpp:330`）⇒ 变更集恒等于全集，增量代码写了也用不上；
   且导出是**偶尔**的人工动作，不在热路径上。
2. **全量重建规避了一整类 bug**：`.d.as` 逐文件摘除要同时摘净继承闭包、mixin 倒排、
   引用倒排、module 归属表，漏一处即产生幽灵符号；而 414 文件重建是秒级（§4）。

⇒ 少一条代码路径，正确性不打折。**真正无可替代的全量触发源只剩 `floatIsFloat64`
配置变更**——它是 `IndexConfig` 输入参数，与文件是否变动无关。

**`.d.as` 事件必须防抖**：清空+重写会让客户端瞬间推来上千条删除+新增事件。
约定：收到 `typeDeclarationDirs` 下的任何事件 → 启动 **500ms 静默窗口**计时器，
窗口内的后续事件只重置计时器；**另设 5s 硬上限**——累计等待超时即强制执行一次重建
（防慢盘下写盘持续数秒、计时器被无限推迟导致重建永不发生；若其后仍有事件，再走一轮）。

**防抖不承担正确性职责**：重建在后台线程构造**新快照**、建好再 `arc-swap` 原子换根
（§6），旧快照全程可服务 ⇒ 「引擎类型瞬间全空」的中间态从根上不存在。
防抖的唯一价值是**避免重复做功**（别为上千条事件跑上千次重建），
因此正确性不依赖 500ms / 5s 这两个时间参数调得准不准。

**FileId 墓碑语义**：`FileId` 注册表是 append-only（[`实现优化.md`](实现优化.md) §1），
删除文件**不回收 id**，只在 `FileMeta` 上打 `alive = false`：

- 已发出的 DefId/UseSite 里的 FileId 引用不会变成悬垂索引；
- 同路径文件重新出现 → 复用原 FileId 并翻回 `alive = true`（路径 intern 表命中）；
- 所有遍历型查询（workspaceSymbol、references 候选集）必须过滤 `!alive` 的 FileId。

## 6. 并发模型

沿用 mylua 已验证的模式，**请求处理单线程，重活后台 + 快照换根**：

| 线程/任务 | 职责 |
|---|---|
| LSP 主循环（单线程） | 全部请求处理；单文件编辑同步在请求线程做（毫秒级） |
| 冷启动后台线程 | Phase 0-2；完成后 `arc-swap` 发布索引快照；期间请求返回空结果或等待（配置项控制） |
| rayon 线程池 | Phase 1/2 并行 parse 与构建 |

- as-core 全部数据结构按「快照语义」设计（arena + u32 索引天然适配）；编辑走 copy-on-write，
  粒度 = 文件。
- 不做跨请求并发语义分析——LSP 客户端串行发请求的场景占绝对多数，先简单后正确。

### 6.1 冷启动期间的编辑不得丢失

冷启动（Phase 0-2）在后台跑，期间客户端已经可以 `didOpen` / `didChange`——
若这些编辑直接作用于「还不存在的索引」，快照一发布就把它们覆盖了。约定：

```
状态机：Loading ──(快照发布)──► Ready

Loading 期间：
  didOpen/didChange → 照常更新 overlay（overlay 独立于索引，永不丢）
                    → 把 FileId 记入 pending_dirty 集合
  语义类请求        → 按配置：返回空结果（默认）或挂起等待 Ready
  非语义请求        → documentSymbol / folding / semanticTokens 可立即服务
                       （只需单文件 CST，不依赖索引）

发布瞬间（单线程内原子完成）：
  arc-swap 换根 → 立刻用 overlay 文本重放 pending_dirty 的每个文件（§5.2 单文件路径）
                → 清空 pending_dirty → 置 Ready
```

- 关键点：**overlay 是索引之外的独立存储**，因此「编辑不丢」只需保证发布后重放一次；
- 重放代价 = 打开文件数（个位数），毫秒级；
- 冷启动本身**读盘时也用 overlay 优先**（§5.1），所以多数情况重放是幂等的空操作。

## 7. 查找链与重载解析（as-core 对外 API）

### 7.1 查找链（架构设计 §4.5 原文落地）

```rust
/// 在 offset 处解析一个名字，返回可见性排序后的候选
pub fn resolve_name(idx: &Index, file: FileId, at: u32, name: Sym) -> Vec<DefId>;
// 内部顺序（架构设计 §4.5 的 0-6 级）：
//   this/super → 局部变量(作用域链上溯) → 类成员(class 沿继承闭包 / struct 单层)
//   → 属性访问器(不带 NOT_PROPERTY 即候选) → mixin fallback → 命名空间链(逐级回退)
//   → 全局/类型本身
// 约束：local 函数按 module 归属表过滤；同名 type/namespace 按语境择一
```

参数是**字节偏移**而非 `Pos`——as-core 不引入行列概念（§3.2.1）。

**mixin 级是短路 fallback，不是候选合并**（架构设计 §4.5.1，引擎
`as_compiler.cpp:13387-13450`）：第 2/3 级只要返回非空就**直接跳过**第 4 级，
mixin 不与真实成员一起进 `resolve_overload`。四条额外守卫：

```rust
// 仅当以下全部成立才查 mixin 倒排：
//   ① 前序级别结果为空
//   ② 有对象上下文（显式 recv 或所在类方法体的 this）  ← 全局函数体内不查
//   ③ 调用点无 scope 限定                              ← NS::Foo(obj) 不查
//   ④ 首参类型经 DerivesOrShadows 匹配接收者（沿继承闭包查倒排）
// 命中后 definition/hover 落回 mixin 函数声明本身（真实全局函数，非合成符号）
```

### 7.2 重载解析（一等模块，P3 阶段即完成骨架）

```rust
pub struct Ranked { pub def: DefId, pub score: OverloadScore }  // 排序键：精确 > 隐式转换 > 通配
pub fn resolve_overload(cands: &[DefId], args: &[TypeId]) -> Vec<Ranked>;
```

| 消费方 | 用法 |
|---|---|
| signatureHelp | 排序决定签名列表顺序与**激活项** |
| completion | `X.Foo(` 后缀过滤 + 排序 |
| references | 重载消歧；**消歧失败报全部重载**（架构设计 §4.6 约定） |
| hover | 选中候选后渲染签名（含 `@outputTypeIndex` 回填的返回类型） |

`?` 通配形参：匹配任意实参、**排序最低**（引擎内部语法 §1.4——避免 `FString.opAdd(?)`
遮蔽 `opAdd(FString)`）。隐式转换表（`opImplConv`、`TSubclassOf→UClass` 等）第一期只建
补全/排序所需子集，完整表留给 P5 诊断（架构设计 §4.3）。

## 8. as-lsp：能力声明与请求路由

- 框架：**tower-lsp-server**（对齐 mylua，规避自研协议层的坑）。
- `textDocumentSync`: Incremental。UTF-16 `Position` ↔ 字节偏移的换算**只在此层发生**
  （经 `range.rs` 原语 + 行首偏移表，§3.2.1），进入 as-core 的一律是字节偏移。
- overlay 表（`FileId -> (version, String)`）归本层持有，规则见 §5.1。
- 能力按里程碑逐步放开（见 §9），未实现的请求不进 capabilities，避免客户端缓存空结果。
- 语言 id：`angelscript-asl`（架构设计 §5，避开与 Hazelight 扩展冲突）。

### 8.1 请求路由表（第一阶段全集 → 里程碑映射）

| LSP 方法 | 数据来源 | 里程碑 |
|---|---|---|
| `documentSymbol` / `foldingRange` | CST 直映射 | M2 |
| `semanticTokens` | CST 映射，legend 对齐旧扩展（`as_typename` 等约 20 类） | M2 |
| `hover` | 查找链 + DefData 签名渲染 + doc | M3 |
| `definition` / `implementation` | 查找链命中（`.d.as` 声明是合法落点） | M3 |
| `references` / `rename` | UseSite 解析缓存 + 重载消歧 | M4 |
| `workspaceSymbol` | 主索引 | M4 |
| `completion` | 查找链上下文 + 成员 + UFUNCTION 名单（`AddUFunction(this, n"\|`）+ 说明符 schema + 命名参数 | M5 |
| `signatureHelp` | 重载集排序 | M5 |
| `inlayHint` | auto 变量推导类型展示（依赖 §4.2 表达式定型） | M5 |
| `publishDiagnostics` | M6 起步：仅 `AS09xx` 工具链诊断（`AS0902` / `AS0903`）；P5 全量 | M6 |
| `didChangeConfiguration` | `floatIsFloat64` 等索引级配置变更 → 全量重建（§5.3） | M2 |

## 9. 里程碑与验收

每个里程碑**可独立演示**；M2 起每个里程碑都有 VSCode 体感验收（扩展最小版随 M2 就位）。

| # | 内容 | 验收标准 |
|---|---|---|
| M0 | Cargo workspace + as-syntax + `dump-tree` | Demo_AS/Script 27 文件 + 414 `.d.as` dump 零 ERROR（复用 grammar 验收口径，口径一致才说明包装层无损） |
| M1 | as-core 三阶段 + `dump-index` + tag 解析 | ① **数量对账**：`dump-index` 的类型数/成员数 与 `_manifest.dctx` 的 `type_count=14864` / `member_count=69337` **人工比对**（该文件仅作开发期参照，运行时不读——D20）；差异须逐条解释（如 20 个被覆盖的 group，风险 7）。② 15 个语义 tag 全部解析，含 4 个语料零出现项的内置单测（风险 8）。③ 继承闭包环检测用例；struct 不建闭包的断言。④ `floatIsFloat64` 两种取值下 `FVector.X` 分别定型为 `float64` / `float32` 的用例 |
| M2 | as-lsp 壳 + documentSymbol/semanticTokens/folding + **VSCode 扩展最小版** | Demo_AS 打开真实体感；semanticTokens 与 Hazelight 扩展同文件截图对照 |
| M3 | 查找链 + hover/definition | as-core 内置单测覆盖查找链 0-6 级命中序（§10）。四类专项用例：`super`、struct 单层、访问器反向默认、**mixin 五条准入条件**（真实成员优先于 mixin 而短路 / 全局函数体内不命中 / `NS::Foo(obj)` 不命中 / 父命名空间的 mixin 可命中 / 首参为接收者祖先类时命中）；as-cli 对语料批量 dump-index 校验 |
| M4 | references/rename/workspaceSymbol + 重载消歧 | 重载函数引用消歧用例（成功/失败双路径）；`$/progress` 长任务；文件增删改名后索引一致性用例（§5.3） |
| M5 | completion/signatureHelp/inlayHint | `X.` 成员补全、`n"\|` UFUNCTION 名单、`UCLASS(` 说明符补全、命名参数补全四类截图验收 + `auto` 变量 inlay 类型展示；**命名实参补全跳过 `InArgN` 占位**（架构设计 §2.4.6） |
| M6 | 诊断起步 | `AS0902` / `AS0903` 生效（`AS09xx` 段其余码号均 retired：`AS0901`/`AS0905` 见 D20，`AS0906` 见 D21）。**M6 只做诊断框架**（`enum DiagCode` + 发布管道 + 抑制注释解析），具体规则进 P5 按需设计（D22） |

对应架构设计 §6：M0-M2 ≈ P3 前半，M3-M5 ≈ P3 后半 + P4，M6 衔接 P5。
扩展（模块四）在 M2 提前就位最小版（languageId + client + 配置项骨架），后续里程碑增量加命令。

## 10. 测试策略

**硬性规则（已写入 AGENTS.md，决策记录 D1）**：Rust 单元测试中的 `.as` 测试代码一律
**内置于 Rust 源码**（字符串字面量），禁止读取 `tests/` 等外部文件——与 mylua 一致，
保证单测封闭可复现。

| 层 | 手段 |
|---|---|
| as-core | 纯库单测：查找链命中序（生成代码图断言）、模板实例化属性测试（随机嵌套组合，架构设计 §7）、继承环 / 缺失父类等**坏源码**容错——用例源码全部内置 |
| as-lsp | tower-lsp-server 测试客户端集成测试（mylua 460+ 条的经验直接移植模式）；用例源码同样内置 |
| 取材参考 | `Demo_AS/Script/Script-Examples`（官方四类示例：Examples/Editor/EnhancedInput/GAS）与 `Saved/AS-Cache`（414 个 `.d.as`）——编写内置用例时从中提炼真实形态（决策记录 D1） |
| 语料批量 | **不进单测**：as-cli 对上述目录 + 引擎插件 `Angelscript/**.as` 做 dump-tree / dump-index 批量校验（开发期验收动作；`Saved/` 可被引擎清空，不构成稳定依赖） |
| 体感验收 | 每里程碑在 VSCode 扩展中人工验收（§9） |

## 11. 开放问题（已全部裁决）

定案详情见 [`实现决策记录.md`](实现决策记录.md)，本节保留编号索引：

| # | 原问题 | 裁决 |
|---|---|---|
| Q1 | 旧 LSP golden 采集 | **放弃 golden 对账**（用户拍板）；验收三支柱 = 内置单测 + as-cli 语料批量 + VSCode 体感 |
| Q2 | preproc `#if EDITOR` 可见性 | 第一期全可见（D7） |
| Q3 | `unresolved_object` 语义 | 语法层 flag，语义按基类型，渲染原样；引擎取证与完整约定见引擎内部语法 §3（D8） |
| Q4 | foldingRange 是否进 M2 | 进，纯 CST 零成本（D9） |

## 12. 风险

| # | 风险 | 对策 |
|---|---|---|
| G1 | tree-sitter 生成物不入库 → clone/CI 直接 build 失败 | as-syntax build.rs 友好报错（§2.1）；README 写明前置命令 |
| G2 | （已消除）golden 依赖引擎在线 | 已裁决放弃 golden（决策记录 D2），验收三支柱见 §10 |
| G3 | 模板实例化缓存膨胀 | TypeId intern 天然去重 + 深度上限 32；实测超限再加 LRU |
| G4 | 增量联动失效粗粒度（类成员变更 → 全部成员访问缓存失效） | 441 文件量级预期无感；先正确后优化 |
| G5 | M0-M5 实现期「顺手加诊断」→ 散落的无 code 报错、无测试、无法抑制 | 保留码表两条轻量纪律：**先占号、统一走 `enum DiagCode`**。想现在报就正式占号 + 配最小复现单测；不想现在做就把素材记进码表 §2 末段。D22 放开的是「必须复刻引擎」的约束，**不是**「可以散落字符串字面量」 |
| G6 | `.d.as` 数据源本身有缺陷（20 个 group 被覆盖、4 个 tag 零语料） | 架构设计 §8 风险 7/8 已登记。**LSP 既不修也不检出** group 覆盖（D20 起不读 manifest，无数据源）——该缺陷在导出器侧修；零语料 tag 形态用内置单测覆盖 |
| G7 | TypeId 规范形式被绕过 → intern 去重失效 | 构造入口私有化 + 随机组合单测（§3.3）；这是「相等即同一」不变量的唯一保护 |
| G8 | 冷启动期编辑丢失 / overlay 与磁盘打架 | §5.1 overlay 唯一真值 + §6.1 发布后重放 `pending_dirty`；两者都需集成测试覆盖（先 `didOpen`+`didChange` 再等 Ready，断言索引含新符号） |
| G9 | `floatIsFloat64` 配错 → 全局类型宽度偏差且**无报错** | 架构设计 §8 风险 11；默认值对齐引擎默认、状态栏常显生效值、M1 双取值用例 |

## 13. 变更记录

| 版本 | 内容 |
|---|
| v1.1 | **基础类型 DefId 来源修正**（D25，M1 实现期语料取证）：§3.3 「基础类型在 `.d.as` 中有真实 DefId」与实际导出物不符（全语料零基础类型声明）——改为「注入合成 builtin DefId（SYNTHETIC，不进声明统计），类型表仍统一走 Named 不设特例；裸 float 按 IndexConfig 归一化」。设计意图（统一 Named、不设特例）不变 |---|
| v1.0 | **`.d.as` 全量重建的理由重写 + 防抖定性**（D24）：§5.3 结论不变（仍全量），但换掉原「不读 manifest ⇒ 无从判断增量」的错误论证，改为「导出恒清空重写 ⇒ 变更集恒等全集 + 偶尔触发」与「全量规避逐文件摘除的幽灵符号 bug」两条；明确唯一不可替代的全量触发源是 `floatIsFloat64` 配置变更；防抖补 **5s 硬上限**，并定性为「只避免重复做功、不承担正确性」（正确性由 `arc-swap` 换根保证）。**§4 流水线图与定性澄清**：图上标出「冷启动构建（Phase 1+2）/ 查询期（Phase 3）」分界；新增 Phase 0 定性段（纯文件发现、等价 mylua 的文件遍历、无需独立调度）+ 其两件实质产出表（文件类别标签、所属根 ⇒ 模块名与 `local` 可见域）+ 与 mylua 两步流程的对照表；注明 Phase 编号**不重排**的理由（D4/D5 标题与架构设计 §4.2 对齐声明已引用该编号，治理规则 1）。**开工前终检修订**：头部版本号补齐至 v1.0 / D1-D24（治理规则 5）；§2.2 `resolve.rs` 注释「五级链」改为「0-6 级」（与 §7.1 对齐）、补登记 `diag.rs`（M6 建，G5 要求）；§4 Phase 2 表 mixin 倒排改为「键**不**预展开到子类 + 查询时沿闭包逐级查」（原文「键要展开到子类」与同格后半句及 D23 裁决自相矛盾）；§3.3 模板实例化段的列表层级断裂修正；§12 G9 归位至 G8 之后；本表统一为倒序 |
| v0.9 | **mixin 语义精确化**（D23）：§4 mixin 倒排产物补「沿继承闭包查（`DerivesOrShadows`）」与「C++ `ScriptMixin` 不入表」；§7.1 明确 mixin 级是**短路 fallback** 而非候选合并，列出四条守卫；§9 M3 验收改为 mixin 五条准入条件专项用例 |
| v0.8 | **诊断按需设计**（D22）：前置索引说明码表新定位；§9 M6 收窄为「只做诊断框架」（`enum DiagCode` + 发布管道 + 抑制注释），具体规则进 P5；§12 G5 重述为「防散落无 code 报错」而非「防发明码号」 |
| v0.7 | **不做版本协商**（D21）：§2.2 明确 `@cache_format` 识别但不消费、as-core 对 `.d.as`「版本」无感；§8.1 与 §9 M6 的诊断码更新（`AS0906` retired） |
| v0.6 | **LSP 不读 manifest**（D20）：§2.2 删除 `manifest.rs`（as-core 只认源码文本，格式版本走 `decl_tags.rs` 的 `@cache_format`）；§3.3 `floatIsFloat64` 改为 `IndexConfig` 输入；§5.3 新增配置变更与 manifest 忽略两行；§8.1 新增 `didChangeConfiguration`；§9 M1 改为人工对账 + 新增浮点双取值用例、M6 诊断码更新；§12 修订 G6、新增 G9 |
| v0.5 | 数据模型与生命周期补全（决策 D15-D18）：§3.2 `DefKind` 给出全集 + 新增 `DefFlags`、新增 §3.2.1（`TextRange` 字节偏移 + 行首偏移表）；§3.3 新增类型**规范形式**约束；§4 Phase 2 产物补 mixin 倒排 / module 归属表 / 行首偏移表，继承闭包限定为 class；§5 重写为「overlay + 编辑时序 + 文件增删改名（含 `.d.as` 防抖与 FileId 墓碑）」；新增 §6.1 冷启动编辑重放；§7.1 查找链改为 0-6 级、参数改字节偏移；§9 M1-M6 验收按新真值收紧；§12 新增 G6-G8 |
| v0.4 | D14 落地：新增 §4.2 表达式定型管线（`auto` 推导链、range-for 双跳协议）；§8.1 请求路由表与 §9 M5 新增 `inlayHint` |
| v0.3 | 挂接 [`实现优化.md`](实现优化.md)：§3 引用 `FileId`/`Sym` intern 实现模板（决策记录 D12/D13） |
| v0.2 | 测试策略定稿：单测源码内置（AGENTS.md 硬性规则）；golden 对账取消（决策记录 D2）；Q1-Q4 全部裁决，§11 改为索引 |
| v0.1 | 首版：crate 内部结构、ID 体系与数据模型、三阶段流水线精确时序（Phase1 即全量 parse 的裁决）、增量失效策略、并发模型、重载解析 API、M0-M6 里程碑与验收、开放问题 Q1-Q4 |
