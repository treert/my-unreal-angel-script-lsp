# Changelog

All notable changes to this extension will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.2] - 2026-09-28

### Added

- Semantic coloring for identifier usage sites: bare identifiers, member access
  chains (`Receiver.Method`), and qualified names (`EMode::Off`) are now resolved
  through the full lookup chain and colored the same as their declarations
  (`as_global_function`, `as_member_function`, `as_local_variable`, …). Falls
  back to syntax-only highlighting while the index is loading. Unresolved
  identifiers stay uncolored.

### Removed

- `missing-type-decls` diagnostic (a warning on every script file when no
  `.d.as` type declarations are indexed): standalone scripts that don't
  reference engine types no longer get warned. `undefined-function` remains
  skipped while no `.d.as` is indexed (engine functions are unresolvable
  there), so no false positives are introduced.

## [0.1.1] - 2026-09-28

### Added

- `undefined-function` semantic diagnostic: calls to undefined functions (bare
  callee unresolved through the full lookup chain — the same resolution used
  by hover / goto definition) are now reported as errors. Suppressible per line
  with `// as-ignore: undefined-function`. Skipped entirely when no `.d.as`
  type declarations are indexed (engine functions are unresolvable there).

## [0.1.0] - 2026-09-28

### Added

- Initial marketplace release: Unreal Angelscript language support backed by a
  tree-sitter grammar and the `as-lsp` Rust language server.
- Hover, completion, signature help, goto definition, references, rename.
- Document symbols, semantic tokens, inlay hints.
- Syntax + semantic diagnostics.
- `.d.as` type declaration indexing (`Saved/AS-Cache`) with file watching.
- `floatIsFloat64` option mirroring the engine's `bScriptFloatIsFloat64`.
- Bundled platform-specific `as-lsp` binaries (`win32-x64`).
