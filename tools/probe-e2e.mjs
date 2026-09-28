// e2e 探针（Phase B 收官验收）：stdio 直连 release as-lsp，验证
// hover / references / rename 三件套 + 冷启动 rebuild 时间。
// 用法：node tools/probe-e2e.mjs
//   （需先 cargo build --release -p as-lsp；语料目录取 config/paths.local.yaml
//     的 paths.test_as，与 tools/test-extension.ps1 同一约定）
import { spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join, resolve } from 'node:path';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');

// 读 config/paths.local.yaml 的 paths.test_as（扁平单层，与 test-extension.ps1 同款解析）
const pathsFile = readFileSync(join(repoRoot, 'config/paths.local.yaml'), 'utf8');
const testAs = pathsFile.match(/^\s*test_as:\s*"([^"]*)"/m)?.[1];
if (!testAs) throw new Error('config/paths.local.yaml 缺 paths.test_as');
const SERVER = join(repoRoot, 'lsp/target/release/as-lsp.exe');
const WS = 'file:///' + testAs.replace(/\\/g, '/').replace(/:\/?/, ':/');

// 行号/列号（UTF-16，ASCII 即字符数）：
//  0: struct ProbeLocal { int Field; }
//  1: void ProbeMain()
//  2: {
//  3:     ProbeLocal L;
//  4:     int F = L.Field;
//  5:     FVector V;
//  6:     float X = V.X;
//  7: }
const doc = [
  'struct ProbeLocal { int Field; }',
  'void ProbeMain()',
  '{',
  '    ProbeLocal L;',
  '    int F = L.Field;',
  '    FVector V;',
  '    float X = V.X;',
  '}',
  '',
].join('\n');
const uri = WS + '/probe_e2e_tmp.as';

const proc = spawn(SERVER, [], { stdio: ['pipe', 'pipe', 'pipe'] });
let buf = Buffer.alloc(0);
let msgId = 0;
const pending = new Map();
let ready = false;
let readyAt = 0;

function send(obj) {
  const body = JSON.stringify(obj);
  proc.stdin.write(`Content-Length: ${Buffer.byteLength(body)}\r\n\r\n${body}`);
}
function request(method, params) {
  return new Promise((resolve, reject) => {
    const id = ++msgId;
    pending.set(id, { resolve, reject });
    send({ jsonrpc: '2.0', id, method, params });
  });
}
function notify(method, params) {
  send({ jsonrpc: '2.0', method, params });
}

proc.stdout.on('data', (d) => {
  buf = Buffer.concat([buf, d]);
  for (;;) {
    const idx = buf.indexOf('\r\n\r\n');
    if (idx < 0) break;
    const header = buf.slice(0, idx).toString();
    const m = /Content-Length:\s*(\d+)/i.exec(header);
    if (!m) throw new Error('bad header: ' + header);
    const len = +m[1];
    if (buf.length < idx + 4 + len) break;
    const body = buf.slice(idx + 4, idx + 4 + len).toString();
    buf = buf.slice(idx + 4 + len);
    const msg = JSON.parse(body);
    if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined)) {
      const p = pending.get(msg.id);
      if (p) {
        pending.delete(msg.id);
        msg.error ? p.reject(new Error(JSON.stringify(msg.error))) : p.resolve(msg.result);
      }
    } else if (msg.id !== undefined && msg.method !== undefined) {
      // server → client 请求：必须应答，否则 initialized 的配置拉取会挂起
      if (msg.method === 'workspace/configuration') {
        const items = msg.params?.items ?? [];
        const result = items.map(() => ({
          floatIsFloat64: true,
          scriptRoots: [],
          typeDeclarationDirs: ['${workspaceFolder}/Saved/AS-Cache'],
          debug: { fileLog: false },
        }));
        send({ jsonrpc: '2.0', id: msg.id, result });
      } else {
        send({ jsonrpc: '2.0', id: msg.id, error: { code: -32601, message: 'probe: method not found' } });
      }
    } else if (msg.method === 'myas/indexStatus' && msg.params?.state === 'ready') {
      if (!ready) {
        ready = true;
        readyAt = Date.now();
        console.log(`[index] ready: files=${msg.params.files} elapsedMs=${msg.params.elapsedMs}`);
      }
    }
  }
});
proc.stderr.on('data', (d) => process.stderr.write('[srv] ' + d));
proc.on('exit', (c) => { if (!procKilled) { console.error('server exited:', c); process.exit(1); } });
let procKilled = false;

let pass = 0, fail = 0;
function check(name, cond, detail) {
  if (cond) { pass++; console.log(`  PASS ${name}`); }
  else { fail++; console.log(`  FAIL ${name} — ${detail ?? ''}`); }
}

// ── 握手 ──────────────────────────────────────────────────────────────
const t0 = Date.now();
const init = await request('initialize', {
  processId: null,
  rootUri: WS,
  workspaceFolders: [{ uri: WS, name: 'corpus' }],
  capabilities: { textDocument: { hover: {}, references: {}, rename: { prepareSupport: true } } },
  clientInfo: { name: 'probe', version: '0' },
});
check('initialize 返回能力表', !!init?.capabilities?.hoverProvider, JSON.stringify(init?.capabilities).slice(0, 100));
notify('initialized', {});
notify('textDocument/didOpen', {
  textDocument: { uri, languageId: 'angelscript-asl', version: 1, text: doc },
});

