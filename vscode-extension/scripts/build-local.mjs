#!/usr/bin/env node
/**
 * build-local.mjs — one-shot "build the .vsix for my current
 * machine" script.
 *
 * Auto-detects (process.platform, process.arch) → VS Code
 * platform target + Rust triple, installs the Rust target if
 * missing, compiles the LSP in release mode, then hands off to
 * the regular packaging pipeline with `MYAS_TARGET` set so the
 * resulting `.vsix` is tagged as platform-specific (Marketplace
 * expects this for native-binary extensions).
 *
 * Typical usage:
 *   cd vscode-extension
 *   npm run build:local        # or: node scripts/build-local.mjs
 *
 * Output:
 *   vscode-extension/my-angel-script-lsp-<version>.vsix
 *
 * This only produces a .vsix for the **current** host. To cover
 * another OS you need a machine/CI runner of that OS, or run
 * `npm run release` there (publish.mjs performs the same
 * build-then-package sequence, then uploads to the Marketplace).
 */

import { readFileSync } from 'node:fs';
import { join } from 'node:path';

import {
  detectHostTarget,
  ensureRustTarget,
  buildLspRelease,
  packageVsix,
  extensionRoot,
} from './lib/host-target.mjs';

const pkg = JSON.parse(readFileSync(join(extensionRoot, 'package.json'), 'utf8'));
const { target, triple } = detectHostTarget();

console.log('--------------------------------------------------');
console.log(`[build-local] host     : ${process.platform}-${process.arch}`);
console.log(`[build-local] target   : ${target}`);
console.log(`[build-local] triple   : ${triple}`);
console.log(`[build-local] version  : ${pkg.version}`);
console.log('--------------------------------------------------');

ensureRustTarget(triple);
buildLspRelease(triple);
packageVsix(target);

const vsixName = `${pkg.name}-${target}-${pkg.version}.vsix`;
console.log('');
console.log(`[build-local] done!`);
console.log(`[build-local] output: ${join(extensionRoot, vsixName)}`);
console.log('');
console.log(`To install locally:`);
console.log(`  code --install-extension "${join(extensionRoot, vsixName)}"`);
