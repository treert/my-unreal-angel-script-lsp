// LSP server stdio smoke test: initialize -> didOpen -> semanticTokens/full
// -> documentSymbol -> hover/definition (M3) -> references/prepareRename/
// rename/workspaceSymbol (M4；$/progress 分批已随 Phase B 缓存退役删除).
// 用例源码内置（D1）。
// 用法: node tests/lsp-smoke.mjs [path-to-as-lsp.exe]
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const exe = process.argv[2]
  ?? fileURLToPath(new URL('../lsp/target/debug/as-lsp.exe', import.meta.url));

const src = 'class Foo : UObject\n{\n    int Count;\n    void Tick(float Delta)\n    {\n        Count = Count + 1;\n        auto Total = Count + 1;\n        Wide(1, 2.5);\n    }\n}\nvoid Wide(int A, float B) {}\nUFUNCTION(Blueprint\nvoid GlobalFn() {}\n';
const uri = 'file:///c%3A/tmp/SmokeTest.as';

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
// ---- M5b: completion ----
// ① `X.` 成员补全：Foo F; F.| —— Tick 体内 line 6 之前无局部，先补一版文本
//    直接复用现有文档：Count/Count 相加在 line 5；此处用 this. 的成员位
send({
  jsonrpc: '2.0', id: 12, method: 'textDocument/completion',
  params: { textDocument: { uri }, position: { line: 5, character: 21 } }, // `Count + 1` 的 1 前
});
await sleep(400);
// ② 命名实参：Print 重载 + InArgN 跳过（虚构函数验证语义；用当前文档的
//    调用语境——Tick(float Delta) 调用点没有；改用裸标识符位验证 Plain
// ---- M5b: completion ②（无前缀 Plain → 全集）----
// line 5 = `        Count = Count + 1;`：char 23 = "+ " 之后（无前缀表达式位）
send({
  jsonrpc: '2.0', id: 13, method: 'textDocument/completion',
  params: { textDocument: { uri }, position: { line: 5, character: 23 } },
});
await sleep(400);
// ---- M5c: completion ③（说明符语境：line 11 `UFUNCTION(Blueprint` 前缀
// Blueprint → BlueprintCallable 等；char 19 = "Blueprint" 尾端点）----
send({
  jsonrpc: '2.0', id: 14, method: 'textDocument/completion',
  params: { textDocument: { uri }, position: { line: 11, character: 19 } },
});
await sleep(400);
// ---- M5d: signatureHelp（line 7 `Wide(1, 2.5);`——光标在 2.5 内 → 第 2 槽）----
send({
  jsonrpc: '2.0', id: 15, method: 'textDocument/signatureHelp',
  params: { textDocument: { uri }, position: { line: 7, character: 18 } },
});
await sleep(400);
// ---- M5d: inlayHint（全文件：auto Total → ": int"）----
send({ jsonrpc: '2.0', id: 16, method: 'textDocument/inlayHint',
  params: { textDocument: { uri }, range: { start: { line: 0, character: 0 }, end: { line: 13, character: 0 } } } });
await sleep(400);
// ---- M6: publishDiagnostics 生命周期 ----
// ① didOpen 已推：parse-error（line 11 `UFUNCTION(Blueprint` 未闭合，ERROR 节点）
// ② didChange 修复 line 11（插入 "Callable)"）→ parse-error 消失（空数组推送）
// ③ didChange 行 0 插入 `))) // as-ignore: parse-error` → 造错 + 同行抑制，仍为空数组
send({
  jsonrpc: '2.0', method: 'textDocument/didChange',
  params: {
    textDocument: { uri, version: 2 },
    contentChanges: [{
      range: { start: { line: 11, character: 19 }, end: { line: 11, character: 19 } },
      text: 'Callable)',
    }],
  },
});
await sleep(500);
send({
  jsonrpc: '2.0', method: 'textDocument/didChange',
  params: {
    textDocument: { uri, version: 3 },
    contentChanges: [{
      range: { start: { line: 0, character: 0 }, end: { line: 0, character: 0 } },
      text: '))) // as-ignore: parse-error\n',
    }],
  },
});
await sleep(500);
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

