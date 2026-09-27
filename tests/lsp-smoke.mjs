// LSP server stdio smoke test: initialize -> didOpen -> semanticTokens/full
// -> documentSymbol. 用例源码内置（D1）。
// 用法: node tests/lsp-smoke.mjs [path-to-as-lsp.exe]
import { spawn } from 'node:child_process';

const exe = process.argv[2]
  ?? 'd:/WorkGit/my-angel-script-lsp/lsp/target/debug/as-lsp.exe';

const src = 'class Foo : UObject\n{\n    int Count;\n    void Tick(float Delta) {}\n}\n';
const uri = 'file:///d%3A/WorkGit/UEProjs/SmokeTest.as';

const p = spawn(exe, [], { stdio: ['pipe', 'pipe', 'pipe'] });
let buf = '';
const responses = new Map(); // id -> parsed result
const notifications = [];

function send(body) {
  const json = JSON.stringify(body);
  const bytes = Buffer.from(json, 'utf8');
  p.stdin.write(`Content-Length: ${bytes.length}\r\n\r\n`);
  p.stdin.write(bytes);
}

p.stdout.on('data', (chunk) => {
  buf += chunk.toString('utf8');
  for (;;) {
    const headerEnd = buf.indexOf('\r\n\r\n');
    if (headerEnd < 0) break;
    const header = buf.slice(0, headerEnd);
    const m = /Content-Length: (\d+)/.exec(header);
    if (!m) { console.error('BAD HEADER:', JSON.stringify(header)); process.exit(1); }
    const len = parseInt(m[1], 10);
    if (buf.length < headerEnd + 4 + len) break;
    const body = buf.slice(headerEnd + 4, headerEnd + 4 + len);
    buf = buf.slice(headerEnd + 4 + len);
    const msg = JSON.parse(body);
    if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined)) {
      responses.set(msg.id, msg);
    } else if (msg.method) {
      notifications.push(msg);
      // server -> client 请求（如 workspace/configuration）给个空回复
      if (msg.id !== undefined) {
        send({ jsonrpc: '2.0', id: msg.id, result: [] });
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
console.log(`SMOKE OK: legend ${legend.length} types, ${tokenCount} tokens, symbols OK, didOpen logged=${opened}`);
p.kill();
process.exit(0);
