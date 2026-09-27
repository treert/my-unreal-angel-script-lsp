# LSP 实现规划（模块三/四 落地设计）

> 版本：v0.3（Q1-Q4 已裁决；`FileId`/`Sym` 的 intern 实现见 [`实现优化.md`](实现优化.md)）
> 定位：把 [`架构设计.md`](架构设计.md) §4/§5/§6 的骨架细化到**可开工**粒度——crate 内部结构、
> 数据模型、流水线时序、里程碑与验收。实现前的最后一份设计文档，开工后转为进度跟踪。
>
> 前置（全部必读）：
> - [`架构设计.md`](架构设计.md)——总体架构、三阶段索引、查找链、路线图 P0-P6
> - [`架构设计-引擎内部语法.md`](架构设计-引擎内部语法.md)——`?` / `auto` 的语义层约定（TypeRef 必须区分）
> - [`诊断码表.md`](诊断码表.md)——诊断码唯一分配处，本文不发明码号
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
├── resolve.rs     # 查找链（架构设计 §4.5 五级链）
├── overload.rs    # 重载解析与排序（一等模块）
└── range.rs       # 字节偏移 ↔ LSP 位置（UTF-16）换算（移植 mylua 成熟方案）
```

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
    kind: DefKind,           // Class | Struct | Enum | EnumValue | Function | Property
                             // | Param | TypeParam | Namespace | Module | AssetDecl
                             // | MixinFunction | Synthetic(...)   // 展开产物
    file: FileId,
    name_range: Range,       // 名字 token（definition/rename/hover 的锚点）
    full_range: Range,        // 整个声明
    parent: Option<DefId>,   // 所属 class / namespace；顶层则为 Module
    origin: Option<DefId>,   // 合成符号 → 源头声明
    // kind 特化数据放 variant：Function 重载组、Property 类型与访问器、
    // Class 的 specifier 集合与 doc 注释等
}
```

- **重载组**：同名同作用域的多个 `Function` DefId 天然共存，重载组是「查询时按 name+scope 聚合」
  的视图，不落库——避免维护组的成员增删同步。
- **属性访问器**（架构设计 §4.5 第 3 级）：`Get<X>`/`Set<X>` 命中时返回**属性访问器视图**
  （读写侧各自的签名），它是对既有 Function DefId 的包装查询，不是新符号。
- **doc 注释**：声明前连续 `//` 行（含 `.d.as` 的 `@tag` 注解）挂在 DefData，hover 直接消费。

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

- 基础类型（`void`/`int`/`float`…）在 `.d.as` 中有真实 DefId，统一走 `Named`，不设特例——
  减少类型表分支。
- `floatIsFloat64`（manifest 设置）：只影响**字面量表达式的推导结果**，不影响类型表本身。
- **模板实例化** `instantiate(DefId, Vec<TypeId>) -> TypeId`：
  - 缓存键即 `(def, args)`，TypeId 相等天然去重；
  - 成员克隆惰性：首次被成员查询触达才展开，展开结果缓存为合成 DefId 列表；
  - 递归保护：深度上限（32）+ 正在展开的 `(def, args)` 占位符，防 `TArray<TArray<…>>` 自指死循环；
  - `@outputTypeIndex` 方法**不**在实例化时处理：重载解析选定候选后从实参类型回填返回类型
    （架构设计 §4.3 原约定）。

## 4. 索引流水线（冷启动时序）

三阶段对齐架构设计 §4.2，本文精确化各阶段的输入输出：

```
Phase 0 收集       Phase 1 声明快扫            Phase 2 全量构建              Phase 3 惰性语义
─────────         ──────────────              ──────────────                ──────────────
scriptRoots ──►  rayon 并行 parse 全部文件 ──► 符号 arena / 成员表 /        请求驱动：
typeDeclDirs      （不做无函数体轻扫）          继承闭包 / 命名空间树 /        表达式类型推导
                 提取全部声明级符号 ────────►  UseSite 记录                  模板实例化
                 主索引（全局名表）              （per-file 引用使用点）        重载排序缓存
```

**Phase 1 即全量 parse，不设独立轻扫描**——441 文件（27 `.as` + 414 `.d.as`）tree-sitter
并行 parse 是秒级开销，为省这点时间维护「两套扫描口径」不值。快扫的产出是**主索引**
（全局名表：`name -> Vec<DefId>`），它使 Phase 2 构建成员表时不存在「依赖类型还没见到」的
时序问题（这正是旧 LSP 四阶段队列要解决的问题，架构设计 §4.2 已论证）。

**Phase 2 构建内容**（仍 rayon 并行，按文件切分、合并冲突极小）：

