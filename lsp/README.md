# lsp — Rust Cargo workspace

my-as-lsp 的 LSP 核心层（架构设计模块三）。架构与里程碑的**真值在 `../docs/`**
（[`../docs/架构设计.md`](../docs/架构设计.md) / [`../docs/LSP实现规划.md`](../docs/LSP实现规划.md)），
本文件只回答「怎么构建、怎么测试、怎么跑验收」。

## crate 结构与依赖方向

```
crates/
├── as-syntax/   # tree-sitter 包装：build.rs 用 cc 编译 ../../grammar 的生成物
├── as-core/     # 索引/类型/查找链（纯库：无 IO、无 async）+ outline/tokens CST 映射
├── as-lsp/      # LSP server 壳（tower-lsp-server）：增量同步、overlay、M2 三请求
└── as-cli/      # 调试与验收工具（dump-tree / dump-index）
```

依赖严格单向，禁止环：

```
as-lsp ──► as-core ──► as-syntax
as-cli ──► as-core
```

as-lsp 现有能力（M5）：`documentSymbol` / `foldingRange` / `semanticTokens(full)`
（legend 19 类，wire 名与 Hazelight 对齐——`as_typename` 等）+ `hover` /
`definition`（查找链 0-6 级，架构设计 §4.5）+ `references` / `rename`（+
`prepareRename`）/ `workspaceSymbol`（M4：重载消歧——失败报全部重载；
rename 只改消歧成功唯一指向目标的站点；`$/progress` 长任务通知）+
**completion**（M5：`X.` 成员 / `A::` 命名空间与 enum / 命名实参（跳过
`InArgN` 占位）/ 关键字与查找链上下文 / `UCLASS(` 等说明符（引擎取证表）/
`AddUFunction(this, n"|")` UFUNCTION 名单；触发符 `. : ( ,`）+
**signatureHelp**（M5：槽位排序 + 实参定型消歧 + 命名实参按名定位）+
**inlayHint**（M5：auto 局部变量与 range-for 迭代变量的推导类型展示）+
textDocumentSync Incremental + `myAngelScriptLsp.*` 配置读取 + 冷启动后台索引
（Loading/Ready 状态机）+ **DidChangeWatchedFiles**（动态注册 `**/*.as`；
`.as` 增删改名入索引；`.d.as` 任一变化防抖 500ms/5s 后全量重建，D24）。

## 前置：先生成 grammar

tree-sitter 生成物**不入库**（`../grammar/.gitignore` 约定）。克隆仓库后首次构建
`as-syntax` 前必须生成，否则 build.rs 会报错并提示同样内容：

```powershell
cd ..\grammar
npm install                 # 装 tree-sitter-cli（一次性）
npx tree-sitter generate    # grammar.js -> src/parser.c
```

## 常用命令

在 `lsp/` 目录下：

```powershell
cargo build --workspace     # 构建全部 crate
cargo test  --workspace     # 单元测试（.as 用例全部内置于 Rust 源码，见 AGENTS.md 硬性规则）
cargo run -p as-lsp         # LSP server（stdio；一般由 VSCode 扩展拉起，不手动跑）
```

server 冒烟（initialize 握手，不依赖 VSCode）：

```powershell
powershell -ExecutionPolicy Bypass -File ..\tests\lsp-smoke.ps1
# 预期：SMOKE OK + serverInfo: my-as-lsp + legend: as_typename present
```

## VSCode 扩展（`../vscode-extension/`，M2 最小版）

开发模式（F5「扩展开发宿主」即可用）：`serverPath` 留空时自动
`cargo run -p as-lsp --manifest-path <仓库>/lsp/Cargo.toml`。
构建：

```powershell
cd ..\vscode-extension
npm install
npm run compile     # esbuild -> dist/extension.js
```

languageId 为 `angelscript-asl`（避开 Hazelight 扩展的 `angelscript`）；
`.as` 后缀的关联归属由两扩展的启用状态决定，需要体感对照时可临时禁用其一。

## as-cli：dump-tree

```powershell
# 单文件：打印整棵 CST（corpus 风格 S-表达式），末尾报 ERROR/MISSING 明细
cargo run -p as-cli -- dump-tree <file.as>

# 目录：递归收集 *.as / *.d.as，逐文件只报状态行（ok / ERR + 汇总）
cargo run -p as-cli -- dump-tree <dir>

# 目录模式下也想看每棵树：
cargo run -p as-cli -- dump-tree --trees <dir>
```

