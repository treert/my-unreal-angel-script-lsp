/**
 * my-as-lsp VSCode 扩展（模块四，M2 最小版）：
 * languageId `angelscript-asl` + LSP client + 配置项骨架（架构设计 §5）。
 * 后续里程碑增量加命令；语法高亮走 LSP semanticTokens（tree-sitter TextMate
 * 替换是远期项）。
 */
import * as path from 'path';
import * as vscode from 'vscode';
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from 'vscode-languageclient/node';

let client: LanguageClient | undefined;

function resolveServerOptions(
  context: vscode.ExtensionContext
): ServerOptions {
  const config = vscode.workspace.getConfiguration('myAngelScriptLsp');
  const serverPath = config.get<string>('serverPath') || '';

  if (serverPath) {
    // 显式指定的可执行文件
    return {
      command: serverPath,
      args: [],
      transport: TransportKind.stdio,
    };
  }

  // 开发模式：从本仓库的 lsp workspace 用 cargo 起 server
  const manifest = path.join(context.extensionPath, '..', 'lsp', 'Cargo.toml');
  return {
    command: 'cargo',
    args: ['run', '--quiet', '-p', 'as-lsp', '--manifest-path', manifest],
    transport: TransportKind.stdio,
  };
}

export function activate(context: vscode.ExtensionContext) {
  const serverOptions = resolveServerOptions(context);

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ language: 'angelscript-asl' }],
  };

  client = new LanguageClient(
    'myAngelScriptLsp',
    'my-as-lsp',
    serverOptions,
    clientOptions
  );

  const restart = vscode.commands.registerCommand(
    'myAngelScriptLsp.restartServer',
    async () => {
      if (client) {
        await client.stop();
        client.start();
      }
    }
  );
  context.subscriptions.push(restart, client);
}

export function deactivate(): Thenable<void> | undefined {
  if (!client) {
    return undefined;
  }
  return client.stop();
}
