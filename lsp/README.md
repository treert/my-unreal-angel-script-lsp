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

as-lsp 现有能力（M2）：`documentSymbol` / `foldingRange` / `semanticTokens(full)`
（legend 19 类，wire 名与 Hazelight 对齐——`as_typename` 等）+ textDocumentSync
Incremental + `myAngelScriptLsp.*` 配置读取。hover/definition 等随 M3。

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

## 约定提醒

- **禁止任何 Rust 格式化**（`cargo fmt` / rustfmt / IDE format），改动只保持局部既有格式；
- **单测 `.as` 用例一律内置源码字符串字面量**，禁止读 `../tests/` 外部文件；
- 文法节点增删后须同步 `crates/as-syntax/src/node.rs`（文件头附再生成命令）。
