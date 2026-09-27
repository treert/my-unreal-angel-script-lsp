// LSP server stdio smoke test: initialize -> didOpen -> semanticTokens/full
// -> documentSymbol -> hover/definition (M3) -> references/prepareRename/
// rename/workspaceSymbol + $/progress (M4). 用例源码内置（D1）。
// 用法: node tests/lsp-smoke.mjs [path-to-as-lsp.exe]
import { spawn } from 'node:child_process';

const exe = process.argv[2]
  ?? 'd:/WorkGit/my-angel-script-lsp/lsp/target/debug/as-lsp.exe';

const src = 'class Foo : UObject\n{\n    int Count;\n    void Tick(float Delta)\n    {\n        Count = Count + 1;\n    }\n}\n';
const uri = 'file:///d%3A/WorkGit/UEProjs/SmokeTest.as';

const p = spawn(exe, [], { stdio: ['pipe', 'pipe', 'pipe'] });
let buf = Buffer.alloc(0);
const responses = new Map(); // id -> parsed result
const notifications = [];

function send(body) {
  const json = JSON.stringify(body);
  const bytes = Buffer.from(json, 'utf8');
  p.stdin.write(`Content-Length: ${bytes.length}\r\n\r\n`);
  p.stdin.write(bytes);
}

// framing 必须按字节切分（Content-Length 是字节数；响应含多字节 UTF-8 时
// 字符串索引会错位——M4 的中文错误消息踩过）
p.stdout.on('data', (chunk) => {
  buf = Buffer.concat([buf, chunk]);
  for (;;) {
    const headerEnd = buf.indexOf('\r\n\r\n');
    if (headerEnd < 0) break;
    const header = buf.slice(0, headerEnd).toString('utf8');
    const m = /Content-Length: (\d+)/.exec(header);
    if (!m) { console.error('BAD HEADER:', JSON.stringify(header)); process.exit(1); }
    const len = parseInt(m[1], 10);
    if (buf.length < headerEnd + 4 + len) break;
    const body = buf.slice(headerEnd + 4, headerEnd + 4 + len).toString('utf8');
    buf = buf.slice(headerEnd + 4 + len);
    const msg = JSON.parse(body);
    if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined)) {
      responses.set(msg.id, msg);
    } else if (msg.method) {
      notifications.push(msg);
      // server -> client 请求：workDoneProgress/create 应答 null（unit），
      // 其余（如 workspace/configuration）应答空数组
      if (msg.id !== undefined) {
        const isProgressCreate = msg.method === 'window/workDoneProgress/create';
        send({ jsonrpc: '2.0', id: msg.id, result: isProgressCreate ? null : [] });
      }
    }
  }
});

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const fail = (msg) => { console.error('SMOKE FAIL:', msg); p.kill(); process.exit(1); };

