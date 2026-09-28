# 打包与发布（Build）

本目录自带一套不依赖 CI 的手动打包/发布脚本，参考 `ai-mylua-lsp` 的三层方案。本文说明设计思路与使用方法。

## 总览

```
npm run release                     # 一键：changelog 落版 → cargo release → vsce package → vsce publish
└─ scripts/publish.mjs
   ├─ scripts/lib/host-target.mjs   # 主机探测（target + Rust triple）、命令执行原语
   ├─ cargo build --release --target <triple> -p as-lsp
   └─ npm run package               # 打出 .vsix
      └─ scripts/package.mjs
         ├─ scripts/prepackage.mjs  # 拷贝 server 二进制到 server/
         └─ vsce package --target <target>
            └─ vscode:prepublish    # esbuild --production → dist/extension.js

npm run build:local                 # 本地打包（不发布）：探测主机 → cargo release → npm run package
└─ scripts/build-local.mjs          # 与 release 共用同一套 build-then-package 流水线
```

## 设计

### 1. server 二进制如何进入 .vsix

Rust LSP server（`lsp/` workspace，二进制名 `as-lsp`）必须以**平台特定**产物打进扩展：

- `scripts/prepackage.mjs` 把 `lsp/target/<triple>/release/as-lsp(.exe)` 拷入本目录 `server/`，每次先清空 `server/`，防止跨平台打包时残留旧二进制（如 macOS 上残留的 `.exe`）。
- `.vscodeignore` 排除 `src/`、`scripts/`、`node_modules/`（扩展代码由 esbuild 打成单文件 `dist/extension.js`），但保留 `server/`。
- `vsce package --target win32-x64` 等平台参数让 Marketplace 只向匹配平台分发该 .vsix。

### 2. 运行时如何找到二进制（src/extension.ts 的 `resolveServerOptions`）

查找顺序：

1. `myAngelScriptLsp.serverPath` 显式指定（调试用）；
2. **生产模式**（商店安装/.vsix 侧载）：`<extension>/server/as-lsp(.exe)` —— 即 prepackage 拷入的路径，与脚本约定保持同步；
3. **开发模式**（F5 EDH）：`cargo run -p as-lsp` 直连仓库 lsp workspace，profile 由 `tools/test-extension.ps1` 写入的 `lsp/target/.build-profile` 决定。

### 3. target / triple 映射

主机探测（`host-target.mjs`）与打包映射（`prepackage.mjs` 的 `TARGET_MAP`）两处维护，需保持同步：

| VS Code target | Rust triple |
|---|---|
| `win32-x64` | `x86_64-pc-windows-msvc` |
| `win32-arm64` | `aarch64-pc-windows-msvc` |
| `darwin-x64` / `darwin-arm64` | `x86_64` / `aarch64-apple-darwin` |
| `linux-x64` / `linux-arm64` | `x86_64` / `aarch64-unknown-linux-gnu` |

（`TARGET_MAP` 另备 `linux-armhf`、`alpine-*` 等，装好对应 toolchain 即可本地交叉打包。）

### 4. CHANGELOG 半自动落版（scripts/publish.mjs）

`vsce publish` 前校验并改写 `CHANGELOG.md`：

- `[Unreleased]` 段必须非空（HTML 注释不算），否则中止——防止发布一个没有变更记录的版本；`--force-changelog` 可强制绕过（不推荐）。
- 把 `[Unreleased]` 改名为 `[<version>] - <今天>`，顶部重新打开一个空 `[Unreleased]`。文件写入发生在 `vsce package` 之前，商店的 Changelog 页签随之更新。
- **同版本重跑守卫**：若 `package.json` 的 version 已等于 CHANGELOG 最新落版版本，视为多平台补发（只发另一平台的 .vsix），跳过落版、不动文件。
- 已存在 `## [<version>] - <日期>` 时报错退出，防止重复发同版本。

默认发布成功后不碰 git（自己提交 CHANGELOG 改动）；加 `--git` 参数则自动 `git commit`。

## 使用方法

### 前置（一次性）

1. Marketplace publisher：在 https://marketplace.visualstudio.com/manage 创建，名字与 `package.json` 的 `publisher` 一致。
2. PAT：https://dev.azure.com → User Settings → Personal Access Tokens，勾选 **Marketplace: Manage** scope。

### 本地打包（不发布）

```powershell
cd vscode-extension
npm run build:local      # 自动：探测主机 → rustup target add → cargo build --release -p as-lsp → vsce package
```

产物：`my-angel-script-lsp-<version>.vsix`（主机平台，`.vsix` 名带平台标记）。脚本结束时打印 `code --install-extension` 安装命令。

也可手动分步（例如只重拷二进制重打包，不重新编译 Rust）：

```powershell
npm run package          # 跳过 cargo build，直接用 lsp/target/release 下的现有二进制
```

### 发布当前平台

```powershell
cd vscode-extension
$env:VSCE_PAT = "<token>"              # 或先 npx @vscode/vsce login <publisher>
# 编辑 CHANGELOG.md 的 [Unreleased] 段 + package.json 的 version
npm run release                        # 可加 --git 自动提交 changelog
```

### 多平台发布

Marketplace 允许同一扩展 ID 挂多个平台包：在每台目标机器（或配好交叉 toolchain 的本机）上，保持 `package.json` 的 version 不变，重复：

```powershell
npm run release          # 每个平台一次；同版本自动跳过 changelog 落版
```

发布管理页：https://marketplace.visualstudio.com/manage/publishers/<publisher>

## 目录约定

| 路径 | 生成物 | git |
|---|---|---|
| `dist/extension.js` | esbuild 产物（prepublish 时生成） | 忽略 |
| `server/as-lsp(.exe)` | prepackage 拷入的 server 二进制 | 忽略 |
| `*.vsix` | vsce 打包产物 | 忽略 |
| `CHANGELOG.md` | 发布时半自动落版 | 入库 |

## 常见问题

- **prepackage 报 release binary missing**：直接用 `npm run build:local`，它会自动完成 `rustup target add` + `cargo build --release -p as-lsp`；若只想重打包，先在 `../lsp` 下 `cargo build --release -p as-lsp`（target 模式再加 `--target <triple>`，提示信息里有现成命令）。
- **改了 Rust server 但 .vsix 行为没变**：`npm run package` 用的是 `lsp/target/release` 的旧二进制；用 `npm run build:local` 重新编译，或先手动 `cargo build --release -p as-lsp`。
