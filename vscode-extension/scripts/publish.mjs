#!/usr/bin/env node
/**
 * publish.mjs — build the current host's .vsix, then push it to
 * the VS Code Marketplace.
 *
 * Prerequisites:
 *   1. A Marketplace publisher (matches the `publisher` field in
 *      `vscode-extension/package.json`). Create one at
 *      https://marketplace.visualstudio.com/manage.
 *   2. A Personal Access Token from https://dev.azure.com with
 *      `Marketplace: Manage` scope. Provide it via either:
 *        - environment variable:  $env:VSCE_PAT=<token> (PowerShell)
 *                                export VSCE_PAT=<token> (bash/zsh)
 *        - prior interactive login: npx @vscode/vsce login <publisher>
 *
 * Usage:
 *   cd vscode-extension
 *   $env:VSCE_PAT = "<token>"        # or skip if you've `vsce login`ed
 *   npm run release                   # or: node scripts/publish.mjs
 *
 * Flow (all guards fail fast BEFORE any build work):
 *   1. validate & bump CHANGELOG.md ([Unreleased] → [<version>] - <today>)
 *   2. rustup target add <host triple>   (idempotent)
 *   3. cargo build --release --target <triple> -p as-lsp
 *   4. npm run package                  (MYAS_TARGET set → platform .vsix)
 *   5. npx vsce publish --packagePath <vsix>
 *
 * This pushes **only the current host's** .vsix. Marketplace
 * accepts multiple platform-specific uploads under the same
 * extension ID — run this script on each OS you want to support.
 */

import { existsSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

import {
  detectHostTarget,
  ensureRustTarget,
  buildLspRelease,
  packageVsix,
  extensionRoot,
  repoRoot,
  run,
} from './lib/host-target.mjs';

/// Numeric comparison of two `x.y.z` version strings (> 0 when a is newer).
function compareVersions(a, b) {
  const pa = a.split('.').map(Number);
  const pb = b.split('.').map(Number);
  for (let i = 0; i < 3; i += 1) {
    if (pa[i] !== pb[i]) return pa[i] - pb[i];
  }
  return 0;
}

/// Latest released version in the CHANGELOG body, or null when no
/// released section exists yet. Only real release headings
/// (`## [x.y.z] - yyyy-mm-dd`) count. Sections are maintained
/// newest-first, but we take the numeric max anyway to stay
/// robust against manual reordering.
function latestReleasedVersion(content) {
  const re = /^## \[(\d+\.\d+\.\d+)\] - \d{4}-\d{2}-\d{2}[ \t]*\r?$/gm;
  let latest = null;
  for (const m of content.matchAll(re)) {
    if (latest === null || compareVersions(m[1], latest) > 0) {
      latest = m[1];
    }
  }
  return latest;
}

/// Semi-automatic changelog bump: validate the [Unreleased] section
/// is non-empty, then rename it to `[<version>] - <today>` and
/// re-open a fresh empty [Unreleased] on top. The file edit lands
/// before `vsce package` so the marketplace-published .vsix carries
/// the updated Changelog tab. The git commit happens later, only
/// after `vsce publish` succeeds (opt in with --git).
///
/// Multi-platform releases run this script once per target with the
/// SAME package.json version. When that version already equals the
/// latest released section in the CHANGELOG, this is a re-run for
/// another platform: skip the bump entirely (no [Unreleased]
/// requirement, no file edit).
///
/// Validation guards (all fail fast BEFORE any build work):
///   - CHANGELOG.md must exist next to package.json.
///   - Unless this is a same-version re-run (see above), it must
///     contain a `## [Unreleased]` heading.
///   - That section must have at least one real entry (HTML
///     comments are ignored). An empty Unreleased aborts release
///     unless --force-changelog is passed.
///   - The target `## [<version>] - <date>` heading must not already
///     exist (prevents double-releasing the same version).
///
/// Returns true when the CHANGELOG was modified, false when skipped.
function bumpChangelog(version) {
  const changelogPath = join(extensionRoot, 'CHANGELOG.md');
  if (!existsSync(changelogPath)) {
    console.error('[publish] CHANGELOG.md not found at', changelogPath);
    console.error('[publish] create it before releasing.');
    process.exit(1);
  }

  const today = new Date().toISOString().slice(0, 10);
  const unreleasedHeader = '## [Unreleased]';
  const releasedHeader = `## [${version}] - ${today}`;

  const content = readFileSync(changelogPath, 'utf8');

  // Same-version re-run guard: multi-platform releases execute this
  // script once per target with an unchanged package.json version.
  // When that version is already the latest released section, this
  // run only ships another platform — require no [Unreleased]
  // entries and leave the CHANGELOG untouched.
  const latest = latestReleasedVersion(content);
  if (latest === version) {
    console.log(`[publish] CHANGELOG latest release is already [${version}] — same-version re-run, skipping changelog bump.`);
    return false;
  }

  if (content.includes(releasedHeader)) {
    console.error(`[publish] CHANGELOG.md already contains "${releasedHeader}".`);
    console.error('[publish] looks like this version was already released.');
    process.exit(1);
  }

  const idx = content.indexOf(unreleasedHeader);
  if (idx === -1) {
    console.error(`[publish] CHANGELOG.md missing a "${unreleasedHeader}" section.`);
    process.exit(1);
  }

  // Body of [Unreleased] = text between its heading line and the
  // next top-level "## " heading (or EOF). HTML comments are
  // stripped before the emptiness check so a lone guidance comment
  // does not satisfy the "must write entries" requirement.
  const afterHeading = content.indexOf('\n', idx) + 1;
  const nextHeadingRel = content.indexOf('\n## ', afterHeading);
  const bodyEnd = nextHeadingRel === -1 ? content.length : nextHeadingRel;
  const body = content
    .slice(afterHeading, bodyEnd)
    .replace(/<!--[\s\S]*?-->/g, '')
    .trim();

  if (body.length === 0) {
    console.error('[publish] [Unreleased] section is empty.');
    console.error('[publish] add changelog entries under [Unreleased] before releasing,');
    console.error('[publish] or re-run with --force-changelog to bypass (not recommended).');
    if (!process.argv.includes('--force-changelog')) {
      process.exit(1);
    }
    console.warn('[publish] --force-changelog set; proceeding with empty changelog.');
  }

  // `before` = everything up to the [Unreleased] heading (any
  // guidance comment stays pinned above the freshly reopened
  // [Unreleased]). `rest` = the old [Unreleased] body + all
  // following sections; it now becomes the body of the released
  // version.
  const before = content.slice(0, idx);
  const rest = content.slice(idx + unreleasedHeader.length);
  const newContent = before + `${unreleasedHeader}\n\n` + releasedHeader + rest;
  writeFileSync(changelogPath, newContent);

  console.log(`[publish] CHANGELOG: [Unreleased] -> [${version}] - ${today}`);
  return true;
}

/// Newest .vsix in the extension root. We re-discover the file
/// name instead of guessing it: vsce's platform-specific naming
/// has changed across versions, and stale .vsix files from earlier
/// manual runs may also be around — the newest mtime wins.
function findLatestVsix() {
  const candidates = readdirSync(extensionRoot)
    .filter((f) => f.endsWith('.vsix'))
    .map((f) => ({ name: f, mtime: statSync(join(extensionRoot, f)).mtimeMs }))
    .sort((a, b) => b.mtime - a.mtime);
  if (candidates.length === 0) return null;
  if (candidates.length > 1) {
    console.log(`[publish] multiple .vsix files found; using newest: ${candidates[0].name}`);
  }
  return candidates[0].name;
}

const pkg = JSON.parse(readFileSync(join(extensionRoot, 'package.json'), 'utf8'));
const { target, triple } = detectHostTarget();

// `vsce` itself will fall back to an interactive prompt if no
// credentials are available, but that defeats the "manual script
// I can run once" UX. Warn early so the user knows to set
// VSCE_PAT — we don't hard-fail because `vsce login` caches
// credentials elsewhere and works without the env var.
if (!process.env.VSCE_PAT) {
  console.warn(
    '[publish] warning: VSCE_PAT is not set. vsce will fall back to ' +
      'cached credentials from `vsce login`. If publishing fails with ' +
      '"Authentication failed", set VSCE_PAT and retry.',
  );
}

console.log('--------------------------------------------------');
console.log(`[publish] host      : ${process.platform}-${process.arch}`);
console.log(`[publish] target    : ${target}`);
console.log(`[publish] publisher : ${pkg.publisher}`);
console.log(`[publish] name      : ${pkg.name}`);
console.log(`[publish] version   : ${pkg.version}`);
console.log('--------------------------------------------------');

// Fail fast on an empty/missing changelog BEFORE we spend time
// building the Rust server and packaging the .vsix. Returns false
// (no-op) when package.json's version already matches the latest
// CHANGELOG release — i.e. a multi-platform re-run.
const changelogBumped = bumpChangelog(pkg.version);

ensureRustTarget(triple);
buildLspRelease(triple);
packageVsix(target);

const vsixName = findLatestVsix();
if (!vsixName) {
  console.error(`[publish] no .vsix found in ${extensionRoot}`);
  console.error(`[publish] did \`npm run package\` succeed with MYAS_TARGET=${target}?`);
  process.exit(1);
}
const vsixPath = join(extensionRoot, vsixName);
console.log(`[publish] publishing ${vsixPath}`);

// `vsce publish --packagePath` auto-derives the --target from the
// platform tag embedded in the .vsix by our earlier
// `vsce package --target ...` call, so we don't need to pass it
// again here.
run(`npx @vscode/vsce publish --packagePath "${vsixPath}"`, { cwd: extensionRoot });

// By default the script leaves git alone after a successful publish
// — you commit the renamed CHANGELOG.md yourself. Pass --git to
// have the script auto-commit it instead (useful in CI).
if (process.argv.includes('--git')) {
  if (!changelogBumped) {
    console.log('[publish] --git set but CHANGELOG was not modified; skipping commit.');
  } else {
    run(`git add vscode-extension/CHANGELOG.md`, { cwd: repoRoot });
    run(`git commit -m "chore(changelog): release ${pkg.version}"`, { cwd: repoRoot });
  }
}

console.log('');
console.log(`[publish] done!`);
console.log(`[publish] Manage: https://marketplace.visualstudio.com/manage/publishers/${pkg.publisher}`);
