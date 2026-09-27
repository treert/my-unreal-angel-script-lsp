# docs — my-as-lsp 设计文档中心

> 本目录是 LSP 实现的**设计文档中心**：架构、落地规划、决策定案、实现技术、诊断码表。
> 语言本身的真值（方言差异、类型语义、设计取舍）在 [`../as-docs/`](../as-docs/README.md)，
> **docs/ 单向引用 as-docs/**。

## 文档索引

| 文档 | 一句话定位 | 什么时候读 |
|---|---|---|
| [`架构设计.md`](架构设计.md) | **总纲**：四模块架构（UE 导出插件 / grammar / Rust LSP / VSCode 扩展）、**`.d.as` 格式真值（§2.4）**、三阶段索引、查找链 0-6 级、路线图 P0-P6 | 第一个读 |
| [`LSP实现规划.md`](LSP实现规划.md) | **落地设计**（模块三/四细化）：crate 内部结构、ID 体系与 `DefKind`/`DefFlags`、流水线时序、表达式定型管线（§4.2）、overlay 与文件生命周期（§5）、并发模型、里程碑 M0-M6 与验收 | 开工前必读 |
| [`实现决策记录.md`](实现决策记录.md) | **定案清单** D1-D24：每条含背景、裁决、理由。已定条目不改写，新决策追加编号；修正既有决策时注明「修正 Dxx」（如 D21 修正 D20、D23 修正 D16、D24 修正 D18） | 遇到「为什么这么定」时查 |
| [`实现优化.md`](实现优化.md) | **基础设施优化技术**：`FileId` 注册表（含墓碑语义）、`Sym` intern（移植自 mylua，含适配改动） | M0 实现 intern 时 |
| [`架构设计-引擎内部语法.md`](架构设计-引擎内部语法.md) | 引擎内部构造的 LSP 约定：`?` 通配类型、`auto`、`unresolved_object`、**模板声明头**四件套（§5 总览） | 处理 `.d.as` 奇异形态时 |
| [`诊断码表.md`](诊断码表.md) | **码号登记处 + 已取证的引擎约束素材库**：编码规则、码号占用总览。诊断规则按需设计，该表**不构成实现承诺**（D22） | 只在做诊断（P5/M6）时 |

## 推荐阅读路径（新会话 / 新协作者）

```
AGENTS.md（仓库说明 + 测试硬性规则）
  └─► 架构设计.md            全景：模块、数据流、路线图
        └─► LSP实现规划.md    开工粒度：crate、数据模型、时序、里程碑
              ├─► 实现决策记录.md   为什么（D1-D24）
              └─► 实现优化.md       怎么做 intern
按需：引擎内部语法.md（`?`/`auto`/`unresolved_object`）、诊断码表.md（P5）
语言背景：../as-docs/README.md 的阅读顺序
```

## 当前项目状态（2026-09）

| 阶段 | 状态 |
|---|---|
| P0 仓库 + BNF | ✅ 完成（`grammar/angelscript.bnf`） |
| P1 UE 导出插件 + `.d.as` 格式 | ✅ 初版完成（`Demo_AS/Saved/AS-Cache`，414 个 `.d.as`）；**格式已对账落文档**（架构设计 §2.4）；`_manifest.dctx` LSP **不读**、格式版本**不校验**（D20/D21）；已知缺陷见下 |
| P2 tree-sitter grammar | ✅ 完成（语料零 ERROR、corpus 40 条全过、生成物不入库）；模板四形态已单独实测通过 |
| 设计阶段 | ✅ 收官（D1-D24 定案，无阻塞项） |
| M0 Cargo workspace + as-syntax + dump-tree | ✅ 完成（`lsp/` 四 crate 就位，依赖单向图成立；27 `.as` + 414 `.d.as` dump 零 ERROR，与 grammar P2 验收同口径） |
| M1 as-core 三阶段 + dump-index + tag 解析 | ✅ 完成（Phase 1 rayon 并行 parse + Phase 2 成员表/继承闭包/类型归一化；15 tag 全解析；对账：type_count 13814 vs manifest 14864、member_count 63338 vs 69337——差额 ≈ 20 个被覆盖 group 的 1050 个类型及其成员，与风险 7 相符；继承环/struct 无闭包/float 双取值/4 零语料 tag 均有内置单测；**D25**：基础类型为合成 builtin DefId） |
| M2 as-lsp 壳 + documentSymbol/semanticTokens/folding + VSCode 扩展最小版 | ✅ 代码完成（tower-lsp-server + 增量同步 + overlay；legend 19 类 wire 名与 Hazelight 对齐 `as_typename`…；扩展 languageId `angelscript-asl` + 配置骨架 + cargo 开发模式。**VSCode 体感验收与 Hazelight 截图对照待人工执行**：F5 扩展开发宿主打开 Demo_AS/Script） |
| M3 查找链 + hover/definition + workspace 索引接入 | ✅ 完成（resolve.rs 0-6 级全量 + mixin 五条准入；expand.rs delegate/event 成员集 + StaticClass 合成（引擎取证 ProcessDelegates / BindStaticClass）；hover.rs snippet fence + doxygen markdown；server 冷启动 Loading/Ready + pending_dirty 重放 + 惰性单文件重索引（D26）；路径分隔符规范化防 FileId 分裂（D26）。77 单测全绿；语料 resolve-stats 命中 93.3%（未命中主要是 EnhancedInput 插件类型不在本机 AS-Cache）；端到端：hover `struct FVector` / definition → `Core.d.as:10103`（与架构设计 §2.2.1 记录一致）。**VSCode 体感验收待人工执行**（EDH + test-extension.ps1） |
| M4 references/rename/workspaceSymbol + 重载消歧 + watched-files | ✅ 完成（uses.rs UseSite 记录 + 引用倒排；references.rs 解析内核——**parent 链上溯**修掉从根下潜的 O(兄弟) 重遍历，全语料首查 163s→0.6s；重载消歧 D28（arity + 可定型实参，失败报全部）；rename 严格匹配；workspaceSymbol D30 过滤；$/progress 长任务；watch.rs DidChangeWatchedFiles 动态注册 + `.as` 增删改名 + `.d.as` 防抖 500ms/5s（D24）；声明面指纹联动失效（D29）。**顺带修 M3 遗留**：expand/mixin 幽灵符号（reindex 后旧 DefId 未过滤 ⇒ 重复合成 namespace 进 main）、range-for 迭代变量死分支（`"range_for_statement"` 节点不存在）。96 单测全绿；语料 ref-stats 68763 站点 99.4% 解析；e2e：FVector 引用 2846 站点/88 文件 + $/progress、watched-files 生命周期（增删复活 + 防抖）。**VSCode 体感验收待人工执行**（EDH：Find All References / Rename / Ctrl+T） |
| M5-M6 实现 | ⬜ 待开工（里程碑与验收见 [`LSP实现规划.md`](LSP实现规划.md) §9） |

### 开工前的已知待办（不阻塞 M0/M1，但须在对应里程碑前处理）

| # | 事项 | 归属 | 登记处 |
|---|---|---|---|
| 1 | 导出器 group 文件名冲突 → 20 个 group 被覆盖、类型丢失 | P1（导出器） | 架构设计 §8 风险 7。**LSP 侧不再检出**（D20，`AS0905` retired），只能在导出器修 |
| 2 | 4 个语料零出现的 tag 必须用内置单测覆盖 | M1 | 架构设计 §8 风险 8；规划 §9 M1 |
| 3 | `floatIsFloat64` 配置错误是静默错误 | M1 / 扩展 | 架构设计 §8 风险 11；规划 §12 G9。缓解：默认值对齐引擎、状态栏常显生效值、M1 双取值用例 |
| 4 | `.as` 中误写 `.d.as` 独有构造（模板头 / `?` / `unresolved_object`）——**按需**决定是否报诊断 | P5 | 架构设计 §8 风险 10；素材见诊断码表 §2 末段 |
| 5 | `.d.as` 目录**自动发现**（免 `typeDeclarationDirs` 配置）——**已搁置** | 待定 | D24 搁置项。需先解消多副本双源冲突（`Saved/AS-Cache` 与版本库过期副本同时入索引 ⇒ 双定义、补全重复、静默错误），消解规则需占新码 `AS0907` |
| — | ~~manifest 无插件/二进制指纹~~ | — | **已关闭**（D20）：LSP 不读 manifest、不做失效校验 |
| — | ~~`@templateSpecialization class TArray<FVector>` 文法未验证~~ | — | **已关闭**（D19）：实测零 ERROR，文法无需改动 |

## 文档治理规则

1. **决策只追加**：`实现决策记录.md` 的已定条目不改写，新决策追加编号并更新变更记录；
   修正既有决策时追加新条目并注明「修正 Dxx 第 n 条」（如 D21 修正 D20），不回改原文；
2. **码号唯一分配**：任何诊断码必须先在 `诊断码表.md` 占号再写实现，且统一走
   `enum DiagCode`（禁止散落字符串字面量）。**诊断规则本身按需设计**——不预先规定
   severity / 措辞 / range / quick-fix（D22）；废弃的码号不复用；
3. **架构变更同步文档**：改 crate 结构 / 数据流 / 能力范围 → 同步更新
   `架构设计.md` 与 `LSP实现规划.md`（bug 修复、纯重构、配置微调除外）；
4. **引擎内部语法新条目**：同类细节（引擎注册串里的奇异形态）追加到
   `架构设计-引擎内部语法.md`，不开新文件；
5. 所有文档修订必须更新自身的变更记录表（含头部版本号，二者必须一致）；
6. **`.d.as` 格式以实际导出物为准**：任何关于文件头、注解 tag、签名形态的描述，
   真值是 `Demo_AS/Saved/AS-Cache` 的实际内容 + `TypeDeclarationExporter.cpp`。
   改这部分必须重新对账并在架构设计 §2.4 更新源码行号引用，不得凭早期设计稿书写。
