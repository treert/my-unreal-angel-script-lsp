// M4 e2e：DidChangeWatchedFiles 全生命周期（规划 §5.3 / §9 M4 验收）。
// 临时工作区（单 workspace 根，Saved/AS-Cache 对齐默认 typeDeclarationDirs）：
// ① 冷启动 Ready；② .as 新增 → workspaceSymbol 可见；③ .as 删除 → 不可见
// （幽灵符号不得残留）；④ 同路径复活（FileId 复用）；⑤ .d.as 连发事件 →
// 防抖后整目录重建生效；⑥ .d.as 删除 → 类型消失。
// 用法: node tests/lsp-e2e-m4.mjs [path-to-as-lsp.exe]
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const exe = process.argv[2]
  ?? 'd:/WorkGit/my-angel-script-lsp/lsp/target/release/as-lsp.exe';

const tmp = mkdtempSync(join(tmpdir(), 'as-lsp-m4-'));
const scriptDir = tmp; // scriptRoots 空 = 全部 workspaceFolders
const cacheDir = join(tmp, 'Saved', 'AS-Cache');
mkdirSync(cacheDir, { recursive: true });

writeFileSync(join(scriptDir, 'MyLib.as'),
  'class MyLib { int Value; }\nvoid UseLib() { MyLib L; int A = L.Value; }\n');
writeFileSync(join(cacheDir, 'Types.d.as'), 'struct FVector { float X; }\n');

const p = spawn(exe, [], { stdio: ['pipe', 'pipe', 'pipe'] });
p.stderr.on('data', (c) => console.error('[server-stderr]', c.toString()));
p.on('exit', (c) => console.error('[server-exit]', c));

// framing 按字节切分（见 lsp-smoke.mjs 同注）
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
    if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined)) {
      responses.set(msg.id, msg);
    } else if (msg.id !== undefined) {
      notifications.push(msg);
      // server -> client 请求：unit 结果的应答 null，其余（configuration）应答 []
      const unit = msg.method === 'window/workDoneProgress/create'
        || msg.method === 'client/registerCapability';
      send({ jsonrpc: '2.0', id: msg.id, result: unit ? null : [] });
    } else if (msg.method) {
      notifications.push(msg);
    }
  }
});

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const fail = (m) => { console.error('M4 E2E FAIL:', m); cleanup(1); };
function cleanup(code) {
  try { rmSync(tmp, { recursive: true, force: true }); } catch {}
  p.kill();
  process.exit(code);
}
const toUri = (s) => 'file:///' + s.replace(/\\/g, '/').replace(/:/g, '%3A');
const libUri = toUri(join(scriptDir, 'MyLib.as'));
const libSrc = 'class MyLib { int Value; }\nvoid UseLib() { MyLib L; int A = L.Value; }\n';

let reqId = 0;
async function request(method, params, timeoutMs = 5000) {
  const id = ++reqId;
  send({ jsonrpc: '2.0', id, method, params });
  for (let i = 0; i < timeoutMs / 50; i++) {
    await sleep(50);
    if (responses.has(id)) return responses.get(id);
  }
  fail(`no response for ${method}`);
}

async function symbolQuery(name) {
  const r = await request('workspace/symbol', { query: name });
  return Array.isArray(r?.result) ? r.result : [];
}

async function hoverAt(uri, line, character) {
  const r = await request('textDocument/hover',
    { textDocument: { uri }, position: { line, character } });
  return r?.result ?? null;
}