// ---- M4: references（Count：声明 + 3 使用点——M5d 源加了 auto 行）----
const refs = responses.get(7);
if (!refs || !refs.result) fail(`no references response: ${JSON.stringify(refs)}`);
const refLocs = refs.result;
if (refLocs.length !== 4) fail(`references Count expect 4 (decl + 3 uses), got ${JSON.stringify(refLocs)}`);
const refLines = refLocs.map((l) => l.range.start.line).sort((a, b) => a - b);
if (JSON.stringify(refLines) !== '[2,5,5,6]') fail(`references lines: ${JSON.stringify(refLines)}`);
if (!refLocs.every((l) => l.uri.includes('SmokeTest.as'))) fail(`references uri: ${JSON.stringify(refLocs)}`);

// ---- M4: prepareRename ----
const prep = responses.get(8);
if (!prep || !prep.result) fail(`no prepareRename response: ${JSON.stringify(prep)}`);
if (prep.result.placeholder !== 'Count') fail(`prepareRename placeholder: ${JSON.stringify(prep.result)}`);
if (prep.result.range.start.line !== 2) fail(`prepareRename range: ${JSON.stringify(prep.result)}`);

// ---- M4: rename（严格匹配 + 声明名 = 4 处编辑——M5d 源加了 auto 行）----
const ren = responses.get(9);
if (!ren || !ren.result) fail(`no rename response: ${JSON.stringify(ren)}`);
const edits = Object.values(ren.result.changes ?? {}).flat();
if (edits.length !== 4) fail(`rename expect 4 edits (decl + 3 uses), got ${JSON.stringify(ren.result)}`);
if (!edits.every((e) => e.newText === 'Count2')) fail(`rename newText: ${JSON.stringify(edits)}`);

// ---- M4: workspaceSymbol ----
const wss = responses.get(10);
if (!wss || !wss.result) fail(`no workspaceSymbol response: ${JSON.stringify(wss)}`);
if (!wss.result.some((s) => s.name === 'Foo')) fail(`workspaceSymbol missing Foo: ${JSON.stringify(wss.result)}`);

// ---- M4: rename 非法名（保留字）→ error ----
const bad = responses.get(11);
if (!bad || !bad.error) fail(`rename 'class' should error: ${JSON.stringify(bad)}`);

// ---- M5b: completion ①（前缀过滤：char21 = 第二个 Count 的尾端点 →
// 前缀 "Count" → 只剩 Count；Delta/其它关键字应被过滤）----
const c1 = responses.get(12);
if (!c1 || !c1.result) fail(`no completion response #12: ${JSON.stringify(c1)}`);
const items1 = c1.result.items ?? c1.result;
if (!Array.isArray(items1) || items1.length === 0) fail(`completion #12 empty: ${JSON.stringify(c1.result)}`);
if (!items1.some((i) => i.label === 'Count')) fail(`completion #12 missing 'Count' (prefix): ${JSON.stringify(items1.map((i) => i.label))}`);
if (items1.some((i) => i.label === 'Delta')) fail(`completion #12 should filter out 'Delta': ${JSON.stringify(items1.map((i) => i.label))}`);

// ---- M5b: completion ②（无前缀 Plain：line5 char18 在 "+ " 之后 →
// 全集；局部 Delta / 成员 Count / 关键字都可见）----
const c2 = responses.get(13);
if (!c2 || !c2.result) fail(`no completion response #13: ${JSON.stringify(c2)}`);
const items2 = c2.result.items ?? c2.result;
if (!items2.some((i) => i.label === 'Delta')) fail(`completion #13 missing local 'Delta': ${JSON.stringify(items2.map((i) => i.label))}`);
if (!items2.some((i) => i.label === 'Count')) fail(`completion #13 missing member 'Count': ${JSON.stringify(items2.map((i) => i.label))}`);
if (!items2.some((i) => i.kind === 14)) fail(`completion #13 missing keyword kind: ${JSON.stringify(items2.slice(0, 5))}`);

// ---- M5c: completion ③（说明符前缀 Blueprint）----
const c3 = responses.get(14);
if (!c3 || !c3.result) fail(`no completion response #14: ${JSON.stringify(c3)}`);
const items3 = c3.result.items ?? c3.result;
const ls3 = items3.map((i) => i.label);
for (const want of ['BlueprintCallable', 'BlueprintEvent', 'BlueprintOverride', 'BlueprintPure']) {
  if (!ls3.includes(want)) fail(`specifier completion missing '${want}': ${JSON.stringify(ls3)}`);
}
if (ls3.includes('Category')) fail(`specifier prefix should filter out 'Category': ${JSON.stringify(ls3)}`);

