/**
 * Shared helpers for the publish scripts.
 *
 * Purpose: derive the VS Code platform target + Rust triple from
 * the **current host** (so a Mac produces a `darwin-*.vsix`, a
 * Windows machine produces `win32-x64.vsix`, etc.), plus thin
 * wrappers around `spawnSync` that forward stdio and exit on
 * non-zero status. Kept in `lib/` so `publish.mjs` /
 * `package.mjs` / `prepackage.mjs` reuse the same detection and
 * execution primitives.
 *
 * Mirrors `ai-mylua-lsp/vscode-extension/scripts/lib/host-target.mjs`
 * with the binary renamed to `as-lsp`.
 */

import { spawnSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
export const extensionRoot = resolve(__dirname, '..', '..');
export const repoRoot = resolve(extensionRoot, '..');
export const lspRoot = resolve(repoRoot, 'lsp');

/**
 * (`process.platform`, `process.arch`) → VS Code platform target +
 * Rust cargo triple. Unknown hosts fall through to an explicit
 * error in `detectHostTarget` rather than silently picking a wrong
 * target.
 *
 * Keep in sync with `prepackage.mjs`'s `TARGET_MAP`.
 */
const HOST_TARGET_MAP = Object.freeze({
  'darwin:arm64': { target: 'darwin-arm64', triple: 'aarch64-apple-darwin' },
  'darwin:x64':   { target: 'darwin-x64',   triple: 'x86_64-apple-darwin' },
  'win32:x64':    { target: 'win32-x64',    triple: 'x86_64-pc-windows-msvc' },
  'win32:arm64':  { target: 'win32-arm64',  triple: 'aarch64-pc-windows-msvc' },
  'linux:x64':    { target: 'linux-x64',    triple: 'x86_64-unknown-linux-gnu' },
  'linux:arm64':  { target: 'linux-arm64',  triple: 'aarch64-unknown-linux-gnu' },
});

/**
 * Return `{ target, triple }` for the current host, or exit with a
 * friendly error if the host is unsupported.
 */
export function detectHostTarget() {
  const key = `${process.platform}:${process.arch}`;
  const entry = HOST_TARGET_MAP[key];
  if (!entry) {
    console.error(
      `[host-target] unsupported host: platform=${process.platform} arch=${process.arch}`,
    );
    console.error(
      `[host-target] supported: ${Object.keys(HOST_TARGET_MAP).join(', ')}`,
    );
    process.exit(1);
  }
  return entry;
}

/**
 * Run a shell command, forwarding stdio, and exit the Node
 * process with the child's status code on failure. Use this
 * instead of `execSync` so long-running output (cargo build)
 * streams in real time.
 *
 * `shell: true` plus a pre-joined command line avoids Node's
 * DEP0190 deprecation warning while still letting npx / rustup
 * etc. resolve `.cmd` shims on Windows.
 */
export function run(line, opts = {}) {
  console.log(`\n$ ${line}`);
  const result = spawnSync(line, {
    stdio: 'inherit',
    shell: true,
    ...opts,
  });
  if (result.status !== 0) {
    console.error(`[run] command failed: ${line} (exit ${result.status})`);
    process.exit(result.status ?? 1);
  }
}

/**
 * Ensure the given Rust triple is installed. Idempotent — if the
 * target is already present, `rustup target add` returns 0
 * without downloading anything.
 */
export function ensureRustTarget(triple) {
  run(`rustup target add ${triple}`);
}

/**
 * Build the as-lsp server in release mode for the given triple.
 * Runs inside `lsp/` so `cargo` resolves the workspace Cargo.toml.
 */
export function buildLspRelease(triple) {
  run(`cargo build --release --target ${triple} -p as-lsp`, { cwd: lspRoot });
}

/**
 * Run the extension's `npm run package` with `MYAS_TARGET` set,
 * which triggers the platform-specific packaging pipeline
 * (prepackage copies `lsp/target/<triple>/release/as-lsp` into
 * `server/` → vsce package --target <target>).
 */
export function packageVsix(target) {
  run('npm run package', {
    cwd: extensionRoot,
    env: { ...process.env, MYAS_TARGET: target },
  });
}
