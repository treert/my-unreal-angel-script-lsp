# my-as-lsp (Unreal Angelscript)

Language support for Unreal Angelscript (the Angelscript dialect used by the
[Unreal Engine Angelscript plugin](https://angelscript.hazelight.se/)), backed
by a tree-sitter grammar and a Rust language server (`as-lsp`).

## Features

- **Hover** — types and documentation for symbols
- **Completion** — keywords, indexed symbols, members
- **Signature help** — parameter info for calls
- **Go to definition / References / Rename**
- **Document symbols** — outline view
- **Semantic tokens** — full semantic highlighting (types, namespaces, fields, …)
- **Inlay hints**
- **Diagnostics** — syntax errors from the tree-sitter grammar plus semantic
  diagnostics from the index
- **`.d.as` type declaration indexing** — reads UE's exported type
  declarations (`Saved/AS-Cache`, produced by the AngelscriptTypeExporter) and
  watches them for changes

## Requirements

- A UE project exporting `.d.as` declarations via the Angelscript plugin's
  type exporter (the default `typeDeclarationDirs` points at
  `${workspaceFolder}/Saved/AS-Cache`).

## Extension Settings

| Setting | Default | Description |
| --- | --- | --- |
| `myAngelScriptLsp.serverPath` | `""` | Absolute path to the `as-lsp` executable. When empty, the bundled `server/as-lsp(.exe)` is used (production) or `cargo run -p as-lsp` (dev). |
| `myAngelScriptLsp.typeDeclarationDirs` | `["${workspaceFolder}/Saved/AS-Cache"]` | `.d.as` type declaration directories. Supports `${workspaceFolder}`. |
| `myAngelScriptLsp.scriptRoots` | `[]` | `.as` script search roots; empty = all workspace roots. |
| `myAngelScriptLsp.floatIsFloat64` | `true` | Mirror of the engine setting `bScriptFloatIsFloat64`. **Must be kept in sync** if you changed it in `DefaultEngine.ini` — a mismatch is a silent type-width error. |
| `myAngelScriptLsp.debug.fileLog` | `false` | Write server debug logs to `.vscode/my-as-lsp.log` in the workspace. |

## Commands

- **my-as-lsp: Restart Language Server**

## Release Notes

See [CHANGELOG.md](CHANGELOG.md).

## Development

```powershell
# extension: compile / watch (esbuild)
npm run compile

# package a host-platform .vsix (server binary must be built first)
cargo build --release -p as-lsp     # in ../lsp
npm run package

# build + publish the current platform to the Marketplace
npm run release
```

The language server lives in [`lsp/`](../lsp) (Rust workspace:
`as-syntax` / `as-core` / `as-lsp` / `as-cli`); the tree-sitter grammar lives
in [`grammar/`](../grammar).
