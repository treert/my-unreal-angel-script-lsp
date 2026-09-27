# lsp — Rust Cargo workspace

my-as-lsp 的 LSP 核心层（架构设计模块三）。架构与里程碑的**真值在 `../docs/`**
（[`../docs/架构设计.md`](../docs/架构设计.md) / [`../docs/LSP实现规划.md`](../docs/LSP实现规划.md)），
本文件只回答「怎么构建、怎么测试、怎么跑验收」。

## crate 结构与依赖方向

```
crates/
├── as-syntax/   # tree-sitter 包装：build.rs 用 cc 编译 ../../grammar 的生成物
├── as-core/     # 索引/类型/查找链（纯库：无 IO、无 async）—— M0 仅 ID/intern 骨架
├── as-lsp/      # LSP server 壳 —— M0 占位 stub（M2 引入 tower-lsp-server）
└── as-cli/      # 调试与验收工具（dump-tree；dump-index 随 M1 就位）
```

依赖严格单向，禁止环：

```
as-lsp ──► as-core ──► as-syntax
as-cli ──► as-core
```

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
cargo run -p as-lsp         # M0 stub：打印版本退出
```

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

## M0 验收（已完成，可随时复跑）

```powershell
cargo run -p as-cli --release -- dump-tree d:\WorkGit\UEProjs\Demo_AS\Script
# 预期：27 files, 0 with errors —— 退出码 0

cargo run -p as-cli --release -- dump-tree d:\WorkGit\UEProjs\Demo_AS\Saved\AS-Cache
# 预期：414 files, 0 with errors —— 退出码 0
```

与 grammar P2 验收（`npx tree-sitter parse` 对同语料零 ERROR）同口径同判据，
用于证明 Rust 包装层无损。

## 约定提醒

- **禁止任何 Rust 格式化**（`cargo fmt` / rustfmt / IDE format），改动只保持局部既有格式；
- **单测 `.as` 用例一律内置源码字符串字面量**，禁止读 `../tests/` 外部文件；
- 文法节点增删后须同步 `crates/as-syntax/src/node.rs`（文件头附再生成命令）。
