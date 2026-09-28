/**
 * my-as-lsp VSCode 扩展（模块四）：languageId `angelscript-asl` + LSP client
 * + 配置项骨架（架构设计 §5）+ 可观测性（仿 mylua：扩展日志通道 + 状态栏）。
 *
 * 可观测性布局（排障入口）：
 * - 输出面板「my-as-lsp (extension)」：扩展侧生命周期日志（激活、server
 *   启动命令、client 状态迁移、配置快照）；
 * - 输出面板「my-as-lsp」：vscode-languageclient 自动捕获的 server
 *   stdout/stderr（含 server 的 window/logMessage，如 didOpen 归属）；
 * - 状态栏：💛 starting → 💚 ready（tooltip 常显 floatIsFloat64 生效值——
 *   架构设计 §8 风险 11 的既定缓解项）→ ⚠️ failed。
 */
import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  State,
} from 'vscode-languageclient/node';

let client: LanguageClient | undefined;
let output: vscode.OutputChannel | undefined;
let statusBarItem: vscode.StatusBarItem | undefined;

function log(msg: string): void {
  const line = `[my-as-lsp] ${msg}`;
  // 双写：自有输出通道 + 扩展宿主控制台（console 落 exthost.log，供
  // 扩展宿主外诊断——server 未起/启动失败也能留下第一现场）
  output?.appendLine(`[${new Date().toISOString()}] ${msg}`);
  console.log(line);
}

function setStatus(text: string, tooltip: string): void {
  if (!statusBarItem) return;
  statusBarItem.text = text;
  statusBarItem.tooltip = tooltip;
}

/** `myas/indexStatus`（server 发布索引快照后的自定义通知）。 */
type IndexStatusParams = {
  state: 'ready';
  files: number;
  floatIsFloat64: boolean;
  elapsedMs?: number;
};

function renderReadyStatus(params: IndexStatusParams): void {
  const elapsed = typeof params.elapsedMs === 'number'
    ? `，耗时 ${(params.elapsedMs / 1000).toFixed(1)}s`
    : '';
  setStatus(
    '💚 my-as-lsp',
    `索引就绪：${params.files} 个文件${elapsed}\n`
      + `floatIsFloat64 = ${params.floatIsFloat64}（引擎 bScriptFloatIsFloat64；`
      + `改过 DefaultEngine.ini 须同步）— 点击打开设置`,
  );
  log(`index ready: ${params.files} files${elapsed}, floatIsFloat64=${params.floatIsFloat64}`);
}

function resolveServerOptions(
  context: vscode.ExtensionContext
): ServerOptions {
  const config = vscode.workspace.getConfiguration('myAngelScriptLsp');
  const serverPath = config.get<string>('serverPath') || '';

  // 注意：不要显式传 `transport: TransportKind.stdio`——vscode-languageclient
  // 对 Executable 形态会在显式 stdio 时**追加 `--stdio` 参数**（多传输 server
  // 的惯例，rust-analyzer 同款）。cargo 不认识该参数会直接退出码 1，server
  // 永远起不来。缺省 transport 即 stdio 且不追加任何参数。
  if (serverPath) {
    // 显式指定的可执行文件
    return {
      command: serverPath,
      args: [],
    };
  }

  // 生产模式（商店安装 / .vsix 侧载）：运行打包进扩展的 server/as-lsp(.exe)
  // （scripts/prepackage.mjs 在打包前从 lsp/target/<triple>/release/ 拷入）。
  if (context.extensionMode === vscode.ExtensionMode.Production) {
    const bin = process.platform === 'win32' ? 'as-lsp.exe' : 'as-lsp';
    const serverBin = path.join(context.extensionPath, 'server', bin);
    if (!fs.existsSync(serverBin)) {
      log(`production server binary missing: ${serverBin}`);
      setStatus(
        '⚠️ my-as-lsp',
        `my-as-lsp：未找到 server 二进制（${serverBin}）— 点击打开设置`,
      );
    }
    return {
      command: serverBin,
      args: [],
    };
  }

  // 开发模式：从本仓库的 lsp workspace 用 cargo 起 server（增量编译后启动）。
  // profile 由 tools/test-extension.ps1 写入 lsp/target/.build-profile
  //（-Release 开关），缺省 debug——不用环境变量：`code` CLI 在 VS Code
  // 已运行时只通过 IPC 转发开窗请求，EDH 不继承脚本 shell 的环境。
  const profile = readDevBuildProfile(context);
  log(`dev server profile: ${profile}`);
  const manifest = path.join(context.extensionPath, '..', 'lsp', 'Cargo.toml');
  return {
    command: 'cargo',
    args: [
      'run', '--quiet',
      ...(profile === 'release' ? ['--release'] : []),
      '-p', 'as-lsp', '--manifest-path', manifest,
    ],
  };
}

