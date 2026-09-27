# my-angel-script-lsp 项目说明

## 当前目标

使用 **tree-sitter（文法）+ Rust（LSP 核心）+ TypeScript（VSCode 扩展）** 实现 Unreal Angelscript 的 LSP。

架构参考 `c:\MyGit\ai-mylua-lsp` 的三层方案，总体设计见 [`docs/架构设计.md`](docs/架构设计.md)，引擎内部语法的专门约定见 [`docs/架构设计-引擎内部语法.md`](docs/架构设计-引擎内部语法.md)，方言差异见 [`docs/原版AngelScript与UE-fork对比.md`](docs/原版AngelScript与UE-fork对比.md)。

## 仓库结构

```
my-angel-script-lsp/
├── grammar/          # tree-sitter 文法（BNF + scanner.c + corpus 测试）
├── lsp/              # Rust LSP Server（Cargo workspace）
│   └── crates/
│       ├── tree-sitter-as/   # tree-sitter 包装 crate（grammar 构建产物）
│       ├── as-core/          # 索引、类型系统、查找链（纯库，无 IO 依赖，可单测）
│       ├── as-lsp/           # LSP server 主 crate（请求路由、增量同步）
│       └── as-cli/           # CLI 入口（dump-tree / dump-index，调试用）
├── vscode-extension/ # VS Code 扩展（TypeScript）
├── tests/            # 手工测试用 .as 文件（可随意增删改，Rust 测试不依赖此目录）
└── docs/             # 设计文档中心
```

## 关键外部目录

| 目录 | 说明 |
|---|---|
| `d:\WorkGit\UnrealEngine\Engine\Plugins\Angelscript` | Angelscript 插件（引擎侧运行时 + 修改过的 AS 解释器源码），语言行为与 VM 实现的**最终真值来源** |
| `c:\MyGit\open-sources\vscode-unreal-angelscript` | 官方 LSP 扩展，可参考其行为做验收对照；**注意官方实现有缺陷**（如类型真值依赖引擎在线），任何歧义以 UnrealEngine 里的 VM 代码为准 |
| `c:\MyGit\ai-mylua-lsp` | 架构参考实现，**正常不需要主动访问**，除非用户主动提示参考其中的某些路径 |