// ---- M5d: signatureHelp ----
const sh = responses.get(15);
if (!sh || !sh.result) fail(`no signatureHelp response: ${JSON.stringify(sh)}`);
if (sh.result.signatures.length !== 1) fail(`signatureHelp expect 1 signature: ${JSON.stringify(sh.result)}`);
if (!/int\s+A,\s*float\s+B/.test(sh.result.signatures[0].label)) {
  fail(`signatureHelp label: ${sh.result.signatures[0].label}`);
}
if (sh.result.activeParameter !== 1) fail(`activeParameter expect 1: ${JSON.stringify(sh.result)}`);

// ---- M5d: inlayHint ----
const ih = responses.get(16);
if (!ih || !ih.result) fail(`no inlayHint response: ${JSON.stringify(ih)}`);
const totalHint = ih.result.find((h) => h.position.line === 6);
if (!totalHint || totalHint.label !== ': int') {
  fail(`inlayHint line6 expect ': int' for auto Total: ${JSON.stringify(ih.result)}`);
}

// ---- M4: $/progress 已随 Phase B（D38）缓存退役删除，不再断言 ----

// ---- M4 可观测性：myas/indexStatus（索引就绪自定义通知）----
// 注：冒烟场景 workspace 为空（rootUri null），初始 ready 的 files=0 是
// 正确行为（overlay 文件按需惰性索引）——数量断言在真实工作区 e2e 做
const ready = notifications.filter((n) => n.method === 'myas/indexStatus');
if (ready.length === 0) fail('no myas/indexStatus notification');
const readyParams = ready[ready.length - 1]?.params ?? {};
if (readyParams.state !== 'ready') fail(`indexStatus state: ${JSON.stringify(readyParams)}`);
if (typeof readyParams.files !== 'number') {
  fail(`indexStatus files: ${JSON.stringify(readyParams)}`);
}
if (typeof readyParams.floatIsFloat64 !== 'boolean') {
  fail(`indexStatus floatIsFloat64: ${JSON.stringify(readyParams)}`);
}

// ---- M6: publishDiagnostics（didOpen 初始推送 + didChange 修复 + 抑制注释）----
const pubs = notifications.filter((n) => n.method === 'textDocument/publishDiagnostics');
if (pubs.length === 0) fail('no publishDiagnostics notifications');
const forDoc = pubs.filter((n) => (n.params?.uri ?? '').includes('SmokeTest.as'));
if (forDoc.length === 0) {
  fail(`no publishDiagnostics for SmokeTest.as: ${JSON.stringify(pubs.map((n) => n.params?.uri))}`);
}
const hasCode = (n, c) => (n.params?.diagnostics ?? []).some((d) => d.code === c);
// 初始推送：parse-error（range 起点在 line 11 或之后）
const withErrIdx = forDoc.findIndex((n) => hasCode(n, 'parse-error'));
if (withErrIdx < 0) fail(`initial publish should contain parse-error: ${JSON.stringify(forDoc.map((n) => n.params))}`);
const errDiag = forDoc[withErrIdx].params.diagnostics.find((d) => d.code === 'parse-error');
if (errDiag.severity !== 1) fail(`parse-error severity expect 1 (Error): ${JSON.stringify(errDiag)}`);
if (errDiag.range.start.line < 11) fail(`parse-error range should be at line 11+: ${JSON.stringify(errDiag)}`);
if (forDoc.some((n) => hasCode(n, 'missing-type-decls'))) {
  fail(`missing-type-decls was removed and must not be published: ${JSON.stringify(forDoc.map((n) => n.params?.diagnostics))}`);
}
// 修复后：空数组推送（parse-error 消失）
const fixedEmpty = forDoc.slice(withErrIdx + 1).filter((n) => (n.params?.diagnostics ?? []).length === 0);
if (fixedEmpty.length === 0) {
  fail(`after fixing line 11 expect an empty publish: ${JSON.stringify(forDoc.map((n) => n.params?.diagnostics))}`);
}
// 同行抑制注释后：最终推送为空数组
const finalPub = forDoc[forDoc.length - 1];
if ((finalPub.params?.diagnostics ?? []).length !== 0) {
  fail(`final publish after suppression should be empty: ${JSON.stringify(finalPub.params)}`);
}

console.log(`SMOKE OK: legend ${legend.length} types, ${tokenCount} tokens, symbols OK, didOpen logged=${opened}, hover/definition/references/rename/workspaceSymbol/completion(+specifier)/signatureHelp/inlayHint OK, indexStatus files=${readyParams.files}, diagnostics pubs=${forDoc.length} (parse-error→fix→suppress，D40 防抖调度)`);
p.kill();
process.exit(0);
