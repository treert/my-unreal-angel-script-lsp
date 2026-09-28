# my-angel-script-lsp 项目说明

## 当前目标

使用 **tree-sitter（文法）+ Rust（LSP 核心）+ TypeScript（VSCode 扩展）** 实现 Unreal Angelscript 的 LSP。

架构参考 `config.paths.ai_mylua_lsp` 的三层方案（路径占位符约定见下方「关键外部目录」节）。全部文档的索引、阅读路径与项目当前状态见 [`docs/README.md`](docs/README.md)（LSP 设计文档中心）与 [`as-docs/README.md`](as-docs/README.md)（语言分析真值）。

**文档分两类，`docs/` 单向引用 `as-docs/`**：
- `as-docs/` — Unreal Angelscript 语言分析（方言差异、struct 语义、设计取舍），与 LSP 实现无关的真值
- `docs/` — my-as-lsp 设计文档（架构、诊断码表、引擎内部语法的 LSP 处理约定）

## 仓库结构

```
my-angel-script-lsp/
├── grammar/          # tree-sitter 文法（BNF + scanner.c + corpus 测试）
├── lsp/              # Rust LSP Server（Cargo workspace）
│   └── crates/
│       ├── as-syntax/        # tree-sitter 包装 crate（grammar 构建产物）
│       ├── as-core/          # 索引、类型系统、查找链（纯库，无 IO 依赖，可单测）
│       ├── as-lsp/           # LSP server 主 crate（请求路由、增量同步）
│       └── as-cli/           # CLI 入口（dump-tree / dump-index，调试用）
├── vscode-extension/ # VS Code 扩展（TypeScript）
├── config/          # 外部依赖路径配置（paths.example.yaml 模板进版本；paths.local.yaml 不入库）
├── tests/            # 手工测试用 .as 文件（可随意增删改，Rust 测试不依赖此目录）
├── as-docs/          # UE Angelscript 语言分析文档（方言差异、设计取舍、struct 专题）
└── docs/             # LSP 设计文档中心（架构、诊断码表）
```

## 测试规则（硬性）

- **Rust 单元测试中的 `.as` 测试代码一律内置于 Rust 源码**（字符串字面量），**禁止**读取 `tests/` 等外部文件——与 `ai-mylua-lsp` 的同一约定。
- 取材参考（不进单元测试）：`config.paths.demo_as`/Script/Script-Examples（官方示例）、`config.paths.demo_as`/Saved/AS-Cache（414 个 `.d.as` 导出声明）——供编写用例时提炼真实形态、以及 as-cli 批量校验使用。复杂形态进单测时**摘录片段内置**，而不是读文件。
- `tests/` 目录仅为手工测试文件，Rust 测试不依赖它。

## 关键外部目录（`config.paths.*` 占位符）

外部依赖目录**因机器而异，不在文档硬编码**：统一在 `config/paths.local.yaml` 配置（复制 `config/paths.example.yaml` 生成，已被 .gitignore 忽略），`tools/` 与 `tests/` 下的脚本从该文件读取（优先级：命令行参数 > local 配置）。**所有文档（`docs/`、`as-docs/`、`lsp/README.md`）中出现的 `config.paths.<key>` 均按该文件解析**——看到此占位符时先读这里，不要问用户要路径。派生子路径约定（写在模板注释里）：`angelscript_plugin` = `{unreal_engine}/Engine/Plugins/Angelscript`、`demo_as_script` = `{demo_as}/Script`、`demo_as_cache` = `{demo_as}/Saved/AS-Cache`。

| 占位符 | 说明 |
|---|---|
| `config.paths.unreal_engine`（Angelscript 插件在其 `Engine/Plugins/Angelscript` 子目录） | 带 Angelscript 插件的 UnrealEngine 引擎（引擎侧运行时 + 修改过的 AS 解释器源码），语言行为与 VM 实现的**最终真值来源** |
| `config.paths.demo_as` | UE 测试工程：AngelscriptTypeExporter 导出 `.d.as` + 官方示例脚本。本机没有时可留空——LSP 测试用 `config.paths.test_as` 的本地拷贝即可 |
| `config.paths.test_as` | LSP 测试用 as 语料目录（`tools/test-extension.ps1` 的默认打开目标） |
| `config.paths.vscode_unreal_angelscript` | 官方 LSP 扩展，可参考其行为做验收对照；**注意官方实现有缺陷**（如类型真值依赖引擎在线），任何歧义以 UnrealEngine 里的 VM 代码为准 |
| `config.paths.ai_mylua_lsp` | 架构参考实现，**正常不需要主动访问**，除非用户主动提示参考其中的某些路径 |
