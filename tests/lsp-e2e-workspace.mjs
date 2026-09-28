// 真实工作区端到端验收（手工工具）：双根（Script + AS-Cache）→ 冷启动 →
// hover 命中 struct FVector → definition 落到 Core.d.as（架构设计 §2.2.1
// 记录的落点 :10103）。用法: node tests/lsp-e2e-workspace.mjs [path-to-as-lsp.exe]
// 语料根取 config/paths.local.yaml 的 paths.demo_as；未配置或目录不存在时跳过。
import { spawn } from 'node:child_process';
import { readFileSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

// config/paths.local.yaml 微型解析（schema 见 config/paths.example.yaml，
// 仅支持单层 `paths:` 下的 `key: "value"` 行——PS1/Node 均无内置 YAML，
// 结构固定故手写）
function readPaths() {
  const file = fileURLToPath(new URL('../config/paths.local.yaml', import.meta.url));
  try {
    const text = readFileSync(file, 'utf8');
    const paths = {};
    let inPaths = false;
    for (const line of text.split(/\r?\n/)) {
      if (!inPaths) {
        if (/^paths:\s*$/.test(line)) inPaths = true;
        continue;
      }
      if (/^\S/.test(line)) break; // 顶层键：paths 节结束
      const m = /^\s*([A-Za-z0-9_]+):\s*"([^"]*)"/.exec(line);
      if (m) paths[m[1]] = m[2];
    }
    return paths;
  } catch {
    return {};
  }
}

const exe = process.argv[2]
  ?? fileURLToPath(new URL('../lsp/target/release/as-lsp.exe', import.meta.url));
const demoAs = readPaths().demo_as ?? '';
const scriptRoot = `${demoAs}/Script`;
const cacheRoot = `${demoAs}/Saved/AS-Cache`;
if (!demoAs || !existsSync(scriptRoot) || !existsSync(cacheRoot)) {
  console.error('SKIP: paths.demo_as 未配置或目录不存在（config/paths.local.yaml，'
    + '模板见 config/paths.example.yaml）。LSP 日常测试可用 paths.test_as 代替。');
  console.error(`      paths.demo_as = ${demoAs || '(empty)'}`);
  process.exit(0);
}
const target = `${scriptRoot}/Script-Examples/Examples/Example_MovingObject.as`;

const src = readFileSync(target, 'utf8');
const uri = 'file:///' + target.replace(/:/g, '%3A').replace(/\\/g, '/');

// M5：didOpen 用内置 probe 文本（overlay 唯一真值，D18——hover/definition/
// references 断言不依赖原文件内容，FVector 字样仍在）。两处补全锚点：
//   line4 `V.` 行尾 → Member（FVector 成员集）
//   line5 `FVector::Zero;` 尾 → Scoped + 前缀 Zero
// 注意行序：首个 FVector 必须在类型位（`FVector V(...)`）——`::` 限定段的
// FVector 解析到 namespace（§2.2.1 语境择一），namespace hover 无签名会
// 让 Ready 轮询假超时。
const probe = [
  'class Probe',
  '{',
  '    void Run()',
  '    {',
  '        FVector V(0.0, 0.0, 0.0);',
  '        FVector::Zero;',
  '        V.',
  '    }',
  '}',
  '',
].join('\n');
const openText = probe;