| 产物 | 说明 |
|---|---|
| 符号 arena + 成员表 | `class C : P` 成员查找沿 supertype 链 = parent 的成员表串联 |
| 继承闭包 | 预计算每个类的祖先链（含环检测——错误源码可能出现环，报诊断不 panic） |
| 命名空间树 | 逐级嵌套关系，供查找链第 4 级回退 |
| UseSite 记录 | 每文件所有「标识符使用点」：`(name, range, 语法角色)`，**不做解析** |
| delegate/event 展开 | `expand.rs` 按 §4.4 规则表生成合成成员 |
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

## 5. 增量更新时序

```
didChange（增量，含 old_tree）
  └─ 单文件增量 parse（毫秒级，请求线程内同步完成）
       ├─ 声明级 diff：增/删/改的 DefId → 更新主索引、成员表、继承闭包（局部重算）
       ├─ 该文件 UseSite 重建 + Phase3 该文件全部缓存失效
       └─ 粗粒度联动失效：若类成员集合变化 → 全局「成员访问解析缓存」失效
            （先正确后优化；441 文件量级重算大概率无感，性能不达标再收敛为
             「依赖该类的文件集」精确失效）
```

- `DidChangeWatchedFiles`：`.d.as` 重导出（`Saved/AS-Cache` 变化）→ 校验 manifest 指纹
  （`AS0901`，诊断码表 §7）→ 全量重建声明索引。
- 多根 workspace：每根独立收集，主索引合并时同名冲突按「项目根优先于引擎根」消解
  （即配置顺序，架构设计 §4.2）。

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

## 7. 查找链与重载解析（as-core 对外 API）

### 7.1 查找链（架构设计 §4.5 原文落地）

```rust
/// 在 position 处解析一个名字，返回可见性排序后的候选
pub fn resolve_name(idx: &Index, file: FileId, pos: Pos, name: Sym) -> Vec<DefId>;
// 内部顺序：局部变量(作用域链上溯) → 类成员(supertype 链) → 属性访问器(Get/Set 读写侧)
//         → 命名空间链(逐级回退) → 全局/类型本身
// 约束：mixin 可见性（args[0] 为本类基类时类内可见）、模块隔离规则
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
- `textDocumentSync`: Incremental（UTF-16 position 直接进 `range.rs` 换算）。
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
| `publishDiagnostics` | M6 起步：仅 `AS09xx` 工具链诊断；P5 全量 | M6 |

## 9. 里程碑与验收

每个里程碑**可独立演示**；M2 起每个里程碑都有 VSCode 体感验收（扩展最小版随 M2 就位）。

| # | 内容 | 验收标准 |
|---|---|---|
| M0 | Cargo workspace + as-syntax + `dump-tree` | Demo_AS/Script 27 文件 + 414 `.d.as` dump 零 ERROR（复用 grammar 验收口径，口径一致才说明包装层无损） |
| M1 | as-core 三阶段 + `dump-index` | dump 输出索引统计（类型/函数/属性数），与 `.d.as` 语料**数量对账**（抽样人工核对）；继承闭包含环检测用例 |
| M2 | as-lsp 壳 + documentSymbol/semanticTokens/folding + **VSCode 扩展最小版** | Demo_AS 打开真实体感；semanticTokens 与 Hazelight 扩展同文件截图对照 |
| M3 | 查找链 + hover/definition | as-core 内置单测覆盖查找链命中序（§10）；as-cli 对语料批量 dump-index 校验 |
| M4 | references/rename/workspaceSymbol + 重载消歧 | 重载函数引用消歧用例（成功/失败双路径）；`$progress` 长任务 |
| M5 | completion/signatureHelp/inlayHint | `X.` 成员补全、`n"\|` UFUNCTION 名单、`UCLASS(` 说明符补全、命名参数补全四类截图验收 + `auto` 变量 inlay 类型展示 |
| M6 | 诊断起步 | `AS0901-0903` 生效；`AS04xx` 按取证占号（进入 P5 流程，码表 §6 纪律） |

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
| G5 | `as04xx` 符号解析诊断码诱惑（实现中顺手报错） | 码表 §9 纪律：P5 前不得发明；顺手发现的问题记进码表待办而非代码 |

## 13. 变更记录

| 版本 | 内容 |
|---|---|
| v0.1 | 首版：crate 内部结构、ID 体系与数据模型、三阶段流水线精确时序（Phase1 即全量 parse 的裁决）、增量失效策略、并发模型、重载解析 API、M0-M6 里程碑与验收、开放问题 Q1-Q4 |
| v0.2 | 测试策略定稿：单测源码内置（AGENTS.md 硬性规则）；golden 对账取消（决策记录 D2）；Q1-Q4 全部裁决，§11 改为索引 |
| v0.3 | 挂接 [`实现优化.md`](实现优化.md)：§3 引用 `FileId`/`Sym` intern 实现模板（决策记录 D12/D13） |