send({ jsonrpc: '2.0', id: 1, method: 'initialize', params: { capabilities: {}, processId: null, rootUri: null } });
await sleep(600);
send({ jsonrpc: '2.0', method: 'initialized', params: {} });
await sleep(200);
send({
  jsonrpc: '2.0', method: 'textDocument/didOpen',
  params: { textDocument: { uri, languageId: 'angelscript-asl', version: 1, text: src } },
});
await sleep(500);
send({ jsonrpc: '2.0', id: 2, method: 'textDocument/semanticTokens/full', params: { textDocument: { uri } } });
await sleep(500);
send({ jsonrpc: '2.0', id: 3, method: 'textDocument/documentSymbol', params: { textDocument: { uri } } });
await sleep(400);
// M3：hover / definition（空 workspace → 冷启动即时 Ready；overlay 即索引文本）
send({
  jsonrpc: '2.0', id: 4, method: 'textDocument/hover',
  params: { textDocument: { uri }, position: { line: 2, character: 8 } }, // Count 声明名
});
await sleep(300);
send({
  jsonrpc: '2.0', id: 5, method: 'textDocument/definition',
  params: { textDocument: { uri }, position: { line: 3, character: 21 } }, // Delta 形参名
});
await sleep(300);
send({
  jsonrpc: '2.0', id: 6, method: 'textDocument/hover',
  params: { textDocument: { uri }, position: { line: 3, character: 20 } }, // Delta 形参名
});
await sleep(400);
// ---- M4：references / prepareRename / rename / workspaceSymbol ----
// Count 声明（line 2 col 8）；使用点在 line 5（col 8 与 col 16）
send({
  jsonrpc: '2.0', id: 7, method: 'textDocument/references',
  params: {
    textDocument: { uri }, position: { line: 2, character: 8 },
    context: { includeDeclaration: true },
  },
});
await sleep(600);
send({
  jsonrpc: '2.0', id: 8, method: 'textDocument/prepareRename',
  params: { textDocument: { uri }, position: { line: 2, character: 8 } },
});
await sleep(400);
send({
  jsonrpc: '2.0', id: 9, method: 'textDocument/rename',
  params: { textDocument: { uri }, position: { line: 2, character: 8 }, newName: 'Count2' },
});
await sleep(600);
send({ jsonrpc: '2.0', id: 10, method: 'workspace/symbol', params: { query: 'Foo' } });
await sleep(400);
send({
  jsonrpc: '2.0', id: 11, method: 'textDocument/rename',
  params: { textDocument: { uri }, position: { line: 2, character: 8 }, newName: 'class' },
});
await sleep(400);
send({ jsonrpc: '2.0', method: 'exit' });
await sleep(800);

const init = responses.get(1);
if (!init || !init.result) fail('no initialize response');
const caps = init.result.capabilities || {};
if (!caps.semanticTokensProvider) fail('no semanticTokensProvider capability');
const legend = caps.semanticTokensProvider.legend?.tokenTypes ?? [];
if (!legend.includes('as_typename')) fail(`legend missing as_typename: ${JSON.stringify(legend)}`);

const tokensMsg = responses.get(2);
if (!tokensMsg || !tokensMsg.result) fail(`no semanticTokens response: ${JSON.stringify(tokensMsg)}`);
const data = tokensMsg.result.data ?? [];
if (!Array.isArray(data) || data.length === 0 || data.length % 5 !== 0) {
  fail(`bad token data: ${JSON.stringify(data)}`);
}
const tokenCount = data.length / 5;
// 解码前几个 token 核对（delta 编码；Legend：11=as_typename, 4=as_member_variable,
// 17=as_typename_primitive, 8=as_member_function, 2=as_parameter）
const expect = [
  [0, 6, 3, legend.indexOf('as_typename')],       // Foo        (line0 col6)
  [0, 6, 7, legend.indexOf('as_typename')],       // UObject    (同行 col12 = +6)
  [2, 4, 3, legend.indexOf('as_typename_primitive')],  // int    (line2 col4)
  [0, 4, 5, legend.indexOf('as_member_variable')],     // Count  (同行 col8 = +4)
];
for (let i = 0; i < expect.length; i++) {
  const [l, c, len, ty] = expect[i];
  const got = [data[i * 5], data[i * 5 + 1], data[i * 5 + 2], data[i * 5 + 3]];
  if (got[0] !== l || got[1] !== c || got[2] !== len || got[3] !== ty) {
    fail(`token #${i}: got ${JSON.stringify(got)}, want ${JSON.stringify([l, c, len, ty])}`);
  }
}

const symsMsg = responses.get(3);
if (!symsMsg || !symsMsg.result) fail(`no documentSymbol response: ${JSON.stringify(symsMsg)}`);
const foo = symsMsg.result[0];
if (!foo || foo.name !== 'Foo' || foo.children.length !== 2) {
  fail(`bad documentSymbol: ${JSON.stringify(symsMsg.result)}`);
}

const opened = notifications.some((n) => n.method === 'window/logMessage' && /didOpen/.test(n.params?.message ?? ''));

// ---- M3: hover ----
const hoverCount = responses.get(4);
if (!hoverCount || !hoverCount.result) fail(`no hover response: ${JSON.stringify(hoverCount)}`);
const hoverValue = hoverCount.result.contents?.value ?? '';
if (!hoverValue.includes('```angelscript_snippet')) fail(`hover missing snippet fence: ${JSON.stringify(hoverValue)}`);
if (!/int\s+Count/.test(hoverValue)) fail(`hover missing 'int Count': ${hoverValue}`);