const p = spawn(exe, [], { stdio: ['pipe', 'pipe', 'pipe'] });
p.stderr.on('data', (c) => console.error('[server-stderr]', c.toString()));
p.on('exit', (c) => console.error('[server-exit]', c));
// framing 按字节切分（Content-Length 是字节数；字符串索引在多字节 UTF-8
// 响应下会错位——M4 踩过，见 lsp-smoke.mjs 同注）
let buf = Buffer.alloc(0);
const responses = new Map();
const notifications = [];
function send(body) {
  const bytes = Buffer.from(JSON.stringify(body));
  p.stdin.write(`Content-Length: ${bytes.length}\r\n\r\n`);
  p.stdin.write(bytes);
}
p.stdout.on('data', (chunk) => {
  buf = Buffer.concat([buf, chunk]);
  for (;;) {
    const h = buf.indexOf('\r\n\r\n');
    if (h < 0) break;
    const len = parseInt(/Content-Length: (\d+)/.exec(buf.slice(0, h).toString('utf8'))[1], 10);
    if (buf.length < h + 4 + len) break;
    const msg = JSON.parse(buf.slice(h + 4, h + 4 + len).toString('utf8'));
    buf = buf.slice(h + 4 + len);
    if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined)) responses.set(msg.id, msg);
    else if (msg.id !== undefined) {
      notifications.push(msg);
      const isProgressCreate = msg.method === 'window/workDoneProgress/create';
      let result = isProgressCreate ? null : [];
      // workspace/configuration：回 [] 会反序列化失败（M4 已知坑）——回真实
      // 配置对象（debug.fileLog 开日志拿重放证据）
      if (msg.method === 'workspace/configuration') {
        result = [{ floatIsFloat64: true, debug: { fileLog: true } }];
      }
      send({ jsonrpc: '2.0', id: msg.id, result });
    } else if (msg.method) notifications.push(msg);
  }
});
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const fail = (m) => { console.error('E2E FAIL:', m); p.kill(); process.exit(1); };

const toUri = (s) => 'file:///' + s.replace(/:/g, '%3A').replace(/\\/g, '/');
send({
  jsonrpc: '2.0', id: 1, method: 'initialize', params: {
    capabilities: {}, processId: null, rootUri: null,
    workspaceFolders: [
      { uri: toUri(scriptRoot), name: 'Script' },
      { uri: toUri(cacheRoot), name: 'AS-Cache' },
    ],
  },
});
await sleep(500);
send({ jsonrpc: '2.0', method: 'initialized', params: {} });
await sleep(200);
send({ jsonrpc: '2.0', method: 'textDocument/didOpen',
  params: { textDocument: { uri, languageId: 'angelscript-asl', version: 1, text: openText } } });

// 先定位 FVector 使用点，用它轮询 Ready（没 Ready 时 hover 返回 null）
const lines = openText.split(/\r?\n/);
function findPos(re) {
  for (let l = 0; l < lines.length; l++) {
    const m = re.exec(lines[l]);
    if (m) return { line: l, character: m.index };
  }
  return null;
}
const fv = findPos(/\bFVector\b/);
if (!fv) fail('no FVector in file');
console.error('FVector at', JSON.stringify(fv), '->', JSON.stringify(lines[fv.line].slice(fv.character, fv.character + 40)));

const t0 = Date.now();
let h = null;
for (let i = 0; i < 120; i++) {
  send({ jsonrpc: '2.0', id: 100 + i, method: 'textDocument/hover',
    params: { textDocument: { uri }, position: fv } });
  await sleep(500);
  h = responses.get(100 + i);
  if (h && h.result) break;
}
console.error(`poll done after ${Date.now() - t0}ms, ready: ${!!(h && h.result)}`);
if (!(h && h.result)) {
  console.error('indexStatus notifications:',
    JSON.stringify(notifications.filter((n) => n.method === 'myas/indexStatus').map((n) => n.params)));
  console.error('all notification methods:',
    JSON.stringify([...new Set(notifications.map((n) => n.method))]));
  console.error('last hover response:', JSON.stringify(h));
  fail('index never became ready (check server stderr above)');
}
const hv = h?.result?.contents?.value ?? '';
if (!hv.includes('```angelscript_snippet')) fail(`hover no fence: ${hv.slice(0, 200)}`);
if (!/struct\s+FVector/.test(hv)) fail(`hover no 'struct FVector': ${hv.slice(0, 300)}`);

send({ jsonrpc: '2.0', id: 201, method: 'textDocument/definition',
  params: { textDocument: { uri }, position: fv } });
await sleep(600);
const d = responses.get(201);
const locs = Array.isArray(d?.result) ? d.result : d?.result ? [d.result] : [];
if (!locs.length) fail('definition no locations');
if (!locs[0].uri.includes('.d.as')) fail(`definition not in .d.as: ${locs[0].uri}`);
console.log(`definition -> ${decodeURIComponent(locs[0].uri).split('/').pop()}:${locs[0].range.start.line + 1}`);