(async () => {
  send({
    jsonrpc: '2.0', id: 1, method: 'initialize', params: {
      capabilities: {
        workspace: { didChangeWatchedFiles: { dynamicRegistration: true } },
      },
      processId: null, rootUri: null,
      workspaceFolders: [{ uri: toUri(scriptDir), name: 'tmp-ws' }],
    },
  });
  await sleep(400);
  send({ jsonrpc: '2.0', method: 'initialized', params: {} });
  await sleep(200);
  const registered = notifications.some((n) => n.method === 'client/registerCapability'
    && n.params?.registrations?.some((r) => r.method === 'workspace/didChangeWatchedFiles'));
  console.log(`watcher registered: ${registered}`);
  send({
    jsonrpc: '2.0', method: 'textDocument/didOpen',
    params: { textDocument: { uri: libUri, languageId: 'angelscript-asl', version: 1, text: libSrc } },
  });

  // ① Ready 轮询：hover line 1 的 `MyLib` 类型使用点（char 16 = M）
  let ready = false;
  for (let i = 0; i < 60; i++) {
    const h = await hoverAt(libUri, 1, 16); // `MyLib L;` 的 MyLib
    if (h) { ready = true; break; }
  }
  if (!ready) fail('index never became ready');
  console.log('ready');

  // ② .as 新增 → workspaceSymbol 可见
  const newMod = join(scriptDir, 'NewMod.as');
  writeFileSync(newMod, 'class NewType { int N; }\n');
  send({
    jsonrpc: '2.0', method: 'workspace/didChangeWatchedFiles',
    params: { changes: [{ uri: toUri(newMod), type: 1 }] },
  });
  await sleep(600);
  if (!(await symbolQuery('NewType')).some((s) => s.name === 'NewType')) {
    fail('.as create: NewType not visible in workspaceSymbol');
  }
  console.log('create OK');

  // ③ .as 删除 → 不可见（幽灵符号不得残留）
  rmSync(newMod);
  send({
    jsonrpc: '2.0', method: 'workspace/didChangeWatchedFiles',
    params: { changes: [{ uri: toUri(newMod), type: 3 }] },
  });
  await sleep(600);
  if ((await symbolQuery('NewType')).some((s) => s.name === 'NewType')) {
    fail('.as delete: NewType still visible (ghost symbol)');
  }
  console.log('delete OK');

  // ④ 同路径复活（FileId 复用，D18）
  writeFileSync(newMod, 'class NewType { int N; int M; }\n');
  send({
    jsonrpc: '2.0', method: 'workspace/didChangeWatchedFiles',
    params: { changes: [{ uri: toUri(newMod), type: 1 }] },
  });
  await sleep(600);
  if (!(await symbolQuery('NewType')).some((s) => s.name === 'NewType')) {
    fail('.as revive: NewType not visible after re-create');
  }
  console.log('revive OK');

  // ⑤ .d.as 连发事件（模拟导出器清空重写）→ 防抖后整目录重建生效
  const declFile = join(cacheDir, 'NewDecl.d.as');
  const burst = [];
  for (let i = 0; i < 50; i++) {
    burst.push({ uri: toUri(declFile), type: 1 });
    burst.push({ uri: toUri(join(cacheDir, 'Types.d.as')), type: 2 });
  }
  writeFileSync(declFile, 'struct DeclOnly { int Q; }\n');
  send({ jsonrpc: '2.0', method: 'workspace/didChangeWatchedFiles', params: { changes: burst } });
  // 500ms 静默窗 + 重建（小工作区秒内）——裕量 3s
  await sleep(3000);
  if (!(await symbolQuery('DeclOnly')).some((s) => s.name === 'DeclOnly')) {
    fail('.d.as debounce: DeclOnly not visible after rebuild');
  }
  console.log('decl debounce OK');

  // ⑥ .d.as 删除 → 类型消失（MyLib 打开中不受 overlay 干扰——事件文件未打开）
  rmSync(declFile);
  send({
    jsonrpc: '2.0', method: 'workspace/didChangeWatchedFiles',
    params: { changes: [{ uri: toUri(declFile), type: 3 }] },
  });
  await sleep(3000);
  if ((await symbolQuery('DeclOnly')).some((s) => s.name === 'DeclOnly')) {
    fail('.d.as delete: DeclOnly still visible');
  }
  console.log('decl delete OK');

  console.log('M4 E2E OK: watched-files lifecycle (create/delete/revive + .d.as debounce)');
  cleanup(0);
})().catch((e) => { console.error(e); cleanup(1); });