// ---- M3: definition（Count 声明自指 → 本文件落点）----
const defCount = responses.get(5);
if (!defCount || !defCount.result) fail(`no definition response: ${JSON.stringify(defCount)}`);
const defArr = Array.isArray(defCount.result) ? defCount.result : [defCount.result];
if (defArr.length === 0) fail('definition returned no locations');
if (!defArr[0].uri.includes('SmokeTest.as')) fail(`definition uri: ${defArr[0].uri}`);
if (defArr[0].range.start.line !== 3) fail(`definition line: ${JSON.stringify(defArr[0].range)}`);

// ---- M3: hover 形参（局部/形参渲染）----
const hoverParam = responses.get(6);
if (!hoverParam || !hoverParam.result) fail(`no param hover: ${JSON.stringify(hoverParam)}`);
const paramValue = hoverParam.result.contents?.value ?? '';
if (!/float\s+Delta/.test(paramValue)) fail(`param hover missing 'float Delta': ${paramValue}`);

// ---- M4: references（Count：声明 + 2 使用点）----
const refs = responses.get(7);
if (!refs || !refs.result) fail(`no references response: ${JSON.stringify(refs)}`);
const refLocs = refs.result;
if (refLocs.length !== 3) fail(`references Count expect 3 (decl + 2 uses), got ${JSON.stringify(refLocs)}`);
const refLines = refLocs.map((l) => l.range.start.line).sort((a, b) => a - b);
if (JSON.stringify(refLines) !== '[2,5,5]') fail(`references lines: ${JSON.stringify(refLines)}`);
if (!refLocs.every((l) => l.uri.includes('SmokeTest.as'))) fail(`references uri: ${JSON.stringify(refLocs)}`);

// ---- M4: prepareRename ----
const prep = responses.get(8);
if (!prep || !prep.result) fail(`no prepareRename response: ${JSON.stringify(prep)}`);
if (prep.result.placeholder !== 'Count') fail(`prepareRename placeholder: ${JSON.stringify(prep.result)}`);
if (prep.result.range.start.line !== 2) fail(`prepareRename range: ${JSON.stringify(prep.result)}`);

// ---- M4: rename（严格匹配 + 声明名 = 3 处编辑）----
const ren = responses.get(9);
if (!ren || !ren.result) fail(`no rename response: ${JSON.stringify(ren)}`);
const edits = Object.values(ren.result.changes ?? {}).flat();
if (edits.length !== 3) fail(`rename expect 3 edits (decl + 2 uses), got ${JSON.stringify(ren.result)}`);
if (!edits.every((e) => e.newText === 'Count2')) fail(`rename newText: ${JSON.stringify(edits)}`);

// ---- M4: workspaceSymbol ----
const wss = responses.get(10);
if (!wss || !wss.result) fail(`no workspaceSymbol response: ${JSON.stringify(wss)}`);
if (!wss.result.some((s) => s.name === 'Foo')) fail(`workspaceSymbol missing Foo: ${JSON.stringify(wss.result)}`);

// ---- M4: rename 非法名（保留字）→ error ----
const bad = responses.get(11);
if (!bad || !bad.error) fail(`rename 'class' should error: ${JSON.stringify(bad)}`);

// ---- M4: $/progress（references 长任务）----
const progressMsgs = notifications.filter((n) => n.method === '$/progress');
if (progressMsgs.length === 0) fail('no $/progress notifications for references');
const beginOk = progressMsgs.some((n) => n.params?.value?.kind === 'begin');
const endOk = progressMsgs.some((n) => n.params?.value?.kind === 'end');
if (!beginOk || !endOk) {
  fail(`progress begin/end missing: ${JSON.stringify(progressMsgs.map((n) => n.params?.value))}`);
}

console.log(`SMOKE OK: legend ${legend.length} types, ${tokenCount} tokens, symbols OK, didOpen logged=${opened}, hover/definition/references/rename/workspaceSymbol OK, $/progress msgs=${progressMsgs.length}`);
p.kill();
process.exit(0);