退出码：任一文件含 ERROR/MISSING 节点、读取失败或非 UTF-8 ⇒ **非 0**（脚本化验收判据）。
`_manifest.dctx` 不匹配 `*.as` 后缀，无需排除。

## as-cli：dump-index（M1）

```powershell
# 构建全工作区索引并输出声明统计（与 manifest 人工对账用——运行时不读 manifest，D20）
cargo run -p as-cli -- dump-index <dir>...

# 裸 float 归一化为 float32（默认 float64，对应引擎 bScriptFloatIsFloat64）
cargo run -p as-cli -- dump-index --float-is-float32 <dir>

# 抽查指定符号（kind + 文件:行:列 + tags + doc 首行）
cargo run -p as-cli -- dump-index --sym FVector <dir>
```

输出含：文件数（script/decl 分列）、按文件类别分列的声明统计、
decl-only 对账口径（type_count~ / member_count~）、继承健康度（闭包数/环数/
未解析基类）、类型归一化统计（interned 数 / 变量声明类型解析率）。
退出码：任一文件 parse 出 ERROR/MISSING 或读取失败 ⇒ 非 0。

## M0 验收（已完成，可随时复跑）

```powershell
cargo run -p as-cli --release -- dump-tree d:\WorkGit\UEProjs\Demo_AS\Script
# 预期：27 files, 0 with errors —— 退出码 0

cargo run -p as-cli --release -- dump-tree d:\WorkGit\UEProjs\Demo_AS\Saved\AS-Cache
# 预期：414 files, 0 with errors —— 退出码 0
```

与 grammar P2 验收（`npx tree-sitter parse` 对同语料零 ERROR）同口径同判据，
用于证明 Rust 包装层无损。

## M1 验收（已完成，可随时复跑）

```powershell
cargo run -p as-cli --release -- dump-index `
    d:\WorkGit\UEProjs\Demo_AS\Script d:\WorkGit\UEProjs\Demo_AS\Saved\AS-Cache
```

对账（manifest：`type_count=14864` / `member_count=69337`，仅作开发期人工参照）：

| 口径（decl-only） | dump-index | manifest | 差额解释 |
|---|---|---|---|
| 类型数（class+struct+enum） | 13814 | 14864 | -1050 ≈ 20 个被覆盖 group 的类型（架构设计 §8 风险 7，LSP 不可见） |
| 成员数（field+method+ctor+dtor+op+vprop） | 63338 | 69337 | -5999，与类型差额比例一致（≈5.7 成员/类型） |

旁证：class 闭包解析中「未解析基类」恰好 20 个——与 20 个丢失 group 闭环对应。
`--sym FVector` 抽查：`struct FVector` 落点 `Core.d.as:10103`，与架构设计
§2.2.1 的源码行号引用一致。

## M3 验收（已完成，可随时复跑）

```powershell
# ① 内置单测（77 条：查找链 0-6 级命中序、mixin 五条准入、delegate 展开、
#    hover 渲染、重索引）
cargo test --workspace

# ② 冒烟（hover/definition 含 snippet fence 断言）
node ..\tests\lsp-smoke.mjs

# ③ 真实工作区端到端（双根布局；本机 Demo_AS 路径）
node ..\tests\lsp-e2e-workspace.mjs
# 预期：E2E OK: hover=struct FVector; definition -> Core.d.as:10103

# ④ 语料批量 + 查找链体检
cargo run -p as-cli --release -- dump-index --resolve-stats `
    d:\WorkGit\UEProjs\Demo_AS\Script d:\WorkGit\UEProjs\Demo_AS\Saved\AS-Cache
# 预期：对账数字不回归（13814/63338）；resolve-stats hit ≈ 93%
#（未命中主要是 EnhancedInput 插件类型不在本机 AS-Cache + 表达式定型
#  推迟子集——f-string 插值/字面量运算等，M4+ 扩）

# ⑤ VSCode 体感验收（人工）：EDH + tools\test-extension.ps1 打开
#    test-as-lsp.code-workspace，hover/F12 跳 .d.as
```

## M5 验收（已完成，可随时复跑）