// 等索引就绪（最多 60s）
while (!ready && Date.now() - t0 < 60000) await new Promise((r) => setTimeout(r, 100));
check('索引就绪（myas/indexStatus ready）', ready, '60s 内未收到');
if (!ready) { proc.kill(); process.exit(1); }

// ── hover ─────────────────────────────────────────────────────────────
console.log('── hover ──');
const hVec = await request('textDocument/hover', { textDocument: { uri }, position: { line: 5, character: 6 } });
check('hover FVector（引擎类型）', !!hVec?.contents && JSON.stringify(hVec.contents).includes('struct FVector'), JSON.stringify(hVec).slice(0, 200));

const hLocal = await request('textDocument/hover', { textDocument: { uri }, position: { line: 3, character: 6 } });
check('hover ProbeLocal（本文件声明）', JSON.stringify(hLocal).includes('struct ProbeLocal'), JSON.stringify(hLocal).slice(0, 200));

const hField = await request('textDocument/hover', { textDocument: { uri }, position: { line: 4, character: 15 } });
check('hover Field（成员访问）', JSON.stringify(hField).includes('int Field'), JSON.stringify(hField).slice(0, 200));

const hX = await request('textDocument/hover', { textDocument: { uri }, position: { line: 6, character: 16 } });
check('hover V.X 的 X（链式成员定型）', JSON.stringify(hX).includes('float X'), JSON.stringify(hX).slice(0, 200));

// ── references ────────────────────────────────────────────────────────
console.log('── references ──');
const refLocal = await request('textDocument/references', {
  textDocument: { uri }, position: { line: 3, character: 6 }, context: { includeDeclaration: false },
});
check('references ProbeLocal = 1 使用点（声明不计）', Array.isArray(refLocal) && refLocal.length === 1
  && refLocal[0].range.start.line === 3, JSON.stringify(refLocal));

const refField = await request('textDocument/references', {
  textDocument: { uri }, position: { line: 0, character: 26 }, context: { includeDeclaration: true },
});
// 声明（line 0）+ 使用（line 4）
check('references Field（含声明）= 2', Array.isArray(refField) && refField.length === 2, JSON.stringify(refField));

const refVec = await request('textDocument/references', {
  textDocument: { uri }, position: { line: 5, character: 6 }, context: { includeDeclaration: false },
});
check('references FVector 跨文件命中（语料 ~2800 站点）', Array.isArray(refVec) && refVec.length > 1000, `got ${refVec?.length}`);
const vecInCore = (refVec ?? []).some((l) => l.uri.includes('Core.d.as'));
check('references FVector 命中跨文件（Core.d.as）', vecInCore);

// ── rename ────────────────────────────────────────────────────────────
console.log('── rename ──');
const prep = await request('textDocument/prepareRename', { textDocument: { uri }, position: { line: 0, character: 26 } });
check('prepareRename Field 给出占位名', prep?.placeholder === 'Field', JSON.stringify(prep));

const ren = await request('textDocument/rename', {
  textDocument: { uri }, position: { line: 0, character: 26 }, newName: 'RenamedField',
});
// URI key 可能被服务端百分号编码（D%3A）且盘符大小写规范化——解码后忽略大小写匹配
const renEntries = Object.entries(ren?.changes ?? {});
const ourEdits = renEntries.find(([k]) => decodeURIComponent(k).toLowerCase() === uri.toLowerCase())?.[1] ?? [];
check('rename Field → 2 处编辑（声明 + 使用）', ourEdits.length === 2
  && ourEdits.some((e) => e.range.start.line === 0 && e.newText === 'RenamedField')
  && ourEdits.some((e) => e.range.start.line === 4), JSON.stringify(ren).slice(0, 300));

const renVec = await request('textDocument/rename', {
  textDocument: { uri }, position: { line: 5, character: 6 }, newName: 'FVector2',
});
const vecEditFiles = Object.keys(renVec?.changes ?? {}).length;
const vecEditCount = Object.values(renVec?.changes ?? {}).reduce((a, v) => a + v.length, 0);
console.log(`  INFO rename FVector（引擎类型）：${vecEditFiles} 文件 / ${vecEditCount} 编辑（策略性观察项——.d.as 只读保护不属 Phase B 范围）`);

// ── didChange 后保鲜（重索引路径） ─────────────────────────────────────
console.log('── didChange 增量 ──');
notify('textDocument/didChange', {
  textDocument: { uri, version: 2 },
  contentChanges: [{ text: doc.replace('int Field;', 'int Field2;') }],
});
await new Promise((r) => setTimeout(r, 300)); // 惰性重索引在下一语义请求时发生
const hField2 = await request('textDocument/hover', {
  textDocument: { uri }, position: { line: 0, character: 26 + 0 },
});
// 新文本里 Field2 在同位置；hover 走新索引
const docV2 = doc.replace('int Field;', 'int Field2;');
const col = docV2.indexOf('Field2');
const hField2b = await request('textDocument/hover', { textDocument: { uri }, position: { line: 0, character: col + 2 } });
check('didChange 后 hover 走新内容（Field2）', JSON.stringify(hField2b).includes('int Field2'), JSON.stringify(hField2b).slice(0, 200));

// ── 收尾 ──────────────────────────────────────────────────────────────
console.log(`\n==== ${pass} passed, ${fail} failed ====`);
procKilled = true;
proc.kill();
process.exit(fail > 0 ? 1 : 0);