/** 读脚本（tools/test-extension.ps1）写入的 dev 构建 profile 标记。 */
function readDevBuildProfile(
  context: vscode.ExtensionContext
): 'debug' | 'release' {
  const marker = path.join(
    context.extensionPath, '..', 'lsp', 'target', '.build-profile'
  );
  try {
    const value = fs.readFileSync(marker, 'utf8').trim();
    if (value === 'release' || value === 'debug') return value;
  } catch {
    // 未 build 过（target/ 不存在）或读取失败：回退 debug
  }
  return 'debug';
}

function logConfigSnapshot(): void {
  const cfg = vscode.workspace.getConfiguration('myAngelScriptLsp');
  log(
    `config: serverPath=${JSON.stringify(cfg.get('serverPath'))}` +
    ` typeDeclarationDirs=${JSON.stringify(cfg.get('typeDeclarationDirs'))}` +
    ` scriptRoots=${JSON.stringify(cfg.get('scriptRoots'))}` +
    ` floatIsFloat64=${cfg.get('floatIsFloat64')}`
  );
}

export function activate(context: vscode.ExtensionContext) {
  output = vscode.window.createOutputChannel('my-as-lsp (extension)');
  context.subscriptions.push(output);

  statusBarItem = vscode.window.createStatusBarItem(
    vscode.StatusBarAlignment.Right,
    100,
  );
  statusBarItem.name = 'my-as-lsp';
  statusBarItem.text = '💛 my-as-lsp';
  statusBarItem.tooltip = 'my-as-lsp：正在启动 language server…';
  // 点击打开本扩展的设置页（publisher.name 来自 package.json）
  statusBarItem.command = {
    command: 'workbench.action.openSettings',
    title: 'Open my-as-lsp Settings',
    arguments: ['@ext:my-as-lsp.my-angel-script-lsp'],
  };
  statusBarItem.show();
  context.subscriptions.push(statusBarItem);

  const folders = vscode.workspace.workspaceFolders;
  log(
    `extension activated (mode=${context.extensionMode === vscode.ExtensionMode.Production ? 'production' : 'dev'}, ` +
    `folders=${folders ? folders.map((f) => f.uri.toString()).join(', ') : '<none>'})`
  );
  logConfigSnapshot();

  const serverOptions = resolveServerOptions(context);
  const args = 'args' in serverOptions ? (serverOptions.args ?? []).join(' ') : '';
  log(`server command: ${serverOptions.command} ${args}`.trimEnd());

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: 'file', language: 'angelscript-asl' }],
  };

  client = new LanguageClient(
    'myAngelScriptLsp',
    'my-as-lsp',
    serverOptions,
    clientOptions
  );

  client.onNotification('myas/indexStatus', (params: IndexStatusParams) => {
    renderReadyStatus(params);
  });

  client.onDidChangeState((e) => {
    const name = (s: State) =>
      s === State.Stopped ? 'Stopped' : s === State.Starting ? 'Starting' : s === State.Running ? 'Running' : String(s);
    log(`client state: ${name(e.oldState)} -> ${name(e.newState)}`);
    if (e.newState === State.Running) {
      setStatus('💛 my-as-lsp', 'my-as-lsp：已连接，正在构建工作区索引…（首次约数秒）');
    }
  });

  const restart = vscode.commands.registerCommand(
    'myAngelScriptLsp.restartServer',
    async () => {
      if (client) {
        log('restartServer: stopping…');
        setStatus('💛 my-as-lsp', 'my-as-lsp：正在重启…');
        await client.stop();
        client.start().then(
          () => log('restartServer: started'),
          (err) => {
            log(`restartServer failed: ${err}`);
            setStatus('⚠️ my-as-lsp', `my-as-lsp：重启失败（${err}）— 点击打开设置`);
          }
        );
      }
    }
  );
  context.subscriptions.push(restart, client);

  // 启动 client（vscode-languageclient v9 不会因 push 进 subscriptions 而自动
  // 启动——漏掉这一句时 server 根本不会被拉起，且没有任何报错）
  client.start().then(
    () => log('client started'),
    (err) => {
      log(`client start FAILED: ${err}`);
      setStatus(
        '⚠️ my-as-lsp',
        `my-as-lsp：启动失败（${err instanceof Error ? err.message : String(err)}）— 点击打开设置`,
      );
    }
  );
}

export function deactivate(): Thenable<void> | undefined {
  if (!client) {
    return undefined;
  }
  return client.stop();
}