```powershell
# ① 内置单测（136 条：expr 定型 9 + completion 15 + specifiers 3 +
#    signature 5 + inlay 6 + 既有全量）
cargo test --workspace

# ② 冒烟（八请求：含 completion Plain/前缀/说明符 + signatureHelp 槽位 +
#    inlayHint auto 断言）
node ..\tests\lsp-smoke.mjs
# 预期：SMOKE OK: ... completion(+specifier)/signatureHelp/inlayHint OK

# ③ 真实工作区端到端：references + Scoped/Member 补全（FVector 87 项）
node ..\tests\lsp-e2e-workspace.mjs
# 预期：E2E OK: ... references=2842; completion scoped/member ok

# ④ 语料批量 + 查找链体检（定型扩展的命中率红利）
cargo run -p as-cli --release -- dump-index --resolve-stats `
    d:\WorkGit\UEProjs\Demo_AS\Script d:\WorkGit\UEProjs\Demo_AS\Saved\AS-Cache
# 预期：对账数字不回归（13814/63338）；resolve-stats hit ≈ 94.9%
#（M4 基线 94.3%——M5a 定型扩展新增命中）

# ⑤ VSCode 体感验收（人工）：EDH 里 `X.` 成员补全 / `A::` 命名空间 /
#    `UCLASS(` 说明符 / `f(Du` 命名实参（InArgN 不出现）四类截图 +
#    `auto` 变量 inlay 类型展示（规划 §9 M5 硬性项）
```

## M4 验收（已完成，可随时复跑）

```powershell
# ① 内置单测（96 条：UseSite 提取含 f-string 插值/声明名排除、references
#    匹配语义、重载消歧成功/失败双路径、文件增删复活一致性、声明面指纹、
#    watched-files 分类 + 防抖时序）
cargo test --workspace

# ② 冒烟（四请求 + $/progress + 非法新名 error 断言）
node ..\tests\lsp-smoke.mjs

# ③ 真实工作区端到端：references FVector（全工作区 2846 站点 / 88 文件）
#    + $/progress begin/end
node ..\tests\lsp-e2e-workspace.mjs
# 预期：E2E OK: ... references=2846

# ④ watched-files 生命周期端到端（临时工作区：.as 增删复活 + .d.as 防抖）
node ..\tests\lsp-e2e-m4.mjs
# 预期：M4 E2E OK: watched-files lifecycle
#（时序敏感段已放宽余量，偶发失败可复跑一次确认）

# ⑤ 语料批量 + 引用解析体检
cargo run -p as-cli --release -- dump-index --ref-stats `
    d:\WorkGit\UEProjs\Demo_AS\Script d:\WorkGit\UEProjs\Demo_AS\Saved\AS-Cache
# 预期：对账数字不回归（13814/63338）；ref-stats 68763 站点、
#   resolved ≈ 99.4%、全语料解析 <1s

# ⑥ VSCode 体感验收（人工）：EDH 里对 FVector / 脚本成员
#    Find All References（Shift+F12）/ Rename（F2）/ 工作区符号（Ctrl+T）
```

## 约定提醒

- **禁止任何 Rust 格式化**（`cargo fmt` / rustfmt / IDE format），改动只保持局部既有格式；
- **单测 `.as` 用例一律内置源码字符串字面量**，禁止读 `../tests/` 外部文件；
- 文法节点增删后须同步 `crates/as-syntax/src/node.rs`（文件头附再生成命令）。

## 调试日志

`myAngelScriptLsp.debug.fileLog`（默认 `false`，重启 server 生效）：开启后
server 侧日志写 `<第一个 workspace 根>/.vscode/my-as-lsp.log`（截断式），并
双写 stderr → VSCode「my-as-lsp」输出通道。记录点：会话头（构建 profile、
exe 路径 + mtime、配置、工作区根）、索引重建（roots / 文件数 / 耗时）、
pending dirty 重放、编辑重索引（含声明面指纹变化）、watched-files 处置、
`.d.as` 防抖触发、references 耗时。关闭时零开销（AtomicBool 快速门）。

设施本体在 `as-core/src/logger.rs`（依赖图底层——as-core / as-lsp / as-cli
都能 `as_log!`；`init` 由 as-lsp 在配置加载后调用）。这是 as-core「纯库
no IO」原则的显式例外：核心逻辑只调 `as_log!`，不做 IO 决策。
