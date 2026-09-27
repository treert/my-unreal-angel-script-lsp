// 真实工作区端到端验收（手工工具，路径为本机 Demo_AS）：双根
// （Script + AS-Cache，对齐 test-as-lsp.code-workspace）→ 冷启动 →
// hover 命中 struct FVector → definition 落到 Core.d.as（架构设计 §2.2.1
// 记录的落点 :10103）。用法: node tests/lsp-e2e-workspace.mjs
import { spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';

const exe = 'd:/WorkGit/my-angel-script-lsp/lsp/target/release/as-lsp.exe';
const scriptRoot = 'd:/WorkGit/UEProjs/Demo_AS/Script';
const cacheRoot = 'd:/WorkGit/UEProjs/Demo_AS/Saved/AS-Cache';
const target = `${scriptRoot}/Script-Examples/Examples/Example_MovingObject.as`;

const src = readFileSync(target, 'utf8');
const uri = 'file:///' + target.replace(/:/g, '%3A').replace(/\\/g, '/');

const p = spawn(exe, [], { stdio: ['pipe', 'pipe', 'pipe'] });
p.stderr.on('data', (c) => console.error('[server-stderr]', c.toString()));
p.on('exit', (c) => console.error('[server-exit]', c));
let buf = '';
const responses = new Map();
function send(body) {
  const bytes = Buffer.from(JSON.stringify(body));
  p.stdin.write(`Content-Length: ${bytes.length}\r\n\r\n`);
  p.stdin.write(bytes);
}
p.stdout.on('data', (chunk) => {
  buf += chunk.toString('utf8');
  for (;;) {
    const h = buf.indexOf('\r\n\r\n');
    if (h < 0) break;
    const len = parseInt(/Content-Length: (\d+)/.exec(buf.slice(0, h))[1], 10);
    if (buf.length < h + 4 + len) break;
    const msg = JSON.parse(buf.slice(h + 4, h + 4 + len));
    buf = buf.slice(h + 4 + len);
    if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined)) responses.set(msg.id, msg);
    else if (msg.id !== undefined) send({ jsonrpc: '2.0', id: msg.id, result: [] });
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
  params: { textDocument: { uri, languageId: 'angelscript-asl', version: 1, text: src } } });

// 先定位 FVector 使用点，用它轮询 Ready（没 Ready 时 hover 返回 null）
const lines = src.split(/\r?\n/);
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
if (!(h && h.result)) fail('index never became ready (check server stderr above)');
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
console.log(`E2E OK: hover=struct FVector; definition -> ${decodeURIComponent(locs[0].uri).split('/').pop()}:${locs[0].range.start.line + 1}`);
p.kill();
process.exit(0);