// M4：references（FVector——.d.as + 脚本两侧全工作区引用）+ $/progress。
// 首次 references 需解析全部候选文件的 UseSite（后续命中缓存），轮询等待
let r = null;
for (let i = 0; i < 60; i++) {
  if (!r) {
    send({ jsonrpc: '2.0', id: 202, method: 'textDocument/references',
      params: { textDocument: { uri }, position: fv, context: { includeDeclaration: false } } });
  }
  await sleep(1000);
  r = responses.get(202);
  if (r) break;
}
if (!r) fail('references: no response within 60s');
const refs = Array.isArray(r?.result) ? r.result : [];
if (refs.length < 100) fail(`references of FVector too few: ${refs.length}`);
const declFiles = new Set(refs.map((l) => decodeURIComponent(l.uri).split('/').pop()));
if (refs.some((l) => !decodeURIComponent(l.uri).endsWith('.d.as') && !decodeURIComponent(l.uri).endsWith('.as'))) {
  fail('references uri not .as');
}
const progressMsgs = notifications.filter((n) => n.method === '$/progress');
const beginOk = progressMsgs.some((n) => n.params?.value?.kind === 'begin');
const endOk = progressMsgs.some((n) => n.params?.value?.kind === 'end');
if (!beginOk || !endOk) fail(`$/progress begin/end missing (${progressMsgs.length} msgs)`);
// myas/indexStatus：真实工作区应报告全部收集文件（27 script + 414 decl）
const ready = notifications.filter((n) => n.method === 'myas/indexStatus').map((n) => n.params);
if (!ready.some((p) => p?.state === 'ready' && p.files === 441)) {
  fail(`myas/indexStatus expected 441 files, got: ${JSON.stringify(ready)}`);
}
console.log(`references -> ${refs.length} sites across ${declFiles.size} files; $/progress msgs=${progressMsgs.length}; indexStatus files=${ready[ready.length - 1]?.files}`);

// ---- M5b: completion（真实工作区索引上的两类语境）----
// ① Scoped：line5 `        FVector::Zero;` —— char20 = "Zero" 尾端点（前缀 Zero）
const scopedPos = { line: 5, character: 20 };
send({ jsonrpc: '2.0', id: 203, method: 'textDocument/completion',
  params: { textDocument: { uri }, position: scopedPos } });
await sleep(800);
const c1 = responses.get(203);
if (!c1 || !c1.result) fail(`no completion #203: ${JSON.stringify(c1)}`);
const items1 = c1.result.items ?? c1.result;
if (!items1.some((i) => i.label === 'ZeroVector')) {
  fail(`Scoped FVector:: should offer ZeroVector (prefix Zero): ${JSON.stringify(items1.slice(0, 10))}`);
}
// ② Member：line6 `        V.` 行尾 → FVector 成员（字段 X/Y/Z + 方法 Dot）
const memberPos = { line: 6, character: 10 };
send({ jsonrpc: '2.0', id: 204, method: 'textDocument/completion',
  params: { textDocument: { uri }, position: memberPos } });
await sleep(800);
const c2 = responses.get(204);
if (!c2 || !c2.result) fail(`no completion #204: ${JSON.stringify(c2)}`);
const items2 = c2.result.items ?? c2.result;
const labels2 = items2.map((i) => i.label);
for (const want of ['X', 'Y', 'Z', 'DotProduct']) {
  if (!labels2.includes(want)) fail(`member completion missing '${want}': ${JSON.stringify(labels2.slice(0, 20))}`);
}
console.log(`completion -> scoped Zero=${items1.length} items (ZeroVector ok); member FVector=${items2.length} items (X/Y/Z/DotProduct ok)`);

console.log(`E2E OK: hover=struct FVector; definition -> ${decodeURIComponent(locs[0].uri).split('/').pop()}:${locs[0].range.start.line + 1}; references=${refs.length}; completion scoped/member ok`);
p.kill();
process.exit(0);
